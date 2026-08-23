//! Pure workspace-pill drag lifecycle and five-zone drop geometry.
//!
//! GPUI supplies the native drag threshold and pointer delivery. This module
//! owns everything after that boundary: deterministic zone selection, zone
//! hysteresis, edge arming, commit/no-op decisions, and interruption cleanup.

use gpui::{Context, Render, Window};
use scribe_common::ids::WorkspaceId;

use crate::layout::Rect;

/// Distance inside a window edge that arms the future tear-out path.
pub const TEAR_ARM_DISTANCE: f32 = 8.0;
/// Distance strictly inside every edge required to disarm tear-out.
pub const TEAR_DISARM_DISTANCE: f32 = 24.0;
/// Travel into a new zone required before its preview replaces the old one.
pub const ZONE_HYSTERESIS: f32 = 4.0;

/// Window-relative pointer coordinates used by the pure drag model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DragPoint {
    pub x: f32,
    pub y: f32,
}

/// One of the five actionable zones inside a target workspace region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceDropZone {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

/// The actionable preview currently shown under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceDropTarget {
    pub workspace_id: WorkspaceId,
    pub zone: WorkspaceDropZone,
}

/// An in-window tree edit selected at pointer release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceDragCommit {
    pub source_workspace_id: WorkspaceId,
    pub target: WorkspaceDropTarget,
}

/// Why a drag was interrupted before an actionable release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceDragCancel {
    Escape,
    WindowBlur,
    SourceDisappeared,
    NonActionableRelease,
}

/// Observable phase of the workspace drag lifecycle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkspaceDragPhase {
    Idle,
    Armed {
        source_workspace_id: WorkspaceId,
    },
    Dragging {
        source_workspace_id: WorkspaceId,
        pointer: DragPoint,
        preview: Option<WorkspaceDropTarget>,
    },
    TearArmed {
        source_workspace_id: WorkspaceId,
        pointer: DragPoint,
    },
    Committing {
        commit: WorkspaceDragCommit,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PendingZone {
    target: WorkspaceDropTarget,
    crossed_at: DragPoint,
}

/// Live geometry sampled for one level-triggered drag update.
#[derive(Clone, Copy)]
pub struct WorkspaceDragUpdate<'a> {
    pub window_pointer: DragPoint,
    pub window_bounds: Rect,
    pub layout_pointer: DragPoint,
    pub regions: &'a [(WorkspaceId, Rect)],
    pub divider_blocked: bool,
    /// Whether `Welcome` negotiated the atomic transfer operation.
    pub tear_enabled: bool,
}

/// Complete client-local workspace drag state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkspaceDrag {
    phase: WorkspaceDragPhase,
    pending_zone: Option<PendingZone>,
}

impl Default for WorkspaceDrag {
    fn default() -> Self {
        Self { phase: WorkspaceDragPhase::Idle, pending_zone: None }
    }
}

impl WorkspaceDrag {
    /// Arm a fresh pill press. A new arm always replaces stale prior state.
    pub fn arm(&mut self, source_workspace_id: WorkspaceId) {
        self.phase = WorkspaceDragPhase::Armed { source_workspace_id };
        self.pending_zone = None;
    }

    /// Recompute edge arming and the actionable zone from current geometry.
    ///
    /// `window_pointer` is relative to the full GPUI window. `layout_pointer`
    /// is relative to the grid/workspace viewport. Keeping both explicit
    /// prevents titlebar height from leaking into the pure zone math.
    pub fn update(&mut self, update: WorkspaceDragUpdate<'_>) {
        let WorkspaceDragUpdate {
            window_pointer,
            window_bounds,
            layout_pointer,
            regions,
            divider_blocked,
            tear_enabled,
        } = update;
        let Some(source_workspace_id) = self.source_workspace_id() else { return };
        if matches!(self.phase, WorkspaceDragPhase::Committing { .. }) {
            return;
        }

        let tear_armed = matches!(self.phase, WorkspaceDragPhase::TearArmed { .. });
        if tear_enabled
            && (tear_candidate_at(window_pointer, window_bounds)
                || (tear_armed && !should_disarm_tear(window_pointer, window_bounds)))
        {
            self.phase =
                WorkspaceDragPhase::TearArmed { source_workspace_id, pointer: window_pointer };
            self.pending_zone = None;
            return;
        }

        let candidate = (!divider_blocked)
            .then(|| drop_target_at(layout_pointer, regions, source_workspace_id))
            .flatten();
        let preview = match self.phase {
            WorkspaceDragPhase::Dragging { preview, .. } => {
                self.apply_zone_hysteresis(preview, candidate, layout_pointer)
            }
            WorkspaceDragPhase::Armed { .. } | WorkspaceDragPhase::TearArmed { .. } => {
                self.pending_zone = None;
                candidate
            }
            WorkspaceDragPhase::Idle | WorkspaceDragPhase::Committing { .. } => return,
        };
        self.phase =
            WorkspaceDragPhase::Dragging { source_workspace_id, pointer: window_pointer, preview };
    }

    /// Finish the pointer gesture, returning only an actionable in-window edit.
    pub fn release(&mut self) -> Option<WorkspaceDragCommit> {
        let commit = match self.phase {
            WorkspaceDragPhase::Dragging { source_workspace_id, preview: Some(target), .. } => {
                Some(WorkspaceDragCommit { source_workspace_id, target })
            }
            _ => None,
        };
        self.pending_zone = None;
        if let Some(commit) = commit {
            self.phase = WorkspaceDragPhase::Committing { commit };
        } else {
            self.cancel(WorkspaceDragCancel::NonActionableRelease);
        }
        commit
    }

    /// Return to idle after the selected tree edit has been attempted.
    pub fn complete_commit(&mut self) {
        if matches!(self.phase, WorkspaceDragPhase::Committing { .. }) {
            self.phase = WorkspaceDragPhase::Idle;
        }
        self.pending_zone = None;
    }

    /// Cancel every non-idle phase without producing a commit.
    pub fn cancel(&mut self, _reason: WorkspaceDragCancel) {
        self.phase = WorkspaceDragPhase::Idle;
        self.pending_zone = None;
    }

    #[must_use]
    pub const fn phase(&self) -> WorkspaceDragPhase {
        self.phase
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        !matches!(self.phase, WorkspaceDragPhase::Idle)
    }

    /// Whether GPUI's native threshold has engaged and the input shield should
    /// claim pointer events from the terminal beneath it.
    #[must_use]
    pub const fn is_engaged(&self) -> bool {
        matches!(
            self.phase,
            WorkspaceDragPhase::Dragging { .. }
                | WorkspaceDragPhase::TearArmed { .. }
                | WorkspaceDragPhase::Committing { .. }
        )
    }

    #[must_use]
    pub const fn source_workspace_id(&self) -> Option<WorkspaceId> {
        match self.phase {
            WorkspaceDragPhase::Idle => None,
            WorkspaceDragPhase::Armed { source_workspace_id }
            | WorkspaceDragPhase::Dragging { source_workspace_id, .. }
            | WorkspaceDragPhase::TearArmed { source_workspace_id, .. } => {
                Some(source_workspace_id)
            }
            WorkspaceDragPhase::Committing { commit } => Some(commit.source_workspace_id),
        }
    }

    #[must_use]
    pub const fn pointer(&self) -> Option<DragPoint> {
        match self.phase {
            WorkspaceDragPhase::Dragging { pointer, .. }
            | WorkspaceDragPhase::TearArmed { pointer, .. } => Some(pointer),
            WorkspaceDragPhase::Idle
            | WorkspaceDragPhase::Armed { .. }
            | WorkspaceDragPhase::Committing { .. } => None,
        }
    }

    #[must_use]
    pub const fn preview(&self) -> Option<WorkspaceDropTarget> {
        match self.phase {
            WorkspaceDragPhase::Dragging { preview, .. } => preview,
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_tear_armed(&self) -> bool {
        matches!(self.phase, WorkspaceDragPhase::TearArmed { .. })
    }

    fn apply_zone_hysteresis(
        &mut self,
        current: Option<WorkspaceDropTarget>,
        candidate: Option<WorkspaceDropTarget>,
        pointer: DragPoint,
    ) -> Option<WorkspaceDropTarget> {
        if candidate == current {
            self.pending_zone = None;
            return current;
        }
        let (Some(current), Some(candidate)) = (current, candidate) else {
            // Source regions, dividers, and chrome must become non-actionable
            // immediately; retaining an old preview here could commit on them.
            self.pending_zone = None;
            return candidate;
        };
        if current.workspace_id != candidate.workspace_id {
            self.pending_zone = None;
            return Some(candidate);
        }

        match self.pending_zone {
            Some(pending) if pending.target == candidate => {
                let travel =
                    (pointer.x - pending.crossed_at.x).hypot(pointer.y - pending.crossed_at.y);
                if travel >= ZONE_HYSTERESIS {
                    self.pending_zone = None;
                    Some(candidate)
                } else {
                    Some(current)
                }
            }
            _ => {
                self.pending_zone = Some(PendingZone { target: candidate, crossed_at: pointer });
                Some(current)
            }
        }
    }
}

/// Resolve the nearest normalized edge, using horizontal precedence on ties.
#[must_use]
pub fn zone_at(point: DragPoint, rect: Rect) -> Option<WorkspaceDropZone> {
    if rect.width <= 0.0
        || rect.height <= 0.0
        || point.x < rect.x
        || point.x > rect.x + rect.width
        || point.y < rect.y
        || point.y > rect.y + rect.height
    {
        return None;
    }

    let left = (point.x - rect.x) / rect.width;
    let right = (rect.x + rect.width - point.x) / rect.width;
    let top = (point.y - rect.y) / rect.height;
    let bottom = (rect.y + rect.height - point.y) / rect.height;
    let horizontal = if left <= right {
        (left, WorkspaceDropZone::Left)
    } else {
        (right, WorkspaceDropZone::Right)
    };
    let vertical = if top <= bottom {
        (top, WorkspaceDropZone::Top)
    } else {
        (bottom, WorkspaceDropZone::Bottom)
    };
    let (distance, zone) = if horizontal.0 <= vertical.0 { horizontal } else { vertical };
    Some(if distance > 1.0 / 3.0 { WorkspaceDropZone::Center } else { zone })
}

/// Rect painted for one zone preview.
#[must_use]
pub fn zone_preview_rect(rect: Rect, zone: WorkspaceDropZone) -> Rect {
    let third_w = rect.width / 3.0;
    let third_h = rect.height / 3.0;
    match zone {
        WorkspaceDropZone::Left => Rect { width: third_w, ..rect },
        WorkspaceDropZone::Right => {
            Rect { x: rect.x + rect.width - third_w, width: third_w, ..rect }
        }
        WorkspaceDropZone::Top => Rect { height: third_h, ..rect },
        WorkspaceDropZone::Bottom => {
            Rect { y: rect.y + rect.height - third_h, height: third_h, ..rect }
        }
        WorkspaceDropZone::Center => {
            Rect { x: rect.x + third_w, y: rect.y + third_h, width: third_w, height: third_h }
        }
    }
}

fn drop_target_at(
    point: DragPoint,
    regions: &[(WorkspaceId, Rect)],
    source_workspace_id: WorkspaceId,
) -> Option<WorkspaceDropTarget> {
    regions.iter().find_map(|(workspace_id, rect)| {
        (*workspace_id != source_workspace_id)
            .then(|| zone_at(point, *rect))
            .flatten()
            .map(|zone| WorkspaceDropTarget { workspace_id: *workspace_id, zone })
    })
}

/// Whether a release is in the universal edge band or beyond the window.
#[must_use]
pub fn tear_candidate_at(point: DragPoint, bounds: Rect) -> bool {
    point.x <= bounds.x + TEAR_ARM_DISTANCE
        || point.x >= bounds.x + bounds.width - TEAR_ARM_DISTANCE
        || point.y <= bounds.y + TEAR_ARM_DISTANCE
        || point.y >= bounds.y + bounds.height - TEAR_ARM_DISTANCE
}

fn should_disarm_tear(point: DragPoint, bounds: Rect) -> bool {
    point.x > bounds.x + TEAR_DISARM_DISTANCE
        && point.x < bounds.x + bounds.width - TEAR_DISARM_DISTANCE
        && point.y > bounds.y + TEAR_DISARM_DISTANCE
        && point.y < bounds.y + bounds.height - TEAR_DISARM_DISTANCE
}

/// Dedicated GPUI marker; tab drags use different payload types.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceDragMarker;

/// Empty native ghost. The shell paints the visible pill ghost as a deferred
/// in-window overlay so its preview and lifecycle share one source of truth.
pub struct EmptyWorkspaceDragGhost;

impl Render for EmptyWorkspaceDragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        gpui::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect { x: 0.0, y: 0.0, width: 300.0, height: 300.0 }
    }

    fn window() -> Rect {
        Rect { x: 0.0, y: 0.0, width: 420.0, height: 300.0 }
    }

    fn update_drag(
        drag: &mut WorkspaceDrag,
        point: DragPoint,
        regions: &[(WorkspaceId, Rect)],
        divider_blocked: bool,
    ) {
        drag.update(WorkspaceDragUpdate {
            window_pointer: point,
            window_bounds: window(),
            layout_pointer: point,
            regions,
            divider_blocked,
            tear_enabled: true,
        });
    }

    // @lat: [[test#Test Harness#GPUI Workspace Drag]]
    #[test]
    fn every_corner_uses_horizontal_tie_break() {
        assert_eq!(zone_at(DragPoint { x: 0.0, y: 0.0 }, rect()), Some(WorkspaceDropZone::Left));
        assert_eq!(zone_at(DragPoint { x: 300.0, y: 0.0 }, rect()), Some(WorkspaceDropZone::Right));
        assert_eq!(zone_at(DragPoint { x: 0.0, y: 300.0 }, rect()), Some(WorkspaceDropZone::Left));
        assert_eq!(
            zone_at(DragPoint { x: 300.0, y: 300.0 }, rect()),
            Some(WorkspaceDropZone::Right)
        );
    }

    #[test]
    fn normalized_nearest_edge_and_all_five_zones_are_deterministic() {
        let wide = Rect { x: 0.0, y: 0.0, width: 300.0, height: 100.0 };
        for (point, expected) in [
            (DragPoint { x: 10.0, y: 50.0 }, WorkspaceDropZone::Left),
            (DragPoint { x: 290.0, y: 50.0 }, WorkspaceDropZone::Right),
            (DragPoint { x: 150.0, y: 0.0 }, WorkspaceDropZone::Top),
            (DragPoint { x: 150.0, y: 100.0 }, WorkspaceDropZone::Bottom),
            (DragPoint { x: 150.0, y: 50.0 }, WorkspaceDropZone::Center),
        ] {
            assert_eq!(zone_at(point, wide), Some(expected));
        }
        assert_eq!(zone_at(DragPoint { x: 10.0, y: 20.0 }, wide), Some(WorkspaceDropZone::Left));
    }

    #[test]
    fn preview_rects_use_one_third_bands_and_center() {
        let region = Rect { x: 30.0, y: 60.0, width: 300.0, height: 150.0 };
        assert_eq!(
            zone_preview_rect(region, WorkspaceDropZone::Left),
            Rect { x: 30.0, y: 60.0, width: 100.0, height: 150.0 }
        );
        assert_eq!(
            zone_preview_rect(region, WorkspaceDropZone::Right),
            Rect { x: 230.0, y: 60.0, width: 100.0, height: 150.0 }
        );
        assert_eq!(
            zone_preview_rect(region, WorkspaceDropZone::Top),
            Rect { x: 30.0, y: 60.0, width: 300.0, height: 50.0 }
        );
        assert_eq!(
            zone_preview_rect(region, WorkspaceDropZone::Bottom),
            Rect { x: 30.0, y: 160.0, width: 300.0, height: 50.0 }
        );
        assert_eq!(
            zone_preview_rect(region, WorkspaceDropZone::Center),
            Rect { x: 130.0, y: 110.0, width: 100.0, height: 50.0 }
        );
    }

    #[test]
    fn zone_transition_requires_four_pixels_past_boundary() {
        let source = WorkspaceId::new();
        let target = WorkspaceId::new();
        let regions = [(target, rect())];
        let mut drag = WorkspaceDrag::default();
        drag.arm(source);
        update_drag(&mut drag, DragPoint { x: 99.0, y: 150.0 }, &regions, false);
        assert_eq!(drag.preview().map(|preview| preview.zone), Some(WorkspaceDropZone::Left));

        for x in [101.0, 104.9] {
            update_drag(&mut drag, DragPoint { x, y: 150.0 }, &regions, false);
            assert_eq!(drag.preview().map(|preview| preview.zone), Some(WorkspaceDropZone::Left));
        }
        update_drag(&mut drag, DragPoint { x: 105.0, y: 150.0 }, &regions, false);
        assert_eq!(drag.preview().map(|preview| preview.zone), Some(WorkspaceDropZone::Center));
    }

    #[test]
    fn eight_pixel_arm_and_strictly_over_twenty_four_disarm() {
        let source = WorkspaceId::new();
        let mut drag = WorkspaceDrag::default();
        drag.arm(source);
        update_drag(&mut drag, DragPoint { x: 8.0, y: 100.0 }, &[], false);
        assert!(drag.is_tear_armed());
        update_drag(&mut drag, DragPoint { x: 24.0, y: 100.0 }, &[], false);
        assert!(drag.is_tear_armed());
        update_drag(&mut drag, DragPoint { x: 25.0, y: 100.0 }, &[], false);
        assert!(matches!(drag.phase(), WorkspaceDragPhase::Dragging { .. }));

        update_drag(&mut drag, DragPoint { x: -1.0, y: 100.0 }, &[], false);
        assert!(drag.is_tear_armed(), "out-of-bounds delivery also arms");
    }

    #[test]
    fn tear_arm_clears_preview_and_reentry_restores_targeting() {
        let source = WorkspaceId::new();
        let target = WorkspaceId::new();
        let regions = [(target, rect())];
        let mut drag = WorkspaceDrag::default();
        drag.arm(source);
        update_drag(&mut drag, DragPoint { x: 150.0, y: 150.0 }, &regions, false);
        assert!(drag.preview().is_some());

        update_drag(&mut drag, DragPoint { x: 4.0, y: 150.0 }, &regions, false);
        assert!(drag.is_tear_armed());
        assert_eq!(drag.preview(), None);

        update_drag(&mut drag, DragPoint { x: 25.0, y: 150.0 }, &regions, false);
        assert!(!drag.is_tear_armed());
        assert!(drag.preview().is_some());
    }

    #[test]
    fn absent_capability_never_arms_tear_out() {
        let source = WorkspaceId::new();
        let mut drag = WorkspaceDrag::default();
        drag.arm(source);
        drag.update(WorkspaceDragUpdate {
            window_pointer: DragPoint { x: 4.0, y: 100.0 },
            window_bounds: window(),
            layout_pointer: DragPoint { x: 4.0, y: 100.0 },
            regions: &[],
            divider_blocked: false,
            tear_enabled: false,
        });
        assert!(!drag.is_tear_armed());
    }

    #[test]
    fn cancel_paths_and_non_actionable_releases_never_commit() {
        let source = WorkspaceId::new();
        for reason in [
            WorkspaceDragCancel::Escape,
            WorkspaceDragCancel::WindowBlur,
            WorkspaceDragCancel::SourceDisappeared,
        ] {
            let mut drag = WorkspaceDrag::default();
            drag.arm(source);
            drag.cancel(reason);
            assert_eq!(drag.phase(), WorkspaceDragPhase::Idle);
            assert_eq!(drag.release(), None);
        }

        let mut armed = WorkspaceDrag::default();
        armed.arm(source);
        assert_eq!(armed.release(), None);
        assert_eq!(armed.phase(), WorkspaceDragPhase::Idle);

        let mut tear = WorkspaceDrag::default();
        tear.arm(source);
        update_drag(&mut tear, DragPoint { x: -1.0, y: 100.0 }, &[], false);
        assert_eq!(tear.release(), None);
        assert_eq!(tear.phase(), WorkspaceDragPhase::Idle);
    }

    #[test]
    fn self_drop_divider_and_center_on_source_are_no_ops() {
        let source = WorkspaceId::new();
        let regions = [(source, rect())];
        for blocked in [false, true] {
            let mut drag = WorkspaceDrag::default();
            drag.arm(source);
            update_drag(&mut drag, DragPoint { x: 150.0, y: 150.0 }, &regions, blocked);
            assert_eq!(drag.preview(), None);
            assert_eq!(drag.release(), None);
            assert_eq!(drag.phase(), WorkspaceDragPhase::Idle);
        }
    }

    #[test]
    fn disappearing_target_clears_preview_and_release_is_a_no_op() {
        let source = WorkspaceId::new();
        let target = WorkspaceId::new();
        let mut drag = WorkspaceDrag::default();
        drag.arm(source);
        update_drag(&mut drag, DragPoint { x: 10.0, y: 150.0 }, &[(target, rect())], false);
        assert!(drag.preview().is_some());
        update_drag(&mut drag, DragPoint { x: 10.0, y: 150.0 }, &[], false);
        assert_eq!(drag.preview(), None);
        assert_eq!(drag.release(), None);
        assert_eq!(drag.phase(), WorkspaceDragPhase::Idle);
    }

    #[test]
    fn actionable_release_enters_committing_then_next_drag_is_healthy() {
        let source = WorkspaceId::new();
        let target = WorkspaceId::new();
        let regions = [(target, rect())];
        let mut drag = WorkspaceDrag::default();
        drag.arm(source);
        update_drag(&mut drag, DragPoint { x: 10.0, y: 150.0 }, &regions, false);
        let commit = drag.release().expect("left edge is actionable");
        assert_eq!(commit.source_workspace_id, source);
        assert_eq!(commit.target.workspace_id, target);
        assert_eq!(commit.target.zone, WorkspaceDropZone::Left);
        assert!(matches!(drag.phase(), WorkspaceDragPhase::Committing { .. }));
        drag.complete_commit();
        assert_eq!(drag.phase(), WorkspaceDragPhase::Idle);

        drag.arm(target);
        assert_eq!(
            drag.phase(),
            WorkspaceDragPhase::Armed { source_workspace_id: target },
            "a completed drag cannot poison the next arm"
        );
    }
}
