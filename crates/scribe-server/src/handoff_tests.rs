//! Integration tests for the hot-reload handoff flow.
//!
//! These tests verify that sessions survive the serialise → restore → activate
//! cycle that occurs during a zero-downtime server upgrade. They use real PTY
//! pairs (via `openpty`) to exercise the full data path without forking child
//! processes or using the IPC socket layer.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use tokio::sync::RwLock;

use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::SessionContext;
use scribe_common::screen::{CellFlags, CursorStyle, ScreenCell, ScreenColor, ScreenSnapshot};

use crate::handoff::{HandoffSession, HandoffState};
use crate::ipc_server::{self, LiveSessionRegistry};
use crate::session_manager::SessionManager;
use crate::workspace_manager::WorkspaceManager;

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
            cols: 80,
            rows: 24,
            cell_width: 1,
            cell_height: 1,
            snapshot: Some(dummy_snapshot(80, 24)),
            title: None,
            shell_name: String::from("zsh"),
            codex_task_label: None,
            cwd: None,
            context: Some(SessionContext {
                remote: true,
                host: Some(String::from("builder")),
                tmux_session: Some(String::from("editor")),
            }),
            ai_state: None,
            ai_provider_hint: None,
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
    let (state, masters, _slaves) = make_handoff_state(1);
    let expected_id = state.sessions[0].session_id;

    let sm = SessionManager::restore_from_handoff(&state, masters, 100).unwrap();

    let pending = sm.pending_session_ids().await;
    assert_eq!(pending.len(), 1, "restored session should be pending");
    assert_eq!(pending[0].0, expected_id);

    let restored = sm.take_session(expected_id).await.unwrap();
    assert_eq!(restored.shell_name, "zsh");
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

    ipc_server::activate_pending_sessions(&sm, &wm, &registry).await;

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
    let (state, masters, _slaves) = make_handoff_state(1);
    let expected_id = state.sessions[0].session_id;

    let sm = Arc::new(SessionManager::restore_from_handoff(&state, masters, 100).unwrap());
    let wm = Arc::new(RwLock::new(WorkspaceManager::new(vec![])));
    let registry = ipc_server::new_live_session_registry();

    ipc_server::activate_pending_sessions(&sm, &wm, &registry).await;
    wait_registry_count(&registry, 1).await;

    let (sessions, fds) = ipc_server::serialize_live_for_handoff(&registry).await;

    assert_eq!(sessions.len(), 1);
    assert_eq!(fds.len(), 1);
    assert_eq!(sessions[0].session_id, expected_id);
    assert!(sessions[0].snapshot.is_some(), "snapshot should be included");
    assert_eq!(sessions[0].shell_name, "zsh");
    assert_eq!(
        sessions[0].context.as_ref().and_then(|context| context.host.as_deref()),
        Some("builder")
    );
}

#[tokio::test]
async fn attach_prefers_handoff_snapshot_over_blank_term() {
    let (state, masters, _slaves) = make_handoff_state(1);
    let session_id = state.sessions[0].session_id;

    let sm = Arc::new(SessionManager::restore_from_handoff(&state, masters, 100).unwrap());
    let wm = Arc::new(RwLock::new(WorkspaceManager::new(vec![])));
    let registry = ipc_server::new_live_session_registry();

    ipc_server::activate_pending_sessions(&sm, &wm, &registry).await;
    wait_registry_count(&registry, 1).await;

    // The handoff snapshot had alt_screen=true and cursor_visible=false.
    // A blank Term would have alt_screen=false and cursor_visible=true.
    {
        let live = registry.read().await;
        let session = live.get(&session_id).unwrap();
        assert!(
            session.handoff_snapshot.is_some(),
            "handoff snapshot should be stored in LiveSession"
        );
    }

    // Simulate what handle_attach_sessions does: take_session_snapshot.
    let term = {
        let live = registry.read().await;
        Arc::clone(&live.get(&session_id).unwrap().term)
    };
    let snapshot = crate::attach_flow::take_session_snapshot(session_id, &term, &registry).await;

    assert!(snapshot.alt_screen, "should return handoff snapshot (alt_screen=true)");
    assert!(!snapshot.cursor_visible, "should return handoff snapshot (cursor_visible=false)");

    // Second call should fall back to the live Term (handoff snapshot consumed).
    let snapshot2 = crate::attach_flow::take_session_snapshot(session_id, &term, &registry).await;
    assert!(!snapshot2.alt_screen, "handoff consumed; blank Term has alt_screen=false");
}
