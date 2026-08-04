use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use futures_util::future::join_all;
use tokio::sync::{Mutex, Semaphore};
use tracing::{info, warn};

use scribe_common::ids::SessionId;
use scribe_common::protocol::{ServerMessage, TerminalSize};
use scribe_common::screen_replay::{SessionReplay, build_session_replay};

use crate::ipc_server::{
    AttachSessionData, AttachedSessionIds, ClientWriter, LiveSessionRegistry, SessionAttachment,
    SessionCommit, SharedWriter, TermCommit, begin_sink_attach, finish_sink_attach,
    note_unpaced_resize_apply, resize_term, send_message, set_pty_winsize,
};
use crate::session_manager::snapshot_term;
use crate::terminal_image_state::TerminalGridObserverHandle;

/// How many sessions may be in the attach replay stage at once, process-wide.
///
/// Each in-flight build holds a whole-grid `ScreenSnapshot` plus its ANSI
/// encoding — ~55 MiB at 200x50 with 10k scrollback — and `AttachSessions`
/// arrives from LAN peers, so an unbounded fan-out made transient allocation a
/// function of how many sessions one request named: 32 entries peaked at
/// 1 595 MiB on the runtime the server actually builds. Eight holds that at
/// 436 MiB, and because the encode moved to the blocking pool it still beats
/// the uncapped inline path per entry at any realistic worker count (spec 017
/// baselines, US2-3).
const MAX_CONCURRENT_REPLAY_BUILDS: usize = 8;

/// Admission control for [`MAX_CONCURRENT_REPLAY_BUILDS`]. Process-wide rather
/// than per-request, because the exposure is the sum over every connected
/// client, not one client's batch. Never closed, so `acquire` only ever fails
/// in a build that has torn the runtime down under us.
static REPLAY_BUILD_SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_REPLAY_BUILDS);

/// Per-session data carried through the attach pipeline.
///
/// The pipeline no longer needs to fan out stored metadata (title, cwd,
/// AI state, git branch, workspace info): those fields travel on the
/// `SessionList`/`SessionInfo` response the client consumed before sending
/// `AttachSessions`, so the attach reply collapses to `SessionCreated` +
/// `SessionReplay` per session.
#[derive(Clone)]
struct AttachEntry {
    session_id: SessionId,
    workspace_id: scribe_common::ids::WorkspaceId,
    shell_name: String,
    client_writer: ClientWriter,
    attachment: SessionAttachment,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    term_commit: SessionCommit,
    terminal_grid_observer: TerminalGridObserverHandle,
    resize_fd: Arc<OwnedFd>,
    target_dims: Option<TerminalSize>,
    has_handoff_snapshot: bool,
    exit_gate: Arc<crate::session_exit::SessionExitGate>,
}

impl From<AttachSessionData> for AttachEntry {
    fn from(data: AttachSessionData) -> Self {
        Self {
            session_id: data.session_id,
            workspace_id: data.workspace_id,
            shell_name: data.shell_name,
            client_writer: data.client_writer,
            attachment: data.attachment,
            term: data.term,
            term_commit: data.term_commit,
            terminal_grid_observer: data.terminal_grid_observer,
            resize_fd: data.resize_fd,
            target_dims: data.target_dims,
            has_handoff_snapshot: data.has_handoff_snapshot,
            exit_gate: data.exit_gate,
        }
    }
}

#[derive(Clone, Copy)]
pub struct AttachClientContext<'a> {
    pub writer: &'a SharedWriter,
    pub attached_ids: &'a AttachedSessionIds,
    /// Feature 015 (T012): add this connection's sink to each session's set
    /// additively (a shared-mode viewer joins) rather than replacing it (the
    /// `SingleController` / legacy takeover re-point).
    pub additive: bool,
}

pub async fn attach_sessions(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    live_sessions: &LiveSessionRegistry,
    client: AttachClientContext<'_>,
) -> HashSet<SessionId> {
    let entries = prepare_attach_entries(session_ids, dimensions, live_sessions).await;
    attach_prepared_entries(
        entries,
        client.writer,
        live_sessions,
        client.attached_ids,
        client.additive,
    )
    .await
}

/// Collapse an `AttachSessions` request to one (session, geometry) pair per
/// distinct id, keeping each id's first occurrence.
///
/// A repeated id must not fan out twice. Both entries would carry the same
/// session's `ClientWriter`, so the second attach re-opens that sink's
/// buffering window while the first is still building its replay — the two
/// `finish_attach` calls then race with different snapshot cursors — and each
/// duplicate pays for another whole-grid snapshot. Left in, replay cost is a
/// function of the request rather than of the session count, which is the
/// amplification the concurrency cap alone cannot bound.
fn distinct_attach_targets(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
) -> Vec<(SessionId, Option<TerminalSize>)> {
    let mut seen = HashSet::with_capacity(session_ids.len());
    let mut targets = Vec::with_capacity(session_ids.len());

    for (i, &session_id) in session_ids.iter().enumerate() {
        if seen.insert(session_id) {
            targets.push((session_id, dimensions.get(i).copied()));
        } else {
            warn!(%session_id, "AttachSessions: duplicate session id in one request, ignored");
        }
    }

    targets
}

async fn prepare_attach_entries(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    live_sessions: &LiveSessionRegistry,
) -> Vec<AttachEntry> {
    let targets = distinct_attach_targets(session_ids, dimensions);
    let mut sessions = live_sessions.write().await;
    let mut entries = Vec::with_capacity(targets.len());

    for (session_id, target_dims) in targets {
        if let Some(session) = sessions.get_mut(&session_id) {
            entries.push(AttachEntry::from(session.prepare_attach_data(session_id, target_dims)));
        } else {
            warn!(%session_id, "AttachSessions: session not found");
        }
    }

    entries
}

/// Run the per-session attach replay concurrently, up to
/// [`MAX_CONCURRENT_REPLAY_BUILDS`] at a time.
///
/// Each session's work (pre-snapshot resize, `SessionReplay` build, wire
/// writes, client-writer install) is an independent future, spawned so the
/// sessions in a batch overlap instead of serializing on one task. The
/// CPU-heavy steps (`snapshot_term`, `snapshot_to_ansi`, zstd compression) run
/// on the blocking pool inside [`take_session_replay`], so they neither occupy
/// a runtime worker nor scale their transient memory with the batch size. The
/// shared IPC writer is a `tokio::sync::Mutex`, which naturally serializes the
/// final wire writes without blocking the parallel snapshot work.
async fn attach_prepared_entries(
    entries: Vec<AttachEntry>,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
    additive: bool,
) -> HashSet<SessionId> {
    let mut handles = Vec::with_capacity(entries.len());
    for entry in entries {
        let writer = Arc::clone(writer);
        let live_sessions = Arc::clone(live_sessions);
        let attached_ids = Arc::clone(attached_ids);
        handles.push(tokio::spawn(async move {
            let session_id = entry.session_id;
            attach_one_session(&entry, &writer, &live_sessions, &attached_ids, additive)
                .await
                .then_some(session_id)
        }));
    }

    let joined = join_all(handles).await;
    let mut attached = HashSet::with_capacity(joined.len());
    for result in joined {
        match result {
            Ok(Some(session_id)) => {
                attached.insert(session_id);
            }
            Ok(None) => {}
            Err(e) => warn!(error = %e, "attach task panicked"),
        }
    }
    attached
}

/// Attach one session, losslessly.
///
/// The sink is installed FIRST, in the buffering state: from that moment every
/// sink-bound frame is held in emission order against the session's commit
/// cursor instead of going to a sink-less no-op send. The replay then snapshots
/// the Term together with the cursor value it reflects, and the flush replays
/// exactly the frames that snapshot is missing. Neither the pre-fix gap (frames
/// emitted between snapshot and install were lost) nor its naive inverse
/// (install first and duplicate everything the snapshot also carries) survives.
///
/// The concurrency slot is taken *before* the sink goes in, not around the
/// build alone: a sink that started buffering and then queued behind the cap
/// would keep accumulating frames it must eventually shed, and a shed backlog
/// costs a full resync replay — the cap would feed the work it exists to bound.
/// Waiting first is free instead, because the snapshot taken after the wait
/// already contains everything emitted during it — with the one exception the
/// exit-gate check below covers.
///
/// Returns whether the session ended up attached.
async fn attach_one_session(
    entry: &AttachEntry,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
    additive: bool,
) -> bool {
    let Ok(_slot) = REPLAY_BUILD_SLOTS.acquire().await else {
        warn!(session_id = %entry.session_id, "replay build admission closed; attach abandoned");
        return false;
    };
    // The wait above is the one place an attach can sit while its target dies.
    // `finalize_session_exit` claims this gate, then fans `SessionExited` out
    // with a `None` commit — a frame no replay snapshot reproduces — and then
    // drops the registry entry. Attaching afterwards would hand the client a
    // pane it can never retire, so a claimed gate ends the attach instead.
    if entry.exit_gate.is_finalized() {
        info!(session_id = %entry.session_id, "session exited while attach queued; not attaching");
        return false;
    }
    begin_sink_attach(&entry.client_writer, writer, additive).await;
    let snapshot_commit = send_attach_replay(entry, writer, live_sessions).await;
    finish_sink_attach(&entry.client_writer, writer, snapshot_commit, entry.session_id);
    install_session_attachment(entry, attached_ids).await;
    true
}

/// Resize, announce, snapshot and send the session's replay; returns the commit
/// cursor value the snapshot was taken at.
///
/// A failed replay build reports cursor 0 so the flush replays everything it
/// buffered — the client is better served by live output than by silence.
async fn send_attach_replay(
    entry: &AttachEntry,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
) -> u64 {
    let session_id = entry.session_id;

    if let Some(size) = entry.target_dims
        && !entry.has_handoff_snapshot
    {
        // Handoff-restored sessions replay the preserved pre-upgrade snapshot
        // first; resizing before that replay can make a live foreground
        // process redraw and overwrite the restored history immediately.
        if size.has_grid() {
            resize_term(&entry.term, &entry.terminal_grid_observer, size.cols, size.rows).await;
            if let Err(error) = set_pty_winsize(entry.resize_fd.as_ref(), size) {
                warn!(%session_id, "pre-snapshot TIOCSWINSZ failed: {error}");
            }
            // This apply bypasses the session's pacer, so the pacer has to be
            // told: a size a previous client's drag left armed must not mature
            // over the geometry this client just attached at, and the report
            // this client sends next belongs in the window that starts here.
            note_unpaced_resize_apply(session_id, live_sessions).await;
        }
    }

    send_message(
        writer,
        &ServerMessage::SessionCreated {
            session_id,
            workspace_id: entry.workspace_id,
            shell_name: entry.shell_name.clone(),
        },
    )
    .await;

    match take_session_replay(session_id, &entry.term, &entry.term_commit, live_sessions).await {
        Ok((replay, snapshot_commit)) => {
            send_message(writer, &ServerMessage::SessionReplay { session_id, replay }).await;
            snapshot_commit
        }
        Err(error) => {
            warn!(%session_id, "build_session_replay failed: {error}");
            0
        }
    }
}

/// Point the session at the attaching connection's attached-session set, the
/// last step of a completed attach.
async fn install_session_attachment(entry: &AttachEntry, attached_ids: &AttachedSessionIds) {
    *entry.attachment.lock().await = Some(Arc::clone(attached_ids));
    info!(session_id = %entry.session_id, "session attached to new client");
}

/// Produce a `SessionReplay` for a live session — the unified primitive that
/// both hot-reload handoff (server-to-server) and client reattach use.
///
/// Drains any legacy v4 handoff snapshot into the Term exactly once, then
/// snapshots the Term and zstd-compresses its ANSI replay. After this call,
/// subsequent attaches and server-side grid reads see the same content — the
/// Term is the durable source of truth.
///
/// The v4 legacy drain path takes a short write lock on the live-session
/// registry to extract the `handoff_snapshot` field; the common v5 case does
/// not need to touch the registry at all and keeps this pipeline lock-free
/// against other parallel attaches.
///
/// Returns the replay together with the commit-cursor value it was taken at,
/// read inside the same `Term` critical section as the snapshot — the single
/// atom that lets a buffering sink tell the frames this replay already contains
/// from the frames it is missing.
///
/// Everything after the registry lock runs on the blocking pool: the grid copy,
/// the ANSI encode and the zstd pass are tens of milliseconds of pure CPU at
/// realistic geometries, and leaving them on a runtime worker stalled every
/// other session's I/O on that thread for the duration. The `Term` lock is
/// still acquired asynchronously and simply carried into the blocking task, so
/// the lock ordering and the snapshot/commit atomicity are unchanged.
pub async fn take_session_replay(
    session_id: SessionId,
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    term_commit: &TermCommit,
    live_sessions: &LiveSessionRegistry,
) -> std::io::Result<(SessionReplay, u64)> {
    let legacy_snapshot = {
        let mut registry = live_sessions.write().await;
        registry
            .get_mut(&session_id)
            .and_then(crate::ipc_server::LiveSession::take_handoff_snapshot)
    };

    let mut guard = Arc::clone(term).lock_owned().await;
    // Safe to read here rather than after the drain below: only the owning PTY
    // reader advances the cursor, and it cannot while this guard is held.
    let commit = term_commit.get();

    tokio::task::spawn_blocking(move || {
        if let Some(snapshot) = legacy_snapshot {
            let ansi = scribe_common::screen_replay::snapshot_to_ansi(&snapshot);
            let mut processor: vte::ansi::Processor = vte::ansi::Processor::new();
            processor.advance(&mut *guard, &ansi);

            // Trim the pseudo-scrollback the encoder's leading ED 2 pushes into
            // history on a fresh grid; keep only the snapshot's true
            // scrollback_rows, then restore the configured cap.
            let scrollback_cap = guard.grid().history_size();
            let kept = (snapshot.scrollback_rows as usize).min(scrollback_cap);
            let grid = guard.grid_mut();
            grid.update_history(kept);
            grid.update_history(scrollback_cap);
        }

        let snapshot = snapshot_term(&guard);
        drop(guard);
        build_session_replay(&snapshot).map(|replay| (replay, commit))
    })
    .await
    .map_err(|error| std::io::Error::other(format!("replay build task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::sync::Arc;

    use alacritty_terminal::Term;
    use alacritty_terminal::grid::Dimensions;
    use scribe_common::framing::read_message;
    use scribe_common::ids::WorkspaceId;
    use scribe_common::protocol::ServerMessage;
    use scribe_common::screen_replay::decompress_session_replay;
    use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
    use tokio::sync::{Mutex, mpsc};

    use crate::session_manager::build_term_config;

    struct TestDimensions;

    impl Dimensions for TestDimensions {
        fn total_lines(&self) -> usize {
            1
        }

        fn screen_lines(&self) -> usize {
            1
        }

        fn columns(&self) -> usize {
            1
        }
    }

    fn unix_stream_pair() -> (tokio::net::UnixStream, tokio::net::UnixStream) {
        let (left, right) = StdUnixStream::pair().unwrap();
        left.set_nonblocking(true).unwrap();
        right.set_nonblocking(true).unwrap();
        (
            tokio::net::UnixStream::from_std(left).unwrap(),
            tokio::net::UnixStream::from_std(right).unwrap(),
        )
    }

    fn make_term(session_id: SessionId) -> Term<ScribeEventListener> {
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<SessionEvent>();
        let listener = ScribeEventListener::new(session_id, event_tx);
        Term::new(build_term_config(1), &TestDimensions, listener)
    }

    fn sample_entry(session_id: SessionId, workspace_id: WorkspaceId) -> AttachEntry {
        AttachEntry {
            session_id,
            workspace_id,
            shell_name: String::from("zsh"),
            client_writer: Arc::new(std::sync::Mutex::new(
                crate::ipc_server::AttachedSinks::default(),
            )),
            attachment: Arc::new(Mutex::new(None)),
            term: Arc::new(Mutex::new(make_term(session_id))),
            term_commit: Arc::new(TermCommit::default()),
            terminal_grid_observer: TerminalGridObserverHandle::default(),
            resize_fd: Arc::new(std::fs::File::open("/dev/null").unwrap().into()),
            target_dims: None,
            has_handoff_snapshot: false,
            exit_gate: Arc::new(crate::session_exit::SessionExitGate::new()),
        }
    }

    #[tokio::test]
    async fn send_attach_replay_emits_session_created_then_session_replay() {
        let live_sessions = crate::ipc_server::new_live_session_registry();
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let entry = sample_entry(session_id, workspace_id);

        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer: SharedWriter = crate::ipc_server::test_shared_writer(server_write);

        send_attach_replay(&entry, &writer, &live_sessions).await;

        assert!(crate::ipc_server::lock_sinks(&entry.client_writer).is_empty());

        let msg1 = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        let ServerMessage::SessionCreated { session_id: got_id, workspace_id: got_ws, shell_name } =
            msg1
        else {
            panic!("expected SessionCreated, got {msg1:?}");
        };
        assert_eq!(got_id, session_id);
        assert_eq!(got_ws, workspace_id);
        assert_eq!(shell_name, "zsh");

        let msg2 = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        let ServerMessage::SessionReplay { session_id: replay_id, replay } = msg2 else {
            panic!("expected SessionReplay, got {msg2:?}");
        };
        assert_eq!(replay_id, session_id);
        assert!(!replay.replay_zstd.is_empty());
        let ansi = decompress_session_replay(&replay).expect("decompress");
        assert!(!ansi.is_empty(), "replay ANSI bytes must be non-empty");
    }

    #[tokio::test]
    async fn attach_prepared_entries_runs_all_sessions_concurrently() {
        let live_sessions = crate::ipc_server::new_live_session_registry();
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();

        let entries = vec![
            sample_entry(SessionId::new(), workspace_a),
            sample_entry(SessionId::new(), workspace_b),
        ];
        let expected_ids: HashSet<SessionId> = entries.iter().map(|e| e.session_id).collect();

        let (server, _client) = unix_stream_pair();
        let (_read, write) = tokio::io::split(server);
        let writer: SharedWriter = crate::ipc_server::test_shared_writer(write);
        let attached_ids = Arc::new(Mutex::new(HashSet::new()));

        let attached =
            attach_prepared_entries(entries, &writer, &live_sessions, &attached_ids, false).await;

        assert_eq!(attached, expected_ids);
    }

    /// A repeated session id in one `AttachSessions` must fan out once. Two
    /// entries would share the session's sink and each pay for a full snapshot,
    /// making replay cost a function of the request rather than the session
    /// count.
    #[test]
    fn duplicate_session_ids_collapse_to_one_attach_target() {
        let first = SessionId::new();
        let second = SessionId::new();
        let big = TerminalSize { cols: 200, rows: 50, cell_width: 0, cell_height: 0 };
        let small = TerminalSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 };

        let targets =
            distinct_attach_targets(&[first, second, first, first], &[big, small, small, small]);

        assert_eq!(targets, vec![(first, Some(big)), (second, Some(small))]);
    }

    /// The fan-out must not start a ninth replay build while eight are in
    /// flight: each holds a whole-grid snapshot plus its ANSI encoding, and
    /// `AttachSessions` is reachable from LAN peers.
    #[tokio::test]
    async fn replay_builds_queue_behind_the_concurrency_cap() {
        let permits = u32::try_from(MAX_CONCURRENT_REPLAY_BUILDS).unwrap();
        let hogged = REPLAY_BUILD_SLOTS.acquire_many(permits).await.unwrap();

        let live_sessions = crate::ipc_server::new_live_session_registry();
        let session_id = SessionId::new();
        let entries = vec![sample_entry(session_id, WorkspaceId::new())];

        let (server, _client) = unix_stream_pair();
        let (_read, write) = tokio::io::split(server);
        let writer: SharedWriter = crate::ipc_server::test_shared_writer(write);
        let attached_ids: AttachedSessionIds = Arc::new(Mutex::new(HashSet::new()));

        let mut fan_out = tokio::spawn(async move {
            attach_prepared_entries(entries, &writer, &live_sessions, &attached_ids, false).await
        });

        // Deterministic, not a race: with every slot held the attach cannot
        // reach its snapshot, so this window can only expire.
        let stalled =
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut fan_out).await;
        assert!(stalled.is_err(), "attach ran with no replay slot available");

        drop(hogged);
        let attached = fan_out.await.unwrap();
        assert_eq!(attached, HashSet::from([session_id]));
    }

    /// Queueing behind the cap is the one window in which an attach's target can
    /// exit under it. `SessionExited` fans out with a `None` commit, so no
    /// replay snapshot reproduces it; attaching anyway would install a sink on
    /// a dead session and leave the client a pane it can never retire.
    #[tokio::test]
    async fn attach_skips_a_session_that_exited_while_queued() {
        let live_sessions = crate::ipc_server::new_live_session_registry();
        let entry = sample_entry(SessionId::new(), WorkspaceId::new());
        let client_writer = Arc::clone(&entry.client_writer);
        assert!(entry.exit_gate.claim_exit(), "a fresh entry's gate must start unclaimed");

        let (server, _client) = unix_stream_pair();
        let (_read, write) = tokio::io::split(server);
        let writer: SharedWriter = crate::ipc_server::test_shared_writer(write);
        let attached_ids: AttachedSessionIds = Arc::new(Mutex::new(HashSet::new()));

        let attached =
            attach_prepared_entries(vec![entry], &writer, &live_sessions, &attached_ids, false)
                .await;

        assert!(attached.is_empty(), "an exited session must not report as attached");
        assert!(
            crate::ipc_server::lock_sinks(&client_writer).is_empty(),
            "no sink may be installed on a session that already exited"
        );
    }
}
