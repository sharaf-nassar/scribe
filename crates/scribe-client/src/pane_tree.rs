//! GPUI entity wrapper around the pure pane split tree ([`LayoutTree`]).
//!
//! [`PaneTree`] holds a [`LayoutTree`] and turns each structural mutation
//! (split, close, ratio change, equalize) into a [`PaneTreeEvent::Changed`]
//! event plus a `cx.notify()`, so subscribers repaint and the owning workspace
//! model can re-report its serialized tree. Splitting halves only the target
//! pane, and closing promotes its sibling; ratios outside the changed subtree
//! stay untouched. Explicit equalize remains the opt-in balancing action.

use gpui::{Context, EventEmitter};

use crate::layout::{
    FocusDirection, LayoutNode, LayoutTree, PaneEdges, PaneId, Rect, SplitDirection,
};

/// Event emitted whenever the pane tree's structure or ratios change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneTreeEvent {
    /// The tree was mutated (split, close, ratio change, or equalize).
    Changed,
}

/// A GPUI model owning one tab's pane split tree.
pub struct PaneTree {
    tree: LayoutTree,
}

impl EventEmitter<PaneTreeEvent> for PaneTree {}

impl PaneTree {
    /// Create a pane tree with a single root pane.
    pub fn new() -> Self {
        Self { tree: LayoutTree::new() }
    }

    /// Create a pane tree from a pre-built root node and initial pane.
    pub fn from_root(root: LayoutNode, initial_pane: PaneId) -> Self {
        Self { tree: LayoutTree::from_root(root, initial_pane) }
    }

    /// Wrap an existing [`LayoutTree`].
    pub fn from_tree(tree: LayoutTree) -> Self {
        Self { tree }
    }

    /// Borrow the underlying pure layout tree.
    pub const fn tree(&self) -> &LayoutTree {
        &self.tree
    }

    /// The initial (root) pane ID assigned when this tree was created.
    pub const fn initial_pane_id(&self) -> PaneId {
        self.tree.initial_pane_id()
    }

    /// All leaf pane IDs in depth-first order.
    pub fn all_pane_ids(&self) -> Vec<PaneId> {
        self.tree.all_pane_ids()
    }

    /// Compute pixel rects for every leaf pane in the tree.
    pub fn compute_rects(&self, viewport: Rect) -> Vec<(PaneId, Rect, PaneEdges)> {
        self.tree.compute_rects(viewport)
    }

    /// Cycle to the next pane after `current` in depth-first order.
    pub fn next_pane(&self, current: PaneId) -> PaneId {
        self.tree.next_pane(current)
    }

    /// Find the nearest pane in the given direction, wrapping at the edge.
    ///
    /// Read-only: does not mutate the tree or emit an event. The caller
    /// supplies the rects (from [`Self::compute_rects`]) for the current layout.
    pub fn find_pane_in_direction(
        &self,
        current: PaneId,
        direction: FocusDirection,
        rects: &[(PaneId, Rect, PaneEdges)],
    ) -> Option<PaneId> {
        self.tree.find_pane_in_direction(current, direction, rects)
    }

    /// Split `pane_id` in the given direction, halving only that pane.
    ///
    /// Returns the new pane's ID, or `None` if the pane was not found. Emits
    /// [`PaneTreeEvent::Changed`] only when a split actually happened.
    pub fn split(
        &mut self,
        pane_id: PaneId,
        direction: SplitDirection,
        cx: &mut Context<Self>,
    ) -> Option<PaneId> {
        let new_id = self.tree.split_pane(pane_id, direction)?;
        Self::changed(cx);
        Some(new_id)
    }

    /// Close `pane_id`, promoting its sibling into the freed extent.
    ///
    /// Returns `true` if the pane was found and removed. The sole root leaf is
    /// never removed. Emits [`PaneTreeEvent::Changed`] only when a pane closed.
    pub fn close(&mut self, pane_id: PaneId, cx: &mut Context<Self>) -> bool {
        if !self.tree.close_pane(pane_id) {
            return false;
        }
        Self::changed(cx);
        true
    }

    /// Set the ratio of the split holding `pane_id` (clamped to 0.1..=0.9).
    ///
    /// Returns `true` if the split was found and the ratio set. Emits
    /// [`PaneTreeEvent::Changed`] only when the ratio was applied.
    pub fn set_ratio(&mut self, pane_id: PaneId, ratio: f32, cx: &mut Context<Self>) -> bool {
        if !self.tree.set_ratio_for_pane(pane_id, ratio) {
            return false;
        }
        Self::changed(cx);
        true
    }

    /// Reset every split ratio so all panes get equal space.
    pub fn equalize(&mut self, cx: &mut Context<Self>) {
        self.tree.equalize_all_ratios();
        Self::changed(cx);
    }

    fn changed(cx: &mut Context<Self>) {
        cx.emit(PaneTreeEvent::Changed);
        cx.notify();
    }
}

impl Default for PaneTree {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::{AppContext as _, Entity, TestAppContext};

    use super::PaneTree;
    use crate::layout::{FocusDirection, LayoutNode, PaneId, Rect, SplitDirection};

    /// Subscribe to a pane tree's events and return a live counter that
    /// increments once per emitted `Changed` event (the enum's only variant).
    fn change_counter(entity: &Entity<PaneTree>, cx: &mut TestAppContext) -> Arc<AtomicUsize> {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::clone(&count);
        cx.update(|app| {
            app.subscribe(entity, move |_, _, _| {
                sink.fetch_add(1, Ordering::SeqCst);
            })
            .detach();
        });
        // Flush the deferred subscription activation before any mutation.
        cx.update(|_| {});
        count
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Pane Tree Model]]
    #[gpui::test]
    fn split_grows_the_tree_and_emits(cx: &mut TestAppContext) {
        let tree = cx.new(|_| PaneTree::new());
        let count = change_counter(&tree, cx);

        let root = tree.read_with(cx, |t, _| t.initial_pane_id());
        let new_pane =
            tree.update(cx, |t, cx| t.split(root, SplitDirection::Horizontal, cx)).unwrap();

        assert_ne!(new_pane, root);
        tree.read_with(cx, |t, _| {
            assert_eq!(t.all_pane_ids().len(), 2);
        });
        assert_eq!(count.load(Ordering::SeqCst), 1, "split emits exactly one Changed");
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Pane Tree Model]]
    #[gpui::test]
    fn split_missing_pane_is_a_noop(cx: &mut TestAppContext) {
        let tree = cx.new(|_| PaneTree::new());
        let count = change_counter(&tree, cx);

        let result =
            tree.update(cx, |t, cx| t.split(PaneId::from_raw(9999), SplitDirection::Vertical, cx));

        assert_eq!(result, None);
        assert_eq!(count.load(Ordering::SeqCst), 0, "no split, no event");
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Pane Tree Model]]
    #[gpui::test]
    fn split_preserves_sibling_ratios(cx: &mut TestAppContext) {
        let a = PaneId::from_raw(1);
        let b = PaneId::from_raw(2);
        let root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.7,
            first: Box::new(LayoutNode::Leaf(a)),
            second: Box::new(LayoutNode::Leaf(b)),
        };
        let tree = cx.new(|_| PaneTree::from_root(root, a));

        let new_pane = tree.update(cx, |t, cx| t.split(b, SplitDirection::Horizontal, cx));
        let new_pane = new_pane.expect("split should succeed");

        tree.read_with(cx, |t, _| {
            let rects = t.compute_rects(Rect { x: 0.0, y: 0.0, width: 100.0, height: 40.0 });
            let rect_for =
                |id| rects.iter().find(|(pane_id, _, _)| *pane_id == id).map(|(_, r, _)| *r);
            assert!(
                (rect_for(a).expect("A exists").width - 70.0).abs() < 0.01,
                "A keeps its ratio"
            );
            assert!((rect_for(b).expect("B exists").width - 15.0).abs() < 0.01, "B's slot halves");
            assert!((rect_for(new_pane).expect("new pane exists").width - 15.0).abs() < 0.01);
        });
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Pane Tree Model]]
    #[gpui::test]
    fn close_promotes_sibling_without_reequalizing(cx: &mut TestAppContext) {
        // A three-pane tree with distinct effective widths. Closing C promotes
        // B into C's parent extent while preserving A's outer split ratio.
        let a = PaneId::from_raw(1);
        let b = PaneId::from_raw(2);
        let c = PaneId::from_raw(3);
        let root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.8,
            first: Box::new(LayoutNode::Leaf(a)),
            second: Box::new(LayoutNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.2,
                first: Box::new(LayoutNode::Leaf(b)),
                second: Box::new(LayoutNode::Leaf(c)),
            }),
        };
        let tree = cx.new(|_| PaneTree::from_root(root, a));
        let count = change_counter(&tree, cx);

        let closed = tree.update(cx, |t, cx| t.close(c, cx));
        assert!(closed);

        tree.read_with(cx, |t, _| {
            let ids = t.all_pane_ids();
            assert_eq!(ids, vec![a, b]);
            let rects = t.compute_rects(Rect { x: 0.0, y: 0.0, width: 100.0, height: 40.0 });
            let a_rect = rects.iter().find(|(id, _, _)| *id == a).map(|(_, r, _)| *r).unwrap();
            let b_rect = rects.iter().find(|(id, _, _)| *id == b).map(|(_, r, _)| *r).unwrap();
            assert!((a_rect.width - 80.0).abs() < 0.01, "A keeps its outer ratio");
            assert!((b_rect.width - 20.0).abs() < 0.01, "B inherits its parent's full extent");
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Pane Tree Model]]
    #[gpui::test]
    fn close_sole_root_leaf_is_a_noop(cx: &mut TestAppContext) {
        let tree = cx.new(|_| PaneTree::new());
        let count = change_counter(&tree, cx);

        let root = tree.read_with(cx, |t, _| t.initial_pane_id());
        let closed = tree.update(cx, |t, cx| t.close(root, cx));

        assert!(!closed, "the only pane cannot be closed");
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Pane Tree Model]]
    #[gpui::test]
    fn set_ratio_clamps_out_of_range_values(cx: &mut TestAppContext) {
        let a = PaneId::from_raw(1);
        let b = PaneId::from_raw(2);
        let root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf(a)),
            second: Box::new(LayoutNode::Leaf(b)),
        };
        let tree = cx.new(|_| PaneTree::from_root(root, a));
        let count = change_counter(&tree, cx);

        // Request a degenerate ratio far above the max; expect a clamp to 0.9.
        assert!(tree.update(cx, |t, cx| t.set_ratio(a, 5.0, cx)));
        tree.read_with(cx, |t, _| {
            let rects = t.compute_rects(Rect { x: 0.0, y: 0.0, width: 100.0, height: 10.0 });
            let a_rect = rects.iter().find(|(id, _, _)| *id == a).map(|(_, r, _)| *r).unwrap();
            assert!((a_rect.width - 90.0).abs() < 0.5, "ratio clamped to 0.9");
        });

        // And far below the min; expect a clamp to 0.1.
        assert!(tree.update(cx, |t, cx| t.set_ratio(a, -1.0, cx)));
        tree.read_with(cx, |t, _| {
            let rects = t.compute_rects(Rect { x: 0.0, y: 0.0, width: 100.0, height: 10.0 });
            let a_rect = rects.iter().find(|(id, _, _)| *id == a).map(|(_, r, _)| *r).unwrap();
            assert!((a_rect.width - 10.0).abs() < 0.5, "ratio clamped to 0.1");
        });

        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Pane Tree Model]]
    #[gpui::test]
    fn directional_focus_wraps_at_the_edge(cx: &mut TestAppContext) {
        // Row of three panes: [ A | B ] | C, matching the pure-logic fixture.
        let a = PaneId::from_raw(1);
        let b = PaneId::from_raw(2);
        let c = PaneId::from_raw(3);
        let root = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 2.0 / 3.0,
            first: Box::new(LayoutNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutNode::Leaf(a)),
                second: Box::new(LayoutNode::Leaf(b)),
            }),
            second: Box::new(LayoutNode::Leaf(c)),
        };
        let tree = cx.new(|_| PaneTree::from_root(root, a));

        tree.read_with(cx, |t, _| {
            let rects = t.compute_rects(Rect { x: 0.0, y: 0.0, width: 150.0, height: 100.0 });
            // Direct neighbor: B is to the right of A.
            assert_eq!(t.find_pane_in_direction(a, FocusDirection::Right, &rects), Some(b));
            // Wrap: moving right off the last pane lands on the leftmost pane.
            assert_eq!(t.find_pane_in_direction(c, FocusDirection::Right, &rects), Some(a));
        });
    }

    // @lat: [[client#GPUI Client Spike#GPUI Layout Entities#Pane Tree Model]]
    #[gpui::test]
    fn equalize_resets_all_ratios(cx: &mut TestAppContext) {
        let a = PaneId::from_raw(1);
        let b = PaneId::from_raw(2);
        let root = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.85,
            first: Box::new(LayoutNode::Leaf(a)),
            second: Box::new(LayoutNode::Leaf(b)),
        };
        let tree = cx.new(|_| PaneTree::from_root(root, a));
        let count = change_counter(&tree, cx);

        tree.update(cx, PaneTree::equalize);
        tree.read_with(cx, |t, _| {
            let rects = t.compute_rects(Rect { x: 0.0, y: 0.0, width: 10.0, height: 100.0 });
            let a_rect = rects.iter().find(|(id, _, _)| *id == a).map(|(_, r, _)| *r).unwrap();
            assert!((a_rect.height - 50.0).abs() < 1.0);
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
