//! Animated slot widths for a tab drag, shared by the titlebar's groups and
//! the shell's lower region bars.
//!
//! A tab drag changes a bar's layout in two ways: a landing gap opens where
//! the tab will go, and the dragged tab's own slot closes once it is aimed at
//! another bar. Both are widths, and both must move continuously when the
//! target changes mid-flight. GPUI's `with_animation` restarts from its start
//! value whenever its key changes, so a gap that moved slot to slot either
//! replayed its opening or jumped. Here each width is a [`Tween`] that is
//! retargeted from wherever it is now: a gap leaving one slot closes while the
//! next one opens, so the tabs between them slide.
//!
//! Gaps are keyed by the tab they sit beside, not by index, so a tab arriving
//! or leaving does not shift a gap onto a neighbour, and the keys are unique
//! across every bar of a window: one [`TabDropMotion`] serves a whole strip, and
//! a gap crossing from one bar to another closes in the first as it opens in
//! the second. Pure: callers pass `now` and the duration (zero when motion is
//! off), and ask [`TabDropMotion::animating`] whether to schedule another frame.

use std::time::{Duration, Instant};

use scribe_common::ids::{SessionId, WorkspaceId};

/// Where a landing gap opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GapSlot {
    /// Before this tab.
    Before(SessionId),
    /// After this tab, the last of its bar.
    After(SessionId),
    /// In this workspace's bar, which has no tabs.
    Empty(WorkspaceId),
}

impl GapSlot {
    /// The gap for inserting at `index` among `tabs`, a bar's tabs in order.
    #[must_use]
    pub fn at(workspace_id: WorkspaceId, tabs: &[SessionId], index: usize) -> Self {
        match (tabs.get(index), tabs.last()) {
            (Some(&before), _) => Self::Before(before),
            (None, Some(&last)) => Self::After(last),
            (None, None) => Self::Empty(workspace_id),
        }
    }
}

/// One width moving between two fractions (0 closed, 1 full) of its size.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Tween {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
}

impl Tween {
    const fn settled(value: f32, now: Instant) -> Self {
        Self { from: value, to: value, start: now, duration: Duration::ZERO }
    }

    fn at(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return self.to;
        }
        let t = (now.saturating_duration_since(self.start).as_secs_f32()
            / self.duration.as_secs_f32())
        .clamp(0.0, 1.0);
        // Ease-out quint, the curve every other tab-drag motion uses.
        let eased = 1.0 - (1.0 - t).powi(5);
        (self.to - self.from).mul_add(eased, self.from)
    }

    fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= self.duration
    }

    /// Head for `to` from the current value, keeping a flight already headed
    /// there untouched so it does not restart every frame.
    fn retarget(&mut self, to: f32, now: Instant, duration: Duration) {
        if (self.to - to).abs() > f32::EPSILON {
            *self = Self { from: self.at(now), to, start: now, duration };
        }
    }
}

/// What a bar should show this frame for a tab drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabDropTarget {
    /// The open landing gap, if the drag is aimed at a bar.
    pub gap: Option<GapSlot>,
    /// The tab whose slot is closed: the dragged tab while it is aimed
    /// elsewhere, or a released tab until the server's list moves it.
    pub closed: Option<SessionId>,
}

/// The animated drop widths of a window's bars.
#[derive(Debug, Clone, Default)]
pub struct TabDropMotion {
    gaps: Vec<(GapSlot, Tween)>,
    slot: Option<(SessionId, Tween)>,
}

impl TabDropMotion {
    /// Move every width toward `target`.
    pub fn update(&mut self, target: TabDropTarget, now: Instant, duration: Duration) {
        for (slot, tween) in &mut self.gaps {
            let open = target.gap == Some(*slot);
            tween.retarget(if open { 1.0 } else { 0.0 }, now, duration);
        }
        if let Some(gap) = target.gap
            && !self.gaps.iter().any(|(slot, _)| *slot == gap)
        {
            let mut tween = Tween::settled(0.0, now);
            tween.retarget(1.0, now, duration);
            self.gaps.push((gap, tween));
        }
        // A closed gap has nothing left to paint.
        self.gaps.retain(|(_, tween)| tween.to > 0.0 || !tween.done(now));

        match (target.closed, self.slot.as_mut()) {
            (Some(closed), Some((session, tween))) if *session == closed => {
                tween.retarget(0.0, now, duration);
            }
            (Some(closed), _) => {
                let mut tween = Tween::settled(1.0, now);
                tween.retarget(0.0, now, duration);
                self.slot = Some((closed, tween));
            }
            (None, Some((_, tween))) => tween.retarget(1.0, now, duration),
            (None, None) => {}
        }
        if self.slot.is_some_and(|(_, tween)| tween.to >= 1.0 && tween.done(now)) {
            self.slot = None;
        }
    }

    /// Close every gap at once, without a closing animation.
    ///
    /// For the frame a released tab lands: the tab itself now fills the
    /// space its gap was holding, and a gap still closing beside it would
    /// leave two slots' worth of room for one tab.
    pub fn snap_gaps_shut(&mut self) {
        self.gaps.clear();
    }

    /// How open the gap before `slot` is, from 0 to 1.
    #[must_use]
    pub fn gap(&self, slot: GapSlot, now: Instant) -> f32 {
        self.gaps
            .iter()
            .find(|(gap, _)| *gap == slot)
            .map_or(0.0, |(_, tween)| tween.at(now).clamp(0.0, 1.0))
    }

    /// How open `session`'s own slot is, from 0 (closed) to 1.
    #[must_use]
    pub fn slot(&self, session: SessionId, now: Instant) -> f32 {
        self.slot
            .filter(|(closing, _)| *closing == session)
            .map_or(1.0, |(_, tween)| tween.at(now).clamp(0.0, 1.0))
    }

    /// Whether any width is still moving, so the bar needs another frame.
    #[must_use]
    pub fn animating(&self, now: Instant) -> bool {
        self.gaps.iter().any(|(_, tween)| !tween.done(now))
            || self.slot.is_some_and(|(_, tween)| !tween.done(now))
    }

    /// Whether this bar has nothing to show, so its state can be dropped.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.gaps.is_empty() && self.slot.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Duration = Duration::from_millis(100);

    fn ms(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    fn aimed(gap: GapSlot) -> TabDropTarget {
        TabDropTarget { gap: Some(gap), closed: None }
    }

    #[test]
    fn gap_slots_name_the_tab_beside_them() {
        let workspace = WorkspaceId::new();
        let tabs = [SessionId::new(), SessionId::new()];
        assert_eq!(GapSlot::at(workspace, &tabs, 0), GapSlot::Before(tabs[0]));
        assert_eq!(GapSlot::at(workspace, &tabs, 1), GapSlot::Before(tabs[1]));
        assert_eq!(GapSlot::at(workspace, &tabs, 2), GapSlot::After(tabs[1]));
        assert_eq!(GapSlot::at(workspace, &tabs, 9), GapSlot::After(tabs[1]));
        assert_eq!(GapSlot::at(workspace, &[], 0), GapSlot::Empty(workspace));
    }

    #[test]
    fn a_gap_opens_from_nothing_and_settles_full() {
        let t0 = Instant::now();
        let before = GapSlot::Before(SessionId::new());
        let mut motion = TabDropMotion::default();
        motion.update(aimed(before), t0, D);
        assert!(motion.gap(before, t0) < 0.01);
        assert!(motion.animating(ms(t0, 50)));
        let half = motion.gap(before, ms(t0, 50));
        assert!(half > 0.5 && half < 1.0, "ease-out is past halfway at the midpoint: {half}");
        assert!((motion.gap(before, ms(t0, 100)) - 1.0).abs() < 1e-6);
        assert!(!motion.animating(ms(t0, 100)));
    }

    #[test]
    fn a_gap_moving_slots_slides_from_where_it_is() {
        let t0 = Instant::now();
        let (a, b) = (GapSlot::Before(SessionId::new()), GapSlot::After(SessionId::new()));
        let mut motion = TabDropMotion::default();
        motion.update(aimed(a), t0, D);
        motion.update(aimed(a), ms(t0, 100), D);
        // Retarget mid-way: the old gap closes from fully open while the new
        // one opens, and their sum stays one slot, so the tabs between slide.
        let at = ms(t0, 100);
        motion.update(aimed(b), at, D);
        assert!((motion.gap(a, at) - 1.0).abs() < 1e-6, "no jump on retarget");
        assert!(motion.gap(b, at) < 1e-6);
        let mid = ms(t0, 150);
        assert!((motion.gap(a, mid) + motion.gap(b, mid) - 1.0).abs() < 1e-4);
        assert!(motion.gap(a, ms(t0, 200)) < 1e-6);
        motion.update(aimed(b), ms(t0, 200), D);
        assert_eq!(motion.gaps.len(), 1, "a fully closed gap is dropped");
    }

    #[test]
    fn an_interrupted_gap_reverses_from_its_current_width() {
        let t0 = Instant::now();
        let a = GapSlot::Before(SessionId::new());
        let mut motion = TabDropMotion::default();
        motion.update(aimed(a), t0, D);
        let at = ms(t0, 30);
        let partial = motion.gap(a, at);
        motion.update(TabDropTarget::default(), at, D);
        assert!((motion.gap(a, at) - partial).abs() < 1e-6);
        assert!(motion.gap(a, ms(t0, 130)) < 1e-6);
    }

    #[test]
    fn a_dragged_slot_closes_and_reopens_smoothly() {
        let t0 = Instant::now();
        let tab = SessionId::new();
        let mut motion = TabDropMotion::default();
        assert!((motion.slot(tab, t0) - 1.0).abs() < 1e-6);
        motion.update(TabDropTarget { gap: None, closed: Some(tab) }, t0, D);
        assert!(motion.slot(tab, ms(t0, 100)) < 1e-6);
        let back = ms(t0, 100);
        motion.update(TabDropTarget::default(), back, D);
        assert!(motion.slot(tab, back) < 1e-6, "reopening starts closed");
        assert!(motion.animating(ms(t0, 150)));
        motion.update(TabDropTarget::default(), ms(t0, 200), D);
        assert!(motion.is_idle());
    }

    #[test]
    fn landing_snaps_the_gap_shut_and_motion_off_is_instant() {
        let t0 = Instant::now();
        let a = GapSlot::Before(SessionId::new());
        let mut motion = TabDropMotion::default();
        motion.update(aimed(a), t0, D);
        motion.snap_gaps_shut();
        assert!(motion.gap(a, t0) < 1e-6);

        let mut still = TabDropMotion::default();
        still.update(aimed(a), t0, Duration::ZERO);
        assert!((still.gap(a, t0) - 1.0).abs() < 1e-6);
        assert!(!still.animating(t0));
    }
}
