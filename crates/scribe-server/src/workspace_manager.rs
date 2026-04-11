use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tracing::{debug, info};

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

#[allow(dead_code, reason = "used by create_workspace, called from CreateWorkspace handler")]
const ACCENT_COLORS: &[&str] =
    &["#a78bfa", "#38bdf8", "#6ee7b7", "#fb7185", "#fbbf24", "#a3e635", "#f472b6", "#22d3ee"];

/// Manages workspace ↔ session relationships, window ↔ session ownership,
/// and auto-names workspaces based on configured root directories.
pub struct WorkspaceManager {
    roots: Vec<PathBuf>,
    workspaces: HashMap<WorkspaceId, Workspace>,
    session_to_workspace: HashMap<SessionId, WorkspaceId>,
    #[allow(dead_code, reason = "used by create_workspace, called from CreateWorkspace handler")]
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
    #[allow(dead_code, reason = "used for workspace identity in future UI sync messages")]
    id: WorkspaceId,
    name: Option<String>,
    /// Absolute path to the project directory (`root / first_component`).
    /// Set alongside `name` when a CWD matches a configured workspace root.
    project_root: Option<PathBuf>,
    sessions: Vec<SessionId>,
    #[allow(dead_code, reason = "sent to UI in future WorkspaceInfo messages")]
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
        let cwd = macos_proc_cwd(child_pid)?;
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

    /// Return the workspace a session belongs to, if any.
    #[allow(dead_code, reason = "used by MoveSession handler and tests")]
    pub fn workspace_for_session(&self, session_id: SessionId) -> Option<WorkspaceId> {
        self.session_to_workspace.get(&session_id).copied()
    }

    /// Return the name, accent color, split direction, and project root of a
    /// workspace.
    ///
    /// Returns `Some(…)` if the workspace exists, `None` otherwise.
    #[allow(
        clippy::type_complexity,
        reason = "flat tuple avoids a one-off struct for an internal API"
    )]
    pub fn workspace_info(
        &self,
        id: WorkspaceId,
    ) -> Option<(Option<String>, String, Option<LayoutDirection>, Option<PathBuf>)> {
        self.workspaces.get(&id).map(|ws| {
            (ws.name.clone(), ws.accent_color.clone(), ws.split_direction, ws.project_root.clone())
        })
    }

    /// Return the legacy single workspace split tree (used by handoff serialisation).
    #[allow(dead_code, reason = "used in handoff serialization and tests")]
    pub fn workspace_tree(&self) -> Option<&WorkspaceTreeNode> {
        self.workspace_tree.as_ref()
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
    #[allow(dead_code, reason = "called from extract_workspace_name_pub in tests")]
    fn extract_workspace_info(&self, cwd: &Path) -> Option<(String, PathBuf)> {
        Self::extract_workspace_info_with_roots(cwd, &self.roots)
    }

    /// Inner helper that takes an explicit roots slice so the borrow checker
    /// is happy when `on_cwd_changed` passes a cloned roots vec.
    ///
    /// Returns `(name, project_root)` where `project_root` is `root / name`.
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
    pub fn restore_from_handoff(
        roots: Vec<PathBuf>,
        workspaces: &[HandoffWorkspace],
        workspace_tree: Option<WorkspaceTreeNode>,
        windows: &[HandoffWindowState],
        valid_session_ids: &HashSet<SessionId>,
    ) -> Self {
        let mut ws_map = HashMap::new();
        let mut session_to_workspace = HashMap::new();

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

            info!(
                workspace_id = %hw.id,
                name = ?hw.name,
                sessions = session_ids.len(),
                dropped_sessions = hw.session_ids.len().saturating_sub(session_ids.len()),
                "restored workspace from handoff"
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

        Self {
            roots,
            workspaces: ws_map,
            session_to_workspace,
            color_index: workspaces.len(),
            workspace_tree,
            window_trees,
            session_to_window,
        }
    }
}

/// Query the CWD of a process on macOS via `proc_pidinfo(PROC_PIDVNODEPATHINFO)`.
#[cfg(target_os = "macos")]
fn macos_proc_cwd(child_pid: u32) -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::mem::MaybeUninit;
    use std::os::raw::c_void;

    const PROC_PIDVNODEPATHINFO: i32 = 9;

    // `proc_vnodepathinfo` is 2 * `vnode_info_path` (each 1152 bytes) = 2304 bytes.
    // `vnode_info_path` = `vnode_info` (128 bytes) + path `[c_char; 1024]`.
    // `pvi_cdir` is the first `vnode_info_path` member; its path starts at byte 128.
    const VIP_PATH_OFFSET: usize = 128;
    const VNODE_INFO_PATH_SIZE: usize = 1152;
    const PROC_VNODEPATHINFO_SIZE: usize = VNODE_INFO_PATH_SIZE * 2;

    #[allow(unsafe_code, reason = "proc_pidinfo FFI is required for macOS CWD detection")]
    {
        unsafe extern "C" {
            fn proc_pidinfo(
                pid: i32,
                flavor: i32,
                arg: u64,
                buffer: *mut c_void,
                buffersize: i32,
            ) -> i32;
        }

        let mut buf = MaybeUninit::<[u8; PROC_VNODEPATHINFO_SIZE]>::uninit();

        let ret = unsafe {
            proc_pidinfo(
                i32::try_from(child_pid).ok()?,
                PROC_PIDVNODEPATHINFO,
                0,
                buf.as_mut_ptr().cast::<c_void>(),
                i32::try_from(PROC_VNODEPATHINFO_SIZE).ok()?,
            )
        };

        if ret <= 0 {
            return None;
        }

        let buf = unsafe { buf.assume_init() };

        // `pvi_cdir.vip_path` starts at VIP_PATH_OFFSET within the first
        // `vnode_info_path` member. Max path length is 1024 bytes (MAXPATHLEN).
        let path_bytes = buf.get(VIP_PATH_OFFSET..VNODE_INFO_PATH_SIZE)?;

        let c_str = CStr::from_bytes_until_nul(path_bytes).ok()?;
        let path = PathBuf::from(c_str.to_str().ok()?);

        if path.as_os_str().is_empty() {
            return None;
        }

        Some(path)
    }
}

// Expose the private helper for unit-testing without making it pub on the
// main type.
#[cfg(test)]
impl WorkspaceManager {
    pub fn extract_workspace_name_pub(&self, cwd: &Path) -> Option<String> {
        self.extract_workspace_info(cwd).map(|(name, _)| name)
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
            }),
            second: Box::new(WorkspaceTreeNode::Leaf {
                workspace_id: ws_b,
                session_ids: vec![],
                pane_trees: vec![],
            }),
        };
        mgr.set_workspace_tree(tree);

        // Serialize for handoff.
        let (workspaces, tree_out, _) = mgr.serialize_for_handoff();
        assert!(tree_out.is_some(), "tree should be present in handoff");

        // Restore from handoff.
        let valid_session_ids = HashSet::from([sess_a, sess_b]);
        let restored = WorkspaceManager::restore_from_handoff(
            vec![],
            &workspaces,
            tree_out,
            &[],
            &valid_session_ids,
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

    #[test]
    fn workspace_tree_none_when_not_set() {
        let mgr = manager_with_roots(vec![]);
        assert!(mgr.workspace_tree().is_none());

        let (_, tree, _) = mgr.serialize_for_handoff();
        assert!(tree.is_none());
    }
}
