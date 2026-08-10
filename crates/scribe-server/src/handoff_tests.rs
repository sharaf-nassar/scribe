//! Integration tests for the hot-reload handoff flow.
//!
//! These tests verify that sessions survive the serialise → restore → activate
//! cycle that occurs during a zero-downtime server upgrade. They use real PTY
//! pairs (via `openpty`) to exercise the full data path without forking child
//! processes or using the IPC socket layer.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use vte::ansi::Processor as AnsiProcessor;

use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::SessionContext;
use scribe_common::screen::{
    CellFlags, CursorStyle, DecPrivateMode, ScreenCell, ScreenColor, ScreenSnapshot,
};
use scribe_common::screen_replay::build_session_replay;
use scribe_pty::event_listener::ScribeEventListener;

use crate::handoff::{HandoffSession, HandoffState};
use crate::ipc_server::{self, LiveSessionRegistry};
use crate::session_manager::{SessionManager, build_term_config, snapshot_term};
use crate::workspace_manager::WorkspaceManager;

#[derive(Clone, Copy)]
struct TestDims {
    cols: usize,
    rows: usize,
}

impl Dimensions for TestDims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Build a fresh Term, feed it bytes through `AnsiProcessor`, and return it.
fn term_with_bytes(bytes: &[u8], cols: usize, rows: usize) -> Term<ScribeEventListener> {
    let (tx, _rx) = mpsc::unbounded_channel();
    let listener = ScribeEventListener::new(SessionId::new(), tx);
    let config = build_term_config(100);
    let mut term = Term::new(config, &TestDims { cols, rows }, listener);
    let mut processor: AnsiProcessor = AnsiProcessor::new();
    processor.advance(&mut term, bytes);
    term
}

/// Build a single-session `HandoffState` whose payload is a v5 replay built
/// from the given `Term`.
fn make_v5_state(term: &Term<ScribeEventListener>) -> (HandoffState, Vec<OwnedFd>, Vec<OwnedFd>) {
    let snap = snapshot_term(term);
    let replay = build_session_replay(&snap).expect("build_session_replay");

    let pty = nix::pty::openpty(None, None).unwrap();
    let session = HandoffSession {
        session_id: SessionId::new(),
        workspace_id: WorkspaceId::new(),
        child_pid: std::process::id(),
        child_identity: crate::child_identity::read_child_identity(std::process::id()),
        cols: snap.cols,
        rows: snap.rows,
        cell_width: 1,
        cell_height: 1,
        snapshot: None,
        session_replay: Some(replay),
        title: None,
        icon_title: None,
        shell_name: String::from("zsh"),
        task_label: None,
        codex_task_label: None,
        cwd: None,
        context: None,
        ai_state: None,
        ai_provider_hint: None,
        prompt_state: None,
        image_state: None,
    };

    let state = HandoffState {
        version: 5,
        sessions: vec![session],
        workspaces: vec![],
        workspace_tree: None,
        windows: vec![],
    };

    (state, vec![pty.master], vec![pty.slave])
}

// ── Helpers ─────────────────────────────────────────────────────

/// Create a minimal `ScreenSnapshot` for testing.
fn dummy_snapshot(cols: u16, rows: u16) -> ScreenSnapshot {
    let cell = ScreenCell {
        c: ' ',
        fg: ScreenColor::Named(0),
        bg: ScreenColor::Named(0),
        flags: CellFlags::default(),
    };

    ScreenSnapshot {
        cells: vec![cell; usize::from(cols) * usize::from(rows)],
        cols,
        rows,
        cursor_col: 0,
        cursor_row: 0,
        cursor_style: CursorStyle::Block,
        cursor_visible: false,
        alt_screen: true,
        active_dec_modes: Vec::new(),
        scrollback: Vec::new(),
        scrollback_rows: 0,
    }
}

/// Build a `HandoffState` with `n` sessions, each backed by a real PTY pair.
///
/// Returns `(state, master_fds, slave_fds)`. The **slave fds must be kept
/// alive** in tests that start PTY reader tasks — dropping them causes
/// the reader to see EOF and remove the session from the registry.
fn make_handoff_state(n: usize) -> (HandoffState, Vec<OwnedFd>, Vec<OwnedFd>) {
    let mut masters = Vec::with_capacity(n);
    let mut slaves = Vec::with_capacity(n);
    let mut sessions = Vec::with_capacity(n);

    for _ in 0..n {
        let pty = nix::pty::openpty(None, None).unwrap();
        sessions.push(HandoffSession {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            child_pid: std::process::id(),
            child_identity: crate::child_identity::read_child_identity(std::process::id()),
            cols: 80,
            rows: 24,
            cell_width: 1,
            cell_height: 1,
            snapshot: Some(dummy_snapshot(80, 24)),
            session_replay: None,
            title: None,
            icon_title: None,
            shell_name: String::from("zsh"),
            task_label: None,
            codex_task_label: None,
            cwd: None,
            context: Some(SessionContext {
                remote: true,
                host: Some(String::from("builder")),
                tmux_session: Some(String::from("editor")),
            }),
            ai_state: None,
            ai_provider_hint: None,
            prompt_state: None,
            image_state: None,
        });
        masters.push(pty.master);
        slaves.push(pty.slave);
    }

    let state = HandoffState {
        version: 4,
        sessions,
        workspaces: vec![],
        workspace_tree: None,
        windows: vec![],
    };

    (state, masters, slaves)
}

/// Poll until the live registry reaches the expected session count.
/// Panics after 500 ms if the count is never reached.
async fn wait_registry_count(registry: &LiveSessionRegistry, expected: usize) {
    for _ in 0..50 {
        if registry.read().await.len() == expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let actual = registry.read().await.len();
    panic!("registry has {actual} sessions, expected {expected} (timed out after 500 ms)");
}

// ── Tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn restore_from_handoff_populates_session_manager() {
    let (mut state, masters, _slaves) = make_handoff_state(1);
    state.sessions[0].title = Some(String::from("window"));
    state.sessions[0].icon_title = Some(String::from("icon"));
    let expected_id = state.sessions[0].session_id;

    let sm = SessionManager::restore_from_handoff(&state, masters, 100).unwrap();

    let pending = sm.pending_session_ids().await;
    assert_eq!(pending.len(), 1, "restored session should be pending");
    assert_eq!(pending[0].0, expected_id);

    let restored = sm.take_session(expected_id).await.unwrap();
    assert_eq!(restored.shell_name, "zsh");
    assert_eq!(restored.title.as_deref(), Some("window"));
    assert_eq!(restored.icon_title.as_deref(), Some("icon"));
    assert_eq!(
        restored.context.as_ref().and_then(|context| context.host.as_deref()),
        Some("builder")
    );
}

#[tokio::test]
async fn activate_moves_sessions_to_live_registry() {
    let (state, masters, _slaves) = make_handoff_state(2);
    let expected_ids: Vec<SessionId> = state.sessions.iter().map(|s| s.session_id).collect();

    let sm = Arc::new(SessionManager::restore_from_handoff(&state, masters, 100).unwrap());
    let wm = Arc::new(RwLock::new(WorkspaceManager::new(vec![])));
    let registry = ipc_server::new_live_session_registry();

    let shares = ipc_server::new_window_shares();
    ipc_server::activate_pending_sessions(&sm, &wm, &registry, &shares).await;

    // SessionManager should now be empty.
    assert!(
        sm.pending_session_ids().await.is_empty(),
        "sessions should have been taken from SessionManager"
    );

    // Wait for spawned insert tasks to complete.
    wait_registry_count(&registry, 2).await;

    let live = registry.read().await;
    for id in &expected_ids {
        assert!(live.contains_key(id), "session {id} missing from registry");
    }
}

#[tokio::test]
async fn serialize_live_returns_activated_sessions() {
    let (mut state, masters, _slaves) = make_handoff_state(1);
    state.sessions[0].title = Some(String::from("window"));
    state.sessions[0].icon_title = Some(String::from("icon"));
    let expected_id = state.sessions[0].session_id;

    let sm = Arc::new(SessionManager::restore_from_handoff(&state, masters, 100).unwrap());
    let wm = Arc::new(RwLock::new(WorkspaceManager::new(vec![])));
    let registry = ipc_server::new_live_session_registry();

    let shares = ipc_server::new_window_shares();
    ipc_server::activate_pending_sessions(&sm, &wm, &registry, &shares).await;
    wait_registry_count(&registry, 1).await;

    let (sessions, fds) = ipc_server::serialize_live_for_handoff(&registry).await;

    assert_eq!(sessions.len(), 1);
    assert_eq!(fds.len(), 1);
    assert_eq!(sessions[0].session_id, expected_id);
    assert!(sessions[0].session_replay.is_some(), "v5 sender must populate session_replay");
    assert!(sessions[0].snapshot.is_none(), "v5 sender must leave legacy snapshot None");
    assert_eq!(sessions[0].shell_name, "zsh");
    assert_eq!(sessions[0].title.as_deref(), Some("window"));
    assert_eq!(sessions[0].icon_title.as_deref(), Some("icon"));
    assert_eq!(
        sessions[0].context.as_ref().and_then(|context| context.host.as_deref()),
        Some("builder")
    );
}

#[tokio::test]
async fn v4_legacy_handoff_snapshot_is_restored_durably() {
    // v4 compat path: the sender populated `snapshot` (legacy), not
    // `session_replay`. On first attach we feed the snapshot through
    // AnsiProcessor into the Term, then every attach sees the same content.
    let (state, masters, _slaves) = make_handoff_state(1);
    let session_id = state.sessions[0].session_id;

    let sm = Arc::new(SessionManager::restore_from_handoff(&state, masters, 100).unwrap());
    let wm = Arc::new(RwLock::new(WorkspaceManager::new(vec![])));
    let registry = ipc_server::new_live_session_registry();

    let shares = ipc_server::new_window_shares();
    ipc_server::activate_pending_sessions(&sm, &wm, &registry, &shares).await;
    wait_registry_count(&registry, 1).await;

    // The legacy handoff snapshot had alt_screen=true and cursor_visible=false.
    // A blank Term would have alt_screen=false and cursor_visible=true.
    {
        let live = registry.read().await;
        let session = live.get(&session_id).unwrap();
        assert!(
            session.handoff_snapshot.is_some(),
            "v4 handoff snapshot should be stored in LiveSession pending first attach"
        );
    }

    // Simulate what handle_attach_sessions does: take_session_replay.
    let (term, term_commit) = {
        let live = registry.read().await;
        let session = live.get(&session_id).unwrap();
        (Arc::clone(&session.term), Arc::clone(&session.term_commit))
    };
    let (first, _) =
        crate::attach_flow::take_session_replay(session_id, &term, &term_commit, &registry)
            .await
            .expect("first take_session_replay");
    assert!(first.alt_screen, "first attach should see legacy snapshot (alt_screen=true)");
    assert!(
        !first.cursor_visible,
        "first attach should see legacy snapshot (cursor_visible=false)"
    );

    // Second attach MUST see the same content — the snapshot was fed into
    // the Term durably. This is the regression guard against the
    // pre-v5 "first-attach-only" bug.
    let (second, _) =
        crate::attach_flow::take_session_replay(session_id, &term, &term_commit, &registry)
            .await
            .expect("second take_session_replay");
    assert!(
        second.alt_screen,
        "second attach must see the durably-restored Term (alt_screen=true)"
    );
    assert!(
        !second.cursor_visible,
        "second attach must see the durably-restored Term (cursor_visible=false)"
    );
}

#[tokio::test]
async fn restore_from_handoff_v5_replay_populates_term_durably() {
    // Source Term with identifiable pre-handoff content.
    let src = term_with_bytes(b"pre-handoff-scrollback\r\nline two\r\n", 80, 24);
    let src_snap = snapshot_term(&src);

    let (state, masters, _slaves) = make_v5_state(&src);
    let session_id = state.sessions[0].session_id;

    let sm = SessionManager::restore_from_handoff(&state, masters, 100).unwrap();

    // v5 path must NOT store a handoff_snapshot — the Term carries state.
    let restored = sm.take_session(session_id).await.expect("restored session");
    assert!(
        restored.handoff_snapshot.is_none(),
        "v5 replay restore must leave handoff_snapshot None; the Term owns the state"
    );

    // Snapshotting the restored Term must reproduce the source's cells.
    let restored_term = restored.term.lock().await;
    let after = snapshot_term(&restored_term);
    assert_eq!(after.cells, src_snap.cells, "visible grid must match source");
    assert_eq!(after.scrollback, src_snap.scrollback, "scrollback must match source");
}

#[tokio::test]
async fn restore_from_handoff_v5_restored_term_survives_multiple_snapshots() {
    // Regression guard against the latent "Term empty after first attach"
    // bug: with v5, the Term is durably populated, so repeated
    // snapshot_term() calls must each produce the same cells.
    let src = term_with_bytes(b"durable content\r\n", 80, 24);
    let src_snap = snapshot_term(&src);

    let (state, masters, _slaves) = make_v5_state(&src);
    let session_id = state.sessions[0].session_id;

    let sm = SessionManager::restore_from_handoff(&state, masters, 100).unwrap();
    let restored = sm.take_session(session_id).await.expect("restored session");

    let first = {
        let guard = restored.term.lock().await;
        snapshot_term(&guard)
    };
    let second = {
        let guard = restored.term.lock().await;
        snapshot_term(&guard)
    };

    assert_eq!(first.cells, src_snap.cells, "first snapshot must match source");
    assert_eq!(first.cells, second.cells, "second snapshot must match first");
}

#[test]
fn dec_private_modes_round_trip_through_ansi_into_term() {
    // F001: a ScreenSnapshot's DEC private mode flags must survive the
    // snapshot_to_ansi -> feed-into-Term path. The encoder emits DECSET
    // sequences for the enabled flags; the VTE processor must set the
    // corresponding TermMode bits on a fresh Term.
    use alacritty_terminal::term::TermMode;
    use scribe_common::screen_replay::snapshot_to_ansi;

    let mut snapshot = dummy_snapshot(80, 24);
    // dummy_snapshot leaves active_dec_modes empty except alt_screen; enable the
    // three under test and drop alt_screen so we exercise the main screen.
    snapshot.alt_screen = false;
    snapshot.active_dec_modes = vec![
        DecPrivateMode::MouseReportClick, // DECSET 1000 -> MOUSE_REPORT_CLICK
        DecPrivateMode::SgrMouse,         // DECSET 1006 -> SGR_MOUSE
        DecPrivateMode::BracketedPaste,   // DECSET 2004 -> BRACKETED_PASTE
        DecPrivateMode::AppCursor,        // DECCKM (\x1b[?1h) -> APP_CURSOR
        DecPrivateMode::AppKeypad,        // DECPAM (\x1b=)    -> APP_KEYPAD
    ];

    let ansi = snapshot_to_ansi(&snapshot);
    let term = term_with_bytes(&ansi, 80, 24);
    let mode = term.mode();

    assert!(
        mode.contains(TermMode::MOUSE_REPORT_CLICK),
        "restored Term must have MOUSE_REPORT_CLICK set from DECSET 1000"
    );
    assert!(
        mode.contains(TermMode::SGR_MOUSE),
        "restored Term must have SGR_MOUSE set from DECSET 1006"
    );
    assert!(
        mode.contains(TermMode::BRACKETED_PASTE),
        "restored Term must have BRACKETED_PASTE set from DECSET 2004"
    );
    // The two non-CSI escapes (DECCKM `\x1b[?1h`, DECPAM `\x1b=`) are verified
    // via Term mode bits rather than a short raw-byte scan, since `\x1b=` has
    // no disambiguating prefix and is collision-prone in a byte search.
    assert!(
        mode.contains(TermMode::APP_CURSOR),
        "restored Term must have APP_CURSOR set from DECCKM"
    );
    assert!(
        mode.contains(TermMode::APP_KEYPAD),
        "restored Term must have APP_KEYPAD set from DECPAM"
    );

    // Cross-check via snapshot_term: re-snapshotting the restored Term must
    // report the same flags, proving the full snapshot -> ANSI -> Term ->
    // snapshot round-trip preserves DEC private modes.
    let restored_snap = snapshot_term(&term);
    assert!(
        restored_snap.active_dec_modes.contains(&DecPrivateMode::MouseReportClick),
        "round-trip lost mouse_report_click"
    );
    assert!(
        restored_snap.active_dec_modes.contains(&DecPrivateMode::SgrMouse),
        "round-trip lost sgr_mouse"
    );
    assert!(
        restored_snap.active_dec_modes.contains(&DecPrivateMode::BracketedPaste),
        "round-trip lost bracketed_paste"
    );
    assert!(
        restored_snap.active_dec_modes.contains(&DecPrivateMode::AppCursor),
        "round-trip lost app_cursor"
    );
    assert!(
        restored_snap.active_dec_modes.contains(&DecPrivateMode::AppKeypad),
        "round-trip lost app_keypad"
    );
}

// ── Spec 017 US7-2: child-identity wire compatibility ───────────────

/// `HandoffSession` as it was shaped before the US7-2 child-identity field —
/// what an N-1 sender puts on the wire. Named `MessagePack` encoding means the
/// only difference that matters is the missing key.
#[derive(serde::Serialize, serde::Deserialize)]
struct PriorHandoffSession {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    child_pid: u32,
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
    snapshot: Option<ScreenSnapshot>,
    session_replay: Option<scribe_common::screen_replay::SessionReplay>,
    title: Option<String>,
    shell_name: String,
    task_label: Option<String>,
    codex_task_label: Option<String>,
    cwd: Option<std::path::PathBuf>,
    context: Option<SessionContext>,
    ai_state: Option<scribe_common::ai_state::AiProcessState>,
    ai_provider_hint: Option<scribe_common::ai_state::AiProvider>,
}

#[derive(serde::Serialize)]
struct PriorHandoffState {
    version: u32,
    sessions: Vec<PriorHandoffSession>,
    workspaces: Vec<crate::handoff::HandoffWorkspace>,
    workspace_tree: Option<scribe_common::protocol::WorkspaceTreeNode>,
    windows: Vec<crate::handoff::HandoffWindowState>,
}

fn prior_session(child_pid: u32) -> PriorHandoffSession {
    PriorHandoffSession {
        session_id: SessionId::new(),
        workspace_id: WorkspaceId::new(),
        child_pid,
        cols: 80,
        rows: 24,
        cell_width: 8,
        cell_height: 16,
        snapshot: None,
        session_replay: None,
        title: Some(String::from("editor")),
        shell_name: String::from("zsh"),
        task_label: None,
        codex_task_label: None,
        cwd: Some(std::path::PathBuf::from("/tmp")),
        context: None,
        ai_state: None,
        ai_provider_hint: None,
    }
}

/// An N-1 payload carries no identity, so the receiver must decode it and
/// treat the child as unproven rather than reject the handoff.
#[test]
fn prior_version_payload_decodes_with_absent_child_identity() {
    let state = PriorHandoffState {
        version: 5,
        sessions: vec![prior_session(4242)],
        workspaces: vec![],
        workspace_tree: None,
        windows: vec![],
    };

    let bytes = rmp_serde::to_vec_named(&state).unwrap();
    let decoded: HandoffState = rmp_serde::from_slice(&bytes).unwrap();

    assert_eq!(decoded.version, 5);
    let session = decoded.sessions.first().expect("one session");
    assert_eq!(session.child_identity, None, "absent field must default to None");
    assert_eq!(session.child_pid, 4242);
    assert_eq!(session.title.as_deref(), Some("editor"));
    assert_eq!(session.icon_title, None, "absent field must default to None");
    assert_eq!(session.shell_name, "zsh");
    assert_eq!(session.cell_width, 8);
    assert_eq!(session.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
}

/// A current-version payload round-trips the identity so the successor can
/// prove the PID before hanging the child up.
#[test]
fn current_version_payload_round_trips_child_identity() {
    let (mut state, _masters, _slaves) = make_handoff_state(1);
    state.version = 6;
    let recorded = crate::child_identity::read_child_identity(std::process::id());
    state.sessions.first_mut().expect("one session").child_identity = recorded;
    state.sessions.first_mut().expect("one session").icon_title = Some(String::from("icon"));

    let bytes = rmp_serde::to_vec_named(&state).unwrap();
    let decoded: HandoffState = rmp_serde::from_slice(&bytes).unwrap();

    assert_eq!(decoded.sessions.first().expect("one session").child_identity, recorded);
    assert_eq!(decoded.sessions.first().expect("one session").icon_title.as_deref(), Some("icon"));
}

/// The reverse direction of the same upgrade rehearsal: an N-1 receiver must
/// ignore the key it has never heard of instead of failing the decode.
#[test]
fn prior_version_receiver_ignores_child_identity_key() {
    let (mut state, _masters, _slaves) = make_handoff_state(1);
    state.sessions.first_mut().expect("one session").child_identity = Some(987_654);

    let bytes = rmp_serde::to_vec_named(&state.sessions).unwrap();
    let decoded: Vec<PriorHandoffSession> = rmp_serde::from_slice(&bytes).unwrap();

    assert_eq!(decoded.first().expect("one session").shell_name, "zsh");
}
