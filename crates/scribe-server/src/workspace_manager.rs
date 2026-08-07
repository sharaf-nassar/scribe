use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{LayoutDirection, ServerMessage, WorkspaceTreeNode};

use serde::{Deserialize, Serialize};

use crate::handoff::HandoffWorkspace;

/// Per-window state transferred during handoff.
#[derive(Serialize, Deserialize)]
pub struct HandoffWindowState {
    pub window_id: WindowId,
    pub session_ids: Vec<SessionId>,
    pub workspace_tree: Option<WorkspaceTreeNode>,
}

const ACCENT_COLORS: &[&str] =
    &["#a78bfa", "#38bdf8", "#6ee7b7", "#fb7185", "#fbbf24", "#a3e635", "#f472b6", "#22d3ee"];

type WorkspaceInfoSummary = (Option<String>, String, Option<LayoutDirection>, Option<PathBuf>);

/// Manages workspace ↔ session relationships, window ↔ session ownership,
/// and auto-names workspaces based on configured root directories.
pub struct WorkspaceManager {
    roots: Vec<PathBuf>,
    workspaces: HashMap<WorkspaceId, Workspace>,
    session_to_workspace: HashMap<SessionId, WorkspaceId>,
    color_index: usize,
    /// Legacy single workspace tree — used as fallback when no per-window
    /// trees exist (backwards compatibility with pre-multi-window handoffs).
    workspace_tree: Option<WorkspaceTreeNode>,
    /// Per-window workspace split trees.  Each client window reports its own
    /// tree via `ReportWorkspaceTree`; the server stores them keyed by window.
    window_trees: HashMap<WindowId, WorkspaceTreeNode>,
    /// Maps each session to the window that owns it.
    session_to_window: HashMap<SessionId, WindowId>,
}

struct Workspace {
    id: WorkspaceId,
    name: Option<String>,
    /// Absolute path to the project directory (`root / first_component`).
    /// Set alongside `name` when a CWD matches a configured workspace root.
    project_root: Option<PathBuf>,
    sessions: Vec<SessionId>,
    accent_color: String,
    /// Direction of the split that created this workspace (`None` for the
    /// initial workspace which was not created by splitting).
    split_direction: Option<LayoutDirection>,
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
            workspace_tree: None,
            window_trees: HashMap::new(),
            session_to_window: HashMap::new(),
        }
    }

    /// Replace workspace roots after a config reload.
    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots;
    }

    /// Create a new workspace and assign it the next accent color from the
    /// rotating palette.
    pub fn create_workspace(&mut self) -> WorkspaceId {
        let id = WorkspaceId::new();
        let color_count = ACCENT_COLORS.len();
        // Compute index before mutating color_index so the assignment is clean.
        let idx = self.color_index % color_count;
        let accent_color = ACCENT_COLORS.get(idx).copied().unwrap_or("#a78bfa").to_owned();
        self.color_index = self.color_index.wrapping_add(1);

        info!(%id, color = %accent_color, "created workspace");

        let workspace = Workspace {
            id,
            name: None,
            project_root: None,
            sessions: Vec::new(),
            accent_color,
            split_direction: None,
        };
        self.workspaces.insert(id, workspace);

        id
    }

    /// Add a session to a workspace.
    ///
    /// When `split_direction` is `Some` and the workspace does not yet exist
    /// it is created automatically (this happens when the client creates a
    /// workspace split — it sends `CreateSession` with a brand-new workspace
    /// ID and the direction of the split).
    pub fn add_session(
        &mut self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        split_direction: Option<LayoutDirection>,
    ) {
        self.session_to_workspace.insert(session_id, workspace_id);

        // Auto-create the workspace for split-created workspaces.
        if !self.workspaces.contains_key(&workspace_id) {
            let color_count = ACCENT_COLORS.len();
            let idx = self.color_index % color_count;
            let accent_color = ACCENT_COLORS.get(idx).copied().unwrap_or("#a78bfa").to_owned();
            self.color_index = self.color_index.wrapping_add(1);

            info!(%workspace_id, color = %accent_color, ?split_direction, "auto-created workspace");

            self.workspaces.insert(
                workspace_id,
                Workspace {
                    id: workspace_id,
                    name: None,
                    project_root: None,
                    sessions: Vec::new(),
                    accent_color,
                    split_direction,
                },
            );
        }

        if let Some(ws) = self.workspaces.get_mut(&workspace_id) {
            ws.sessions.push(session_id);
            debug!(%session_id, %workspace_id, "added session to workspace");
        }
    }

    /// Re-key a session into another workspace (`ClientMessage::MoveSession`).
    ///
    /// The split flow seeds its session through the old workspace and moves it
    /// once the client's pane adopts the new region, so membership here — what
    /// `SessionList`, CWD auto-naming, and handoff persistence all read — must
    /// follow. The client only names workspaces the server minted; an unknown
    /// target is dropped rather than auto-created.
    pub fn move_session(&mut self, session_id: SessionId, target: WorkspaceId) -> bool {
        if !self.workspaces.contains_key(&target) {
            warn!(%session_id, %target, "session move ignored: unknown target workspace");
            return false;
        }
        if let Some(previous) = self.session_to_workspace.insert(session_id, target)
            && let Some(ws) = self.workspaces.get_mut(&previous)
        {
            ws.sessions.retain(|&s| s != session_id);
        }
        if let Some(ws) = self.workspaces.get_mut(&target)
            && !ws.sessions.contains(&session_id)
        {
            ws.sessions.push(session_id);
        }
        info!(%session_id, %target, "moved session to workspace");
        true
    }

    /// Drop a workspace whose last region closed (`ClientMessage::CloseWorkspace`).
    ///
    /// The client closes the region's sessions first, so the workspace is
    /// empty by the time a truthful close arrives. A close naming a workspace
    /// that still has sessions is refused outright: it means the sender's
    /// layout is stale (a client that redialed across an upgrade after another
    /// client reshaped the window), and honouring it unlinked live sessions
    /// into an unlisted limbo no later `SessionList` could show — the failure
    /// that emptied whole windows across a server upgrade.
    pub fn close_workspace(&mut self, workspace_id: WorkspaceId) {
        let Some(ws) = self.workspaces.get(&workspace_id) else {
            debug!(%workspace_id, "close ignored: unknown workspace");
            return;
        };
        if !ws.sessions.is_empty() {
            warn!(
                %workspace_id,
                sessions = ws.sessions.len(),
                "workspace close refused: sessions still live here"
            );
            return;
        }
        self.workspaces.remove(&workspace_id);
        info!(%workspace_id, "closed workspace");
    }

    /// Remove a session from its workspace.
    pub fn remove_session(&mut self, session_id: SessionId) {
        if let Some(workspace_id) = self.session_to_workspace.remove(&session_id)
            && let Some(ws) = self.workspaces.get_mut(&workspace_id)
        {
            ws.sessions.retain(|&s| s != session_id);
            debug!(%session_id, %workspace_id, "removed session from workspace");
        }
    }

    /// Called when the CWD of a session changes.
    ///
    /// Matches the CWD against configured roots. When a match is found the
    /// first path component after the root prefix becomes the workspace name.
    /// The name updates whenever the user moves to a different project root.
    ///
    /// Returns `Some(ServerMessage::WorkspaceNamed { … })` when the name
    /// changes, `None` otherwise.
    pub fn on_cwd_changed(&mut self, session_id: SessionId, cwd: &Path) -> Option<ServerMessage> {
        let workspace_id = *self.session_to_workspace.get(&session_id)?;

        // Extract name and project root from roots. Clone to avoid borrowing
        // self while the mutable borrow of workspaces is needed below.
        let roots = self.roots.clone();
        let info = Self::extract_workspace_info_with_roots(cwd, &roots);

        let ws = self.workspaces.get_mut(&workspace_id)?;

        if let Some((name, project_root)) = info {
            // Only send a message when the name or project root actually changes.
            if ws.name.as_ref() == Some(&name) && ws.project_root.as_ref() == Some(&project_root) {
                return None;
            }
            ws.name = Some(name.clone());
            ws.project_root = Some(project_root.clone());
            info!(%workspace_id, %name, "workspace auto-named from CWD");
            Some(ServerMessage::WorkspaceNamed {
                workspace_id,
                name,
                project_root: Some(project_root),
            })
        } else {
            // CWD is outside all workspace roots — clear name if previously set.
            if ws.name.is_none() && ws.project_root.is_none() {
                return None;
            }
            ws.name = None;
            ws.project_root = None;
            info!(%workspace_id, "workspace name cleared (CWD outside roots)");
            Some(ServerMessage::WorkspaceNamed {
                workspace_id,
                name: String::new(),
                project_root: None,
            })
        }
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

    /// macOS fallback: use `proc_pidinfo` to read the child process CWD,
    /// then delegate to `on_cwd_changed`.
    #[cfg(target_os = "macos")]
    pub fn check_cwd_fallback(
        &mut self,
        session_id: SessionId,
        child_pid: u32,
    ) -> Option<ServerMessage> {
        if child_pid == 0 {
            debug!(%session_id, "skipping proc CWD check: child_pid is 0");
            return None;
        }
        let cwd = crate::macos_proc::macos_proc_cwd(child_pid)?;
        self.on_cwd_changed(session_id, &cwd)
    }

    /// Stub for platforms other than Linux and macOS — always returns `None`.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn check_cwd_fallback(
        &mut self,
        _session_id: SessionId,
        _child_pid: u32,
    ) -> Option<ServerMessage> {
        None
    }

    /// Return the name, accent color, split direction, and project root of a
    /// workspace.
    ///
    /// Returns `Some(…)` if the workspace exists, `None` otherwise.
    pub fn workspace_info(&self, id: WorkspaceId) -> Option<WorkspaceInfoSummary> {
        self.workspaces.get(&id).map(|ws| {
            (ws.name.clone(), ws.accent_color.clone(), ws.split_direction, ws.project_root.clone())
        })
    }

    /// Return the workspace tree for a specific window.
    ///
    /// Falls back to the legacy single tree only when no per-window trees
    /// have been stored yet (backwards compatibility with pre-multi-window
    /// servers). Once any window has reported a tree, each window gets
    /// only its own.
    pub fn window_tree(&self, window_id: WindowId) -> Option<&WorkspaceTreeNode> {
        if self.window_trees.is_empty() {
            // Legacy mode — no per-window trees exist, use the global one.
            self.workspace_tree.as_ref()
        } else {
            self.window_trees.get(&window_id)
        }
    }

    /// Replace the stored workspace split tree with a new one reported by
    /// the client.
    pub fn set_workspace_tree(&mut self, tree: WorkspaceTreeNode) {
        self.workspace_tree = Some(tree);
    }

    /// Store a per-window workspace tree reported by a client.
    pub fn set_window_tree(&mut self, window_id: WindowId, tree: WorkspaceTreeNode) {
        self.window_trees.insert(window_id, tree);
    }

    // ── Window tracking ──────────────────────────────────────────────

    /// Assign a session to a window.
    pub fn assign_session_to_window(&mut self, window_id: WindowId, session_id: SessionId) {
        self.session_to_window.insert(session_id, window_id);
        debug!(%session_id, %window_id, "assigned session to window");
    }

    /// Return all session IDs belonging to a window, in workspace-stored order.
    pub fn sessions_for_window(&self, window_id: WindowId) -> Vec<SessionId> {
        // Collect which sessions belong to this window.
        let window_sids: HashSet<SessionId> = self
            .session_to_window
            .iter()
            .filter(|&(_, &wid)| wid == window_id)
            .map(|(&sid, _)| sid)
            .collect();

        // Walk workspaces and emit sessions in their stored order,
        // filtered to only those belonging to this window.
        self.workspaces
            .values()
            .flat_map(|ws| &ws.sessions)
            .copied()
            .filter(|sid| window_sids.contains(sid))
            .collect()
    }

    /// Distinct display names of the workspaces owning this window's sessions,
    /// in the window's stored session order. Unnamed workspaces are skipped and
    /// duplicates removed. Feeds the feature-013 remote connect picker's window
    /// list (FR-005).
    #[must_use]
    pub fn workspace_names_for_window(&self, window_id: WindowId) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for session_id in self.sessions_for_window(window_id) {
            if let Some(ws_id) = self.session_to_workspace.get(&session_id)
                && let Some(ws) = self.workspaces.get(ws_id)
                && let Some(name) = ws.name.as_ref()
                && !names.iter().any(|existing| existing == name)
            {
                names.push(name.clone());
            }
        }
        names
    }

    /// Reorder the sessions in a workspace to match the given order.
    ///
    /// Sessions in `ordered` that don't belong to the workspace are ignored.
    /// Sessions in the workspace but missing from `ordered` are appended at the end.
    pub fn reorder_sessions(&mut self, workspace_id: WorkspaceId, ordered: &[SessionId]) {
        let Some(ws) = self.workspaces.get_mut(&workspace_id) else { return };
        let existing: HashSet<SessionId> = ws.sessions.iter().copied().collect();
        let mut new_order: Vec<SessionId> =
            ordered.iter().copied().filter(|s| existing.contains(s)).collect();
        // Append any sessions not in the ordered list (shouldn't happen normally).
        for &s in &ws.sessions {
            if !new_order.contains(&s) {
                new_order.push(s);
            }
        }
        ws.sessions = new_order;
    }

    /// Return the window that owns a session, if any.
    pub fn window_for_session(&self, session_id: SessionId) -> Option<WindowId> {
        self.session_to_window.get(&session_id).copied()
    }

    /// Return all window IDs that have at least one session.
    pub fn window_ids_with_sessions(&self) -> HashSet<WindowId> {
        self.session_to_window.values().copied().collect()
    }

    /// Remove a window and all its session→window mappings.
    pub fn remove_window(&mut self, window_id: WindowId) {
        self.session_to_window.retain(|_, wid| *wid != window_id);
        self.window_trees.remove(&window_id);
        info!(%window_id, "removed window from registry");
    }

    /// Remove a session's window association (called when a session is closed).
    pub fn remove_session_from_window(&mut self, session_id: SessionId) {
        if let Some(window_id) = self.session_to_window.remove(&session_id) {
            debug!(%session_id, %window_id, "removed session from window");
        }
    }

    /// Extract the workspace name and project root from a CWD path by matching
    /// against the configured roots and taking the first component after the
    /// root prefix.
    fn extract_workspace_info_with_roots(
        cwd: &Path,
        roots: &[PathBuf],
    ) -> Option<(String, PathBuf)> {
        roots.iter().find_map(|root| {
            let suffix = cwd.strip_prefix(root).ok()?;
            let first = suffix.components().next()?;
            let name = first.as_os_str().to_string_lossy().into_owned();
            let project_root = root.join(&name);
            Some((name, project_root))
        })
    }

    /// Serialise all workspaces for a hot-reload handoff.
    pub fn serialize_for_handoff(
        &self,
    ) -> (Vec<HandoffWorkspace>, Option<WorkspaceTreeNode>, Vec<HandoffWindowState>) {
        let flat = self
            .workspaces
            .values()
            .map(|ws| HandoffWorkspace {
                id: ws.id,
                name: ws.name.clone(),
                accent_color: ws.accent_color.clone(),
                session_ids: ws.sessions.clone(),
                split_direction: ws.split_direction,
                project_root: ws.project_root.clone(),
            })
            .collect();

        // Include all windows that have sessions OR trees (a window whose
        // last session was closed still needs its tree preserved).
        let mut all_window_ids = self.window_ids_with_sessions();
        all_window_ids.extend(self.window_trees.keys());

        let windows: Vec<HandoffWindowState> = all_window_ids
            .into_iter()
            .map(|wid| {
                let session_ids = self.sessions_for_window(wid);
                let tree = self.window_trees.get(&wid).cloned();
                HandoffWindowState { window_id: wid, session_ids, workspace_tree: tree }
            })
            .collect();

        (flat, self.workspace_tree.clone(), windows)
    }

    /// Reconstruct a `WorkspaceManager` from handoff state.
    ///
    /// `valid_sessions` is each admitted session paired with the workspace id
    /// its own record carries. The maps restored from `workspaces` / `windows`
    /// are the primary source, but they are self-healed against those records
    /// afterwards: membership lost in an earlier generation (a stale client's
    /// refused-today destructive close, a dropped map entry) must not orphan a
    /// live session forever, because an unmapped session appears in no
    /// window's `SessionList` and is unreachable by every client.
    pub fn restore_from_handoff(
        roots: Vec<PathBuf>,
        workspaces: &[HandoffWorkspace],
        workspace_tree: Option<WorkspaceTreeNode>,
        windows: &[HandoffWindowState],
        valid_sessions: &[(SessionId, WorkspaceId)],
    ) -> Self {
        let valid_session_ids: HashSet<SessionId> =
            valid_sessions.iter().map(|(session_id, _)| *session_id).collect();
        let valid_session_ids = &valid_session_ids;
        let mut ws_map = HashMap::new();
        let mut session_to_workspace = HashMap::new();
        let mut dropped_session_total = 0usize;

        for hw in workspaces {
            let session_ids: Vec<SessionId> = hw
                .session_ids
                .iter()
                .copied()
                .filter(|session_id| valid_session_ids.contains(session_id))
                .collect();

            for &session_id in &session_ids {
                session_to_workspace.insert(session_id, hw.id);
            }

            ws_map.insert(
                hw.id,
                Workspace {
                    id: hw.id,
                    name: hw.name.clone(),
                    project_root: hw.project_root.clone(),
                    sessions: session_ids.clone(),
                    accent_color: hw.accent_color.clone(),
                    split_direction: hw.split_direction,
                },
            );

            let dropped = hw.session_ids.len().saturating_sub(session_ids.len());
            dropped_session_total += dropped;
            info!(
                workspace_id = %hw.id,
                name = ?hw.name,
                sessions = session_ids.len(),
                dropped_sessions = dropped,
                "restored workspace from handoff"
            );
        }

        // A silent empty or lossy restore is exactly the failure mode a
        // post-upgrade log inspection needs to see — warn with counts.
        if workspaces.is_empty() {
            warn!("handoff restored 0 workspaces — successor starts with an empty layout");
        }
        if dropped_session_total > 0 {
            warn!(
                dropped_sessions = dropped_session_total,
                restored_sessions = valid_session_ids.len(),
                "handoff restore dropped sessions not admitted by the session manager"
            );
        }

        let mut session_to_window = HashMap::new();
        let mut window_trees = HashMap::new();
        for hw in windows {
            let session_ids: Vec<SessionId> = hw
                .session_ids
                .iter()
                .copied()
                .filter(|session_id| valid_session_ids.contains(session_id))
                .collect();

            for &session_id in &session_ids {
                session_to_window.insert(session_id, hw.window_id);
            }
            if let Some(tree) = &hw.workspace_tree {
                window_trees.insert(hw.window_id, tree.clone());
            }
            info!(
                window_id = %hw.window_id,
                sessions = session_ids.len(),
                dropped_sessions = hw.session_ids.len().saturating_sub(session_ids.len()),
                "restored window from handoff"
            );
        }

        let mut manager = Self {
            roots,
            workspaces: ws_map,
            session_to_workspace,
            color_index: workspaces.len(),
            workspace_tree,
            window_trees,
            session_to_window,
        };
        manager.heal_restored_memberships(valid_sessions);
        manager
    }

    /// Re-file every restored session the maps above lost.
    ///
    /// A session missing from `session_to_workspace` is re-added under the
    /// workspace its own record names, auto-creating the workspace when the
    /// map lost that too. A session missing from `session_to_window` is
    /// assigned to the window owning a sibling of its workspace, falling back
    /// to the restored window with the most sessions — a session in the wrong
    /// window is a tab the user can move; a session in no window is invisible.
    fn heal_restored_memberships(&mut self, valid_sessions: &[(SessionId, WorkspaceId)]) {
        let mut healed_workspaces = 0usize;
        let mut healed_windows = 0usize;
        for &(session_id, workspace_id) in valid_sessions {
            if !self.session_to_workspace.contains_key(&session_id) {
                self.add_session(workspace_id, session_id, None);
                healed_workspaces += 1;
            }
            if self.session_to_window.contains_key(&session_id) {
                continue;
            }
            match self.adoptive_window_for(session_id) {
                Some(window_id) => {
                    self.session_to_window.insert(session_id, window_id);
                    healed_windows += 1;
                }
                None => {
                    warn!(%session_id, "restored session has no window and none exists to adopt it");
                }
            }
        }
        if healed_workspaces > 0 || healed_windows > 0 {
            warn!(
                healed_workspaces,
                healed_windows, "handoff restore re-filed sessions the persisted maps had lost"
            );
        }
    }

    /// The window that should adopt an orphaned session: a sibling from its
    /// workspace names it directly, otherwise the restored window with the
    /// most sessions, so orphans surface in the user's main window.
    fn adoptive_window_for(&self, session_id: SessionId) -> Option<WindowId> {
        let workspace_id = self.session_to_workspace.get(&session_id)?;
        let sibling_window = self
            .workspaces
            .get(workspace_id)?
            .sessions
            .iter()
            .filter(|&&sibling| sibling != session_id)
            .find_map(|sibling| self.session_to_window.get(sibling).copied());
        sibling_window.or_else(|| {
            let mut counts: HashMap<WindowId, usize> = HashMap::new();
            for &window_id in self.session_to_window.values() {
                *counts.entry(window_id).or_default() += 1;
            }
            counts
                .into_iter()
                .max_by_key(|&(window_id, count)| (count, window_id.to_string()))
                .map(|(window_id, _)| window_id)
        })
    }
}

// Expose the private helper for unit-testing without making it pub on the
// main type.
#[cfg(test)]
impl WorkspaceManager {
    fn workspace_for_session(&self, session_id: SessionId) -> Option<WorkspaceId> {
        self.session_to_workspace.get(&session_id).copied()
    }

    fn sessions_in_workspace(&self, workspace_id: WorkspaceId) -> Vec<SessionId> {
        self.workspaces.get(&workspace_id).map(|ws| ws.sessions.clone()).unwrap_or_default()
    }

    fn workspace_tree(&self) -> Option<&WorkspaceTreeNode> {
        self.workspace_tree.as_ref()
    }

    fn extract_workspace_name_pub(&self, cwd: &Path) -> Option<String> {
        Self::extract_workspace_info_with_roots(cwd, &self.roots).map(|(name, _)| name)
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
        mgr.add_session(ws_id, sess_id, None);

        let msg = mgr.on_cwd_changed(sess_id, Path::new("/work/myapp/src"));
        assert!(matches!(
            msg,
            Some(ServerMessage::WorkspaceNamed { name, .. }) if name == "myapp"
        ));
    }

    #[test]
    fn workspace_name_updates_on_new_root() {
        let mut mgr = manager_with_roots(vec!["/work"]);
        let ws_id = mgr.create_workspace();
        let sess_id = SessionId::new();
        mgr.add_session(ws_id, sess_id, None);

        mgr.on_cwd_changed(sess_id, Path::new("/work/first/src"));
        // CWD change to a different project root should rename.
        let msg = mgr.on_cwd_changed(sess_id, Path::new("/work/second/src"));
        assert!(matches!(
            msg,
            Some(ServerMessage::WorkspaceNamed { name, .. }) if name == "second"
        ));
    }

    #[test]
    fn reloaded_roots_are_used_for_workspace_naming() {
        let mut mgr = manager_with_roots(vec![]);
        let ws_id = mgr.create_workspace();
        let sess_id = SessionId::new();
        mgr.add_session(ws_id, sess_id, None);

        mgr.set_roots(vec![PathBuf::from("/home/user/work")]);
        let msg = mgr.on_cwd_changed(sess_id, Path::new("/home/user/work/scribe"));

        assert!(matches!(
            msg,
            Some(ServerMessage::WorkspaceNamed { name, project_root, .. })
                if name == "scribe" && project_root == Some(PathBuf::from("/home/user/work/scribe"))
        ));
    }

    #[test]
    fn workspace_name_stable_within_same_root() {
        let mut mgr = manager_with_roots(vec!["/work"]);
        let ws_id = mgr.create_workspace();
        let sess_id = SessionId::new();
        mgr.add_session(ws_id, sess_id, None);

        mgr.on_cwd_changed(sess_id, Path::new("/work/myapp/src"));
        // Deeper navigation within the same project should not re-send.
        let msg = mgr.on_cwd_changed(sess_id, Path::new("/work/myapp/tests"));
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
        mgr.add_session(ws_id, sess_id, None);
        assert_eq!(mgr.workspace_for_session(sess_id), Some(ws_id));
        mgr.remove_session(sess_id);
        assert_eq!(mgr.workspace_for_session(sess_id), None);
    }

    #[test]
    fn move_session_rekeys_membership() {
        let mut mgr = manager_with_roots(vec![]);
        let ws_old = mgr.create_workspace();
        let ws_new = mgr.create_workspace();
        let sess = SessionId::new();
        mgr.add_session(ws_old, sess, None);

        // The split flow: session seeded through the old workspace, moved once
        // the new region's pane adopts it.
        assert!(mgr.move_session(sess, ws_new));
        assert_eq!(mgr.workspace_for_session(sess), Some(ws_new));
        assert_eq!(mgr.sessions_in_workspace(ws_old), Vec::new());
        assert_eq!(mgr.sessions_in_workspace(ws_new), vec![sess]);

        // Unknown target: dropped, membership unchanged.
        assert!(!mgr.move_session(sess, WorkspaceId::new()));
        assert_eq!(mgr.workspace_for_session(sess), Some(ws_new));
    }

    // @lat: [[server#Workspaces#Destructive close refusal]]
    #[test]
    fn close_with_live_sessions_is_refused() {
        let mut mgr = manager_with_roots(vec![]);
        let ws = mgr.create_workspace();
        let sess = SessionId::new();
        mgr.add_session(ws, sess, None);
        // A stale client's close must not unlink a live session into limbo.
        mgr.close_workspace(ws);
        assert_eq!(mgr.workspace_for_session(sess), Some(ws));
        assert!(mgr.workspace_info(ws).is_some());
        // Once the session is gone the close is truthful and lands.
        mgr.remove_session(sess);
        mgr.close_workspace(ws);
        assert!(mgr.workspace_info(ws).is_none());
    }

    #[test]
    fn workspace_tree_survives_handoff_roundtrip() {
        let mut mgr = manager_with_roots(vec![]);
        let ws_a = mgr.create_workspace();
        let ws_b = mgr.create_workspace();
        let sess_a = SessionId::new();
        let sess_b = SessionId::new();
        mgr.add_session(ws_a, sess_a, None);
        mgr.add_session(ws_b, sess_b, Some(LayoutDirection::Horizontal));

        // Simulate a client reporting a split tree.
        let tree = WorkspaceTreeNode::Split {
            direction: LayoutDirection::Vertical,
            ratio: 0.4,
            first: Box::new(WorkspaceTreeNode::Leaf {
                workspace_id: ws_a,
                session_ids: vec![],
                pane_trees: vec![],
                active_tab_index: 0,
            }),
            second: Box::new(WorkspaceTreeNode::Leaf {
                workspace_id: ws_b,
                session_ids: vec![],
                pane_trees: vec![],
                active_tab_index: 0,
            }),
        };
        mgr.set_workspace_tree(tree);

        // Serialize for handoff.
        let (workspaces, tree_out, _) = mgr.serialize_for_handoff();
        assert!(tree_out.is_some(), "tree should be present in handoff");

        // Restore from handoff.
        let restored = WorkspaceManager::restore_from_handoff(
            vec![],
            &workspaces,
            tree_out,
            &[],
            &[(sess_a, ws_a), (sess_b, ws_b)],
        );

        // Verify sessions survived.
        assert_eq!(restored.workspace_for_session(sess_a), Some(ws_a));
        assert_eq!(restored.workspace_for_session(sess_b), Some(ws_b));

        // Verify the tree survived.
        let restored_tree = restored.workspace_tree().expect("tree should survive handoff");
        match restored_tree {
            WorkspaceTreeNode::Split { direction, ratio, .. } => {
                assert_eq!(*direction, LayoutDirection::Vertical);
                assert!((*ratio - 0.4).abs() < f32::EPSILON);
            }
            WorkspaceTreeNode::Leaf { .. } => panic!("expected Split, got Leaf"),
        }
    }

    // @lat: [[server#Handoff#State Transfer#Membership self-healing]]
    #[test]
    fn restore_heals_memberships_from_session_records() {
        // The persisted maps know only `mapped` in `ws_known` / `win_main`;
        // `orphan_known_ws` lost both maps but names a workspace the maps
        // still have, and `orphan_lost_ws` names one they lost entirely —
        // the exact wreckage a stale client's destructive closes left.
        let mut mgr = manager_with_roots(vec![]);
        let ws_known = mgr.create_workspace();
        let mapped = SessionId::new();
        let orphan_known_ws = SessionId::new();
        let orphan_lost_ws = SessionId::new();
        let ws_lost = WorkspaceId::new();
        let win_main = WindowId::new();
        mgr.add_session(ws_known, mapped, None);
        mgr.assign_session_to_window(win_main, mapped);

        let (workspaces, tree, windows) = mgr.serialize_for_handoff();
        let restored = WorkspaceManager::restore_from_handoff(
            vec![],
            &workspaces,
            tree,
            &windows,
            &[(mapped, ws_known), (orphan_known_ws, ws_known), (orphan_lost_ws, ws_lost)],
        );

        // The mapped session is untouched; both orphans are re-filed under
        // the workspace their own record names, auto-created when lost.
        assert_eq!(restored.workspace_for_session(mapped), Some(ws_known));
        assert_eq!(restored.workspace_for_session(orphan_known_ws), Some(ws_known));
        assert_eq!(restored.workspace_for_session(orphan_lost_ws), Some(ws_lost));
        // Every orphan lands in a window again — sibling's window first,
        // busiest window as the fallback — so `SessionList` can show it.
        assert_eq!(restored.window_for_session(orphan_known_ws), Some(win_main));
        assert_eq!(restored.window_for_session(orphan_lost_ws), Some(win_main));
        assert_eq!(restored.sessions_for_window(win_main).len(), 3);
    }

    #[test]
    fn workspace_tree_none_when_not_set() {
        let mgr = manager_with_roots(vec![]);
        assert!(mgr.workspace_tree().is_none());

        let (_, tree, _) = mgr.serialize_for_handoff();
        assert!(tree.is_none());
    }
}
