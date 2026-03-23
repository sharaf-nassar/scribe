use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::{debug, info};

use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::ServerMessage;

use crate::handoff::HandoffWorkspace;

#[allow(dead_code, reason = "used by create_workspace, called from CreateWorkspace handler")]
const ACCENT_COLORS: &[&str] =
    &["#a78bfa", "#38bdf8", "#6ee7b7", "#fb7185", "#fbbf24", "#a3e635", "#f472b6", "#22d3ee"];

/// Manages workspace ↔ session relationships and auto-names workspaces
/// based on configured root directories.
pub struct WorkspaceManager {
    roots: Vec<PathBuf>,
    workspaces: HashMap<WorkspaceId, Workspace>,
    session_to_workspace: HashMap<SessionId, WorkspaceId>,
    #[allow(dead_code, reason = "used by create_workspace, called from CreateWorkspace handler")]
    color_index: usize,
}

struct Workspace {
    #[allow(dead_code, reason = "used for workspace identity in future UI sync messages")]
    id: WorkspaceId,
    name: Option<String>,
    sessions: Vec<SessionId>,
    #[allow(dead_code, reason = "sent to UI in future WorkspaceInfo messages")]
    accent_color: String,
}

impl WorkspaceManager {
    /// Create a new workspace manager with the given root directories.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            workspaces: HashMap::new(),
            session_to_workspace: HashMap::new(),
            color_index: 0,
        }
    }

    /// Create a new workspace and assign it the next accent color from the
    /// rotating palette.
    #[allow(dead_code, reason = "called from CreateWorkspace handler and tests")]
    pub fn create_workspace(&mut self) -> WorkspaceId {
        let id = WorkspaceId::new();
        let color_count = ACCENT_COLORS.len();
        // Compute index before mutating color_index so the assignment is clean.
        let idx = self.color_index % color_count;
        let accent_color = ACCENT_COLORS.get(idx).copied().unwrap_or("#a78bfa").to_owned();
        self.color_index = self.color_index.wrapping_add(1);

        info!(%id, color = %accent_color, "created workspace");

        let workspace = Workspace { id, name: None, sessions: Vec::new(), accent_color };
        self.workspaces.insert(id, workspace);

        id
    }

    /// Add a session to a workspace.
    pub fn add_session(&mut self, workspace_id: WorkspaceId, session_id: SessionId) {
        self.session_to_workspace.insert(session_id, workspace_id);
        if let Some(ws) = self.workspaces.get_mut(&workspace_id) {
            ws.sessions.push(session_id);
            debug!(%session_id, %workspace_id, "added session to workspace");
        }
    }

    /// Remove a session from its workspace.
    #[allow(dead_code, reason = "called from CloseSession handler and tests")]
    pub fn remove_session(&mut self, session_id: SessionId) {
        if let Some(workspace_id) = self.session_to_workspace.remove(&session_id) {
            if let Some(ws) = self.workspaces.get_mut(&workspace_id) {
                ws.sessions.retain(|&s| s != session_id);
                debug!(%session_id, %workspace_id, "removed session from workspace");
            }
        }
    }

    /// Called when the CWD of a session changes.
    ///
    /// If the workspace containing this session is still unnamed, the CWD is
    /// matched against configured roots. When a match is found the first path
    /// component after the root prefix becomes the workspace name (sticky —
    /// once named, stays named).
    ///
    /// Returns `Some(ServerMessage::WorkspaceNamed { … })` when a name is
    /// newly assigned, `None` otherwise.
    pub fn on_cwd_changed(&mut self, session_id: SessionId, cwd: &Path) -> Option<ServerMessage> {
        let workspace_id = *self.session_to_workspace.get(&session_id)?;

        // Check if already named before extracting the name.
        let is_named = self.workspaces.get(&workspace_id).is_some_and(|ws| ws.name.is_some());
        if is_named {
            return None;
        }

        // Extract name from roots. Clone to avoid borrowing self while
        // the mutable borrow of workspaces is needed below.
        let roots = self.roots.clone();
        let name = Self::extract_workspace_name_with_roots(cwd, &roots)?;

        // Now mutably borrow the workspace to set the name.
        let ws = self.workspaces.get_mut(&workspace_id)?;
        ws.name = Some(name.clone());
        info!(%workspace_id, %name, "workspace auto-named from CWD");

        Some(ServerMessage::WorkspaceNamed { workspace_id, name })
    }

    /// Linux-only fallback: read the CWD of `child_pid` from `/proc/{pid}/cwd`
    /// and delegate to `on_cwd_changed`.
    #[cfg(target_os = "linux")]
    pub fn check_cwd_fallback(
        &mut self,
        session_id: SessionId,
        child_pid: u32,
    ) -> Option<ServerMessage> {
        if child_pid == 0 {
            debug!(%session_id, "skipping /proc CWD check: child_pid is 0");
            return None;
        }
        let proc_cwd = PathBuf::from(format!("/proc/{child_pid}/cwd"));
        let cwd = std::fs::read_link(&proc_cwd)
            .map_err(|e| {
                debug!(%session_id, pid = child_pid, "could not read /proc/pid/cwd: {e}");
                e
            })
            .ok()?;
        self.on_cwd_changed(session_id, &cwd)
    }

    /// Non-Linux stub — always returns `None`.
    #[cfg(not(target_os = "linux"))]
    pub fn check_cwd_fallback(
        &mut self,
        _session_id: SessionId,
        _child_pid: u32,
    ) -> Option<ServerMessage> {
        None
    }

    /// Return the workspace a session belongs to, if any.
    #[allow(dead_code, reason = "used by MoveSession handler and tests")]
    pub fn workspace_for_session(&self, session_id: SessionId) -> Option<WorkspaceId> {
        self.session_to_workspace.get(&session_id).copied()
    }

    /// Return the name and accent color of a workspace.
    ///
    /// Returns `Some((name, accent_color))` if the workspace exists, `None`
    /// otherwise.
    pub fn workspace_info(&self, id: WorkspaceId) -> Option<(Option<String>, String)> {
        self.workspaces.get(&id).map(|ws| (ws.name.clone(), ws.accent_color.clone()))
    }

    /// Extract the workspace name from a CWD path by matching against the
    /// configured roots and taking the first component after the root prefix.
    #[allow(dead_code, reason = "called from extract_workspace_name_pub in tests")]
    fn extract_workspace_name(&self, cwd: &Path) -> Option<String> {
        Self::extract_workspace_name_with_roots(cwd, &self.roots)
    }

    /// Inner helper that takes an explicit roots slice so the borrow checker
    /// is happy when `on_cwd_changed` passes a cloned roots vec.
    fn extract_workspace_name_with_roots(cwd: &Path, roots: &[PathBuf]) -> Option<String> {
        roots.iter().find_map(|root| {
            let suffix = cwd.strip_prefix(root).ok()?;
            // Take the first component of the relative path.
            suffix.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned())
        })
    }

    /// Serialise all workspaces for a hot-reload handoff.
    pub fn serialize_for_handoff(&self) -> Vec<HandoffWorkspace> {
        self.workspaces
            .values()
            .map(|ws| HandoffWorkspace {
                id: ws.id,
                name: ws.name.clone(),
                accent_color: ws.accent_color.clone(),
                session_ids: ws.sessions.clone(),
            })
            .collect()
    }

    /// Reconstruct a `WorkspaceManager` from handoff state.
    pub fn restore_from_handoff(roots: Vec<PathBuf>, workspaces: &[HandoffWorkspace]) -> Self {
        let mut ws_map = HashMap::new();
        let mut session_to_workspace = HashMap::new();

        for hw in workspaces {
            for &session_id in &hw.session_ids {
                session_to_workspace.insert(session_id, hw.id);
            }

            ws_map.insert(
                hw.id,
                Workspace {
                    id: hw.id,
                    name: hw.name.clone(),
                    sessions: hw.session_ids.clone(),
                    accent_color: hw.accent_color.clone(),
                },
            );

            info!(
                workspace_id = %hw.id,
                name = ?hw.name,
                sessions = hw.session_ids.len(),
                "restored workspace from handoff"
            );
        }

        Self { roots, workspaces: ws_map, session_to_workspace, color_index: workspaces.len() }
    }
}

// Expose the private helper for unit-testing without making it pub on the
// main type.
#[cfg(test)]
impl WorkspaceManager {
    pub fn extract_workspace_name_pub(&self, cwd: &Path) -> Option<String> {
        self.extract_workspace_name(cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_with_roots(roots: Vec<&str>) -> WorkspaceManager {
        WorkspaceManager::new(roots.into_iter().map(PathBuf::from).collect())
    }

    #[test]
    fn extracts_first_component_after_root() {
        let mgr = manager_with_roots(vec!["/home/user/work"]);
        let result = mgr.extract_workspace_name_pub(Path::new("/home/user/work/quill/src"));
        assert_eq!(result, Some("quill".to_owned()));
    }

    #[test]
    fn returns_none_when_no_root_matches() {
        let mgr = manager_with_roots(vec!["/home/user/work"]);
        let result = mgr.extract_workspace_name_pub(Path::new("/tmp/other"));
        assert!(result.is_none());
    }

    #[test]
    fn direct_root_child_named_correctly() {
        let mgr = manager_with_roots(vec!["/home/user/work"]);
        let result = mgr.extract_workspace_name_pub(Path::new("/home/user/work/myproject"));
        assert_eq!(result, Some("myproject".to_owned()));
    }

    #[test]
    fn workspace_is_named_on_cwd_change() {
        let mut mgr = manager_with_roots(vec!["/work"]);
        let ws_id = mgr.create_workspace();
        let sess_id = SessionId::new();
        mgr.add_session(ws_id, sess_id);

        let msg = mgr.on_cwd_changed(sess_id, Path::new("/work/myapp/src"));
        assert!(matches!(
            msg,
            Some(ServerMessage::WorkspaceNamed { name, .. }) if name == "myapp"
        ));
    }

    #[test]
    fn workspace_name_is_sticky() {
        let mut mgr = manager_with_roots(vec!["/work"]);
        let ws_id = mgr.create_workspace();
        let sess_id = SessionId::new();
        mgr.add_session(ws_id, sess_id);

        mgr.on_cwd_changed(sess_id, Path::new("/work/first/src"));
        // Second CWD change should not rename.
        let msg = mgr.on_cwd_changed(sess_id, Path::new("/work/second/src"));
        assert!(msg.is_none());
    }

    #[test]
    fn color_palette_rotates() {
        let mut mgr = manager_with_roots(vec![]);
        let ids: Vec<WorkspaceId> =
            (0..=ACCENT_COLORS.len()).map(|_| mgr.create_workspace()).collect();
        // Just verify we created the right number without panicking.
        assert_eq!(ids.len(), ACCENT_COLORS.len() + 1);
    }

    #[test]
    fn remove_session_cleans_up() {
        let mut mgr = manager_with_roots(vec![]);
        let ws_id = mgr.create_workspace();
        let sess_id = SessionId::new();
        mgr.add_session(ws_id, sess_id);
        assert_eq!(mgr.workspace_for_session(sess_id), Some(ws_id));
        mgr.remove_session(sess_id);
        assert_eq!(mgr.workspace_for_session(sess_id), None);
    }
}
