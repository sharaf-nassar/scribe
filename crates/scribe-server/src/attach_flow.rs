use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use futures_util::future::join_all;
use tokio::sync::Mutex;
use tracing::{info, warn};

use scribe_common::ids::SessionId;
use scribe_common::protocol::{ServerMessage, TerminalSize};
use scribe_common::screen_replay::{SessionReplay, build_session_replay};

use crate::ipc_server::{
    AttachSessionData, AttachedSessionIds, ClientWriter, LiveSessionRegistry, SessionAttachment,
    SessionCommit, SharedWriter, TermCommit, begin_sink_attach, finish_sink_attach, resize_term,
    send_message, set_pty_winsize,
};
use crate::session_manager::snapshot_term;

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
    resize_fd: Arc<OwnedFd>,
    target_dims: Option<TerminalSize>,
    has_handoff_snapshot: bool,
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
            resize_fd: data.resize_fd,
            target_dims: data.target_dims,
            has_handoff_snapshot: data.has_handoff_snapshot,
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

async fn prepare_attach_entries(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    live_sessions: &LiveSessionRegistry,
) -> Vec<AttachEntry> {
    let mut sessions = live_sessions.write().await;
    let mut entries = Vec::with_capacity(session_ids.len());

    for (i, &session_id) in session_ids.iter().enumerate() {
        let target_dims = dimensions.get(i).copied();
        if let Some(session) = sessions.get_mut(&session_id) {
            entries.push(AttachEntry::from(session.prepare_attach_data(session_id, target_dims)));
        } else {
            warn!(%session_id, "AttachSessions: session not found");
        }
    }

    entries
}

/// Run the per-session attach replay concurrently.
///
/// Each session's work (pre-snapshot resize, `SessionReplay` build, wire
/// writes, client-writer install) is an independent future. We spawn them
/// onto the tokio runtime so CPU-heavy steps (`snapshot_term`,
/// `snapshot_to_ansi`, zstd compression) can use separate worker threads
/// instead of serializing on one task. The shared IPC writer is a
/// `tokio::sync::Mutex`, which naturally serializes the final wire writes
/// without blocking the parallel snapshot work.
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
            attach_one_session(&entry, &writer, &live_sessions, &attached_ids, additive).await;
            session_id
        }));
    }

    let joined = join_all(handles).await;
    let mut attached = HashSet::with_capacity(joined.len());
    for result in joined {
        match result {
            Ok(session_id) => {
                attached.insert(session_id);
            }
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
async fn attach_one_session(
    entry: &AttachEntry,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
    additive: bool,
) {
    begin_sink_attach(&entry.client_writer, writer, additive).await;
    let snapshot_commit = send_attach_replay(entry, writer, live_sessions).await;
    finish_sink_attach(&entry.client_writer, writer, snapshot_commit, entry.session_id);
    install_session_attachment(entry, attached_ids).await;
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
            resize_term(&entry.term, size.cols, size.rows).await;
            if let Err(error) = set_pty_winsize(entry.resize_fd.as_ref(), size) {
                warn!(%session_id, "pre-snapshot TIOCSWINSZ failed: {error}");
            }
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

    if let Some(snapshot) = legacy_snapshot {
        let ansi = scribe_common::screen_replay::snapshot_to_ansi(&snapshot);
        let mut processor: vte::ansi::Processor = vte::ansi::Processor::new();
        let mut guard = term.lock().await;
        processor.advance(&mut *guard, &ansi);

        // Trim the pseudo-scrollback the encoder's leading ED 2 pushes into
        // history on a fresh grid; keep only the snapshot's true
        // scrollback_rows, then restore the configured cap.
        let scrollback_cap = guard.grid().history_size();
        let kept = (snapshot.scrollback_rows as usize).min(scrollback_cap);
        let grid = guard.grid_mut();
        grid.update_history(kept);
        grid.update_history(scrollback_cap);

        let fresh = snapshot_term(&guard);
        let commit = term_commit.get();
        drop(guard);
        return build_session_replay(&fresh).map(|replay| (replay, commit));
    }

    let guard = term.lock().await;
    let snapshot = snapshot_term(&guard);
    let commit = term_commit.get();
    drop(guard);
    build_session_replay(&snapshot).map(|replay| (replay, commit))
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
            resize_fd: Arc::new(std::fs::File::open("/dev/null").unwrap().into()),
            target_dims: None,
            has_handoff_snapshot: false,
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
}
