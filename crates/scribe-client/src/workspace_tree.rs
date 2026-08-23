//! GPUI entity wrapper around the window-level workspace tree ([`WindowLayout`]).
//!
//! [`WorkspaceTree`] owns a [`WindowLayout`] and the running
//! `PaneId -> SessionId` map needed to serialize per-tab pane trees. Every
//! mutation (split, tab add/remove, active-tab change, ratio change, slot
//! edit) re-serializes the tree and emits [`WorkspaceTreeEvent::Report`]
//! carrying the exact [`WorkspaceTreeNode`] the client sends to the server as
//! `ReportWorkspaceTree`, so the server's per-window tree stays current for
//! reconnect and handoff.

use std::collections::HashMap;

use gpui::{Context, EventEmitter};
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::{PaneTreeNode, WorkspaceTreeError, WorkspaceTreeNode};

use crate::layout::{PaneId, Rect, SplitDirection};
use crate::workspace_drag::WorkspaceDropZone;
use crate::workspace_layout::{WindowLayout, WorkspaceSlot};

/// Event emitted after every workspace-tree mutation.
///
/// The payload is the serialized tree that the client forwards to the server
/// via `ClientMessage::ReportWorkspaceTree`.
#[derive(Debug, Clone)]
pub enum WorkspaceTreeEvent {
    /// The tree changed; carries the fresh serialized snapshot.
    Report(WorkspaceTreeNode),
}

/// A GPUI model owning the window's workspace split tree.
pub struct WorkspaceTree {
    layout: WindowLayout,
    /// Maps each live pane to the session it hosts, so [`WindowLayout::to_tree`]
    /// can serialize per-tab pane split trees.
    pane_to_session: HashMap<PaneId, SessionId>,
}

impl EventEmitter<WorkspaceTreeEvent> for WorkspaceTree {}

impl WorkspaceTree {
    /// Create a window layout with a single empty workspace.
    pub fn new(workspace_id: WorkspaceId, accent: Option<[f32; 4]>) -> Self {
        Self { layout: WindowLayout::new(workspace_id, accent), pane_to_session: HashMap::new() }
    }

    /// Rebuild a workspace tree from a serialized [`WorkspaceTreeNode`].
    ///
    /// Only topology and workspace IDs are restored; tabs and pane trees are
    /// repopulated by the caller via [`Self::add_tab_with_pane_tree`].
    pub fn from_tree(tree: &WorkspaceTreeNode) -> Self {
        Self { layout: WindowLayout::from_tree(tree), pane_to_session: HashMap::new() }
    }

    /// Borrow the underlying pure window layout.
    pub const fn layout(&self) -> &WindowLayout {
        &self.layout
    }

    /// The currently focused workspace ID.
    pub const fn focused_workspace_id(&self) -> WorkspaceId {
        self.layout.focused_workspace_id()
    }

    /// All workspace IDs in tree order.
    pub fn workspace_ids_in_order(&self) -> Vec<WorkspaceId> {
        self.layout.workspace_ids_in_order()
    }

    /// Look up a workspace slot by ID.
    pub fn find_workspace(&self, id: WorkspaceId) -> Option<&WorkspaceSlot> {
        self.layout.find_workspace(id)
    }

    /// Serialize the current tree with the running pane→session map.
    pub fn to_tree(&self) -> WorkspaceTreeNode {
        self.layout.to_tree(&self.pane_to_session)
    }

    /// Set the focused workspace and report the tree.
    pub fn set_focused_workspace(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.layout.set_focused_workspace(id);
        self.report(cx);
    }

    /// Rename an existing workspace region, then report the tree.
    ///
    /// The shell builds its first region before the server has answered with a
    /// `Welcome` / `SessionList`, so the root region starts on a client-minted
    /// ID and adopts the server's real [`WorkspaceId`] here as soon as one is
    /// known. Returns `false` (without reporting) when `old_id` is not present.
    pub fn set_workspace_id(
        &mut self,
        old_id: WorkspaceId,
        new_id: WorkspaceId,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.layout.set_workspace_id(old_id, new_id) {
            return false;
        }
        self.report(cx);
        true
    }

    /// Remove a workspace region, promoting its sibling, then report the tree.
    ///
    /// Returns `false` (without reporting) when the region is the last one in
    /// the window — a window always has at least one workspace region. Callers
    /// must drop the region's tabs with [`Self::remove_tab`] first, so the
    /// pane→session map does not outlive the panes it names.
    pub fn remove_workspace(&mut self, workspace_id: WorkspaceId, cx: &mut Context<Self>) -> bool {
        if !self.layout.remove_workspace(workspace_id) {
            return false;
        }
        self.report(cx);
        true
    }

    /// Split the focused workspace, then report the tree.
    ///
    /// Returns the new workspace ID, or `None` when the focused workspace was
    /// not found. Emits a report only on success.
    pub fn split_workspace(
        &mut self,
        direction: SplitDirection,
        accent: Option<[f32; 4]>,
        cx: &mut Context<Self>,
    ) -> Option<WorkspaceId> {
        let new_id = self.layout.split_workspace(direction, accent)?;
        self.report(cx);
        Some(new_id)
    }

    /// Add an empty tab to a workspace and record its root pane→session, then
    /// report the tree.
    ///
    /// Returns the new tab's root [`PaneId`], or `None` if the workspace was
    /// not found.
    pub fn add_tab(
        &mut self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        cx: &mut Context<Self>,
    ) -> Option<PaneId> {
        let pane_id = self.layout.add_tab(workspace_id, session_id)?;
        self.pane_to_session.insert(pane_id, session_id);
        self.report(cx);
        Some(pane_id)
    }

    /// Add a tab restoring a serialized pane split tree, recording every
    /// pane→session pair, then report the tree.
    ///
    /// Returns the `(SessionId, PaneId)` pairs (root first), or `None` if the
    /// workspace was not found.
    pub fn add_tab_with_pane_tree(
        &mut self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        pane_tree: &PaneTreeNode,
        cx: &mut Context<Self>,
    ) -> Option<Vec<(SessionId, PaneId)>> {
        let pairs = self.layout.add_tab_with_pane_tree(workspace_id, session_id, pane_tree)?;
        for &(sid, pid) in &pairs {
            self.pane_to_session.insert(pid, sid);
        }
        self.report(cx);
        Some(pairs)
    }

    /// Remove a tab (and its pane→session entries), then report the tree.
    pub fn remove_tab(
        &mut self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        cx: &mut Context<Self>,
    ) {
        self.layout.remove_tab(workspace_id, session_id);
        self.pane_to_session.retain(|_, sid| *sid != session_id);
        self.report(cx);
    }

    /// Set the active tab index for a workspace, then report the tree.
    ///
    /// Returns `false` (without reporting) when the workspace is missing or the
    /// index is out of bounds.
    pub fn set_active_tab(
        &mut self,
        workspace_id: WorkspaceId,
        index: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.layout.set_active_tab(workspace_id, index) {
            return false;
        }
        self.report(cx);
        true
    }

    /// Rearrange one workspace at a target zone, then emit one fresh tree.
    pub fn rearrange_workspace(
        &mut self,
        source_workspace_id: WorkspaceId,
        target_workspace_id: WorkspaceId,
        zone: WorkspaceDropZone,
        cx: &mut Context<Self>,
    ) -> Result<bool, WorkspaceTreeError> {
        if !self.layout.rearrange_workspace(source_workspace_id, target_workspace_id, zone)? {
            return Ok(false);
        }
        self.report(cx);
        Ok(true)
    }

    /// Reset every workspace split ratio so regions share the window evenly,
    /// then report the tree.
    pub fn equalize_ratios(&mut self, cx: &mut Context<Self>) {
        self.layout.equalize_all_workspace_ratios();
        self.report(cx);
    }

    /// Set the ratio of the split between two workspaces (clamped 0.1..=0.9),
    /// then report the tree.
    pub fn set_workspace_ratio(
        &mut self,
        first_ws: WorkspaceId,
        second_ws: WorkspaceId,
        new_ratio: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.layout.set_workspace_ratio(first_ws, second_ws, new_ratio) {
            return false;
        }
        self.report(cx);
        true
    }

    /// Compute the pixel rect for each workspace leaf.
    pub fn compute_workspace_rects(&self, viewport: Rect) -> Vec<(WorkspaceId, Rect)> {
        self.layout.compute_workspace_rects(viewport)
    }

    /// Mutate a workspace slot in place (name, project root, accent, tabs),
    /// then report the tree.
    ///
    /// Returns `None` (without reporting) when the workspace was not found.
    pub fn update_slot<R>(
        &mut self,
        workspace_id: WorkspaceId,
        edit: impl FnOnce(&mut WorkspaceSlot) -> R,
        cx: &mut Context<Self>,
    ) -> Option<R> {
        let slot = self.layout.find_workspace_mut(workspace_id)?;
        let result = edit(slot);
        self.report(cx);
        Some(result)
    }

    fn report(&mut self, cx: &mut Context<Self>) {
        let tree = self.layout.to_tree(&self.pane_to_session);
        cx.emit(WorkspaceTreeEvent::Report(tree));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::{AppContext as _, Entity, TestAppContext};
    use scribe_common::ids::{SessionId, WorkspaceId};
    use scribe_common::protocol::{PaneTreeNode, WorkspaceTreeNode};

    use super::{WorkspaceTree, WorkspaceTreeEvent};
    use crate::layout::SplitDirection;
    use crate::workspace_drag::WorkspaceDropZone;

    type Reports = (Arc<AtomicUsize>, Rc<RefCell<Option<WorkspaceTreeNode>>>);

    /// Return a leaf node's workspace ID, panicking if the node is a split.
    fn leaf_id(node: &WorkspaceTreeNode) -> WorkspaceId {
        match node {
            WorkspaceTreeNode::Leaf { workspace_id, .. } => *workspace_id,
            WorkspaceTreeNode::Split { .. } => panic!("expected a leaf node"),
        }
    }

    /// Record one report event into the shared counter and last-tree cell.
    fn push_report(
        event: &WorkspaceTreeEvent,
        count: &AtomicUsize,
        last: &RefCell<Option<WorkspaceTreeNode>>,
    ) {
        match event {
            WorkspaceTreeEvent::Report(tree) => {
                count.fetch_add(1, Ordering::SeqCst);
                *last.borrow_mut() = Some(tree.clone());
            }
        }
    }

    /// Subscribe to a workspace tree's report events, returning a counter and a
    /// cell holding the most recently reported tree.
    fn report_sink(entity: &Entity<WorkspaceTree>, cx: &mut TestAppContext) -> Reports {
        let count = Arc::new(AtomicUsize::new(0));
        let last: Rc<RefCell<Option<WorkspaceTreeNode>>> = Rc::new(RefCell::new(None));
        let count_sink = Arc::clone(&count);
        let last_sink = Rc::clone(&last);
        cx.update(|app| {
            app.subscribe(entity, move |_, event, _| {
                push_report(event, &count_sink, &last_sink);
            })
            .detach();
        });
        // Flush the deferred subscription activation before any mutation.
        cx.update(|_| {});
        (count, last)
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Workspace Tree Model]]
    #[gpui::test]
    fn split_reports_a_two_leaf_tree(cx: &mut TestAppContext) {
        let ws_a = WorkspaceId::new();
        let tree = cx.new(|_| WorkspaceTree::new(ws_a, None));
        let (count, last) = report_sink(&tree, cx);

        let ws_b = tree
            .update(cx, |t, cx| t.split_workspace(SplitDirection::Horizontal, None, cx))
            .unwrap();

        assert_eq!(count.load(Ordering::SeqCst), 1, "split emits one report");
        let reported = last.borrow().clone().unwrap();
        let (first, second) = match reported {
            WorkspaceTreeNode::Split { first, second, .. } => (first, second),
            WorkspaceTreeNode::Leaf { .. } => panic!("expected a split after split_workspace"),
        };
        assert_eq!(leaf_id(&first), ws_a);
        assert_eq!(leaf_id(&second), ws_b);
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Workspace Tree Model]]
    #[gpui::test]
    fn slot_carries_accent_name_and_project_root(cx: &mut TestAppContext) {
        let ws = WorkspaceId::new();
        let accent = [0.1, 0.2, 0.3, 1.0];
        let tree = cx.new(|_| WorkspaceTree::new(ws, Some(accent)));
        let (count, _) = report_sink(&tree, cx);

        // Accent is carried from construction.
        tree.read_with(cx, |t, _| {
            assert_eq!(t.find_workspace(ws).map(|s| s.accent_color), Some(accent));
        });

        // Name and project root are editable via update_slot, which reports.
        let root = std::path::PathBuf::from("/home/dev/project");
        tree.update(cx, |t, cx| {
            t.update_slot(
                ws,
                |slot| {
                    slot.name = Some("api".to_owned());
                    slot.project_root = Some(root.clone());
                },
                cx,
            )
        })
        .unwrap();

        tree.read_with(cx, |t, _| {
            let slot = t.find_workspace(ws).unwrap();
            assert_eq!(slot.name.as_deref(), Some("api"));
            assert_eq!(
                slot.project_root.as_deref(),
                Some(std::path::Path::new("/home/dev/project"))
            );
        });
        assert_eq!(count.load(Ordering::SeqCst), 1, "one slot edit, one report");
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Workspace Tree Model]]
    #[gpui::test]
    fn add_and_remove_tabs_track_active_and_report(cx: &mut TestAppContext) {
        let ws = WorkspaceId::new();
        let tree = cx.new(|_| WorkspaceTree::new(ws, None));
        let (count, _) = report_sink(&tree, cx);

        let s1 = SessionId::new();
        let s2 = SessionId::new();
        tree.update(cx, |t, cx| t.add_tab(ws, s1, cx)).unwrap();
        tree.update(cx, |t, cx| t.add_tab(ws, s2, cx)).unwrap();

        // Adding a tab makes it active (last-pushed).
        tree.read_with(cx, |t, _| {
            let slot = t.find_workspace(ws).unwrap();
            assert_eq!(slot.tab_count(), 2);
            assert_eq!(slot.active_tab, 1);
        });

        // Removing the active tab clamps active_tab back into range.
        tree.update(cx, |t, cx| t.remove_tab(ws, s2, cx));
        tree.read_with(cx, |t, _| {
            let slot = t.find_workspace(ws).unwrap();
            assert_eq!(slot.tab_count(), 1);
            assert_eq!(slot.active_tab, 0);
        });

        assert_eq!(count.load(Ordering::SeqCst), 3, "two adds + one remove all report");
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Workspace Tree Model]]
    #[gpui::test]
    fn set_active_tab_restores_focused_tab_on_reconnect(cx: &mut TestAppContext) {
        // Author side: three tabs, the middle one active. The serialized tree
        // must carry active_tab_index = 1.
        let ws = WorkspaceId::new();
        let s_first = SessionId::new();
        let s_middle = SessionId::new();
        let s_last = SessionId::new();
        let leaf = |sid| PaneTreeNode::Leaf { session_id: sid };

        let authored = cx.new(|_| WorkspaceTree::new(ws, None));
        let _author_sink = report_sink(&authored, cx);
        authored.update(cx, |t, cx| {
            t.add_tab_with_pane_tree(ws, s_first, &leaf(s_first), cx);
            t.add_tab_with_pane_tree(ws, s_middle, &leaf(s_middle), cx);
            t.add_tab_with_pane_tree(ws, s_last, &leaf(s_last), cx);
            assert!(t.set_active_tab(ws, 1, cx), "focus the middle tab");
        });
        let wire = authored.read_with(cx, |t, _| t.to_tree());
        let leaf_active = match &wire {
            WorkspaceTreeNode::Leaf { active_tab_index, session_ids, .. } => {
                assert_eq!(session_ids.len(), 3);
                *active_tab_index
            }
            WorkspaceTreeNode::Split { .. } => panic!("expected leaf"),
        };
        assert_eq!(leaf_active, 1);

        // Reconnect side: from_tree yields the empty slot, the restore loop
        // pushes tabs (each auto-activating the last), and the post-pass
        // set_active_tab restores the originally focused tab.
        let restored = cx.new(|_| WorkspaceTree::from_tree(&wire));
        let (count, _) = report_sink(&restored, cx);
        restored.update(cx, |t, cx| {
            for sid in [s_first, s_middle, s_last] {
                t.add_tab_with_pane_tree(ws, sid, &leaf(sid), cx);
            }
        });
        restored.read_with(cx, |t, _| {
            assert_eq!(
                t.find_workspace(ws).map(|s| s.active_tab),
                Some(2),
                "restore lands on the last-pushed tab before the post-pass",
            );
        });
        let applied = restored.update(cx, |t, cx| t.set_active_tab(ws, leaf_active, cx));
        assert!(applied);
        restored.read_with(cx, |t, _| {
            assert_eq!(
                t.find_workspace(ws).map(|s| s.active_tab),
                Some(1),
                "post-pass restores the originally focused tab",
            );
        });
        // Three restore adds + one restore set_active_tab.
        assert_eq!(count.load(Ordering::SeqCst), 4);
    }

    // @lat: [[test#Test Harness#GPUI Workspace Drag]]
    #[gpui::test]
    fn workspace_rearrange_reports_once_and_focuses_the_source(cx: &mut TestAppContext) {
        let ws_a = WorkspaceId::new();
        let tree = cx.new(|_| WorkspaceTree::new(ws_a, None));
        let (count, last) = report_sink(&tree, cx);
        let ws_b = tree
            .update(cx, |tree, cx| tree.split_workspace(SplitDirection::Horizontal, None, cx))
            .expect("second workspace");
        let before = count.load(Ordering::SeqCst);

        assert_eq!(
            tree.update(cx, |tree, cx| {
                tree.rearrange_workspace(ws_a, ws_b, WorkspaceDropZone::Right, cx)
            }),
            Ok(true)
        );
        assert_eq!(count.load(Ordering::SeqCst), before + 1);
        tree.read_with(cx, |tree, _| {
            assert_eq!(tree.focused_workspace_id(), ws_a);
            assert_eq!(tree.workspace_ids_in_order(), vec![ws_b, ws_a]);
        });
        assert!(last.borrow().is_some(), "the rearranged tree is reportable");
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Workspace Tree Model]]
    #[gpui::test]
    fn set_workspace_ratio_clamps_and_reports(cx: &mut TestAppContext) {
        let ws_a = WorkspaceId::new();
        let tree = cx.new(|_| WorkspaceTree::new(ws_a, None));
        let (count, last) = report_sink(&tree, cx);

        let ws_b = tree
            .update(cx, |t, cx| t.split_workspace(SplitDirection::Horizontal, None, cx))
            .unwrap();
        // Request a degenerate ratio; expect a clamp to 0.9 in the report.
        assert!(tree.update(cx, |t, cx| t.set_workspace_ratio(ws_a, ws_b, 3.0, cx)));

        match last.borrow().clone().unwrap() {
            WorkspaceTreeNode::Split { ratio, .. } => {
                assert!((ratio - 0.9).abs() < f32::EPSILON, "ratio clamped to 0.9");
            }
            WorkspaceTreeNode::Leaf { .. } => panic!("expected split"),
        }
        assert_eq!(count.load(Ordering::SeqCst), 2, "split + ratio each report");
    }
}
