//! Where every tab in one window was last painted.
//!
//! Tabs flex beside a workspace pill whose width follows its text, so where a
//! tab sits is a layout result no arithmetic can reproduce; every estimate
//! that tried (equal-share slots behind a character-count pill width) drifted
//! from what the user saw. Each tab, and each open landing gap, records its
//! own painted box here from a zero-ink prepaint probe, and everything that
//! needs a tab's position reads it back: the grid's joins at paint time (a
//! frame's prepaint finishes before any paint, so they see that frame's boxes)
//! and drag geometry at event time (the same last-painted frame GPUI's own hit
//! testing uses). The titlebar and the lower region bars write into one store,
//! because one window's drag can move a tab between them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{AnyElement, IntoElement as _, Styled as _, canvas};
use scribe_common::ids::{SessionId, WorkspaceId};

use crate::tab_drop_motion::GapSlot;

/// `(left, width)` in window pixels.
pub type Span = (f32, f32);

#[derive(Default)]
struct Boxes {
    tabs: HashMap<SessionId, Span>,
    gaps: HashMap<GapSlot, Span>,
}

/// One window's painted tab boxes, shared by every bar that paints tabs.
#[derive(Clone, Default)]
pub struct PaintedTabs(Rc<RefCell<Boxes>>);

impl PaintedTabs {
    /// The painted box of `session_id`'s tab, if a frame has painted it.
    #[must_use]
    pub fn tab(&self, session_id: SessionId) -> Option<Span> {
        self.0.borrow().tabs.get(&session_id).copied()
    }

    /// A zero-ink overlay recording the tab it sits in, pinned to every edge
    /// so the record is the tab's own box rather than its padding-inset
    /// content: the card, the join below it, and the drop slots all span the
    /// full tab.
    #[must_use]
    pub fn tab_probe(&self, session_id: SessionId) -> AnyElement {
        let store = self.clone();
        Self::probe(move |span| {
            store.0.borrow_mut().tabs.insert(session_id, span);
        })
    }

    /// A zero-ink overlay recording the landing gap it sits in.
    #[must_use]
    pub fn gap_probe(&self, slot: GapSlot) -> AnyElement {
        let store = self.clone();
        Self::probe(move |span| {
            store.0.borrow_mut().gaps.insert(slot, span);
        })
    }

    fn probe(record: impl Fn(Span) + 'static) -> AnyElement {
        canvas(
            move |bounds, _window, _cx| {
                record((f32::from(bounds.origin.x), f32::from(bounds.size.width)));
            },
            |_, (), _window, _cx| {},
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    /// Start a frame: forget every tab that is no longer in a bar, and every
    /// gap, which the frame's probes record again while one is still open.
    ///
    /// A tab whose slot is closing paints no probe and keeps its last
    /// full-width box on purpose: that is the width its drag ghost keeps.
    pub fn begin_frame(&self, live: impl Fn(SessionId) -> bool) {
        let mut boxes = self.0.borrow_mut();
        boxes.tabs.retain(|session_id, _| live(*session_id));
        boxes.gaps.clear();
    }

    /// The painted width of one tab in a bar, `0.0` when none has painted.
    /// Tabs in one bar share their run equally, so any one stands for all.
    #[must_use]
    pub fn slot_width(&self, tabs: &[SessionId]) -> f32 {
        let boxes = self.0.borrow();
        tabs.iter().find_map(|session_id| boxes.tabs.get(session_id)).map_or(0.0, |span| span.1)
    }

    /// Where a tab dropped at `pointer_x` lands among a bar's `tabs`: before
    /// the first tab whose centre is right of the pointer.
    ///
    /// Centres are taken where the tabs rest. The landing gap a drag opens in
    /// this bar pushes every tab after it right, so measuring against the
    /// pushed boxes would let the gap the index opened move the index itself.
    #[must_use]
    pub fn insertion_index(
        &self,
        pointer_x: f32,
        workspace_id: WorkspaceId,
        tabs: &[SessionId],
    ) -> usize {
        let boxes = self.0.borrow();
        let gap = (0..=tabs.len())
            .find_map(|index| boxes.gaps.get(&GapSlot::at(workspace_id, tabs, index)).copied());
        tabs.iter()
            .filter_map(|session_id| boxes.tabs.get(session_id))
            .filter(|&&(left, width)| {
                let rest = match gap {
                    Some((gap_left, gap_width)) if gap_left <= left => left - gap_width,
                    _ => left,
                };
                rest + width / 2.0 < pointer_x
            })
            .count()
    }

    /// The left edge a tab dropped at `index` among a bar's `tabs` lands on:
    /// its open landing gap when one is painted, else the boundary between
    /// the painted tabs either side. `None` for a bar nothing has painted in.
    #[must_use]
    pub fn landing_left(
        &self,
        workspace_id: WorkspaceId,
        tabs: &[SessionId],
        index: usize,
    ) -> Option<f32> {
        let boxes = self.0.borrow();
        if let Some(&(left, _)) = boxes.gaps.get(&GapSlot::at(workspace_id, tabs, index)) {
            return Some(left);
        }
        match tabs.get(index) {
            Some(session_id) => boxes.tabs.get(session_id).map(|span| span.0),
            None => tabs.last().and_then(|last| boxes.tabs.get(last)).map(|span| span.0 + span.1),
        }
    }

    #[cfg(test)]
    fn record_tab(&self, session_id: SessionId, span: Span) {
        self.0.borrow_mut().tabs.insert(session_id, span);
    }

    #[cfg(test)]
    fn record_gap(&self, slot: GapSlot, span: Span) {
        self.0.borrow_mut().gaps.insert(slot, span);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bar of three 100px tabs whose run starts at x=60, after a pill.
    fn bar() -> (PaintedTabs, WorkspaceId, Vec<SessionId>) {
        let painted = PaintedTabs::default();
        let tabs: Vec<SessionId> = (0..3).map(|_| SessionId::new()).collect();
        for (session_id, left) in tabs.iter().zip([60.0, 160.0, 260.0]) {
            painted.record_tab(*session_id, (left, 100.0));
        }
        (painted, WorkspaceId::new(), tabs)
    }

    // @lat: [[test#GPUI Client Headless Suites#Drops land between painted tab centres]]
    // @lat: [[test#GPUI Client Headless Suites#Atomic tab-subtree region transfer]]
    #[test]
    fn drops_land_between_painted_tab_centres() {
        let (painted, workspace, tabs) = bar();
        assert_eq!(painted.insertion_index(100.0, workspace, &tabs), 0, "left of the first centre");
        assert_eq!(painted.insertion_index(120.0, workspace, &tabs), 1, "past the first centre");
        assert_eq!(painted.insertion_index(359.0, workspace, &tabs), 3, "past the last centre");
        assert_eq!(painted.insertion_index(359.0, workspace, &[]), 0, "an empty bar");

        // A 100px gap open before the second tab pushes it and the third right;
        // the index still reads their resting centres, so it does not move.
        painted.record_tab(tabs[1], (260.0, 100.0));
        painted.record_tab(tabs[2], (360.0, 100.0));
        painted.record_gap(GapSlot::Before(tabs[1]), (160.0, 100.0));
        assert_eq!(painted.insertion_index(205.0, workspace, &tabs), 1, "over the open gap");
        assert_eq!(painted.insertion_index(215.0, workspace, &tabs), 2, "past the rest centre");
    }

    // @lat: [[test#GPUI Client Headless Suites#A drop lands on its painted gap]]
    #[test]
    fn a_drop_lands_on_its_painted_gap() {
        let (painted, workspace, tabs) = bar();
        assert_eq!(painted.landing_left(workspace, &tabs, 1), Some(160.0), "the boundary");
        assert_eq!(painted.landing_left(workspace, &tabs, 3), Some(360.0), "after the last");
        assert_eq!(painted.landing_left(workspace, &[], 0), None, "nothing painted");
        painted.record_gap(GapSlot::Empty(workspace), (90.0, 40.0));
        assert_eq!(painted.landing_left(workspace, &[], 0), Some(90.0), "an empty bar's gap");
        assert!((painted.slot_width(&tabs) - 100.0).abs() < f32::EPSILON);

        painted.begin_frame(|session_id| session_id != tabs[0]);
        assert_eq!(painted.tab(tabs[0]), None, "a tab that left every bar is forgotten");
        assert_eq!(painted.landing_left(workspace, &[], 0), None, "gaps are per frame");
    }
}
