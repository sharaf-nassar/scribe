//! Point-in-time assembly for the agent API's `World` and `Siblings` replies.
//!
//! [`capture`] takes the server's read guards in this order: live sessions,
//! window shares, workspace manager. A transport-owned callback copies each
//! private live-session record into the narrow allowlist below, giving both
//! replies one coherent source image without exposing `SessionInfo` or retained
//! prompt data.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use scribe_common::agent::{
    AgentError, AgentSession, AgentWindow, AgentWorkspace, AgentWorldSnapshot,
};
use scribe_common::ai_state::{AiProcessState, AiProvider};
use scribe_common::config::SharingMode;
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::epoch_secs;
use tokio::sync::RwLock;

static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(1);

/// Read-only view of one window share, implemented by `ipc_server`'s
/// `WindowShare`. This is a nominal seam: the binary recompiles `ipc_server`
/// as its own module tree while re-exporting THIS module from the library
/// crate, so both compiles implement the single trait [`capture`] is bounded
/// on instead of naming two nominally distinct `WindowShare` types.
pub trait ShareView {
    /// The share's current sharing mode.
    fn sharing_mode(&self) -> SharingMode;
    /// How many participants are attached to the share.
    fn participant_count(&self) -> usize;
}

/// Read-only view of the workspace manager, implemented by
/// `workspace_manager::WorkspaceManager` in both compiles — the same nominal
/// seam as [`ShareView`].
pub trait WorkspaceView {
    /// Every window that currently has sessions assigned.
    fn window_ids_with_sessions(&self) -> HashSet<WindowId>;
    /// Workspace names shown for a window, in `ListWindows` order.
    fn workspace_names_for_window(&self, window_id: WindowId) -> Vec<String>;
    /// How many sessions a window currently holds.
    fn window_session_count(&self, window_id: WindowId) -> usize;
    /// The window a session is assigned to, if any.
    fn window_for_session(&self, session_id: SessionId) -> Option<WindowId>;
    /// A workspace's user-visible name, if it has one.
    fn workspace_name(&self, workspace_id: WorkspaceId) -> Option<String>;
}

/// Window fields copied from the same guards used by `ListWindows`.
#[derive(Debug, Clone)]
struct CapturedWindow {
    pub window_id: WindowId,
    pub workspace_names: Vec<String>,
    pub session_count: usize,
    pub connected: bool,
    pub sharing_mode: SharingMode,
    pub participant_count: usize,
}

/// Workspace membership copied while the workspace-manager guard is held.
#[derive(Debug, Clone)]
struct CapturedWorkspace {
    pub workspace_id: WorkspaceId,
    pub name: Option<String>,
    pub window_id: WindowId,
    pub session_ids: Vec<SessionId>,
}

/// Allowlisted live-session metadata. Prompt and conversation data never enter
/// this type, so they cannot accidentally reach the agent DTO. Pub because the
/// binary's recompiled `ipc_server` builds it inside its transport-owned copy
/// callback.
#[derive(Debug, Clone)]
pub struct CapturedSession {
    pub session_id: SessionId,
    pub window_id: WindowId,
    pub workspace_id: WorkspaceId,
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub ai_state: Option<AiProcessState>,
    pub ai_provider_hint: Option<AiProvider>,
    pub task_label: Option<String>,
}

/// One immutable image copied from the three authoritative server registries.
/// Pub (opaque, private fields) because it names the output of the public
/// dispatcher's world-capture seam; in-crate `capture` is the only builder.
#[derive(Debug, Clone)]
pub struct Capture {
    windows: Vec<CapturedWindow>,
    workspaces: Vec<CapturedWorkspace>,
    sessions: Vec<CapturedSession>,
    snapshot_id: u64,
    captured_at: u64,
}

impl Capture {
    /// Stamp copied registry data while the caller still holds all three read
    /// guards. Formatting and filtering happen after the guards are released.
    fn new(
        windows: Vec<CapturedWindow>,
        workspaces: Vec<CapturedWorkspace>,
        sessions: Vec<CapturedSession>,
    ) -> Self {
        Self {
            windows,
            workspaces,
            sessions,
            snapshot_id: NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed),
            captured_at: epoch_secs(SystemTime::now()).unwrap_or_default(),
        }
    }
}

// @lat: [[server#Server#Agent API#World and siblings]]
/// Copy the three authoritative registries under one ordered, short-lived read
/// transaction. The callback is defined by the transport owner so private
/// `LiveSession` fields remain private to `ipc_server` while this module owns
/// aggregation and DTO filtering.
pub async fn capture<S, Share, Workspaces, CopySession, SessionState, ShareState>(
    live_sessions: &Arc<RwLock<HashMap<SessionId, S, SessionState>>>,
    window_shares: &Arc<RwLock<HashMap<WindowId, Share, ShareState>>>,
    workspace_manager: &Arc<RwLock<Workspaces>>,
    copy_session: CopySession,
) -> Capture
where
    Share: ShareView,
    Workspaces: WorkspaceView,
    CopySession: Fn(SessionId, &S, Option<WindowId>) -> CapturedSession,
    SessionState: std::hash::BuildHasher,
    ShareState: std::hash::BuildHasher,
{
    // Server order: live sessions before workspace manager. Window shares sit
    // between them so the final pair still matches ListWindows' shares ->
    // workspace-manager order. No I/O or terminal lock is touched here.
    let live = live_sessions.read().await;
    let shares = window_shares.read().await;
    let workspaces = workspace_manager.read().await;

    let mut window_ids = workspaces.window_ids_with_sessions();
    window_ids.extend(shares.keys().copied());
    let windows = window_ids
        .into_iter()
        .map(|window_id| {
            let share = shares.get(&window_id);
            CapturedWindow {
                window_id,
                workspace_names: workspaces.workspace_names_for_window(window_id),
                session_count: workspaces.window_session_count(window_id),
                connected: share.is_some(),
                sharing_mode: share.map_or(SharingMode::SingleController, ShareView::sharing_mode),
                participant_count: share.map_or(0, ShareView::participant_count),
            }
        })
        .collect();

    let mut session_ids: Vec<SessionId> = live.keys().copied().collect();
    session_ids.sort_by_key(|session_id| session_id.to_full_string());
    let sessions: Vec<CapturedSession> = session_ids
        .into_iter()
        .filter_map(|session_id| {
            live.get(&session_id).map(|session| {
                copy_session(session_id, session, workspaces.window_for_session(session_id))
            })
        })
        .collect();

    let mut workspace_rows: HashMap<(WorkspaceId, WindowId), CapturedWorkspace> = HashMap::new();
    for session in &sessions {
        let workspace = workspace_rows
            .entry((session.workspace_id, session.window_id))
            .or_insert_with(|| CapturedWorkspace {
                workspace_id: session.workspace_id,
                name: workspaces.workspace_name(session.workspace_id),
                window_id: session.window_id,
                session_ids: Vec::new(),
            });
        workspace.session_ids.push(session.session_id);
    }

    Capture::new(windows, workspace_rows.into_values().collect(), sessions)
}

/// Build the server-wide reply, marking at most one matching live session as
/// the caller. An absent or stale origin is orientation-only and marks none.
pub(crate) fn world(capture: Capture, origin: Option<SessionId>) -> AgentWorldSnapshot {
    let Capture { mut windows, mut workspaces, mut sessions, snapshot_id, captured_at } = capture;
    windows.sort_by_key(|window| window.window_id.to_full_string());
    workspaces.sort_by_key(|workspace| {
        (workspace.workspace_id.to_full_string(), workspace.window_id.to_full_string())
    });
    sessions.sort_by_key(|session| session.session_id.to_full_string());

    let mut caller_marked = false;
    let sessions = sessions
        .into_iter()
        .map(|session| {
            let is_caller = !caller_marked && origin == Some(session.session_id);
            caller_marked |= is_caller;
            agent_session(session, is_caller)
        })
        .collect();

    AgentWorldSnapshot {
        windows: windows.into_iter().map(agent_window).collect(),
        workspaces: workspaces.into_iter().map(agent_workspace).collect(),
        sessions,
        snapshot_id,
        captured_at,
    }
}

/// Build one world snapshot, then narrow that same snapshot to the origin
/// window. A missing or stale origin is a typed `NotFound`.
pub(crate) fn siblings(
    capture: Capture,
    origin: Option<SessionId>,
) -> Result<AgentWorldSnapshot, AgentError> {
    let Some(origin) = origin else {
        return Err(origin_not_found());
    };
    filter_to_origin_window(world(capture, Some(origin)), origin)
}

fn filter_to_origin_window(
    mut snapshot: AgentWorldSnapshot,
    origin: SessionId,
) -> Result<AgentWorldSnapshot, AgentError> {
    let Some(window_id) = snapshot
        .sessions
        .iter()
        .find_map(|session| (session.session_id == origin).then_some(session.window_id))
    else {
        return Err(origin_not_found());
    };

    snapshot.windows.retain(|window| window.window_id == window_id);
    snapshot.workspaces.retain(|workspace| workspace.window_id == window_id);
    snapshot.sessions.retain(|session| session.window_id == window_id);
    Ok(snapshot)
}

fn agent_window(window: CapturedWindow) -> AgentWindow {
    AgentWindow {
        window_id: window.window_id,
        workspace_names: window.workspace_names,
        session_count: window.session_count,
        connected: window.connected,
        sharing_mode: window.sharing_mode,
        participant_count: window.participant_count,
    }
}

fn agent_workspace(workspace: CapturedWorkspace) -> AgentWorkspace {
    AgentWorkspace {
        workspace_id: workspace.workspace_id,
        name: workspace.name,
        window_id: workspace.window_id,
        session_ids: workspace.session_ids,
    }
}

fn agent_session(session: CapturedSession, is_caller: bool) -> AgentSession {
    let (provider, ai_state, context_fill_percent) =
        session.ai_state.map_or((session.ai_provider_hint, None, None), |state| {
            (Some(state.provider), Some(state.state), state.context)
        });
    AgentSession {
        session_id: session.session_id,
        window_id: session.window_id,
        workspace_id: session.workspace_id,
        title: session.title,
        cwd: session.cwd,
        provider,
        ai_state,
        task_label: session.task_label,
        context_fill_percent,
        is_caller,
    }
}

fn origin_not_found() -> AgentError {
    AgentError::NotFound { message: "origin session is not live".into() }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use scribe_common::agent::{AgentError, AgentWorldSnapshot};
    use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
    use scribe_common::config::SharingMode;
    use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
    use tokio::sync::RwLock;

    use crate::ipc_server::WindowShares;
    use crate::workspace_manager::WorkspaceManager;

    use super::{
        Capture, CapturedSession, CapturedWindow, CapturedWorkspace, capture, siblings, world,
    };

    struct Fixture {
        capture: Capture,
        first_window: WindowId,
        second_window: WindowId,
        caller: SessionId,
        sibling: SessionId,
        other: SessionId,
    }

    fn fixture_windows(first_window: WindowId, second_window: WindowId) -> Vec<CapturedWindow> {
        vec![
            CapturedWindow {
                window_id: second_window,
                workspace_names: vec![String::from("other")],
                session_count: 1,
                connected: false,
                sharing_mode: SharingMode::SingleController,
                participant_count: 0,
            },
            CapturedWindow {
                window_id: first_window,
                workspace_names: vec![String::from("scribe")],
                session_count: 2,
                connected: true,
                sharing_mode: SharingMode::FreeForAll,
                participant_count: 2,
            },
        ]
    }

    /// The caller's fully-populated session row, including the retained AI
    /// state whose non-allowlisted fields must never surface in a DTO.
    fn caller_session(
        session_id: SessionId,
        window_id: WindowId,
        workspace_id: WorkspaceId,
    ) -> CapturedSession {
        CapturedSession {
            session_id,
            window_id,
            workspace_id,
            title: Some(String::from("tests")),
            cwd: Some(PathBuf::from("/work/scribe")),
            ai_state: Some(AiProcessState {
                provider: AiProvider::ClaudeCode,
                state: AiState::Processing,
                tool: Some(String::from("hidden tool")),
                agent: Some(String::from("hidden agent")),
                model: Some(String::from("hidden model")),
                context: Some(42),
                conversation_id: Some(String::from("hidden conversation")),
            }),
            ai_provider_hint: Some(AiProvider::CodexCode),
            task_label: Some(String::from("Run tests")),
        }
    }

    fn fixture() -> Fixture {
        let first_window = WindowId::new();
        let second_window = WindowId::new();
        let first_workspace = WorkspaceId::new();
        let second_workspace = WorkspaceId::new();
        let caller = SessionId::new();
        let sibling = SessionId::new();
        let other = SessionId::new();

        let capture = Capture {
            windows: fixture_windows(first_window, second_window),
            workspaces: vec![
                CapturedWorkspace {
                    workspace_id: first_workspace,
                    name: Some(String::from("scribe")),
                    window_id: first_window,
                    session_ids: vec![caller, sibling],
                },
                CapturedWorkspace {
                    workspace_id: second_workspace,
                    name: Some(String::from("other")),
                    window_id: second_window,
                    session_ids: vec![other],
                },
            ],
            sessions: vec![
                caller_session(caller, first_window, first_workspace),
                CapturedSession {
                    session_id: sibling,
                    window_id: first_window,
                    workspace_id: first_workspace,
                    title: None,
                    cwd: None,
                    ai_state: None,
                    ai_provider_hint: Some(AiProvider::Pi),
                    task_label: None,
                },
                CapturedSession {
                    session_id: other,
                    window_id: second_window,
                    workspace_id: second_workspace,
                    title: None,
                    cwd: None,
                    ai_state: None,
                    ai_provider_hint: None,
                    task_label: None,
                },
            ],
            snapshot_id: 77,
            captured_at: 88,
        };

        Fixture { capture, first_window, second_window, caller, sibling, other }
    }

    fn caller_count(snapshot: &AgentWorldSnapshot) -> usize {
        snapshot.sessions.iter().filter(|session| session.is_caller).count()
    }

    #[tokio::test]
    async fn capture_aggregates_live_workspaces_and_list_windows_fields_together() {
        let window_id = WindowId::new();
        let session_id = SessionId::new();
        let mut workspaces = WorkspaceManager::new(vec![PathBuf::from("/work")]);
        let workspace_id = workspaces.create_workspace();
        workspaces.add_session(workspace_id, session_id, None);
        workspaces.assign_session_to_window(window_id, session_id);
        let _ = workspaces.on_cwd_changed(session_id, &PathBuf::from("/work/scribe"));

        let live = Arc::new(RwLock::new(HashMap::from([(session_id, workspace_id)])));
        let shares: WindowShares = Arc::new(RwLock::new(HashMap::new()));
        let workspaces = Arc::new(RwLock::new(workspaces));
        let capture =
            capture(&live, &shares, &workspaces, |id, workspace, mapped_window| CapturedSession {
                session_id: id,
                window_id: mapped_window.unwrap_or(window_id),
                workspace_id: *workspace,
                title: None,
                cwd: None,
                ai_state: None,
                ai_provider_hint: None,
                task_label: None,
            })
            .await;
        let snapshot = world(capture, Some(session_id));

        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(caller_count(&snapshot), 1);
        let window = snapshot.windows.first();
        assert!(window.is_some(), "captured window is present");
        if let Some(window) = window {
            assert_eq!(window.window_id, window_id);
            assert_eq!(window.workspace_names, vec![String::from("scribe")]);
            assert_eq!(window.session_count, 1);
            assert!(!window.connected);
            assert_eq!(window.sharing_mode, SharingMode::SingleController);
            assert_eq!(window.participant_count, 0);
        }
    }

    #[test]
    fn world_preserves_one_consistent_capture_and_allowlisted_ai_fields() {
        let fixture = fixture();
        let snapshot = world(fixture.capture, Some(fixture.caller));

        assert_eq!(snapshot.snapshot_id, 77);
        assert_eq!(snapshot.captured_at, 88);
        assert_eq!(caller_count(&snapshot), 1);
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.workspaces.len(), 2);
        assert_eq!(snapshot.sessions.len(), 3);

        let caller = snapshot.sessions.iter().find(|session| session.session_id == fixture.caller);
        assert!(caller.is_some(), "caller session is present");
        if let Some(caller) = caller {
            assert_eq!(caller.provider, Some(AiProvider::ClaudeCode));
            assert_eq!(caller.ai_state, Some(AiState::Processing));
            assert_eq!(caller.context_fill_percent, Some(42));
            assert_eq!(caller.task_label.as_deref(), Some("Run tests"));
        }

        let sibling =
            snapshot.sessions.iter().find(|session| session.session_id == fixture.sibling);
        assert!(sibling.is_some(), "sibling session is present");
        if let Some(sibling) = sibling {
            assert_eq!(sibling.provider, Some(AiProvider::Pi));
            assert_eq!(sibling.ai_state, None);
            assert_eq!(sibling.context_fill_percent, None);
        }

        let other = snapshot.sessions.iter().find(|session| session.session_id == fixture.other);
        assert!(other.is_some(), "other session is present");
        if let Some(other) = other {
            assert_eq!(other.provider, None);
            assert_eq!(other.ai_state, None);
            assert_eq!(other.task_label, None);
            assert_eq!(other.context_fill_percent, None);
        }
    }

    #[test]
    fn world_marks_no_caller_for_a_stale_or_absent_origin() {
        let fixture = fixture();
        assert_eq!(caller_count(&world(fixture.capture.clone(), None)), 0);
        assert_eq!(caller_count(&world(fixture.capture, Some(SessionId::new()))), 0);
    }

    #[test]
    fn siblings_filters_the_same_snapshot_to_the_origin_window() {
        let fixture = fixture();
        let filtered = siblings(fixture.capture, Some(fixture.caller)).ok();
        assert!(filtered.is_some(), "valid origin filters successfully");
        if let Some(snapshot) = filtered {
            assert_eq!(snapshot.snapshot_id, 77);
            assert_eq!(snapshot.captured_at, 88);
            assert_eq!(caller_count(&snapshot), 1);
            assert!(snapshot.windows.iter().all(|window| window.window_id == fixture.first_window));
            assert!(
                snapshot
                    .workspaces
                    .iter()
                    .all(|workspace| workspace.window_id == fixture.first_window)
            );
            assert!(
                snapshot.sessions.iter().all(|session| session.window_id == fixture.first_window)
            );
            assert!(
                !snapshot.windows.iter().any(|window| window.window_id == fixture.second_window)
            );
        }
    }

    #[test]
    fn siblings_rejects_missing_or_stale_origins() {
        let fixture = fixture();
        assert!(matches!(
            siblings(fixture.capture.clone(), None),
            Err(AgentError::NotFound { .. })
        ));
        assert!(matches!(
            siblings(fixture.capture, Some(SessionId::new())),
            Err(AgentError::NotFound { .. })
        ));
    }

    #[test]
    fn windows_use_the_same_stable_order_and_fields_as_list_windows() {
        let fixture = fixture();
        let expected_first =
            if fixture.first_window.to_full_string() < fixture.second_window.to_full_string() {
                fixture.first_window
            } else {
                fixture.second_window
            };
        let snapshot = world(fixture.capture, None);
        assert_eq!(snapshot.windows.first().map(|window| window.window_id), Some(expected_first));

        let first = snapshot.windows.iter().find(|window| window.window_id == fixture.first_window);
        assert!(first.is_some(), "first window is present");
        if let Some(first) = first {
            assert_eq!(first.workspace_names, vec![String::from("scribe")]);
            assert_eq!(first.session_count, 2);
            assert!(first.connected);
            assert_eq!(first.sharing_mode, SharingMode::FreeForAll);
            assert_eq!(first.participant_count, 2);
        }
    }

    #[test]
    fn capture_allocates_one_identity_for_every_entry_in_a_reply() {
        let fixture = fixture();
        let capture = Capture::new(
            fixture.capture.windows,
            fixture.capture.workspaces,
            fixture.capture.sessions,
        );
        let snapshot = world(capture, None);
        assert!(snapshot.snapshot_id > 0);
        assert!(snapshot.captured_at > 0);
    }
}
