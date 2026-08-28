//! Pure workspace-pill drag lifecycle and five-zone drop geometry.
//!
//! GPUI supplies the native drag threshold and pointer delivery. This module
//! owns everything after that boundary: deterministic zone selection, zone
//! hysteresis, edge arming, commit/no-op decisions, interruption cleanup, the
//! purely visual motion frames the shell paints over that lifecycle, and the
//! opt-in input-to-paint probe that measures it.

use std::time::{Duration, Instant};

use gpui::{Context, Render, Window};
use scribe_common::{
    ids::WorkspaceId,
    protocol::{WorkspaceMoveOperation, WorkspaceTreeEdge},
};

use crate::layout::Rect;

/// Distance inside a window edge that arms the future tear-out path.
pub const TEAR_ARM_DISTANCE: f32 = 8.0;
/// Distance strictly inside every edge required to disarm tear-out.
pub const TEAR_DISARM_DISTANCE: f32 = 24.0;
/// Travel into a new zone required before its preview replaces the old one.
pub const ZONE_HYSTERESIS: f32 = 4.0;

/// Requested length of a zone highlight's fade in or out. Passed through
/// [`AnimationSettings::transition`](crate::animation::AnimationSettings::transition),
/// so it is capped by `MAX_TRANSITION` and collapses to zero with motion off.
pub const ZONE_FADE: Duration = Duration::from_millis(120);

/// Requested length of the ghost's post-release travel — settling onto the
/// committed zone, or snapping back to the grab point on a cancelled drag.
pub const GHOST_TRAVEL: Duration = Duration::from_millis(150);

/// Environment variable that enables the input-to-paint drag probe. Truthy
/// values follow the same spellings as [`crate::animation::DISABLE_ANIMATIONS_ENV`].
pub const DRAG_PROBE_ENV: &str = "SCRIBE_DRAG_PROBE";

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
    grab_point: Option<DragPoint>,
}

impl Default for WorkspaceDrag {
    fn default() -> Self {
        Self { phase: WorkspaceDragPhase::Idle, pending_zone: None, grab_point: None }
    }
}

impl WorkspaceDrag {
    /// Arm a fresh pill press. A new arm always replaces stale prior state.
    pub fn arm(&mut self, source_workspace_id: WorkspaceId) {
        self.phase = WorkspaceDragPhase::Armed { source_workspace_id };
        self.pending_zone = None;
        self.grab_point = None;
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

        self.grab_point.get_or_insert(window_pointer);
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
            WorkspaceDragPhase::Idle => return,
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
        self.cancel();
        commit
    }

    /// Cancel every non-idle phase without producing a commit.
    pub fn cancel(&mut self) {
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
            WorkspaceDragPhase::Dragging { .. } | WorkspaceDragPhase::TearArmed { .. }
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
        }
    }

    /// Where the pointer was when GPUI first delivered this drag — within the
    /// native threshold of the pill, so it is where a snap-back travels to.
    #[must_use]
    pub const fn grab_point(&self) -> Option<DragPoint> {
        self.grab_point
    }

    #[must_use]
    pub const fn pointer(&self) -> Option<DragPoint> {
        match self.phase {
            WorkspaceDragPhase::Dragging { pointer, .. }
            | WorkspaceDragPhase::TearArmed { pointer, .. } => Some(pointer),
            WorkspaceDragPhase::Idle | WorkspaceDragPhase::Armed { .. } => None,
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

/// The one sentence a *center* target explains itself with, wherever it is
/// offered.
///
/// The pointer preview and the command palette share this so the two never
/// drift: a center drop is a two-way exchange, not an insertion, and that is
/// the only thing about it a user cannot see from the highlight alone. Edge
/// zones return nothing, which is what keeps the wording off them.
#[must_use]
pub fn swap_hint(zone: WorkspaceDropZone, target_name: Option<&str>) -> Option<String> {
    if zone != WorkspaceDropZone::Center {
        return None;
    }
    Some(swap_hint_text(target_name))
}

/// The center-target sentence for a target named `target_name`, falling back to
/// generic wording when the name is missing or blank.
#[must_use]
pub fn swap_hint_text(target_name: Option<&str>) -> String {
    target_name.map(str::trim).filter(|name| !name.is_empty()).map_or_else(
        || "Swap places with this workspace".to_owned(),
        |name| format!("Swap places with {name}"),
    )
}

/// Lower one resolved zone onto the structural operation a cross-window move
/// requests. The four edges insert; the center exchanges.
#[must_use]
pub const fn move_operation_for(zone: WorkspaceDropZone) -> WorkspaceMoveOperation {
    match zone {
        WorkspaceDropZone::Left => {
            WorkspaceMoveOperation::InsertAtEdge { edge: WorkspaceTreeEdge::Left }
        }
        WorkspaceDropZone::Right => {
            WorkspaceMoveOperation::InsertAtEdge { edge: WorkspaceTreeEdge::Right }
        }
        WorkspaceDropZone::Top => {
            WorkspaceMoveOperation::InsertAtEdge { edge: WorkspaceTreeEdge::Top }
        }
        WorkspaceDropZone::Bottom => {
            WorkspaceMoveOperation::InsertAtEdge { edge: WorkspaceTreeEdge::Bottom }
        }
        WorkspaceDropZone::Center => WorkspaceMoveOperation::Swap,
    }
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

/// Which way a zone highlight is animating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneFade {
    /// A newly actionable zone appearing under the pointer.
    In,
    /// The zone the pointer just left, painted until its fade elapses.
    Out,
}

/// Opacity multiplier for a zone highlight at animation `progress`.
///
/// At `progress == 1.0` a fade-in is fully opaque and a fade-out is fully
/// transparent, which is exactly what the zero-duration path paints on its
/// first frame — the end state is identical either way.
#[must_use]
pub fn zone_fade_opacity(fade: ZoneFade, progress: f32) -> f32 {
    let progress = progress.clamp(0.0, 1.0);
    match fade {
        ZoneFade::In => progress,
        ZoneFade::Out => 1.0 - progress,
    }
}

/// One frame of the ghost's post-release travel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GhostTravelFrame {
    pub x: f32,
    pub y: f32,
    pub opacity: f32,
}

/// Interpolate the ghost from its release point to where the gesture ended:
/// the committed zone for a settle, the grab point for a snap-back.
///
/// The travel is purely visual — the tree edit already happened at release —
/// so the end state at `progress == 1.0` is an invisible ghost parked on `to`.
#[must_use]
pub fn ghost_travel_frame(from: DragPoint, to: DragPoint, progress: f32) -> GhostTravelFrame {
    let progress = progress.clamp(0.0, 1.0);
    GhostTravelFrame {
        x: (to.x - from.x).mul_add(progress, from.x),
        y: (to.y - from.y).mul_add(progress, from.y),
        opacity: 1.0 - progress,
    }
}

/// Painted width and height of the pill ghost.
pub const GHOST_SIZE: (f32, f32) = (160.0, 28.0);

/// Top-left the ghost paints at for a pointer at `point`, kept inside `bounds`.
///
/// The post-release travel starts and ends on ghost origins rather than raw
/// pointer positions, so the animated pill leaves exactly where it was.
#[must_use]
pub fn ghost_origin(point: DragPoint, bounds: Rect) -> DragPoint {
    let (width, height) = GHOST_SIZE;
    let (left, top) = (bounds.x + 4.0, bounds.y + 4.0);
    DragPoint {
        x: (point.x + 12.0).clamp(left, (bounds.x + bounds.width - width - 4.0).max(left)),
        y: (point.y + 12.0).clamp(top, (bounds.y + bounds.height - height - 4.0).max(top)),
    }
}

/// The ghost's purely visual travel after the gesture ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GhostTravel {
    pub workspace_id: WorkspaceId,
    pub from: DragPoint,
    pub to: DragPoint,
    started: Instant,
    travel: Duration,
}

/// Drag feedback that outlives the state which produced it: the zone highlight
/// the pointer just left, and the ghost's settle / snap-back travel.
///
/// Every entry point takes the *resolved* transition length, so the
/// reduced-motion path (`appearance.animations` off or `SCRIBE_DISABLE_ANIMATIONS`
/// set, both yielding [`Duration::ZERO`]) records nothing and paints nothing —
/// which is the identical end state these transitions animate towards.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WorkspaceDragMotion {
    shown_zone: Option<WorkspaceDropTarget>,
    fading_zone: Option<(WorkspaceDropTarget, Instant)>,
    ghost: Option<GhostTravel>,
}

impl WorkspaceDragMotion {
    /// Fold this frame's live preview in and answer which zone, if any, should
    /// still be painted fading out. `fade` is the resolved fade length.
    pub fn track_zone(
        &mut self,
        live: Option<WorkspaceDropTarget>,
        fade: Duration,
    ) -> Option<WorkspaceDropTarget> {
        if live != self.shown_zone {
            self.fading_zone = self.shown_zone.map(|zone| (zone, Instant::now()));
            self.shown_zone = live;
        }
        let (zone, started) = self.fading_zone?;
        if started.elapsed() >= fade {
            self.fading_zone = None;
            return None;
        }
        Some(zone)
    }

    /// Record the ghost travel a finished gesture leaves behind.
    pub fn begin_ghost_travel(
        &mut self,
        workspace_id: WorkspaceId,
        from: DragPoint,
        to: DragPoint,
        travel: Duration,
    ) {
        self.ghost = (travel > Duration::ZERO).then(|| GhostTravel {
            workspace_id,
            from,
            to,
            started: Instant::now(),
            travel,
        });
    }

    /// Workspace whose ghost owns the transient feedback after release.
    #[must_use]
    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.ghost.map(|ghost| ghost.workspace_id)
    }

    /// The ghost travel still on screen, dropped once its transition elapsed.
    pub fn ghost_travel(&mut self) -> Option<GhostTravel> {
        let ghost = self.ghost?;
        if ghost.started.elapsed() >= ghost.travel {
            self.ghost = None;
            return None;
        }
        Some(ghost)
    }

    /// Drop every transient layer, for a new gesture or a vanished workspace.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Input-to-paint stopwatch for one drag frame.
///
/// Inert unless [`DRAG_PROBE_ENV`] is truthy at construction: with the probe
/// off every entry point is a bool check that stores nothing and logs nothing.
#[derive(Debug, Clone, Copy)]
pub struct DragProbe {
    enabled: bool,
    ingested: Option<Instant>,
}

impl DragProbe {
    /// Resolve the probe from the process environment. Call once at startup.
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os(DRAG_PROBE_ENV)
                .as_deref()
                .is_some_and(crate::animation::env_is_truthy),
        )
    }

    const fn new(enabled: bool) -> Self {
        Self { enabled, ingested: None }
    }

    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }

    /// Stamp the moment a pointer event entered the drag state machine.
    pub fn ingest(&mut self) {
        if self.enabled {
            self.ingested = Some(Instant::now());
        }
    }

    /// Claim the pending stamp so the frame that paints the overlay can close
    /// it. A frame that paints no overlay simply drops the sample.
    pub fn take_ingested(&mut self) -> Option<Instant> {
        self.ingested.take()
    }
}

/// Emit one input-to-paint sample, called from the overlay's paint callback.
///
/// `tests/e2e` probe scripts parse `input_to_paint_us` out of the client log;
/// nothing is emitted unless [`DragProbe`] handed out a stamp.
pub fn record_input_to_paint(ingested: Instant) {
    let micros = u64::try_from(ingested.elapsed().as_micros()).unwrap_or(u64::MAX);
    tracing::info!(
        target: "scribe::drag_probe",
        input_to_paint_us = micros,
        "workspace drag overlay painted"
    );
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
    use std::ffi::OsStr;

    use super::*;
    use crate::animation::{AnimationSettings, MAX_TRANSITION};

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
    fn cancel_and_non_actionable_releases_never_commit() {
        let source = WorkspaceId::new();
        let mut cancelled = WorkspaceDrag::default();
        cancelled.arm(source);
        cancelled.cancel();
        assert_eq!(cancelled.phase(), WorkspaceDragPhase::Idle);
        assert_eq!(cancelled.release(), None);

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
    fn actionable_release_resets_then_next_drag_is_healthy() {
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
        assert_eq!(drag.phase(), WorkspaceDragPhase::Idle);

        drag.arm(target);
        assert_eq!(
            drag.phase(),
            WorkspaceDragPhase::Armed { source_workspace_id: target },
            "a completed drag cannot poison the next arm"
        );
    }

    #[test]
    fn both_reduced_motion_settings_select_zero_duration_under_the_shared_cap() {
        let motion = AnimationSettings::resolve_with_env(true, None);
        for requested in [ZONE_FADE, GHOST_TRAVEL] {
            assert!(requested <= MAX_TRANSITION, "{requested:?} exceeds the transition cap");
            assert_eq!(motion.duration(requested), requested);
        }

        for settings in [
            AnimationSettings::resolve_with_env(false, None),
            AnimationSettings::resolve_with_env(true, Some(OsStr::new("1"))),
        ] {
            assert_eq!(settings.duration(ZONE_FADE), Duration::ZERO);
            assert_eq!(settings.duration(GHOST_TRAVEL), Duration::ZERO);
        }
    }

    #[test]
    fn animated_frames_land_on_the_state_the_zero_duration_path_paints() {
        // GPUI resolves a zero-duration (or reduce-motion) animation to
        // progress 1.0 on its first frame, so the end frame *is* the static
        // state both settings paths must agree on.
        assert!((zone_fade_opacity(ZoneFade::In, 1.0) - 1.0).abs() <= f32::EPSILON);
        assert!(zone_fade_opacity(ZoneFade::Out, 1.0).abs() <= f32::EPSILON);
        assert!(zone_fade_opacity(ZoneFade::In, 0.0).abs() <= f32::EPSILON);

        let from = DragPoint { x: 10.0, y: 20.0 };
        let to = DragPoint { x: 110.0, y: 220.0 };
        assert_eq!(
            ghost_travel_frame(from, to, 1.0),
            GhostTravelFrame { x: to.x, y: to.y, opacity: 0.0 }
        );
        assert_eq!(
            ghost_travel_frame(from, to, 0.0),
            GhostTravelFrame { x: from.x, y: from.y, opacity: 1.0 }
        );
        assert_eq!(
            ghost_travel_frame(from, to, 0.5),
            GhostTravelFrame { x: 60.0, y: 120.0, opacity: 0.5 }
        );
    }

    #[test]
    fn zero_duration_records_no_transient_layer_while_motion_paints_one() {
        let workspace_id = WorkspaceId::new();
        let target = WorkspaceDropTarget { workspace_id, zone: WorkspaceDropZone::Left };
        let point = DragPoint { x: 1.0, y: 2.0 };

        let mut still = WorkspaceDragMotion::default();
        assert_eq!(still.track_zone(Some(target), Duration::ZERO), None);
        assert_eq!(still.track_zone(None, Duration::ZERO), None, "nothing fades out");
        still.begin_ghost_travel(workspace_id, point, point, Duration::ZERO);
        assert_eq!(still.ghost_travel(), None, "nothing travels");

        let mut animated = WorkspaceDragMotion::default();
        assert_eq!(animated.track_zone(Some(target), ZONE_FADE), None);
        assert_eq!(animated.track_zone(None, ZONE_FADE), Some(target), "the left zone fades out");
        animated.begin_ghost_travel(workspace_id, point, point, GHOST_TRAVEL);
        assert_eq!(animated.ghost_travel().map(|travel| travel.workspace_id), Some(workspace_id));

        animated.clear();
        assert_eq!(animated, WorkspaceDragMotion::default());
    }

    #[test]
    fn the_drag_probe_stays_inert_until_its_env_switch_is_set() {
        let mut off = DragProbe::new(false);
        off.ingest();
        assert_eq!(off.take_ingested(), None);

        let mut on = DragProbe::new(true);
        on.ingest();
        assert!(on.take_ingested().is_some());
        assert_eq!(on.take_ingested(), None, "one sample closes one paint");
    }

    // @lat: [[test#Test Harness#GPUI Workspace Drag]]
    #[test]
    fn only_center_targets_explain_themselves_and_every_edge_stays_silent() {
        for zone in [
            WorkspaceDropZone::Left,
            WorkspaceDropZone::Right,
            WorkspaceDropZone::Top,
            WorkspaceDropZone::Bottom,
        ] {
            assert_eq!(swap_hint(zone, Some("Docs")), None, "{zone:?} is an insertion");
            assert_eq!(swap_hint(zone, None), None);
        }
        assert_eq!(
            swap_hint(WorkspaceDropZone::Center, Some("Docs")),
            Some("Swap places with Docs".to_owned())
        );
        // A blank or whitespace name is not a usable name, so both fall back to
        // the same generic sentence a nameless region gets.
        let generic = swap_hint(WorkspaceDropZone::Center, None);
        assert_eq!(generic, Some("Swap places with this workspace".to_owned()));
        assert_eq!(swap_hint(WorkspaceDropZone::Center, Some("   ")), generic);
        // The palette entry text and the pointer preview text are one string.
        assert_eq!(swap_hint_text(Some("Docs")), "Swap places with Docs");
        assert_eq!(swap_hint_text(None), generic.unwrap());
    }

    #[test]
    fn every_zone_lowers_onto_its_own_move_operation() {
        use scribe_common::protocol::{WorkspaceMoveOperation, WorkspaceTreeEdge};
        for (zone, edge) in [
            (WorkspaceDropZone::Left, WorkspaceTreeEdge::Left),
            (WorkspaceDropZone::Right, WorkspaceTreeEdge::Right),
            (WorkspaceDropZone::Top, WorkspaceTreeEdge::Top),
            (WorkspaceDropZone::Bottom, WorkspaceTreeEdge::Bottom),
        ] {
            assert_eq!(move_operation_for(zone), WorkspaceMoveOperation::InsertAtEdge { edge });
        }
        assert_eq!(move_operation_for(WorkspaceDropZone::Center), WorkspaceMoveOperation::Swap);
    }

    #[test]
    fn the_ghost_stays_inside_the_window_it_is_painted_in() {
        let bounds = Rect { x: 0.0, y: 0.0, width: 200.0, height: 100.0 };
        let (width, height) = GHOST_SIZE;
        assert_eq!(
            ghost_origin(DragPoint { x: 4.0, y: 4.0 }, bounds),
            DragPoint { x: 16.0, y: 16.0 }
        );
        assert_eq!(
            ghost_origin(DragPoint { x: 400.0, y: 400.0 }, bounds),
            DragPoint { x: bounds.width - width - 4.0, y: bounds.height - height - 4.0 }
        );
        assert_eq!(
            ghost_origin(DragPoint { x: -40.0, y: -40.0 }, bounds),
            DragPoint { x: 4.0, y: 4.0 }
        );
    }
}
