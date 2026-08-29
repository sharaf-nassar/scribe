use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{
    LayoutDirection, PaneTreeNode, ServerMessage, WorkspaceMoveOperation, WorkspaceMoveRefusal,
    WorkspaceTransferRefusal, WorkspaceTreeEdge, WorkspaceTreeError, WorkspaceTreeNode,
    active_tab_index_after_departure,
};

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
type WorkspaceTransferTrees = (WorkspaceTreeNode, WorkspaceTreeNode);
type WorkspaceMoveTrees = (Option<WorkspaceTreeNode>, WorkspaceTreeNode);

struct MovedTabSubtree {
    tab_session_id: SessionId,
    pane_tree: Option<PaneTreeNode>,
    sessions: Vec<SessionId>,
    was_active: bool,
}

/// Session ownership changes derived for one existing-window workspace move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMoveCandidates {
    pub source_sessions: Vec<SessionId>,
    pub target_sessions: Vec<SessionId>,
    pub source_closed: bool,
}

/// Ids-only input to one server-derived existing-window workspace move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceMoveRequest {
    pub source_window: WindowId,
    pub workspace_id: WorkspaceId,
    pub target_window: WindowId,
    pub target_workspace_id: WorkspaceId,
    pub operation: WorkspaceMoveOperation,
}

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
        crate::state_dump::mark_dirty();
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
        crate::state_dump::mark_dirty();
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
        crate::state_dump::mark_dirty();
        info!(%workspace_id, "closed workspace");
    }

    /// Remove a session from its workspace.
    pub fn remove_session(&mut self, session_id: SessionId) {
        if let Some(workspace_id) = self.session_to_workspace.remove(&session_id)
            && let Some(ws) = self.workspaces.get_mut(&workspace_id)
        {
            ws.sessions.retain(|&s| s != session_id);
            crate::state_dump::mark_dirty();
            debug!(%session_id, %workspace_id, "removed session from workspace");
        }
    }

    /// Called when the CWD of a session changes.
    ///
    /// Matches the CWD against configured roots. When a match is found the
    /// first path component after the root prefix becomes the workspace name.
    /// Only the first session in workspace order may update or clear the shared
    /// name and project root.
    ///
    /// Returns `Some(ServerMessage::WorkspaceNamed { … })` when the name
    /// changes, `None` otherwise.
    pub fn on_cwd_changed(&mut self, session_id: SessionId, cwd: &Path) -> Option<ServerMessage> {
        let workspace_id = *self.session_to_workspace.get(&session_id)?;
        if self.workspaces.get(&workspace_id)?.sessions.first() != Some(&session_id) {
            return None;
        }

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
        crate::state_dump::mark_dirty();
        self.window_trees.insert(window_id, tree);
    }

    // ── Workspace transfer (spec 029) ────────────────────────────────

    /// Validate a workspace transfer without mutating anything.
    ///
    /// Returns the sessions that would move (the workspace's authoritative
    /// membership) or the typed refusal the requester gets. Both post-move
    /// trees are derivable exactly when this returns `Ok`: the source
    /// window's stored tree contains the workspace and at least one other
    /// leaf.
    pub fn validate_workspace_transfer(
        &self,
        source_window: WindowId,
        workspace_id: WorkspaceId,
        target_window: WindowId,
    ) -> Result<Vec<SessionId>, WorkspaceTransferRefusal> {
        self.check_workspace_transfer(source_window, workspace_id, target_window)
            .map(|(moved, _)| moved)
    }

    /// Atomically commit a workspace transfer: derive both post-move trees
    /// through the shared `WorkspaceTreeNode` operations, re-key every moved
    /// session's window mapping, and store the target window's tree. Refusal
    /// leaves every map byte-identical — validation runs before the first
    /// mutation and the tree edit happens on a clone.
    pub fn transfer_workspace(
        &mut self,
        source_window: WindowId,
        workspace_id: WorkspaceId,
        target_window: WindowId,
    ) -> Result<Vec<SessionId>, WorkspaceTransferRefusal> {
        let (moved, (source_tree, target_tree)) =
            self.check_workspace_transfer(source_window, workspace_id, target_window)?;

        crate::state_dump::mark_dirty();
        self.window_trees.insert(source_window, source_tree);
        self.window_trees.insert(target_window, target_tree);
        for &session_id in &moved {
            self.session_to_window.insert(session_id, target_window);
        }
        info!(
            %workspace_id,
            %source_window,
            %target_window,
            sessions = moved.len(),
            "transferred workspace to a new window"
        );
        Ok(moved)
    }

    /// Shared validation + tree derivation for the two entry points above.
    /// Pure: derives the post-move `(source, target)` trees on clones.
    fn check_workspace_transfer(
        &self,
        source_window: WindowId,
        workspace_id: WorkspaceId,
        target_window: WindowId,
    ) -> Result<(Vec<SessionId>, WorkspaceTransferTrees), WorkspaceTransferRefusal> {
        if target_window == source_window
            || self.window_trees.contains_key(&target_window)
            || self.session_to_window.values().any(|&window| window == target_window)
        {
            return Err(WorkspaceTransferRefusal::TargetWindowIdCollision);
        }
        let Some(workspace) = self.workspaces.get(&workspace_id) else {
            return Err(WorkspaceTransferRefusal::UnknownWorkspace);
        };
        let moved = workspace.sessions.clone();
        if !moved.iter().all(|session| self.session_to_window.get(session) == Some(&source_window))
        {
            return Err(WorkspaceTransferRefusal::NotWorkspaceOwner);
        }
        let Some(tree) = self.window_trees.get(&source_window) else {
            // No reported layout to derive from. A window whose sessions all
            // belong to this workspace is a single-region window (nothing
            // would remain); anything else cannot prove region ownership.
            let window_sessions = self.sessions_for_window(source_window);
            let all_here = !window_sessions.is_empty()
                && window_sessions
                    .iter()
                    .all(|session| self.session_to_workspace.get(session) == Some(&workspace_id));
            return Err(if all_here {
                WorkspaceTransferRefusal::SoleWorkspace
            } else {
                WorkspaceTransferRefusal::NotWorkspaceOwner
            });
        };

        let mut source_tree = tree.clone();
        let extracted =
            source_tree.extract_workspace(workspace_id).map_err(|error| match error {
                WorkspaceTreeError::SoleWorkspace { .. } => WorkspaceTransferRefusal::SoleWorkspace,
                WorkspaceTreeError::WorkspaceNotFound { .. }
                | WorkspaceTreeError::InsertedWorkspaceMustBeLeaf
                | WorkspaceTreeError::WorkspaceAlreadyPresent { .. } => {
                    WorkspaceTransferRefusal::NotWorkspaceOwner
                }
            })?;
        Ok((moved, (source_tree, extracted)))
    }

    // ── Workspace move (existing destination window) ────────────────

    /// Validate an edge insertion or bidirectional swap without mutation.
    pub fn validate_workspace_move(
        &self,
        request: WorkspaceMoveRequest,
    ) -> Result<WorkspaceMoveCandidates, WorkspaceMoveRefusal> {
        self.check_workspace_move(request).map(|(candidates, _)| candidates)
    }

    /// Commit one existing-window workspace move from server-derived trees.
    ///
    /// Both directions of a swap are applied under this one mutable borrow.
    /// Validation and tree edits happen on clones before the first registry
    /// mutation, so a refusal leaves workspace ownership byte-identical.
    pub fn move_workspace(
        &mut self,
        request: WorkspaceMoveRequest,
    ) -> Result<WorkspaceMoveCandidates, WorkspaceMoveRefusal> {
        let (candidates, (source_tree, target_tree)) = self.check_workspace_move(request)?;
        let WorkspaceMoveRequest {
            source_window,
            workspace_id,
            target_window,
            target_workspace_id,
            operation,
        } = request;

        crate::state_dump::mark_dirty();
        if matches!(operation, WorkspaceMoveOperation::MoveTab { .. }) {
            self.window_trees.insert(source_window, target_tree);
            self.move_tab_membership(
                &candidates.source_sessions,
                workspace_id,
                target_workspace_id,
            );
        } else {
            if let Some(source_tree) = source_tree {
                self.window_trees.insert(source_window, source_tree);
            } else {
                self.window_trees.remove(&source_window);
            }
            self.window_trees.insert(target_window, target_tree);
            for &session_id in &candidates.source_sessions {
                self.session_to_window.insert(session_id, target_window);
            }
            for &session_id in &candidates.target_sessions {
                self.session_to_window.insert(session_id, source_window);
            }
        }
        info!(
            %workspace_id,
            %source_window,
            %target_window,
            target_workspace = %target_workspace_id,
            ?operation,
            source_closed = candidates.source_closed,
            "moved workspace between existing windows"
        );
        Ok(candidates)
    }

    /// Pure validation and post-tree derivation for [`Self::move_workspace`].
    fn check_workspace_move(
        &self,
        request: WorkspaceMoveRequest,
    ) -> Result<(WorkspaceMoveCandidates, WorkspaceMoveTrees), WorkspaceMoveRefusal> {
        let WorkspaceMoveRequest {
            source_window,
            workspace_id,
            target_window,
            target_workspace_id,
            operation,
        } = request;
        if let WorkspaceMoveOperation::MoveTab { tab_session_id, target_index } = operation {
            return self.check_tab_subtree_move(request, tab_session_id, target_index);
        }
        if source_window == target_window {
            return Err(WorkspaceMoveRefusal::TargetWindowUnavailable);
        }
        let source_sessions = self
            .workspaces
            .get(&workspace_id)
            .ok_or(WorkspaceMoveRefusal::UnknownWorkspace)?
            .sessions
            .clone();
        if !self.sessions_owned_by_window(&source_sessions, source_window) {
            return Err(WorkspaceMoveRefusal::NotWorkspaceOwner);
        }
        let source_tree = self.source_move_tree(request, &source_sessions)?;
        let target_tree = self
            .window_trees
            .get(&target_window)
            .ok_or(WorkspaceMoveRefusal::TargetWindowUnavailable)?;
        let target_sessions = self
            .workspaces
            .get(&target_workspace_id)
            .ok_or(WorkspaceMoveRefusal::TargetWorkspaceUnavailable)?
            .sessions
            .clone();
        if !self.sessions_owned_by_window(&target_sessions, target_window) {
            return Err(WorkspaceMoveRefusal::TargetWorkspaceUnavailable);
        }

        let (source_tree, target_tree, source_closed) = match operation {
            WorkspaceMoveOperation::InsertAtEdge { edge } => Self::derive_edge_move(
                source_tree.clone(),
                target_tree.clone(),
                workspace_id,
                target_workspace_id,
                edge,
            )?,
            WorkspaceMoveOperation::Swap => Self::derive_swap(
                source_tree.clone(),
                target_tree.clone(),
                workspace_id,
                target_workspace_id,
            )?,
            WorkspaceMoveOperation::MoveTab { .. } => {
                return Err(WorkspaceMoveRefusal::NotWorkspaceOwner);
            }
        };
        Ok((
            WorkspaceMoveCandidates {
                source_sessions,
                target_sessions: if operation == WorkspaceMoveOperation::Swap {
                    target_sessions
                } else {
                    Vec::new()
                },
                source_closed,
            },
            (source_tree, target_tree),
        ))
    }

    fn check_tab_subtree_move(
        &self,
        request: WorkspaceMoveRequest,
        tab_session_id: SessionId,
        target_index: usize,
    ) -> Result<(WorkspaceMoveCandidates, WorkspaceMoveTrees), WorkspaceMoveRefusal> {
        let WorkspaceMoveRequest {
            source_window,
            workspace_id,
            target_window,
            target_workspace_id,
            ..
        } = request;
        if source_window != target_window {
            return Err(WorkspaceMoveRefusal::TargetWindowUnavailable);
        }
        if workspace_id == target_workspace_id {
            return Err(WorkspaceMoveRefusal::TargetWorkspaceUnavailable);
        }
        if !self.workspaces.contains_key(&workspace_id) {
            return Err(WorkspaceMoveRefusal::UnknownWorkspace);
        }
        let target_sessions = self
            .workspaces
            .get(&target_workspace_id)
            .ok_or(WorkspaceMoveRefusal::TargetWorkspaceUnavailable)?
            .sessions
            .clone();
        if !self.sessions_owned_by_window(&target_sessions, target_window) {
            return Err(WorkspaceMoveRefusal::TargetWorkspaceUnavailable);
        }
        let mut tree = self
            .window_trees
            .get(&source_window)
            .cloned()
            .ok_or(WorkspaceMoveRefusal::NotWorkspaceOwner)?;
        let moved = Self::take_tab_subtree(&mut tree, workspace_id, tab_session_id)?;
        if !moved.sessions.iter().all(|session_id| {
            self.session_to_window.get(session_id) == Some(&source_window)
                && self.session_to_workspace.get(session_id) == Some(&workspace_id)
        }) {
            return Err(WorkspaceMoveRefusal::NotWorkspaceOwner);
        }
        let moved_sessions = moved.sessions.clone();
        if Self::workspace_leaf_is_empty(&tree, workspace_id) {
            let source_sessions = &self
                .workspaces
                .get(&workspace_id)
                .ok_or(WorkspaceMoveRefusal::UnknownWorkspace)?
                .sessions;
            if source_sessions.iter().any(|session_id| !moved.sessions.contains(session_id)) {
                return Err(WorkspaceMoveRefusal::NotWorkspaceOwner);
            }
            tree.extract_workspace(workspace_id)
                .map_err(|_| WorkspaceMoveRefusal::NotWorkspaceOwner)?;
        }
        Self::insert_tab_subtree(&mut tree, target_workspace_id, target_index, moved)?;
        Ok((
            WorkspaceMoveCandidates {
                source_sessions: moved_sessions,
                target_sessions: Vec::new(),
                source_closed: false,
            },
            (Some(tree.clone()), tree),
        ))
    }

    fn take_tab_subtree(
        node: &mut WorkspaceTreeNode,
        workspace_id: WorkspaceId,
        tab_session_id: SessionId,
    ) -> Result<MovedTabSubtree, WorkspaceMoveRefusal> {
        match node {
            WorkspaceTreeNode::Leaf {
                workspace_id: leaf_id,
                session_ids,
                pane_trees,
                active_tab_index,
            } if *leaf_id == workspace_id => {
                let index = session_ids
                    .iter()
                    .position(|session_id| *session_id == tab_session_id)
                    .ok_or(WorkspaceMoveRefusal::NotWorkspaceOwner)?;
                pane_trees.resize(session_ids.len(), None);
                let previously_active_index = *active_tab_index;
                let showing = session_ids.get(previously_active_index).copied();
                let was_active = showing == Some(tab_session_id);
                session_ids.remove(index);
                let pane_tree = pane_trees.remove(index);
                *active_tab_index = active_tab_index_after_departure(
                    session_ids.len(),
                    index,
                    previously_active_index,
                );
                let mut sessions = Vec::new();
                if let Some(tree) = pane_tree.as_ref() {
                    Self::collect_pane_sessions(tree, &mut sessions);
                }
                if !sessions.contains(&tab_session_id) {
                    sessions.insert(0, tab_session_id);
                }
                Ok(MovedTabSubtree { tab_session_id, pane_tree, sessions, was_active })
            }
            WorkspaceTreeNode::Split { first, second, .. } => {
                if Self::workspace_tree_has_leaf(first, workspace_id) {
                    Self::take_tab_subtree(first, workspace_id, tab_session_id)
                } else {
                    Self::take_tab_subtree(second, workspace_id, tab_session_id)
                }
            }
            WorkspaceTreeNode::Leaf { .. } => Err(WorkspaceMoveRefusal::NotWorkspaceOwner),
        }
    }

    fn insert_tab_subtree(
        node: &mut WorkspaceTreeNode,
        workspace_id: WorkspaceId,
        target_index: usize,
        moved: MovedTabSubtree,
    ) -> Result<(), WorkspaceMoveRefusal> {
        match node {
            WorkspaceTreeNode::Leaf {
                workspace_id: leaf_id,
                session_ids,
                pane_trees,
                active_tab_index,
            } if *leaf_id == workspace_id => {
                if target_index > session_ids.len() {
                    return Err(WorkspaceMoveRefusal::TargetWorkspaceUnavailable);
                }
                pane_trees.resize(session_ids.len(), None);
                let showing = session_ids.get(*active_tab_index).copied();
                session_ids.insert(target_index, moved.tab_session_id);
                pane_trees.insert(target_index, moved.pane_tree);
                *active_tab_index = if moved.was_active {
                    target_index
                } else {
                    Self::active_index_or(session_ids, showing)
                };
                Ok(())
            }
            WorkspaceTreeNode::Split { first, second, .. } => {
                if Self::workspace_tree_has_leaf(first, workspace_id) {
                    Self::insert_tab_subtree(first, workspace_id, target_index, moved)
                } else {
                    Self::insert_tab_subtree(second, workspace_id, target_index, moved)
                }
            }
            WorkspaceTreeNode::Leaf { .. } => Err(WorkspaceMoveRefusal::TargetWorkspaceUnavailable),
        }
    }

    fn active_index_or(session_ids: &[SessionId], showing: Option<SessionId>) -> usize {
        showing
            .and_then(|session_id| {
                session_ids.iter().position(|candidate| *candidate == session_id)
            })
            .unwrap_or(0)
    }

    fn workspace_tree_find_leaf<'a, P>(
        node: &'a WorkspaceTreeNode,
        workspace_id: WorkspaceId,
        predicate: &P,
    ) -> Option<&'a WorkspaceTreeNode>
    where
        P: Fn(&WorkspaceTreeNode) -> bool,
    {
        match node {
            WorkspaceTreeNode::Leaf { workspace_id: leaf_id, .. }
                if *leaf_id == workspace_id && predicate(node) =>
            {
                Some(node)
            }
            WorkspaceTreeNode::Split { first, second, .. } => {
                Self::workspace_tree_find_leaf(first, workspace_id, predicate)
                    .or_else(|| Self::workspace_tree_find_leaf(second, workspace_id, predicate))
            }
            WorkspaceTreeNode::Leaf { .. } => None,
        }
    }

    fn workspace_tree_has_leaf(node: &WorkspaceTreeNode, workspace_id: WorkspaceId) -> bool {
        Self::workspace_tree_find_leaf(node, workspace_id, &|_| true).is_some()
    }

    fn workspace_leaf_is_empty(node: &WorkspaceTreeNode, workspace_id: WorkspaceId) -> bool {
        Self::workspace_tree_find_leaf(node, workspace_id, &|leaf| {
            matches!(leaf, WorkspaceTreeNode::Leaf { session_ids, .. } if session_ids.is_empty())
        })
        .is_some()
    }

    fn collect_pane_sessions(node: &PaneTreeNode, sessions: &mut Vec<SessionId>) {
        match node {
            PaneTreeNode::Leaf { session_id } => {
                if !sessions.contains(session_id) {
                    sessions.push(*session_id);
                }
            }
            PaneTreeNode::Split { first, second, .. } => {
                Self::collect_pane_sessions(first, sessions);
                Self::collect_pane_sessions(second, sessions);
            }
        }
    }

    fn move_tab_membership(
        &mut self,
        sessions: &[SessionId],
        source: WorkspaceId,
        target: WorkspaceId,
    ) {
        if let Some(workspace) = self.workspaces.get_mut(&source) {
            workspace.sessions.retain(|session_id| !sessions.contains(session_id));
        }
        if let Some(workspace) = self.workspaces.get_mut(&target) {
            let additions: Vec<SessionId> = sessions
                .iter()
                .copied()
                .filter(|session_id| !workspace.sessions.contains(session_id))
                .collect();
            workspace.sessions.extend(additions);
        }
        for &session_id in sessions {
            self.session_to_workspace.insert(session_id, target);
        }
        if self.workspaces.get(&source).is_some_and(|workspace| workspace.sessions.is_empty()) {
            self.workspaces.remove(&source);
        }
    }

    fn sessions_owned_by_window(&self, sessions: &[SessionId], window: WindowId) -> bool {
        sessions.iter().all(|session| self.session_to_window.get(session) == Some(&window))
    }

    fn source_move_tree(
        &self,
        request: WorkspaceMoveRequest,
        source_sessions: &[SessionId],
    ) -> Result<&WorkspaceTreeNode, WorkspaceMoveRefusal> {
        if let Some(tree) = self.window_trees.get(&request.source_window) {
            return Ok(tree);
        }
        let all_source = !source_sessions.is_empty()
            && self.sessions_for_window(request.source_window).iter().all(|session| {
                self.session_to_workspace.get(session) == Some(&request.workspace_id)
            });
        Err(if all_source && request.operation == WorkspaceMoveOperation::Swap {
            WorkspaceMoveRefusal::SoleWorkspace
        } else {
            WorkspaceMoveRefusal::NotWorkspaceOwner
        })
    }

    fn derive_edge_move(
        mut source_tree: WorkspaceTreeNode,
        mut target_tree: WorkspaceTreeNode,
        workspace_id: WorkspaceId,
        target_workspace_id: WorkspaceId,
        edge: WorkspaceTreeEdge,
    ) -> Result<(Option<WorkspaceTreeNode>, WorkspaceTreeNode, bool), WorkspaceMoveRefusal> {
        let source_closed = matches!(source_tree, WorkspaceTreeNode::Leaf { .. });
        let detached = Self::detach_workspace_leaf(&mut source_tree, workspace_id)?;
        target_tree.insert_workspace_at_edge(target_workspace_id, edge, detached).map_err(
            |error| match error {
                WorkspaceTreeError::WorkspaceNotFound { .. } => {
                    WorkspaceMoveRefusal::TargetWorkspaceUnavailable
                }
                WorkspaceTreeError::InsertedWorkspaceMustBeLeaf
                | WorkspaceTreeError::WorkspaceAlreadyPresent { .. }
                | WorkspaceTreeError::SoleWorkspace { .. } => {
                    WorkspaceMoveRefusal::NotWorkspaceOwner
                }
            },
        )?;
        Ok(((!source_closed).then_some(source_tree), target_tree, source_closed))
    }

    fn detach_workspace_leaf(
        source_tree: &mut WorkspaceTreeNode,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceTreeNode, WorkspaceMoveRefusal> {
        match source_tree.extract_workspace(workspace_id) {
            Ok(detached) => Ok(detached),
            Err(WorkspaceTreeError::SoleWorkspace { .. }) if matches!(source_tree, WorkspaceTreeNode::Leaf { workspace_id: id, .. } if *id == workspace_id) => {
                Ok(source_tree.clone())
            }
            Err(
                WorkspaceTreeError::WorkspaceNotFound { .. }
                | WorkspaceTreeError::InsertedWorkspaceMustBeLeaf
                | WorkspaceTreeError::WorkspaceAlreadyPresent { .. }
                | WorkspaceTreeError::SoleWorkspace { .. },
            ) => Err(WorkspaceMoveRefusal::NotWorkspaceOwner),
        }
    }

    fn derive_swap(
        source_tree: WorkspaceTreeNode,
        target_tree: WorkspaceTreeNode,
        workspace_id: WorkspaceId,
        target_workspace_id: WorkspaceId,
    ) -> Result<(Option<WorkspaceTreeNode>, WorkspaceTreeNode, bool), WorkspaceMoveRefusal> {
        if matches!(source_tree, WorkspaceTreeNode::Leaf { .. }) {
            return Err(WorkspaceMoveRefusal::SoleWorkspace);
        }
        let mut joined = WorkspaceTreeNode::Split {
            direction: LayoutDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(source_tree),
            second: Box::new(target_tree),
        };
        joined.swap_workspaces(workspace_id, target_workspace_id).map_err(|error| match error {
            WorkspaceTreeError::WorkspaceNotFound { workspace_id: missing }
                if missing == target_workspace_id =>
            {
                WorkspaceMoveRefusal::TargetWorkspaceUnavailable
            }
            WorkspaceTreeError::WorkspaceNotFound { .. }
            | WorkspaceTreeError::InsertedWorkspaceMustBeLeaf
            | WorkspaceTreeError::WorkspaceAlreadyPresent { .. }
            | WorkspaceTreeError::SoleWorkspace { .. } => WorkspaceMoveRefusal::NotWorkspaceOwner,
        })?;
        match joined {
            WorkspaceTreeNode::Split { first, second, .. } => Ok((Some(*first), *second, false)),
            WorkspaceTreeNode::Leaf { .. } => Err(WorkspaceMoveRefusal::NotWorkspaceOwner),
        }
    }

    // ── Window tracking ──────────────────────────────────────────────

    /// Assign a session to a window.
    pub fn assign_session_to_window(&mut self, window_id: WindowId, session_id: SessionId) {
        self.session_to_window.insert(session_id, window_id);
        debug!(%session_id, %window_id, "assigned session to window");
    }

    /// Return all session IDs belonging to a window, in the order the window
    /// itself last reported.
    ///
    /// This is the order a reconnecting client's tab strip is built in when it
    /// has no tree to adopt, so it has to be stable and it has to mean
    /// something. Walking `self.workspaces` — a `HashMap` — as the only source
    /// did neither: a multi-region window's tabs came back grouped in whatever
    /// order the map happened to iterate, differently on every server process.
    ///
    /// The window's own reported tree leads, because its leaves carry the user's
    /// tab order per region. Sessions created since that report (or belonging to
    /// a window that never reported one) follow in workspace-stored order, with
    /// the workspaces themselves in a stable id order so the tail cannot shuffle
    /// either.
    pub fn sessions_for_window(&self, window_id: WindowId) -> Vec<SessionId> {
        // Collect which sessions belong to this window.
        let window_sids: HashSet<SessionId> = self
            .session_to_window
            .iter()
            .filter(|&(_, &wid)| wid == window_id)
            .map(|(&sid, _)| sid)
            .collect();

        // The window's own reported tree leads; everything it does not name
        // follows in workspace-stored order, workspaces in a stable id order.
        let mut candidates: Vec<SessionId> = Vec::with_capacity(window_sids.len());
        if let Some(tree) = self.window_trees.get(&window_id) {
            collect_tree_sessions(tree, &mut candidates);
        }
        let mut workspace_ids: Vec<WorkspaceId> = self.workspaces.keys().copied().collect();
        workspace_ids.sort_by_key(|id| id.to_full_string());
        for workspace_id in workspace_ids {
            let Some(workspace) = self.workspaces.get(&workspace_id) else { continue };
            candidates.extend(workspace.sessions.iter().copied());
        }

        let mut ordered: Vec<SessionId> = Vec::with_capacity(window_sids.len());
        for session_id in candidates {
            if window_sids.contains(&session_id) && !ordered.contains(&session_id) {
                ordered.push(session_id);
            }
        }
        ordered
    }

    /// Whether `workspace_id` owns a session visible in `window_id`.
    #[must_use]
    pub fn window_contains_workspace(
        &self,
        window_id: WindowId,
        workspace_id: WorkspaceId,
    ) -> bool {
        self.sessions_for_window(window_id)
            .iter()
            .any(|session_id| self.session_to_workspace.get(session_id) == Some(&workspace_id))
    }

    /// Whether every session named by a reported tree is authoritatively owned
    /// by `window_id`. Empty leaves are allowed (they carry no stale session
    /// authority); a pre-transfer source report that still names moved tabs is
    /// rejected by this check instead of restoring the detached workspace.
    #[must_use]
    pub fn reported_tree_belongs_to_window(
        &self,
        window_id: WindowId,
        tree: &WorkspaceTreeNode,
    ) -> bool {
        let mut session_ids = Vec::new();
        collect_tree_sessions(tree, &mut session_ids);
        session_ids
            .into_iter()
            .all(|session_id| self.window_for_session(session_id) == Some(window_id))
    }

    /// Connected-window identities containing a workspace rooted at `project_root`.
    #[must_use]
    pub fn windows_for_project_root(&self, project_root: &Path) -> HashSet<WindowId> {
        self.window_workspaces_for_project_root(project_root)
            .into_iter()
            .map(|(window_id, _)| window_id)
            .collect()
    }

    /// Connected window/workspace pairs rooted at `project_root`.
    #[must_use]
    pub fn window_workspaces_for_project_root(
        &self,
        project_root: &Path,
    ) -> HashSet<(WindowId, WorkspaceId)> {
        self.session_to_window
            .iter()
            .filter_map(|(session_id, window_id)| {
                let workspace_id = self.session_to_workspace.get(session_id)?;
                let workspace = self.workspaces.get(workspace_id)?;
                (workspace.project_root.as_deref() == Some(project_root))
                    .then_some((*window_id, *workspace_id))
            })
            .collect()
    }

    /// Whether one window contains a workspace rooted at `project_root`.
    #[must_use]
    pub fn window_contains_project_root(&self, window_id: WindowId, project_root: &Path) -> bool {
        self.windows_for_project_root(project_root).contains(&window_id)
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
        crate::state_dump::mark_dirty();
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

/// Agent world-capture view (spec 027). Implemented against the library's
/// trait via `crate::agent_api` in both compiles of this file — the binary
/// re-exports the library's `agent_api`, so its recompiled `WorkspaceManager`
/// still satisfies the one nominal bound `agent_api::world::capture` uses.
impl crate::agent_api::world::WorkspaceView for WorkspaceManager {
    fn window_ids_with_sessions(&self) -> HashSet<WindowId> {
        Self::window_ids_with_sessions(self)
    }

    fn workspace_names_for_window(&self, window_id: WindowId) -> Vec<String> {
        Self::workspace_names_for_window(self, window_id)
    }

    fn window_session_count(&self, window_id: WindowId) -> usize {
        self.sessions_for_window(window_id).len()
    }

    fn window_for_session(&self, session_id: SessionId) -> Option<WindowId> {
        Self::window_for_session(self, session_id)
    }

    fn workspace_name(&self, workspace_id: WorkspaceId) -> Option<String> {
        self.workspace_info(workspace_id).and_then(|(name, _, _, _)| name)
    }
}

/// Every session a reported workspace tree names, in left-to-right region order
/// and, within a region, in the client's own tab order.
fn collect_tree_sessions(node: &WorkspaceTreeNode, out: &mut Vec<SessionId>) {
    match node {
        WorkspaceTreeNode::Leaf { session_ids, .. } => out.extend(session_ids.iter().copied()),
        WorkspaceTreeNode::Split { first, second, .. } => {
            collect_tree_sessions(first, out);
            collect_tree_sessions(second, out);
        }
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

    // @lat: [[server#Workspaces#Auto-Naming]]
    #[test]
    fn only_first_session_controls_workspace_name_and_project_root() {
        let mut mgr = manager_with_roots(vec!["/work"]);
        let ws_id = mgr.create_workspace();
        let first = SessionId::new();
        let second = SessionId::new();
        mgr.add_session(ws_id, first, None);
        mgr.add_session(ws_id, second, None);

        assert!(matches!(
            mgr.on_cwd_changed(first, Path::new("/work/first/src")),
            Some(ServerMessage::WorkspaceNamed { name, project_root, .. })
                if name == "first" && project_root == Some(PathBuf::from("/work/first"))
        ));
        assert!(mgr.on_cwd_changed(second, Path::new("/work/second/src")).is_none());
        assert!(mgr.on_cwd_changed(second, Path::new("/tmp")).is_none());
        let info = mgr.workspace_info(ws_id).expect("workspace info");
        assert_eq!(info.0.as_deref(), Some("first"));
        assert_eq!(info.3, Some(PathBuf::from("/work/first")));

        assert!(matches!(
            mgr.on_cwd_changed(first, Path::new("/work/renamed/src")),
            Some(ServerMessage::WorkspaceNamed { name, project_root, .. })
                if name == "renamed" && project_root == Some(PathBuf::from("/work/renamed"))
        ));
        assert!(matches!(
            mgr.on_cwd_changed(first, Path::new("/tmp")),
            Some(ServerMessage::WorkspaceNamed { name, project_root, .. })
                if name.is_empty() && project_root.is_none()
        ));

        mgr.reorder_sessions(ws_id, &[second, first]);
        assert!(mgr.on_cwd_changed(first, Path::new("/work/ignored")).is_none());
        assert!(matches!(
            mgr.on_cwd_changed(second, Path::new("/work/promoted/src")),
            Some(ServerMessage::WorkspaceNamed { name, project_root, .. })
                if name == "promoted" && project_root == Some(PathBuf::from("/work/promoted"))
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

    #[test]
    fn window_contains_only_its_own_workspaces() {
        let mut mgr = manager_with_roots(vec![]);
        let ours = mgr.create_workspace();
        let theirs = mgr.create_workspace();
        let our_session = SessionId::new();
        let their_session = SessionId::new();
        let window = WindowId::new();
        mgr.add_session(ours, our_session, None);
        mgr.add_session(theirs, their_session, None);
        mgr.assign_session_to_window(window, our_session);

        assert!(mgr.window_contains_workspace(window, ours));
        assert!(!mgr.window_contains_workspace(window, theirs));
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

    // @lat: [[server#Workspaces#Per-Window Trees#Session order follows the reported tree]]
    #[test]
    fn session_order_follows_the_reported_tree() {
        let mut mgr = manager_with_roots(vec![]);
        let window = WindowId::new();
        let left = mgr.create_workspace();
        let right = mgr.create_workspace();
        let (l1, l2, r1) = (SessionId::new(), SessionId::new(), SessionId::new());
        for (workspace, session) in [(left, l1), (left, l2), (right, r1)] {
            mgr.add_session(workspace, session, None);
            mgr.assign_session_to_window(window, session);
        }

        // The client reports the user's order: the right-hand region first, and
        // its own tabs reversed. Walking the workspace map instead would answer
        // in whatever order it iterates, differently per process.
        mgr.set_window_tree(
            window,
            WorkspaceTreeNode::Split {
                direction: LayoutDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(WorkspaceTreeNode::Leaf {
                    workspace_id: right,
                    session_ids: vec![r1],
                    pane_trees: vec![None],
                    active_tab_index: 0,
                }),
                second: Box::new(WorkspaceTreeNode::Leaf {
                    workspace_id: left,
                    session_ids: vec![l2, l1],
                    pane_trees: vec![None, None],
                    active_tab_index: 0,
                }),
            },
        );
        assert_eq!(mgr.sessions_for_window(window), vec![r1, l2, l1]);

        // A session created since that report is not lost: it follows the ones
        // the tree names rather than displacing them.
        let fresh = SessionId::new();
        mgr.add_session(left, fresh, None);
        mgr.assign_session_to_window(window, fresh);
        assert_eq!(mgr.sessions_for_window(window), vec![r1, l2, l1, fresh]);

        // Another window's sessions never leak in.
        let other_window = WindowId::new();
        let theirs = SessionId::new();
        mgr.add_session(left, theirs, None);
        mgr.assign_session_to_window(other_window, theirs);
        assert_eq!(mgr.sessions_for_window(window), vec![r1, l2, l1, fresh]);
        assert_eq!(mgr.sessions_for_window(other_window), vec![theirs]);
    }

    fn leaf(workspace_id: WorkspaceId, session_ids: Vec<SessionId>) -> WorkspaceTreeNode {
        let pane_trees = session_ids.iter().map(|_| None).collect();
        WorkspaceTreeNode::Leaf { workspace_id, session_ids, pane_trees, active_tab_index: 0 }
    }

    #[test]
    fn workspace_leaf_predicates_scan_all_duplicate_workspace_leaves() {
        let workspace = WorkspaceId::new();
        let tree = WorkspaceTreeNode::Split {
            direction: LayoutDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(workspace, vec![SessionId::new()])),
            second: Box::new(leaf(workspace, Vec::new())),
        };

        assert!(WorkspaceManager::workspace_tree_has_leaf(&tree, workspace));
        assert!(WorkspaceManager::workspace_leaf_is_empty(&tree, workspace));
    }

    /// One window holding two workspaces side by side, ready to transfer.
    fn transfer_fixture() -> (WorkspaceManager, WindowId, WorkspaceId, WorkspaceId, Vec<SessionId>)
    {
        let mut mgr = manager_with_roots(vec![]);
        let window = WindowId::new();
        let left = mgr.create_workspace();
        let right = mgr.create_workspace();
        let (l1, l2, r1) = (SessionId::new(), SessionId::new(), SessionId::new());
        for (workspace, session) in [(left, l1), (left, l2), (right, r1)] {
            mgr.add_session(workspace, session, None);
            mgr.assign_session_to_window(window, session);
        }
        mgr.set_window_tree(
            window,
            WorkspaceTreeNode::Split {
                direction: LayoutDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(leaf(left, vec![l1, l2])),
                second: Box::new(leaf(right, vec![r1])),
            },
        );
        (mgr, window, left, right, vec![l1, l2, r1])
    }

    // @lat: [[server#Workspace Transfer#In-gate commit]]
    #[test]
    fn transfer_moves_sessions_and_derives_both_trees() {
        let (mut mgr, window, left, right, sessions) = transfer_fixture();
        let (l1, l2, r1) = (sessions[0], sessions[1], sessions[2]);
        let target = WindowId::new();

        let stale_source_tree = mgr.window_tree(window).cloned().expect("source tree");
        let moved = mgr.transfer_workspace(window, left, target).expect("transfer commits");
        assert_eq!(moved, vec![l1, l2]);

        // Every moved session's window mapping re-keys; the rest stay.
        assert_eq!(mgr.window_for_session(l1), Some(target));
        assert_eq!(mgr.window_for_session(l2), Some(target));
        assert_eq!(mgr.window_for_session(r1), Some(window));
        // Workspace identity is untouched by the move.
        assert_eq!(mgr.workspace_for_session(l1), Some(left));

        // The source tree collapsed to the remaining leaf; the target tree is
        // exactly the extracted leaf, both derived via the shared tree ops.
        assert_eq!(mgr.window_tree(window), Some(&leaf(right, vec![r1])));
        assert_eq!(mgr.window_tree(target), Some(&leaf(left, vec![l1, l2])));
        assert_eq!(mgr.sessions_for_window(target), vec![l1, l2]);
        assert_eq!(mgr.sessions_for_window(window), vec![r1]);
        assert!(
            !mgr.reported_tree_belongs_to_window(window, &stale_source_tree),
            "a pre-transfer source report cannot restore moved sessions"
        );
        assert!(mgr.reported_tree_belongs_to_window(target, &leaf(left, vec![l1, l2])));
    }

    // @lat: [[server#Workspace Transfer#Typed refusals leave state byte-identical]]
    #[test]
    fn transfer_refusals_are_typed_and_leave_state_byte_identical() {
        let (mut manager, source_window, left, _right, sessions) = transfer_fixture();
        let foreign_window = WindowId::new();
        let foreign_session = SessionId::new();
        let foreign_workspace = manager.create_workspace();
        manager.add_session(foreign_workspace, foreign_session, None);
        manager.assign_session_to_window(foreign_window, foreign_session);
        manager.set_window_tree(foreign_window, leaf(foreign_workspace, vec![foreign_session]));

        let snapshot = |state: &WorkspaceManager| {
            let (workspaces, tree, mut windows) = state.serialize_for_handoff();
            windows.sort_by_key(|entry| entry.window_id.to_full_string());
            (
                rmp_serde::to_vec_named(&workspaces).expect("workspaces"),
                rmp_serde::to_vec_named(&tree).expect("tree"),
                rmp_serde::to_vec_named(&windows).expect("windows"),
            )
        };
        let before = snapshot(&manager);

        let cases = [
            // Unknown workspace id.
            (
                source_window,
                WorkspaceId::new(),
                WindowId::new(),
                WorkspaceTransferRefusal::UnknownWorkspace,
            ),
            // Another window's workspace is not this connection's to move.
            (
                source_window,
                foreign_workspace,
                WindowId::new(),
                WorkspaceTransferRefusal::NotWorkspaceOwner,
            ),
            // The target id collides with the source window itself…
            (source_window, left, source_window, WorkspaceTransferRefusal::TargetWindowIdCollision),
            // …or with any window the registries already know.
            (
                source_window,
                left,
                foreign_window,
                WorkspaceTransferRefusal::TargetWindowIdCollision,
            ),
            // A window's only workspace cannot tear out.
            (
                foreign_window,
                foreign_workspace,
                WindowId::new(),
                WorkspaceTransferRefusal::SoleWorkspace,
            ),
        ];
        for (source, workspace, target, expected) in cases {
            assert_eq!(
                manager.validate_workspace_transfer(source, workspace, target),
                Err(expected)
            );
            assert_eq!(manager.transfer_workspace(source, workspace, target), Err(expected));
        }
        assert_eq!(snapshot(&manager), before, "refused transfers mutate nothing");
        drop(sessions);
    }

    #[test]
    fn transfer_without_a_reported_tree_refuses_honestly() {
        // Sole-workspace window that never reported a tree: nothing would
        // remain after the move, so the refusal is SoleWorkspace.
        let mut mgr = manager_with_roots(vec![]);
        let window = WindowId::new();
        let only = mgr.create_workspace();
        let session = SessionId::new();
        mgr.add_session(only, session, None);
        mgr.assign_session_to_window(window, session);
        assert_eq!(
            mgr.transfer_workspace(window, only, WindowId::new()),
            Err(WorkspaceTransferRefusal::SoleWorkspace)
        );

        // Multi-workspace window without a tree: region ownership cannot be
        // derived, so the transfer refuses rather than inventing a layout.
        let second = mgr.create_workspace();
        let second_session = SessionId::new();
        mgr.add_session(second, second_session, None);
        mgr.assign_session_to_window(window, second_session);
        assert_eq!(
            mgr.transfer_workspace(window, only, WindowId::new()),
            Err(WorkspaceTransferRefusal::NotWorkspaceOwner)
        );
    }

    // @lat: [[server#Workspace Move#Edge insertion]]
    #[test]
    fn edge_move_inserts_into_populated_target_and_preserves_leaf_payload() {
        let (mut manager, source, moved_workspace, staying_workspace, sessions) =
            transfer_fixture();
        let target = WindowId::new();
        let target_workspace = manager.create_workspace();
        let target_session = SessionId::new();
        manager.add_session(target_workspace, target_session, None);
        manager.assign_session_to_window(target, target_session);
        manager.set_window_tree(target, leaf(target_workspace, vec![target_session]));

        let mut moved_leaf = leaf(moved_workspace, vec![sessions[0], sessions[1]]);
        if let WorkspaceTreeNode::Leaf { active_tab_index, .. } = &mut moved_leaf {
            *active_tab_index = 1;
        }
        manager.set_window_tree(
            source,
            WorkspaceTreeNode::Split {
                direction: LayoutDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(moved_leaf.clone()),
                second: Box::new(leaf(staying_workspace, vec![sessions[2]])),
            },
        );

        let moved = manager
            .move_workspace(WorkspaceMoveRequest {
                source_window: source,
                workspace_id: moved_workspace,
                target_window: target,
                target_workspace_id: target_workspace,
                operation: WorkspaceMoveOperation::InsertAtEdge { edge: WorkspaceTreeEdge::Left },
            })
            .expect("edge move commits");
        assert_eq!(moved.source_sessions, vec![sessions[0], sessions[1]]);
        assert!(moved.target_sessions.is_empty());
        assert!(!moved.source_closed);
        assert_eq!(manager.window_tree(source), Some(&leaf(staying_workspace, vec![sessions[2]])));
        assert_eq!(
            manager.window_tree(target),
            Some(&WorkspaceTreeNode::Split {
                direction: LayoutDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(moved_leaf),
                second: Box::new(leaf(target_workspace, vec![target_session])),
            })
        );
        for session in [sessions[0], sessions[1]] {
            assert_eq!(manager.window_for_session(session), Some(target));
            assert_eq!(manager.workspace_for_session(session), Some(moved_workspace));
        }
    }

    // @lat: [[server#Workspace Move#Bidirectional swap]]
    #[test]
    fn swap_exchanges_outgoing_slots_and_ownership_in_one_commit() {
        let (mut manager, source, left, right, source_sessions) = transfer_fixture();
        let target = WindowId::new();
        let target_left = manager.create_workspace();
        let target_right = manager.create_workspace();
        let target_left_session = SessionId::new();
        let target_right_session = SessionId::new();
        for (workspace, session) in
            [(target_left, target_left_session), (target_right, target_right_session)]
        {
            manager.add_session(workspace, session, None);
            manager.assign_session_to_window(target, session);
        }
        manager.set_window_tree(
            target,
            WorkspaceTreeNode::Split {
                direction: LayoutDirection::Vertical,
                ratio: 0.25,
                first: Box::new(leaf(target_left, vec![target_left_session])),
                second: Box::new(leaf(target_right, vec![target_right_session])),
            },
        );

        let moved = manager
            .move_workspace(WorkspaceMoveRequest {
                source_window: source,
                workspace_id: left,
                target_window: target,
                target_workspace_id: target_left,
                operation: WorkspaceMoveOperation::Swap,
            })
            .expect("swap commits");
        assert_eq!(moved.source_sessions, source_sessions[..2]);
        assert_eq!(moved.target_sessions, vec![target_left_session]);
        assert!(!moved.source_closed);
        assert_eq!(
            manager.window_tree(source),
            Some(&WorkspaceTreeNode::Split {
                direction: LayoutDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(leaf(target_left, vec![target_left_session])),
                second: Box::new(leaf(right, vec![source_sessions[2]])),
            })
        );
        assert_eq!(
            manager.window_tree(target),
            Some(&WorkspaceTreeNode::Split {
                direction: LayoutDirection::Vertical,
                ratio: 0.25,
                first: Box::new(leaf(left, source_sessions[..2].to_vec())),
                second: Box::new(leaf(target_right, vec![target_right_session])),
            })
        );
        assert_eq!(manager.window_for_session(target_left_session), Some(source));
        assert_eq!(manager.window_for_session(source_sessions[0]), Some(target));
    }

    // @lat: [[server#Workspace Move#Sole-source reattachment]]
    #[test]
    fn sole_source_edge_closes_shell_but_swap_refuses_without_mutation() {
        let mut manager = manager_with_roots(vec![]);
        let source = WindowId::new();
        let target = WindowId::new();
        let source_workspace = manager.create_workspace();
        let target_workspace = manager.create_workspace();
        let source_session = SessionId::new();
        let target_session = SessionId::new();
        for (window, workspace, session) in
            [(source, source_workspace, source_session), (target, target_workspace, target_session)]
        {
            manager.add_session(workspace, session, None);
            manager.assign_session_to_window(window, session);
            manager.set_window_tree(window, leaf(workspace, vec![session]));
        }
        let snapshot = |state: &WorkspaceManager| {
            let (mut workspaces, tree, mut windows) = state.serialize_for_handoff();
            workspaces.sort_by_key(|workspace| workspace.id.to_full_string());
            windows.sort_by_key(|window| window.window_id.to_full_string());
            rmp_serde::to_vec_named(&(workspaces, tree, windows)).expect("serialize manager")
        };
        let before = snapshot(&manager);
        assert_eq!(
            manager.move_workspace(WorkspaceMoveRequest {
                source_window: source,
                workspace_id: source_workspace,
                target_window: target,
                target_workspace_id: target_workspace,
                operation: WorkspaceMoveOperation::Swap,
            }),
            Err(WorkspaceMoveRefusal::SoleWorkspace)
        );
        assert_eq!(snapshot(&manager), before);

        let moved = manager
            .move_workspace(WorkspaceMoveRequest {
                source_window: source,
                workspace_id: source_workspace,
                target_window: target,
                target_workspace_id: target_workspace,
                operation: WorkspaceMoveOperation::InsertAtEdge { edge: WorkspaceTreeEdge::Bottom },
            })
            .expect("sole-source edge move commits");
        assert!(moved.source_closed);
        assert!(manager.window_tree(source).is_none());
        assert!(manager.sessions_for_window(source).is_empty());
        assert_eq!(manager.window_for_session(source_session), Some(target));
    }

    fn pane_subtree(root: SessionId, pane: SessionId) -> PaneTreeNode {
        PaneTreeNode::Split {
            direction: LayoutDirection::Horizontal,
            ratio: 0.35,
            first: Box::new(PaneTreeNode::Leaf { session_id: root }),
            second: Box::new(PaneTreeNode::Leaf { session_id: pane }),
        }
    }

    fn tab_subtree_fixture()
    -> (WorkspaceManager, WindowId, WorkspaceId, WorkspaceId, [SessionId; 5], PaneTreeNode) {
        let mut manager = manager_with_roots(vec![]);
        let window = WindowId::new();
        let top = manager.create_workspace();
        let lower = manager.create_workspace();
        let sessions = std::array::from_fn(|_| SessionId::new());
        let split = pane_subtree(sessions[0], sessions[1]);
        for session in &sessions[..3] {
            manager.add_session(top, *session, None);
            manager.assign_session_to_window(window, *session);
        }
        for session in &sessions[3..] {
            manager.add_session(lower, *session, None);
            manager.assign_session_to_window(window, *session);
        }
        manager.set_window_tree(
            window,
            WorkspaceTreeNode::Split {
                direction: LayoutDirection::Vertical,
                ratio: 0.5,
                first: Box::new(WorkspaceTreeNode::Leaf {
                    workspace_id: top,
                    session_ids: vec![sessions[0], sessions[2]],
                    pane_trees: vec![Some(split.clone()), None],
                    active_tab_index: 0,
                }),
                second: Box::new(WorkspaceTreeNode::Leaf {
                    workspace_id: lower,
                    session_ids: vec![sessions[3], sessions[4]],
                    pane_trees: vec![None, None],
                    active_tab_index: 1,
                }),
            },
        );
        (manager, window, top, lower, sessions, split)
    }

    // @lat: [[server#Workspace Move#Atomic tab-subtree transfer]]
    #[test]
    fn pane_tab_moves_between_top_and_lower_regions_in_both_directions() {
        let (mut manager, window, top, lower, sessions, split) = tab_subtree_fixture();
        let move_tab = |source, target, index| WorkspaceMoveRequest {
            source_window: window,
            workspace_id: source,
            target_window: window,
            target_workspace_id: target,
            operation: WorkspaceMoveOperation::MoveTab {
                tab_session_id: sessions[0],
                target_index: index,
            },
        };

        let moved = manager.move_workspace(move_tab(top, lower, 1)).expect("top to lower commits");
        assert_eq!(moved.source_sessions, vec![sessions[0], sessions[1]]);
        assert_eq!(manager.workspace_for_session(sessions[0]), Some(lower));
        assert_eq!(manager.workspace_for_session(sessions[1]), Some(lower));
        assert_eq!(manager.window_for_session(sessions[1]), Some(window));
        assert_eq!(
            manager.window_tree(window),
            Some(&WorkspaceTreeNode::Split {
                direction: LayoutDirection::Vertical,
                ratio: 0.5,
                first: Box::new(WorkspaceTreeNode::Leaf {
                    workspace_id: top,
                    session_ids: vec![sessions[2]],
                    pane_trees: vec![None],
                    active_tab_index: 0,
                }),
                second: Box::new(WorkspaceTreeNode::Leaf {
                    workspace_id: lower,
                    session_ids: vec![sessions[3], sessions[0], sessions[4]],
                    pane_trees: vec![None, Some(split.clone()), None],
                    active_tab_index: 1,
                }),
            })
        );

        manager.move_workspace(move_tab(lower, top, 0)).expect("lower to top commits");
        assert_eq!(manager.workspace_for_session(sessions[0]), Some(top));
        assert_eq!(manager.workspace_for_session(sessions[1]), Some(top));
        assert_eq!(
            manager.window_tree(window),
            Some(&WorkspaceTreeNode::Split {
                direction: LayoutDirection::Vertical,
                ratio: 0.5,
                first: Box::new(WorkspaceTreeNode::Leaf {
                    workspace_id: top,
                    session_ids: vec![sessions[0], sessions[2]],
                    pane_trees: vec![Some(split), None],
                    active_tab_index: 0,
                }),
                second: Box::new(WorkspaceTreeNode::Leaf {
                    workspace_id: lower,
                    session_ids: vec![sessions[3], sessions[4]],
                    pane_trees: vec![None, None],
                    active_tab_index: 1,
                }),
            })
        );
    }

    #[test]
    fn moving_an_inactive_tab_keeps_both_regions_showing_the_same_tabs() {
        let (mut manager, window, top, lower, sessions, _) = tab_subtree_fixture();
        manager
            .move_workspace(WorkspaceMoveRequest {
                source_window: window,
                workspace_id: top,
                target_window: window,
                target_workspace_id: lower,
                operation: WorkspaceMoveOperation::MoveTab {
                    tab_session_id: sessions[2],
                    target_index: 0,
                },
            })
            .expect("inactive tab move commits");

        let WorkspaceTreeNode::Split { first, second, .. } =
            manager.window_tree(window).expect("two regions survive")
        else {
            panic!("both regions still have tabs")
        };
        assert!(matches!(
            first.as_ref(),
            WorkspaceTreeNode::Leaf { session_ids, active_tab_index: 0, .. }
                if session_ids == &[sessions[0]]
        ));
        assert!(matches!(
            second.as_ref(),
            WorkspaceTreeNode::Leaf { session_ids, active_tab_index: 2, .. }
                if session_ids == &[sessions[2], sessions[3], sessions[4]]
        ));
    }

    // @lat: [[server#Workspace Move#Atomic tab-subtree transfer]]
    #[test]
    fn tab_subtree_move_handoff_round_trips_and_refusals_mutate_nothing() {
        let (mut manager, window, top, lower, sessions, split) = tab_subtree_fixture();
        let request = WorkspaceMoveRequest {
            source_window: window,
            workspace_id: top,
            target_window: window,
            target_workspace_id: lower,
            operation: WorkspaceMoveOperation::MoveTab {
                tab_session_id: sessions[0],
                target_index: 1,
            },
        };
        manager.move_workspace(request).expect("tab subtree move commits");
        let committed_tree = manager.window_tree(window).cloned().expect("committed tree");
        let (workspaces, tree, windows) = manager.serialize_for_handoff();
        let valid = vec![
            (sessions[0], lower),
            (sessions[1], lower),
            (sessions[2], top),
            (sessions[3], lower),
            (sessions[4], lower),
        ];
        let restored =
            WorkspaceManager::restore_from_handoff(Vec::new(), &workspaces, tree, &windows, &valid);
        assert_eq!(restored.window_tree(window), Some(&committed_tree));
        assert_eq!(restored.workspace_for_session(sessions[1]), Some(lower));
        assert_eq!(restored.window_for_session(sessions[1]), Some(window));

        let snapshot = rmp_serde::to_vec_named(&(
            manager.serialize_for_handoff(),
            manager.sessions_in_workspace(top),
            manager.sessions_in_workspace(lower),
        ))
        .expect("serialize manager snapshot");
        for operation in [
            WorkspaceMoveOperation::MoveTab { tab_session_id: SessionId::new(), target_index: 0 },
            WorkspaceMoveOperation::MoveTab { tab_session_id: sessions[0], target_index: 99 },
        ] {
            assert!(
                manager
                    .move_workspace(WorkspaceMoveRequest {
                        source_window: window,
                        workspace_id: lower,
                        target_window: window,
                        target_workspace_id: top,
                        operation,
                    })
                    .is_err()
            );
        }
        assert_eq!(
            rmp_serde::to_vec_named(&(
                manager.serialize_for_handoff(),
                manager.sessions_in_workspace(top),
                manager.sessions_in_workspace(lower),
            ))
            .expect("serialize manager after refusals"),
            snapshot,
            "typed refusals leave tree and membership byte-identical"
        );
        assert!(matches!(split, PaneTreeNode::Split { .. }));
    }

    // @lat: [[server#Workspace Move#Atomic tab-subtree transfer]]
    #[test]
    fn moving_a_regions_last_tab_collapses_it_after_commit() {
        let (mut manager, window, top, lower, sessions, split) = tab_subtree_fixture();
        manager
            .move_workspace(WorkspaceMoveRequest {
                source_window: window,
                workspace_id: top,
                target_window: window,
                target_workspace_id: lower,
                operation: WorkspaceMoveOperation::MoveTab {
                    tab_session_id: sessions[2],
                    target_index: 0,
                },
            })
            .expect("first tab move commits");
        manager
            .move_workspace(WorkspaceMoveRequest {
                source_window: window,
                workspace_id: top,
                target_window: window,
                target_workspace_id: lower,
                operation: WorkspaceMoveOperation::MoveTab {
                    tab_session_id: sessions[0],
                    target_index: 1,
                },
            })
            .expect("last tab move commits");

        let WorkspaceTreeNode::Leaf { workspace_id, session_ids, pane_trees, active_tab_index } =
            manager.window_tree(window).expect("source region collapsed")
        else {
            panic!("empty source region must collapse")
        };
        assert_eq!(*workspace_id, lower);
        assert_eq!(session_ids, &[sessions[2], sessions[0], sessions[3], sessions[4]]);
        assert_eq!(pane_trees, &[None, Some(split), None, None]);
        assert_eq!(*active_tab_index, 1);
        assert!(manager.workspace_info(top).is_none(), "empty source workspace is retired");
    }

    #[test]
    fn workspace_tree_none_when_not_set() {
        let mgr = manager_with_roots(vec![]);
        assert!(mgr.workspace_tree().is_none());

        let (_, tree, _) = mgr.serialize_for_handoff();
        assert!(tree.is_none());
    }
}
