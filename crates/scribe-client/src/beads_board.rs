//! Constellation workspace board: compact five-column Beads state for GPUI.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, FontWeight, MouseButton, Rgba, Role, SharedString, div, linear_color_stop,
    linear_gradient, prelude::*, px, uniform_list,
};

use crate::layout::Rect;
use crate::opacity::surface;
use scribe_common::ids::WorkspaceId;
use scribe_common::protocol::{BeadsBoardItem, BeadsBoardSnapshot, BeadsBoardState};

/// One of the things whose hover keeps a board open. They overlap, so each is
/// tracked on its own rather than as a single flag.
#[derive(Clone, Copy)]
pub enum HoverSource {
    /// The workspace's bead in the titlebar or a region bar.
    Bead = 1,
    /// The board itself.
    Board = 2,
    /// A control inside the board, which takes hover away from the board.
    Control = 4,
}

/// Reader-owned snapshots plus GPUI-owned hover/pin intent.
///
/// Every one of these is keyed by workspace because a board is a region's own
/// furniture: two regions side by side each own a board, and pinning or
/// hovering one says nothing about the other.
#[derive(Debug, Clone, Default)]
pub struct BeadsBoards {
    states: HashMap<WorkspaceId, BeadsBoardState>,
    retry_after: HashMap<WorkspaceId, Instant>,
    /// Which of a board's hover sources the pointer is on, per workspace.
    /// An entry with no sources left is a board inside its grace period.
    hovered: HashMap<WorkspaceId, u8>,
    pinned: HashSet<WorkspaceId>,
    /// Pins read back from the window record, held until their region shows
    /// up. A restored pin names a workspace the layout has not adopted yet, and
    /// pruning against the live layout would drop it before it could apply.
    pending_pins: HashSet<WorkspaceId>,
    hover_expires: HashMap<WorkspaceId, Instant>,
    /// Text a card asked to put on the clipboard, drained by the view on the
    /// next frame. The board is built by a free function with no reach into
    /// the window's clipboard handle, so the request is parked here the way
    /// hover and pin intent already are.
    pending_copy: Option<String>,
    /// Steps away from the board's designed text size, one tenth each. Held as
    /// steps rather than a factor so the default stays a plain `Default`.
    text_scale_steps: i8,
    /// Boards dragged off the designed height, by workspace. Per workspace and
    /// not per window like the text size, because the strip a pinned board
    /// takes comes out of that region's terminal and no other's.
    heights: HashMap<WorkspaceId, f32>,
    /// The bottom-bar drag in flight, if any.
    resize: Option<BoardResize>,
}

/// One board's bottom bar, held by the pointer.
///
/// The press position and the height it started from are both kept so the drag
/// stays a delta: a pointer that outruns a frame still resolves to the height
/// the gesture asked for rather than to wherever it was last sampled.
#[derive(Debug, Clone, Copy)]
struct BoardResize {
    workspace_id: WorkspaceId,
    press_y: f32,
    from_height: f32,
}

impl BeadsBoards {
    pub fn update(&mut self, workspace_id: WorkspaceId, state: BeadsBoardState) {
        if !matches!(state, BeadsBoardState::Unavailable { .. }) {
            self.retry_after.remove(&workspace_id);
        }
        self.states.insert(workspace_id, state);
    }

    pub fn detected(&self, workspace_id: WorkspaceId) -> bool {
        matches!(
            self.states.get(&workspace_id),
            Some(BeadsBoardState::Loading { cached: Some(_) } | BeadsBoardState::Ready { .. })
        )
    }

    pub fn state(&self, workspace_id: WorkspaceId) -> Option<&BeadsBoardState> {
        self.states.get(&workspace_id)
    }

    pub fn needs_refresh(&self, workspace_id: WorkspaceId, max_age: Duration) -> bool {
        let Some(state) = self.states.get(&workspace_id) else { return true };
        match state {
            BeadsBoardState::Ready { snapshot, stale, .. } => {
                let now: u64 = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                *stale
                    || Duration::from_millis(now.saturating_sub(snapshot.refreshed_at_epoch_ms))
                        >= max_age
            }
            BeadsBoardState::Loading { .. } => false,
            BeadsBoardState::NotDetected | BeadsBoardState::Unavailable { .. } => true,
        }
    }

    pub fn due_retry(&mut self, after: Duration) -> Option<WorkspaceId> {
        let now = Instant::now();
        let workspace_id = self.states.iter().find_map(|(workspace_id, state)| {
            matches!(state, BeadsBoardState::Unavailable { .. })
                .then_some(*workspace_id)
                .filter(|id| self.retry_after.get(id).is_none_or(|deadline| *deadline <= now))
        })?;
        self.retry_after.insert(workspace_id, now + after);
        Some(workspace_id)
    }

    /// Every board painting this frame, each with whether it is pinned.
    ///
    /// One entry per workspace, never one per window: each region paints its
    /// own board, so hovering one region's bead while another region's board
    /// is pinned shows both, and focus enters into none of it.
    pub fn visible(&self) -> Vec<(WorkspaceId, bool)> {
        self.pinned
            .iter()
            .map(|workspace_id| (*workspace_id, true))
            .chain(
                self.hovered
                    .keys()
                    .filter(|workspace_id| !self.pinned.contains(workspace_id))
                    .map(|workspace_id| (*workspace_id, false)),
            )
            .collect()
    }

    /// Ask for `text` to be put on the clipboard.
    pub fn copy(&mut self, text: String) {
        self.pending_copy = Some(text);
    }

    /// Take the copy a card asked for, if any.
    pub fn take_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }

    /// How much bigger or smaller than designed the board's text is.
    pub fn text_scale(&self) -> f32 {
        1.0 + f32::from(self.text_scale_steps) * TEXT_SCALE_STEP
    }

    /// Nudge every board's text size, clamped to what the fixed-height strip
    /// can still show a readable row in.
    pub fn adjust_text_scale(&mut self, steps: i8) {
        self.text_scale_steps =
            (self.text_scale_steps + steps).clamp(MIN_TEXT_SCALE_STEPS, MAX_TEXT_SCALE_STEPS);
    }

    /// How tall `workspace_id`'s board paints — and, while it is pinned, how
    /// much of its region it reserves.
    pub fn height(&self, workspace_id: WorkspaceId) -> f32 {
        self.heights.get(&workspace_id).copied().unwrap_or(BEADS_BOARD_HEIGHT)
    }

    /// Grab `workspace_id`'s bottom bar at `y`, in the coordinates the drag
    /// will be reported in.
    pub fn start_resize(&mut self, workspace_id: WorkspaceId, y: f32) {
        self.resize =
            Some(BoardResize { workspace_id, press_y: y, from_height: self.height(workspace_id) });
    }

    /// The board whose bar the pointer is holding, if any.
    pub fn resizing(&self) -> Option<WorkspaceId> {
        self.resize.map(|drag| drag.workspace_id)
    }

    /// Take the drag to `y`, keeping the board between one readable issue row
    /// and `max`. Reports whether the height actually moved.
    pub fn resize_to(&mut self, y: f32, max: f32) -> bool {
        let Some(drag) = self.resize else { return false };
        let floor = self.min_height();
        let height = (drag.from_height + y - drag.press_y).clamp(floor, max.max(floor));
        let moved = (height - self.height(drag.workspace_id)).abs() > f32::EPSILON;
        self.heights.insert(drag.workspace_id, height);
        moved
    }

    /// Let go of the bar, reporting whether a drag was in flight.
    pub fn end_resize(&mut self) -> bool {
        self.resize.take().is_some()
    }

    /// The shortest board that still shows a lane head with one issue under it.
    fn min_height(&self) -> f32 {
        Metrics { scale: self.text_scale(), height: 0.0 }.min_height()
    }

    /// Whether `workspace_id`'s board is pinned open.
    pub fn is_pinned(&self, workspace_id: WorkspaceId) -> bool {
        self.pinned.contains(&workspace_id)
    }

    /// Pin or unpin one region's board, leaving every other region alone.
    pub fn toggle_pin(&mut self, workspace_id: WorkspaceId) {
        if !self.pinned.remove(&workspace_id) {
            self.pinned.insert(workspace_id);
        }
    }

    /// Every pinned board, in a stable order so a caller comparing this
    /// against a persisted list sees a change only when one really happened.
    pub fn pinned(&self) -> Vec<WorkspaceId> {
        let mut pinned: Vec<WorkspaceId> = self.pinned.iter().copied().collect();
        pinned.sort_by_key(WorkspaceId::as_uuid);
        pinned
    }

    /// Take the pins a previous run of this window left behind. They apply as
    /// each named region appears.
    pub fn restore_pins(&mut self, pinned: impl IntoIterator<Item = WorkspaceId>) {
        self.pending_pins.extend(pinned);
    }

    /// Drop every workspace this window no longer shows a region for.
    ///
    /// Reconciled against the live layout rather than hooked onto each path
    /// that can close a region: a leaked entry would keep re-requesting a board
    /// for a workspace nobody can see, every thirty seconds, forever.
    pub fn retain_regions(&mut self, live: &HashSet<WorkspaceId>) {
        for workspace_id in live {
            if self.pending_pins.remove(workspace_id) {
                self.pinned.insert(*workspace_id);
            }
        }
        self.states.retain(|workspace_id, _| live.contains(workspace_id));
        self.retry_after.retain(|workspace_id, _| live.contains(workspace_id));
        self.hover_expires.retain(|workspace_id, _| live.contains(workspace_id));
        self.hovered.retain(|workspace_id, _| live.contains(workspace_id));
        self.pinned.retain(|workspace_id| live.contains(workspace_id));
        self.heights.retain(|workspace_id, _| live.contains(workspace_id));
    }

    /// Report the pointer entering or leaving one of the things that keep
    /// `workspace_id`'s board open.
    ///
    /// Sources are tracked separately because they overlap and report out of
    /// order: a control inside the board takes the hover from the board, which
    /// then reports a leave it never had. Only when the last source is gone
    /// does the board start closing, and even then a grace period covers the
    /// gap the pointer crosses on its way from the bead.
    pub fn hover(&mut self, workspace_id: WorkspaceId, source: HoverSource, hovered: bool) {
        let sources = self.hovered.entry(workspace_id).or_default();
        if hovered {
            *sources |= source as u8;
        } else {
            *sources &= !(source as u8);
        }
        if *sources == 0 {
            self.hover_expires.insert(workspace_id, Instant::now() + Duration::from_millis(150));
        } else {
            self.hover_expires.remove(&workspace_id);
        }
    }

    pub fn expire_hover(&mut self) -> bool {
        let now = Instant::now();
        let held = self.resizing();
        let due: Vec<WorkspaceId> = self
            .hover_expires
            .iter()
            .filter(|(_, deadline)| now >= **deadline)
            .map(|(workspace_id, _)| *workspace_id)
            // A drag of the bottom bar takes the pointer off the board it is
            // resizing, and closing the board mid-gesture would end the drag.
            .filter(|workspace_id| held != Some(*workspace_id))
            .collect();
        for workspace_id in &due {
            self.hovered.remove(workspace_id);
            self.hover_expires.remove(workspace_id);
        }
        !due.is_empty()
    }
}

/// Height a board opens at, shared by the paint and by the region reservation
/// a pinned board makes, so the two cannot disagree. A drag of the bottom bar
/// moves it for that one workspace.
pub const BEADS_BOARD_HEIGHT: f32 = 197.0;

/// How far either side of the bottom bar counts as grabbing it, matching the
/// tolerance a pane divider gives its own one-pixel line.
pub const BEADS_BOARD_GRIP: f32 = 4.0;

/// The board's palette, derived from the live theme.
///
/// The board takes its structure, weights and alphas from
/// `.impeccable/mocks/beads-compact-live-overview.html`, but the mock's fixed
/// colours are read off the theme instead so a board belongs to whatever
/// palette the terminal is wearing. Queue states take the ANSI colours their
/// meaning already implies — ready cyan, in progress blue, blocked red, done
/// green — which every theme defines.
///
/// Where the mock lays its issues on the bare ground, an issue here is a raised
/// card, so the palette carries two grounds: the strip's, which the lane heads
/// sit on, and the card's, which every word inside an issue sits on.
#[derive(Debug, Clone, Copy)]
pub struct BeadsBoardColors {
    /// The strip's ground, already composited with the window's opacity.
    pub ground: Rgba,
    /// A card's fill, lit from the top: the pair are the two ends of its
    /// gradient, and the second is the flat colour the card reads as.
    pub card_top: Rgba,
    pub card: Rgba,
    pub card_hover_top: Rgba,
    pub card_hover: Rgba,
    pub card_border: Rgba,
    pub card_border_hover: Rgba,
    pub title: Rgba,
    pub queue_name: Rgba,
    pub queue_name_active: Rgba,
    pub muted: Rgba,
    pub hairline: Rgba,
    pub chevron: Rgba,
    pub button_hover: Rgba,
    pub epic: Rgba,
    pub backlog_state: Rgba,
    pub ready_state: Rgba,
    pub progress_state: Rgba,
    pub blocked_state: Rgba,
    pub done_state: Rgba,
    /// P0 through P4, hottest first.
    pub priorities: [Rgba; 5],
}

impl BeadsBoardColors {
    /// Derive the board's palette from the chrome slots and the ANSI ramp.
    #[must_use]
    pub fn from_theme(
        chrome: &scribe_common::theme::ChromeColors,
        ansi: &[[f32; 4]; 16],
        opacity: f32,
    ) -> Self {
        let ground = surface(chrome.tab_bar_bg, opacity);
        // Read against the ground as it is actually seen: the strip may be
        // translucent, but the text sits on what the eye reads as its ground.
        let ink = Rgba { a: 1.0, ..ground };
        // Elevation is light, in every theme: a raised card is the ground
        // carried toward white, which on a pale theme lands on the white a
        // paper card would be and leaves its border and its shadow to carry
        // the lift.
        let card = mix(ink, WHITE, 0.055);
        let card_hover = mix(ink, WHITE, 0.105);
        // A border has to darken on a pale theme and lighten on a dark one, so
        // it moves toward whichever end the ground is not.
        let border_target = if luminance(ink) < 0.5 { WHITE } else { BLACK };
        // Text clears a contrast floor against the ground it is read on, and
        // the two grounds pull opposite ways: the card is lighter, which is
        // the worse case for a dark theme's pale ink and the better one for a
        // pale theme's dark ink. So every word is measured on both. The mock's
        // tones are relative to the mock's own ground, and a theme whose muted
        // slot or ANSI red is close to its background reproduces the ratio,
        // not the legibility: alpha-reducing an already-dim slot is what made
        // blockers and ids unreadable.
        let anywhere = |color: Rgba| readable_anywhere(color, ink, card, BODY_CONTRAST);
        let text = anywhere(slot(chrome.tab_text_active));
        let muted = anywhere(slot(chrome.tab_text));
        let hairline = slot(chrome.tab_separator);
        let blocked = anywhere(slot(ansi[BRIGHT_RED]));
        // P0 takes the more saturated of the theme's two reds, and P1 is that
        // same red pulled toward the neutral. Derived from each other rather
        // than from a slot each, because lifting a colour to clear the floor
        // washes it out, and two slots lifted by different amounts can come
        // out ranked either way round.
        // A heat scale, because that is what a priority is: red, then amber,
        // then yellow, then out. Every step is a different hue rather than the
        // same red at three strengths, which is what a reader has to tell
        // apart at a glance and across a wash this faint.
        let vivid = |plain: Rgba, bright: Rgba| {
            if vividness(plain) >= vividness(bright) { plain } else { bright }
        };
        let critical = vivid(slot(ansi[RED]), slot(ansi[BRIGHT_RED]));
        let caution = vivid(slot(ansi[YELLOW]), slot(ansi[BRIGHT_YELLOW]));
        // No terminal palette carries an amber, so it is mixed from the two it
        // sits between, which keeps it in this theme's own reds and yellows.
        let high = mix(critical, caution, 0.45);
        let progress = slot(ansi[BRIGHT_BLUE]);
        let epic = anywhere(mix(slot(ansi[BRIGHT_MAGENTA]), muted, 0.45));
        Self {
            ground,
            card_top: mix(card, WHITE, 0.03),
            card,
            card_hover_top: mix(card_hover, WHITE, 0.03),
            card_hover,
            card_border: mix(card, border_target, 0.1),
            card_border_hover: mix(card, border_target, 0.22),
            title: text,
            // The mock keeps three steps of brightness across the strip: the
            // issue title, a queue name, and the lane the eye should land on.
            // The steps are taken between the floor and the ground, never
            // below the floor.
            queue_name: mix(text, muted, 0.35),
            queue_name_active: text,
            muted,
            hairline,
            // Marks rather than text, so they clear the lower floor a
            // non-text element needs.
            chevron: readable(muted, ink, MARK_CONTRAST),
            button_hover: alpha(text, 0.08),
            // An epic is a grouping label, not another muted field: it
            // takes the one ANSI hue no queue or priority has claimed, pulled
            // most of the way to muted so it stays quiet beside the id.
            epic,
            backlog_state: readable(muted, ink, MARK_CONTRAST),
            ready_state: readable(slot(ansi[BRIGHT_CYAN]), ink, MARK_CONTRAST),
            progress_state: readable(progress, ink, MARK_CONTRAST),
            blocked_state: readable(blocked, ink, MARK_CONTRAST),
            done_state: readable(slot(ansi[BRIGHT_GREEN]), ink, MARK_CONTRAST),
            priorities: [
                // A theme carries two of each hue and either can be the pale
                // one, so each step takes the more saturated: a washed-out
                // pink says less than a deep red however light it is, and the
                // ranking cannot be left to which slot a theme happened to
                // fill.
                anywhere(critical),
                anywhere(high),
                anywhere(caution),
                // Off the scale: the last two carry no heat, so they carry no
                // hue either.
                muted,
                // Stepped toward the ground for the hierarchy, then lifted
                // back if that step took it through the floor.
                anywhere(mix(muted, ground, 0.25)),
            ],
        }
    }

    /// A queue's colour as words rather than as a mark: the totals sit on the
    /// board's own ground, so they clear the floor a reader needs, not the
    /// lower one a dot gets away with.
    fn count_ink(&self, state: Rgba) -> Rgba {
        readable(state, Rgba { a: 1.0, ..self.ground }, BODY_CONTRAST)
    }

    fn priority(&self, priority: u8) -> Rgba {
        self.priorities.get(usize::from(priority)).copied().unwrap_or(self.muted)
    }

    /// The wash behind a priority and the ink that stays readable on it.
    ///
    /// The wash weakens as the rank falls, so the hierarchy survives a theme
    /// whose reds are close: P0 is both the most saturated colour on the card
    /// and the strongest mark, and neither depends on the other.
    fn priority_mark(&self, priority: u8) -> PriorityMark {
        let color = self.priority(priority);
        let weight = PRIORITY_WEIGHTS
            .get(usize::from(priority))
            .copied()
            .unwrap_or(PRIORITY_FAINTEST_WEIGHT);
        let (floor, ceiling) = PRIORITY_TINT_RANGE;
        let tint = (weight / reach(color, self.card).max(0.02)).clamp(floor, ceiling);
        let fill = mix(self.card, color, (tint * BADGE_OF_WASH).clamp(0.2, 1.0));
        // A filled badge is read the other way round from everything else on
        // the card: the colour is the ground, so the digits take whichever end
        // of the range that ground is not.
        let on_fill = if luminance(fill) < 0.5 { WHITE } else { BLACK };
        PriorityMark { ink: readable(on_fill, fill, BODY_CONTRAST), fill }
    }
}

/// One priority's mark: the wash laid behind it, and the ink for its digits.
#[derive(Clone, Copy)]
struct PriorityMark {
    /// The digits' colour, which is read on the badge rather than on the card.
    ink: Rgba,
    /// The badge's own fill, laid over the card at a strength solved from the
    /// rank, so a hue near the card reads as strongly as one far from it.
    fill: Rgba,
}

/// How far a colour sits from a ground, averaged over the channels: how much
/// of itself a wash of it lays down.
fn reach(color: Rgba, ground: Rgba) -> f32 {
    ((color.r - ground.r).abs() + (color.g - ground.g).abs() + (color.b - ground.b).abs()) / 3.0
}

/// How saturated a colour is, which is what makes it read as urgent: a washed
/// out pink says less than a deep red however light it is.
fn vividness(color: Rgba) -> f32 {
    let high = color.r.max(color.g).max(color.b);
    let low = color.r.min(color.g).min(color.b);
    if high <= 0.0 { 0.0 } else { (high - low) / high }
}

/// Contrast a colour carrying words must reach against the board's ground,
/// and the lower one a dot, a chevron, or a hairline-thin mark needs. The text
/// floor is WCAG AA for body text; marks carry no reading load, so holding
/// them to it would only wash the queue colours out.
const BODY_CONTRAST: f32 = 4.5;
const MARK_CONTRAST: f32 = 3.0;

const RED: usize = 1;
const YELLOW: usize = 3;
const BRIGHT_RED: usize = 9;
const BRIGHT_GREEN: usize = 10;
const BRIGHT_YELLOW: usize = 11;
const BRIGHT_MAGENTA: usize = 13;
const BRIGHT_BLUE: usize = 12;
const BRIGHT_CYAN: usize = 14;

const WHITE: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
const BLACK: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

const LANE_HEAD_HEIGHT: f32 = 36.0;
/// One issue's share of a lane: the card itself, then the gap that separates it
/// from the next.
const ISSUE_HEIGHT: f32 = 50.0;
const CARD_GAP: f32 = 4.0;
const CARD_RADIUS: f32 = 4.0;
const RAIL_TOP: f32 = 17.0;
/// Where the text-size controls sit in the strip's top right, and how wide
/// they run: two square buttons with a gap between them.
const SCALE_BUTTON: f32 = 15.0;
const SCALE_CONTROLS_RIGHT: f32 = 6.0;
const SCALE_CONTROLS_GAP: f32 = 3.0;
/// The rail's own right inset. It stops a clear gap short of the controls
/// rather than running under them, which is what the extra eight pixels are.
const RAIL_RIGHT: f32 = SCALE_CONTROLS_RIGHT + 2.0 * SCALE_BUTTON + SCALE_CONTROLS_GAP + 8.0;
const QUEUE_LINE_HEIGHT: f32 = 20.0;
const LANES_BOTTOM_PAD: f32 = 7.0;
const CHEVRON_GUTTER: f32 = 3.0;

/// How much colour each rank's badge carries, as a mean channel distance from
/// the card it sits on. P0 through P4.
///
/// The tint is solved for this rather than fixed, because a hue far from the
/// card needs less of itself to make the same mark than one that sits near it:
/// at one fixed tint a vivid yellow at P2 out-shouts a dulled red at P1, which
/// is a ranking the reader can see and the ramp did not intend.
const PRIORITY_WEIGHTS: [f32; 5] = [0.105, 0.078, 0.058, 0.042, 0.032];
const PRIORITY_FAINTEST_WEIGHT: f32 = 0.032;
/// What a solved tint may not go past: a colour sitting almost on the card
/// would otherwise be asked for a wash stronger than itself.
const PRIORITY_TINT_RANGE: (f32, f32) = (0.04, 0.5);
/// How much more of a colour a badge carries than a broad wash of the same
/// rank. Scaled so the hottest rank lands on the colour itself and the coolest
/// keeps a fill it can still be read against.
const BADGE_OF_WASH: f32 = 3.0;
/// The card's own top padding.
const CARD_PAD_TOP: f32 = 6.0;
/// The title's line box, shared by the badge beside it so both sit on one
/// line.
const TITLE_LINE: f32 = 17.0;

/// The wash that zones a lane's column, and the halo behind its node. Both are
/// laid translucent so the rail they cross still shows through.
const LANE_WASH: f32 = 0.05;
const NODE_HALO: f32 = 0.12;
const NODE_HALO_ACTIVE: f32 = 0.2;

const TEXT_SCALE_STEP: f32 = 0.1;
const MIN_TEXT_SCALE_STEPS: i8 = -2;
const MAX_TEXT_SCALE_STEPS: i8 = 6;

/// Every board size at the current text scale, inside a strip of `height`.
///
/// The strip's outer height moves only when the bottom bar is dragged — a
/// pinned board reserved exactly that much from its region — so growing the
/// text takes the space out of the lane bodies rather than out of the terminal
/// below.
#[derive(Clone, Copy)]
struct Metrics {
    scale: f32,
    height: f32,
}

impl Metrics {
    fn at(self, designed: f32) -> gpui::Pixels {
        px(designed * self.scale)
    }

    fn head(self) -> f32 {
        LANE_HEAD_HEIGHT * self.scale
    }

    fn body(self) -> f32 {
        (self.height - self.head() - LANES_BOTTOM_PAD - 1.0).max(0.0)
    }

    /// The strip height at which the body is exactly one issue row, which is
    /// as short as a resize may take the board.
    fn min_height(self) -> f32 {
        self.head() + ISSUE_HEIGHT * self.scale + CHEVRON_GUTTER + LANES_BOTTOM_PAD + 1.0
    }

    fn issues(self) -> f32 {
        (self.body() - CHEVRON_GUTTER).max(0.0)
    }
}

pub struct BeadsBoardRender {
    /// The strip at the top of this workspace's region, in grid-band
    /// coordinates. The board is a region citizen, never a window-wide band:
    /// a window showing two regions side by side must keep each board over the
    /// terminal it describes.
    pub rect: Rect,
    /// Hovered boards float over the panes; a pinned board fills space its
    /// region already reserved for it.
    pub overlay: bool,
    pub hover_state: std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    pub workspace_id: WorkspaceId,
    /// Text scale shared by every board in this window.
    pub scale: f32,
    /// The live theme's board palette.
    pub colors: BeadsBoardColors,
}

/// One queue's column, as the mock lays it out.
struct Lane<'a> {
    name: &'static str,
    /// What this queue says when it holds nothing. Written per queue because
    /// an empty one means something different in each: no work waiting, none
    /// picked up, nothing held back, nothing finished yet.
    empty: &'static str,
    total: u32,
    items: &'a [BeadsBoardItem],
    /// This queue's items off a later snapshot: the lane body is virtualised,
    /// and its visible rows are built at layout time from the shared state
    /// rather than from the borrow the build frame held.
    queue: fn(&BeadsBoardSnapshot) -> &[BeadsBoardItem],
    /// The queue's colour, worn by its node and the rail beneath it.
    state: Rgba,
    /// The queues either side, which this lane's wash travels to meet.
    blend: LaneBlend,
    /// In progress is the lane the eye should land on.
    accent: LaneAccent,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum LaneAccent {
    None,
    Progress,
}

/// The queues either side of a lane, or nothing where the board ends.
#[derive(Clone, Copy)]
struct LaneBlend {
    left: Option<Rgba>,
    right: Option<Rgba>,
}

impl LaneBlend {
    /// A lane that has not been told its neighbours yet, which washes flat.
    const NONE: Self = Self { left: None, right: None };
}

/// Paint the compact live overview over the top of its own region.
pub fn render(
    workspace_name: &str,
    state: Option<&BeadsBoardState>,
    wiring: BeadsBoardRender,
) -> AnyElement {
    let BeadsBoardRender { rect, overlay, hover_state, workspace_id, scale, colors } = wiring;
    let colors = &colors;
    // The rect is the strip the region gave the board, already clamped to what
    // that region has: painting to it rather than to a height of its own is
    // what keeps a dragged board from hanging past its own terminal.
    let metrics = Metrics { scale, height: rect.height };
    let (snapshot, status) = board_content(state);
    let board = div()
        .id(SharedString::from(format!("beads-board-{workspace_id}")))
        .aria_label(format!("{workspace_name} Beads overview"))
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width))
        .h(px(rect.height))
        .flex()
        .flex_col()
        .bg(colors.ground)
        .border_b_1()
        .border_color(colors.hairline)
        .on_hover({
            let hover_state = std::sync::Arc::clone(&hover_state);
            move |hovered: &bool, _window, _app| {
                if let Ok(mut boards) = hover_state.lock() {
                    boards.hover(workspace_id, HoverSource::Board, *hovered);
                }
            }
        })
        .child(text_size_controls(&hover_state, workspace_id, colors));
    let board = match snapshot {
        Some(snapshot) => board.child(lanes(snapshot, workspace_id, &hover_state, colors, metrics)),
        // The mock draws no empty, loading, or unavailable state, so those keep
        // the one line of copy the board has always shown for them.
        None => board.child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(metrics.at(12.0))
                .text_color(colors.muted)
                .child(status),
        ),
    };
    // A hovered board floats over live panes and needs the lift to read as
    // separate; a pinned one sits in space the region gave up for it.
    if overlay { board.shadow_lg().into_any_element() } else { board.into_any_element() }
}

/// The board's own text-size control, parked in the strip's top right corner.
///
/// Sized and coloured from the board's tokens rather than the chrome's, and
/// opaque so it breaks the rail behind it the way a queue line does.
fn text_size_controls(
    state: &std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    workspace_id: WorkspaceId,
    colors: &BeadsBoardColors,
) -> AnyElement {
    div()
        .absolute()
        .right(px(SCALE_CONTROLS_RIGHT))
        .top(px(7.0))
        .flex()
        .gap(px(SCALE_CONTROLS_GAP))
        .bg(colors.ground)
        .child(scale_button(state, workspace_id, colors, ScaleStep::Larger))
        .child(scale_button(state, workspace_id, colors, ScaleStep::Smaller))
        .into_any_element()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScaleStep {
    Smaller,
    Larger,
}

impl ScaleStep {
    fn steps(self) -> i8 {
        match self {
            Self::Smaller => -1,
            Self::Larger => 1,
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Smaller => "\u{2212}",
            Self::Larger => "+",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Smaller => "Smaller board text",
            Self::Larger => "Larger board text",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Smaller => "smaller",
            Self::Larger => "larger",
        }
    }
}

fn scale_button(
    state: &std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    workspace_id: WorkspaceId,
    colors: &BeadsBoardColors,
    step: ScaleStep,
) -> AnyElement {
    let state = std::sync::Arc::clone(state);
    div()
        .id(SharedString::from(format!("beads-text-{}-{workspace_id}", step.key())))
        .role(Role::Button)
        .aria_label(step.label())
        .size(px(SCALE_BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.0))
        .bg(colors.ground)
        .border_1()
        .border_color(colors.hairline)
        .font_family("monospace")
        .text_size(px(10.0))
        .line_height(px(10.0))
        .text_color(colors.muted)
        .cursor_pointer()
        .hover(|button| button.bg(colors.button_hover).text_color(colors.title))
        .on_hover({
            let state = std::sync::Arc::clone(&state);
            move |hovered: &bool, _window, _app| {
                if let Ok(mut boards) = state.lock() {
                    boards.hover(workspace_id, HoverSource::Control, *hovered);
                }
            }
        })
        // The grid below owns the pointer for selection, so the press stops
        // here rather than starting a drag in the terminal behind the board.
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, _app| {
            if let Ok(mut boards) = state.lock() {
                boards.adjust_text_scale(step.steps());
            }
            window.refresh();
        })
        .child(step.glyph())
        .into_any_element()
}

fn lanes(
    snapshot: &BeadsBoardSnapshot,
    workspace_id: WorkspaceId,
    state: &std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    colors: &BeadsBoardColors,
    metrics: Metrics,
) -> AnyElement {
    let mut specs = [
        Lane {
            name: "Backlog",
            empty: "Empty",
            total: snapshot.backlog_total,
            items: &snapshot.backlog,
            queue: |board| &board.backlog,
            state: colors.backlog_state,
            blend: LaneBlend::NONE,
            accent: LaneAccent::None,
        },
        Lane {
            name: "Ready",
            empty: "None ready",
            total: snapshot.ready_total,
            items: &snapshot.ready,
            queue: |board| &board.ready,
            state: colors.ready_state,
            blend: LaneBlend::NONE,
            accent: LaneAccent::None,
        },
        Lane {
            name: "In progress",
            empty: "Idle",
            total: snapshot.in_progress_total,
            items: &snapshot.in_progress,
            queue: |board| &board.in_progress,
            state: colors.progress_state,
            blend: LaneBlend::NONE,
            accent: LaneAccent::Progress,
        },
        Lane {
            name: "Blocked",
            empty: "Clear",
            total: snapshot.blocked_total,
            items: &snapshot.blocked,
            queue: |board| &board.blocked,
            state: colors.blocked_state,
            blend: LaneBlend::NONE,
            accent: LaneAccent::None,
        },
        Lane {
            name: "Done",
            empty: "None yet",
            total: snapshot.done_total,
            items: &snapshot.done,
            queue: |board| &board.done,
            state: colors.done_state,
            blend: LaneBlend::NONE,
            accent: LaneAccent::None,
        },
    ];
    // Each lane's wash reaches its neighbours' colours at the two boundaries it
    // owns, so the queue it hands over to has to travel with it. Filled once
    // the five are known rather than written into each by hand.
    let states: Vec<Rgba> = specs.iter().map(|lane| lane.state).collect();
    for (index, spec) in specs.iter_mut().enumerate() {
        spec.blend = LaneBlend {
            left: index.checked_sub(1).and_then(|left| states.get(left).copied()),
            right: states.get(index + 1).copied(),
        };
    }
    div()
        .relative()
        .h_full()
        .flex()
        .px(px(8.0))
        .pb(px(7.0))
        .child(rail(colors, metrics))
        .children(specs.iter().map(|spec| lane(spec, workspace_id, state, colors, metrics)))
        .into_any_element()
}

/// The thread running behind the queue nodes, tinted by the lanes it passes.
///
/// The mock's five-stop gradient becomes four two-stop segments because that is
/// what [`gpui::linear_gradient`] carries; the stops land in the same places.
fn rail(colors: &BeadsBoardColors, metrics: Metrics) -> AnyElement {
    let cyan = colors.ready_state;
    let indigo = colors.progress_state;
    let coral = colors.blocked_state;
    div()
        .absolute()
        .left(px(24.0))
        .right(px(RAIL_RIGHT))
        .top(metrics.at(RAIL_TOP))
        .h(px(1.0))
        .bg(colors.hairline)
        .flex()
        .child(rail_segment(fade(cyan), alpha(cyan, 0.41)))
        .child(rail_segment(alpha(cyan, 0.41), alpha(indigo, 0.53)))
        .child(rail_segment(alpha(indigo, 0.53), alpha(coral, 0.41)))
        .child(rail_segment(alpha(coral, 0.41), fade(coral)))
        .into_any_element()
}

fn rail_segment(from: Rgba, to: Rgba) -> AnyElement {
    div()
        .flex_1()
        .h_full()
        .bg(linear_gradient(90.0, linear_color_stop(from, 0.0), linear_color_stop(to, 1.0)))
        .into_any_element()
}

fn lane(
    spec: &Lane<'_>,
    workspace_id: WorkspaceId,
    state: &std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    colors: &BeadsBoardColors,
    metrics: Metrics,
) -> AnyElement {
    div()
        .flex_1()
        .min_w(px(0.0))
        .relative()
        .px(px(8.0))
        // The wash comes first so everything else sits on it. It is what makes
        // a lane read as a column of its own rather than as five sets of rows
        // sharing one ground.
        .child(lane_wash(spec))
        .child(lane_head(spec, colors, metrics))
        .child(lane_body(spec, workspace_id, state, colors, metrics))
        .into_any_element()
}

/// The queue's colour zoning its column, over the whole depth of the strip.
///
/// One band per queue, run edge to edge so the five meet with no ground between
/// them: the board reads as five columns of colour rather than as five headings
/// over one shared ground. Translucent rather than mixed into a solid, because
/// the rail is painted before the lanes and a solid wash would erase the length
/// of it that crosses this column.
///
/// The colour is flat across the middle third and travels to meet its
/// neighbours over the outer two, so a boundary is a crossing rather than a
/// step. Both sides of a boundary compute the same midpoint of the same two
/// queues, which is what makes the two gradients meet without a seam of their
/// own.
fn lane_wash(spec: &Lane<'_>) -> AnyElement {
    let own = alpha(spec.state, LANE_WASH);
    let meeting = |neighbour: Option<Rgba>| {
        neighbour.map_or(own, |other| alpha(mix(spec.state, other, 0.5), LANE_WASH))
    };
    div()
        .absolute()
        .left_0()
        .right_0()
        .top_0()
        // Past the padding the lanes hold at the bottom for the chevron, so a
        // column runs the full depth and stops on the board's own bottom bar
        // instead of a hand's width above it.
        .bottom(px(-LANES_BOTTOM_PAD))
        .flex()
        .child(div().flex_1().bg(linear_gradient(
            90.0,
            linear_color_stop(meeting(spec.blend.left), 0.0),
            linear_color_stop(own, 1.0),
        )))
        .child(div().flex_1().bg(own))
        .child(div().flex_1().bg(linear_gradient(
            90.0,
            linear_color_stop(own, 0.0),
            linear_color_stop(meeting(spec.blend.right), 1.0),
        )))
        .into_any_element()
}

fn lane_head(spec: &Lane<'_>, colors: &BeadsBoardColors, metrics: Metrics) -> AnyElement {
    let name_color = if spec.accent == LaneAccent::Progress {
        colors.queue_name_active
    } else {
        colors.queue_name
    };
    // The count wears its queue's own colour, lifted to carry words rather
    // than to be a mark: it sits on the board's ground with nothing behind it.
    let count_ink = colors.count_ink(spec.state);
    let halo = if spec.accent == LaneAccent::Progress { NODE_HALO_ACTIVE } else { NODE_HALO };
    div()
        .relative()
        .h(px(metrics.head()))
        .flex()
        .items_start()
        .px(px(4.0))
        .child(
            div()
                .relative()
                .flex_none()
                .size(metrics.at(9.0))
                .mt(metrics.at(13.0))
                .rounded_full()
                .bg(spec.state)
                .border_2()
                .border_color(colors.ground)
                .child(
                    div()
                        .absolute()
                        .left(metrics.at(2.0))
                        .top(metrics.at(7.0))
                        .w(px(1.0))
                        .h(metrics.at(14.0))
                        .bg(alpha(spec.state, 0.45)),
                ),
        )
        .child(
            div()
                .min_w(px(0.0))
                .mt(metrics.at(7.0))
                // The gap to the node is padding, not margin: the patch has to
                // start where the node ends, or the rail shows through between
                // the two and reads as a line joining a dot to a word.
                .pl(metrics.at(11.0))
                .pr(px(6.0))
                .flex()
                // Both children share one line box and centre in it. Baseline
                // alignment left the smaller count sitting low.
                .items_center()
                .gap(px(6.0))
                // Opaque, so the queue line breaks the rail rather than
                // sitting on top of it — and opaque in the wash's own colour,
                // since a patch of bare ground would read as a hole in it.
                .bg(mix(colors.ground, spec.state, LANE_WASH))
                .child(
                    div()
                        .flex_none()
                        .text_size(metrics.at(12.0))
                        .line_height(metrics.at(QUEUE_LINE_HEIGHT))
                        .font_weight(FontWeight(650.0))
                        .text_color(name_color)
                        .child(spec.name),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family("monospace")
                        .text_size(metrics.at(10.0))
                        .line_height(metrics.at(13.0))
                        .font_weight(FontWeight(600.0))
                        .text_color(count_ink)
                        .child(spec.total.to_string()),
                ),
        )
        // The halo comes last, after the patch the queue line lays down: the
        // halo reaches past the node and the patch starts at the node's edge,
        // so anything painted before it comes back with a bite out of it. It
        // washes over the node as well as around it, which a glow of the
        // node's own colour is welcome to do — the dot keeps its colour and
        // its rim picks the hue up. Absolute either way, so it takes no place
        // in the row.
        .child(
            div()
                .absolute()
                .left(px(1.0))
                .top(metrics.at(10.0))
                .size(metrics.at(15.0))
                .rounded_full()
                .bg(alpha(spec.state, halo)),
        )
        .into_any_element()
}

fn lane_body(
    spec: &Lane<'_>,
    workspace_id: WorkspaceId,
    state: &std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    colors: &BeadsBoardColors,
    metrics: Metrics,
) -> AnyElement {
    if spec.items.is_empty() {
        return div()
            .relative()
            .h(px(metrics.body()))
            .child(empty_lane(spec, colors, metrics))
            .into_any_element();
    }
    // Virtualised: a full queue is 200 rows and only ~3 are visible, so the
    // closure builds cards for the range uniform_list asks for and no more.
    // It runs at layout time, after the build frame's borrow of the snapshot
    // is gone, so it re-reads this queue from the shared state; an index that
    // outlives its snapshot resolves to nothing rather than to a stale card.
    let queue = spec.queue;
    let closure_state = std::sync::Arc::clone(state);
    let closure_colors = *colors;
    div()
        .relative()
        .h(px(metrics.body()))
        .child(
            uniform_list(
                SharedString::from(format!("beads-lane-{workspace_id}-{}", spec.name)),
                spec.items.len(),
                move |range, _window, _app| {
                    let Ok(boards) = closure_state.lock() else { return Vec::new() };
                    let (snapshot, _) = board_content(boards.state(workspace_id));
                    let Some(snapshot) = snapshot else { return Vec::new() };
                    let items = queue(snapshot);
                    range
                        .filter_map(|index| items.get(index))
                        .map(|item| {
                            // The row is the uniform unit, the card's gap and
                            // all: uniform_list measures an item's taffy size,
                            // which a margin is outside of, so the gap rides
                            // inside a fixed-height row as padding instead.
                            div().h(metrics.at(ISSUE_HEIGHT)).pb(metrics.at(CARD_GAP)).child(issue(
                                item,
                                CardContext {
                                    workspace_id,
                                    state: &closure_state,
                                    colors: &closure_colors,
                                    metrics,
                                },
                            ))
                        })
                        .collect()
                },
            )
            .h(px(metrics.issues()))
            .pr(px(4.0)),
        )
        .child(
            div()
                .absolute()
                .right(px(2.0))
                .bottom(px(1.0))
                .text_size(metrics.at(9.0))
                .line_height(metrics.at(9.0))
                .text_color(colors.chevron)
                .child("⌄"),
        )
        .into_any_element()
}

/// What an empty queue says, in the slot its first card would have taken.
///
/// A dashed ghost of a card rather than a bare word: an empty column with a
/// heading floating over nothing reads as content that failed to arrive, and
/// the outline is what says the queue itself is the empty thing.
fn empty_lane(spec: &Lane<'_>, colors: &BeadsBoardColors, metrics: Metrics) -> AnyElement {
    div()
        .h(metrics.at(ISSUE_HEIGHT - CARD_GAP))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(CARD_RADIUS))
        .border_1()
        .border_dashed()
        .border_color(alpha(spec.state, 0.3))
        .text_size(metrics.at(9.5))
        .line_height(metrics.at(12.0))
        .text_color(colors.muted)
        .child(spec.empty)
        .into_any_element()
}

/// One issue as a raised card: a gradient fill under a hairline, with the
/// title's own line above its metadata.
fn issue(item: &BeadsBoardItem, card: CardContext<'_>) -> AnyElement {
    let CardContext { colors, metrics, .. } = card;
    let mark = colors.priority_mark(item.priority);
    div()
        .h(metrics.at(ISSUE_HEIGHT - CARD_GAP))
        .flex_none()
        .relative()
        .overflow_hidden()
        .rounded(px(CARD_RADIUS))
        .border_1()
        .border_color(colors.card_border)
        // Lit from the top, which is the whole of the card's relief: a flat
        // fill on a flat ground is the shape the board had before.
        .bg(linear_gradient(
            180.0,
            linear_color_stop(colors.card_top, 0.0),
            linear_color_stop(colors.card, 1.0),
        ))
        .hover(|raised| {
            raised
                .bg(linear_gradient(
                    180.0,
                    linear_color_stop(colors.card_hover_top, 0.0),
                    linear_color_stop(colors.card_hover, 1.0),
                ))
                .border_color(colors.card_border_hover)
                .shadow_xs()
        })
        .flex()
        .flex_col()
        .gap(px(2.0))
        .pt(px(CARD_PAD_TOP))
        .px(px(8.0))
        .pb(px(6.0))
        .child(issue_title(item, mark, card))
        .child(issue_meta(item, card))
        .into_any_element()
}

/// The priority badge and the title, which owns the rest of its line: the
/// title is the only line a reader scans, so nothing else shares its row.
fn issue_title(item: &BeadsBoardItem, mark: PriorityMark, card: CardContext<'_>) -> AnyElement {
    let CardContext { colors, metrics, .. } = card;
    div()
        .h(metrics.at(TITLE_LINE))
        .flex()
        .items_center()
        .gap(px(5.0))
        .child(
            div()
                .flex_none()
                .rounded(px(3.0))
                .px(px(4.0))
                .bg(mark.fill)
                .font_family("monospace")
                .text_size(metrics.at(9.5))
                .line_height(metrics.at(13.0))
                .font_weight(FontWeight(700.0))
                .text_color(mark.ink)
                .child(format!("P{}", item.priority)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(metrics.at(12.0))
                .line_height(metrics.at(TITLE_LINE))
                .font_weight(FontWeight(650.0))
                .text_color(colors.title)
                .child(item.title.clone()),
        )
        .into_any_element()
}

/// The id at the left of the card's second line and the epic at its right.
fn issue_meta(item: &BeadsBoardItem, card: CardContext<'_>) -> AnyElement {
    let CardContext { workspace_id, state, colors, metrics } = card;
    div()
        .h(metrics.at(12.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .overflow_hidden()
        .child(copyable(
            CopyTarget {
                key: format!("beads-id-{workspace_id}-{}", item.id),
                label: format!("Copy issue {}", item.id),
                text: item.id.clone(),
                shown: short_id(&item.id).to_owned(),
                state,
            },
            div()
                .flex_none()
                .font_family("monospace")
                .text_size(metrics.at(9.0))
                .line_height(metrics.at(12.0))
                .font_weight(FontWeight(500.0))
                .text_color(colors.muted),
            colors,
        ))
        // The slack sits here, between the two, rather than being left to
        // justify-content: a grown container fills its row and then has
        // nothing to justify, which reads as left-aligned.
        .child(div().flex_1().min_w(px(0.0)))
        .children(item.parent_epic_name.as_ref().map(|name| epic_label(name, &item.id, card)))
        .into_any_element()
}

/// What every card on one board shares.
#[derive(Clone, Copy)]
struct CardContext<'a> {
    workspace_id: WorkspaceId,
    state: &'a std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    colors: &'a BeadsBoardColors,
    metrics: Metrics,
}

/// One line of card metadata the pointer can lift onto the clipboard.
struct CopyTarget<'a> {
    key: String,
    label: String,
    /// What lands on the clipboard, which is the full id even where the card
    /// shows the short one: a shortened id is not one anything else accepts.
    text: String,
    shown: String,
    state: &'a std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
}

/// Make `styled` a click-to-copy target for `target`.
fn copyable(target: CopyTarget<'_>, styled: gpui::Div, colors: &BeadsBoardColors) -> AnyElement {
    let CopyTarget { key, label, text, shown, state } = target;
    let state = std::sync::Arc::clone(state);
    let hover = colors.title;
    styled
        .id(SharedString::from(key))
        .role(Role::Button)
        .aria_label(label)
        .cursor_pointer()
        .hover(move |line| line.text_color(hover))
        // The grid below owns the pointer for selection, so the press stops
        // here rather than starting a drag in the terminal behind the board.
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, _window, _app| {
            if let Ok(mut boards) = state.lock() {
                boards.copy(text.clone());
            }
        })
        .child(shown)
        .into_any_element()
}

/// How much of an epic name a card shows before it is cut short, and the
/// shortest prefix a word boundary may leave. Measured against every epic in
/// this machine's Beads projects: a median name is 21 characters and one word,
/// so the cap leaves half of them untouched and trims the long tail.
const EPIC_MAX_CHARS: usize = 24;

/// An epic name shortened to what tells one epic from another.
///
/// Cut at a word boundary — space, hyphen, underscore, or slash, so a
/// slug-style name breaks as readably as a sentence one — and only when that
/// boundary leaves at least half the budget; otherwise the cut is hard, since
/// a boundary near the start would throw away more than it saves.
fn short_epic(name: &str) -> String {
    let mut head: String = name.chars().take(EPIC_MAX_CHARS).collect();
    if head.chars().count() == name.chars().count() {
        return head;
    }
    if let Some(boundary) = head.rfind([' ', '-', '_', '/'])
        && head[..boundary].chars().count() >= EPIC_MAX_CHARS / 2
    {
        head.truncate(boundary);
    }
    while head.ends_with([' ', '-', '_', '/']) {
        head.pop();
    }
    head.push('\u{2026}');
    head
}

/// An issue id without the project it belongs to: `nasha-lab-byd.12` reads as
/// `byd.12`, and a board of one project repeats that prefix on every card.
///
/// The tail after the last `-`, so a project whose own name carries one keeps
/// working. An id that is all prefix, or has no `-` at all, is left alone —
/// half an id is worse than a long one.
fn short_id(id: &str) -> &str {
    id.rsplit_once('-').map_or(id, |(_, tail)| if tail.is_empty() { id } else { tail })
}

/// The epic a card belongs to, on the right of the id's line.
///
/// Plain text in its own hue: the mock's diamond, a tinted tag, and a rule
/// beneath it were each tried in front of the name, and none of them said
/// anything the hue was not already saying.
fn epic_label(name: &str, issue_id: &str, card: CardContext<'_>) -> AnyElement {
    let CardContext { workspace_id, state, colors, metrics } = card;
    div()
        // Sized by its own content, so the spacer before it decides where it
        // sits: the row's right edge.
        .min_w(px(0.0))
        // Held off the id, which the slack alone cannot guarantee once a long
        // name has eaten it: the two read as one string when they meet.
        .ml(px(8.0))
        .flex()
        .items_center()
        .overflow_hidden()
        .child(copyable(
            CopyTarget {
                key: format!("beads-epic-{workspace_id}-{issue_id}"),
                label: format!("Copy epic {name}"),
                // Copied in full, shown as its topic: the whole name is
                // what another tool would be given.
                text: name.to_owned(),
                shown: short_epic(name),
                state,
            },
            div()
                .truncate()
                .text_right()
                .text_size(metrics.at(9.0))
                .line_height(metrics.at(12.0))
                .text_color(colors.epic),
            colors,
        ))
        .into_any_element()
}

fn board_content(state: Option<&BeadsBoardState>) -> (Option<&BeadsBoardSnapshot>, String) {
    match state {
        Some(BeadsBoardState::Ready { snapshot, .. }) => (Some(snapshot), String::new()),
        Some(BeadsBoardState::Loading { cached: Some(snapshot) }) => {
            (Some(snapshot), String::new())
        }
        Some(BeadsBoardState::Loading { cached: None }) | None => (None, "Loading board…".into()),
        Some(BeadsBoardState::Unavailable { message }) => (None, message.clone()),
        Some(BeadsBoardState::NotDetected) => (None, "No Beads project".into()),
    }
}

/// `color` lifted away from `ground` until it clears `min_ratio`.
///
/// Flattened onto the ground first, so an alpha-reduced slot is measured as it
/// is seen, then mixed toward white or black — whichever direction the ground
/// is not — in sixteenths. A colour already clearing the floor comes back
/// untouched, so a theme with good contrast keeps its own tones exactly.
fn readable(color: Rgba, ground: Rgba, min_ratio: f32) -> Rgba {
    let flat = over(color, ground);
    if contrast(flat, ground) >= min_ratio {
        return flat;
    }
    let target = if luminance(ground) < 0.5 { WHITE } else { BLACK };
    let mut lifted = flat;
    for step in 1..=16_u8 {
        lifted = mix(flat, target, f32::from(step) / 16.0);
        if contrast(lifted, ground) >= min_ratio {
            break;
        }
    }
    lifted
}

/// `color` lifted until it clears `min_ratio` on both grounds it is read on.
///
/// The strip's ground and a card's raised fill pull opposite ways depending on
/// the theme's polarity, so neither is the strict one to measure against. Two
/// passes settle it: the second lifts further only if the first left the ink
/// short on the other ground.
fn readable_anywhere(color: Rgba, ground: Rgba, card: Rgba, min_ratio: f32) -> Rgba {
    readable(readable(color, ground, min_ratio), card, min_ratio)
}

/// `color` composited over `ground`, so an alpha carries into a solid colour.
fn over(color: Rgba, ground: Rgba) -> Rgba {
    mix(ground, Rgba { a: 1.0, ..color }, color.a)
}

/// `from` moved `amount` of the way toward `to`.
fn mix(from: Rgba, to: Rgba, amount: f32) -> Rgba {
    let amount = amount.clamp(0.0, 1.0);
    Rgba {
        r: (to.r - from.r).mul_add(amount, from.r),
        g: (to.g - from.g).mul_add(amount, from.g),
        b: (to.b - from.b).mul_add(amount, from.b),
        a: 1.0,
    }
}

/// WCAG contrast ratio between two opaque colours.
fn contrast(a: Rgba, b: Rgba) -> f32 {
    let (high, low) = {
        let (a, b) = (luminance(a), luminance(b));
        if a >= b { (a, b) } else { (b, a) }
    };
    (high + 0.05) / (low + 0.05)
}

/// WCAG relative luminance, which linearises each channel first.
fn luminance(color: Rgba) -> f32 {
    fn linear(channel: f32) -> f32 {
        if channel <= 0.040_45 { channel / 12.92 } else { ((channel + 0.055) / 1.055).powf(2.4) }
    }
    0.2126f32.mul_add(linear(color.r), 0.7152f32.mul_add(linear(color.g), 0.0722 * linear(color.b)))
}

/// One theme slot as a colour, keeping the alpha the derivation gave it.
fn slot(color: [f32; 4]) -> Rgba {
    Rgba { r: color[0], g: color[1], b: color[2], a: color[3] }
}

fn alpha(color: Rgba, a: f32) -> Rgba {
    Rgba { a, ..color }
}

fn fade(color: Rgba) -> Rgba {
    alpha(color, 0.0)
}
#[cfg(test)]
mod tests {
    use scribe_common::theme::ChromeColors;

    use super::*;

    /// Chrome slots the board never reads, filled so a test can name only the
    /// few it does.
    fn chrome_slots(fill: [f32; 4]) -> ChromeColors {
        ChromeColors {
            tab_bar_bg: fill,
            tab_bar_active_bg: fill,
            tab_text: fill,
            tab_text_active: fill,
            tab_separator: fill,
            status_bar_bg: fill,
            status_bar_text: fill,
            divider: fill,
            accent: fill,
            scrollbar: fill,
            tab_bar_gradient_top: fill,
            status_bar_separator: fill,
            prompt_bar_first_row_bg: fill,
            prompt_bar_second_row_bg: fill,
            prompt_bar_text: fill,
            prompt_bar_icon_first: fill,
            prompt_bar_icon_latest: fill,
        }
    }

    fn visible_sorted(boards: &BeadsBoards) -> Vec<(WorkspaceId, bool)> {
        let mut visible = boards.visible();
        visible.sort_by_key(|(workspace_id, _)| workspace_id.to_string());
        visible
    }

    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn every_region_pins_hovers_and_closes_its_own_board() {
        let mut ids = [WorkspaceId::new(), WorkspaceId::new(), WorkspaceId::new()];
        ids.sort_by_key(ToString::to_string);
        let [left, middle, right] = ids;
        let mut boards = BeadsBoards::default();

        // Two regions pinned at once, and a third hovered beside them: each
        // region owns a board, so none of these displaces another.
        boards.toggle_pin(left);
        boards.toggle_pin(right);
        boards.hover(middle, HoverSource::Bead, true);
        assert_eq!(
            visible_sorted(&boards),
            [(left, true), (middle, false), (right, true)],
            "focus is not an input, and no board hides another"
        );

        // Hovering a pinned region's bead leaves it pinned rather than
        // downgrading it to a hover that expires.
        boards.hover(left, HoverSource::Bead, true);
        assert!(boards.is_pinned(left));
        assert_eq!(visible_sorted(&boards).into_iter().filter(|(_, p)| *p).count(), 2);

        // Unpinning one leaves the other alone.
        boards.toggle_pin(right);
        assert!(!boards.is_pinned(right));
        assert!(boards.is_pinned(left));

        // Text size is one setting for the window, clamped so the fixed-height
        // strip can still show a readable row.
        assert!((boards.text_scale() - 1.0).abs() < f32::EPSILON);
        boards.adjust_text_scale(-1);
        assert!((boards.text_scale() - 0.9).abs() < 0.001);
        for _ in 0..20 {
            boards.adjust_text_scale(1);
        }
        assert!((boards.text_scale() - 1.6).abs() < 0.001, "grows without bound");
        for _ in 0..20 {
            boards.adjust_text_scale(-1);
        }
        assert!((boards.text_scale() - 0.8).abs() < 0.001, "shrinks without bound");
        boards.adjust_text_scale(2);

        // A region that leaves the window takes its board state with it.
        boards.retain_regions(&HashSet::from([middle]));
        assert_eq!(visible_sorted(&boards), [(middle, false)]);
        // Leaving only starts the grace period the board needs while the
        // pointer crosses onto it, or onto a control inside it.
        boards.hover(middle, HoverSource::Bead, false);
        assert_eq!(visible_sorted(&boards), [(middle, false)]);
        std::thread::sleep(Duration::from_millis(160));
        assert!(boards.expire_hover());
        assert!(boards.visible().is_empty());
    }

    /// A pin outlives the window it was made in: the record names a workspace
    /// the layout has not adopted yet, so the pin waits rather than being
    /// pruned by the reconcile that runs before the region appears.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn restored_pins_wait_for_their_region_to_come_back() {
        let restored = WorkspaceId::new();
        let never_seen = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.restore_pins([restored, never_seen]);

        // The layout is still empty on the first frames after a restart.
        boards.retain_regions(&HashSet::new());
        assert!(boards.visible().is_empty());
        assert!(!boards.is_pinned(restored));

        // The region arrives and takes its pin with it.
        boards.retain_regions(&HashSet::from([restored]));
        assert!(boards.is_pinned(restored));
        assert_eq!(boards.visible(), [(restored, true)]);

        // Pins are handed over once, so unpinning is not undone next frame.
        boards.toggle_pin(restored);
        boards.retain_regions(&HashSet::from([restored]));
        assert!(!boards.is_pinned(restored), "a restored pin came back after being cleared");

        // The order is stable, so a caller diffing against the record sees a
        // change only when one really happened.
        let mut ids = [WorkspaceId::new(), WorkspaceId::new()];
        ids.sort_by_key(WorkspaceId::as_uuid);
        boards.toggle_pin(ids[1]);
        boards.toggle_pin(ids[0]);
        assert_eq!(boards.pinned(), ids);
    }

    /// Dragging one board's bottom bar resizes that board and no other, stays
    /// inside what its region can give, and holds the board open while the
    /// pointer is off it.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn the_bottom_bar_drags_one_boards_height() {
        let left = WorkspaceId::new();
        let right = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.toggle_pin(left);
        boards.toggle_pin(right);
        assert!((boards.height(left) - BEADS_BOARD_HEIGHT).abs() < f32::EPSILON);

        // The drag is a delta from where it was grabbed, so a pointer that
        // jumps a frame still lands where the gesture asked.
        boards.start_resize(left, 300.0);
        assert_eq!(boards.resizing(), Some(left));
        assert!(boards.resize_to(360.0, 600.0));
        assert!((boards.height(left) - (BEADS_BOARD_HEIGHT + 60.0)).abs() < 0.001);
        assert!(
            (boards.height(right) - BEADS_BOARD_HEIGHT).abs() < f32::EPSILON,
            "resizing one region's board moved another's"
        );

        // The pointer leaving the board it is resizing does not close it.
        boards.hover(left, HoverSource::Board, true);
        boards.hover(left, HoverSource::Board, false);
        std::thread::sleep(Duration::from_millis(160));
        assert!(!boards.expire_hover());

        // Dragging up stops at one readable issue row, and down at what the
        // caller says the region can spare.
        assert!(boards.resize_to(-1000.0, 600.0));
        let floor = boards.height(left);
        assert!(floor > LANE_HEAD_HEIGHT && floor < BEADS_BOARD_HEIGHT, "collapsed to {floor}");
        boards.resize_to(4000.0, 240.0);
        assert!((boards.height(left) - 240.0).abs() < 0.001);
        // Even a region with nothing to spare keeps the board readable.
        boards.resize_to(4000.0, 0.0);
        assert!((boards.height(left) - floor).abs() < 0.001);

        assert!(boards.end_resize());
        assert!(!boards.end_resize());
        assert!(!boards.resize_to(500.0, 600.0), "a released bar kept resizing");

        // A region that leaves the window takes its height with it, so the
        // next workspace to land there opens at the designed size.
        boards.retain_regions(&HashSet::from([right]));
        assert!((boards.height(left) - BEADS_BOARD_HEIGHT).abs() < f32::EPSILON);
    }

    /// P0 has to outrank P1 whichever of a theme's two reds is the washed-out
    /// one, and the mark has to rank them even where a reader cannot name the
    /// hue: the wash strengthens as the priority does.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn the_priority_ramp_puts_p0_hottest() {
        // The shape that put P0 below P1 in the field: a bright red that is a
        // pale pink, beside a plain red that is deep.
        let ground = [0.06, 0.07, 0.08, 1.0];
        let mut ansi = [[0.5, 0.5, 0.5, 1.0]; 16];
        ansi[RED] = [0.78, 0.11, 0.13, 1.0];
        ansi[BRIGHT_RED] = [0.93, 0.72, 0.74, 1.0];
        let chrome = ChromeColors {
            tab_bar_bg: ground,
            tab_text: [0.55, 0.56, 0.58, 1.0],
            tab_text_active: [0.9, 0.91, 0.93, 1.0],
            ..chrome_slots(ground)
        };

        let colors = BeadsBoardColors::from_theme(&chrome, &ansi, 1.0);

        let hottest = vividness(colors.priorities[0]);
        let below = vividness(colors.priorities[1]);
        assert!(hottest > below, "P0 reads at {hottest:.2} saturation against P1 at {below:.2}");

        // And the wash ranks every step, so a card says which of two issues is
        // hotter before its digits are read. Measured as laid down rather than
        // as a tint, since the tint is solved for exactly this: a hue further
        // from the card makes the same mark with less of itself.
        let laid = |priority: u8| reach(colors.priority_mark(priority).fill, colors.card);
        for priority in 0..4u8 {
            let (strong, weak) = (laid(priority), laid(priority + 1));
            assert!(
                strong > weak,
                "the P{priority} badge carries {strong:.3} against the next rank's {weak:.3}"
            );
        }
    }

    /// A theme whose muted slot and ANSI red sit close to its background must
    /// still produce a board that can be read: the tones are relative to the
    /// mock's ground, and reproducing the ratio there is not legibility here.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn every_text_colour_clears_the_contrast_floor() {
        let ground = [0.06, 0.08, 0.07, 1.0];
        // Barely off the background, the shape that made blockers and ids
        // unreadable in the field.
        let dim = [0.22, 0.24, 0.23, 1.0];
        let dim_red = [0.35, 0.12, 0.12, 1.0];
        let mut ansi = [[0.5, 0.5, 0.5, 1.0]; 16];
        ansi[RED] = dim_red;
        ansi[BRIGHT_RED] = dim_red;
        ansi[BRIGHT_MAGENTA] = [0.3, 0.15, 0.3, 1.0];
        ansi[YELLOW] = [0.42, 0.34, 0.1, 1.0];
        ansi[BRIGHT_YELLOW] = [0.5, 0.42, 0.15, 1.0];
        ansi[BRIGHT_CYAN] = [0.1, 0.3, 0.3, 1.0];
        ansi[BRIGHT_BLUE] = [0.15, 0.15, 0.4, 1.0];
        ansi[BRIGHT_GREEN] = [0.12, 0.3, 0.15, 1.0];
        let chrome = ChromeColors {
            tab_bar_bg: ground,
            tab_text: dim,
            tab_text_active: [0.4, 0.42, 0.41, 1.0],
            tab_separator: [0.3, 0.3, 0.3, 0.3],
            ..chrome_slots(ground)
        };

        let colors = BeadsBoardColors::from_theme(&chrome, &ansi, 1.0);

        let ink = Rgba { a: 1.0, ..colors.ground };
        // Words are read on two grounds now — the strip's and the raised
        // card's — and a lift that satisfies one can leave the other short.
        for (name, color) in [
            ("title", colors.title),
            ("queue name", colors.queue_name),
            ("queue name active", colors.queue_name_active),
            ("muted", colors.muted),
            ("P0", colors.priorities[0]),
            ("P1", colors.priorities[1]),
            ("P2", colors.priorities[2]),
            ("P3", colors.priorities[3]),
            ("P4", colors.priorities[4]),
            ("epic", colors.epic),
        ] {
            for (surface, under) in [(ink, "the board"), (colors.card, "a card")] {
                let ratio = contrast(color, surface);
                assert!(ratio >= BODY_CONTRAST - 0.01, "{name} reads at {ratio:.2}:1 on {under}");
            }
        }
        // A queue's colour is a mark on the rail and a word in the head, and
        // the two floors are not the same: the total is lifted again for the
        // one it has to clear as text.
        for (name, color) in [
            ("backlog", colors.backlog_state),
            ("ready", colors.ready_state),
            ("in progress", colors.progress_state),
            ("blocked", colors.blocked_state),
            ("done", colors.done_state),
        ] {
            let ratio = contrast(colors.count_ink(color), ink);
            assert!(ratio >= BODY_CONTRAST - 0.01, "the {name} total reads at {ratio:.2}:1");
        }
        for (name, color) in [
            ("chevron", colors.chevron),
            ("backlog", colors.backlog_state),
            ("ready", colors.ready_state),
            ("in progress", colors.progress_state),
            ("blocked", colors.blocked_state),
            ("done", colors.done_state),
        ] {
            let ratio = contrast(color, ink);
            assert!(ratio >= MARK_CONTRAST - 0.01, "{name} mark reads at {ratio:.2}:1");
        }

        // A theme that already reads well keeps its own tones untouched.
        let bright = Rgba { r: 0.9, g: 0.9, b: 0.9, a: 1.0 };
        assert_eq!(readable(bright, ink, BODY_CONTRAST), bright);
    }

    /// A card cannot reach the window's clipboard, so it parks the text and
    /// the view lifts it on the next frame.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn a_copy_request_is_taken_once() {
        let mut boards = BeadsBoards::default();
        assert_eq!(boards.take_copy(), None);

        boards.copy("nasha-lab-byd.12".to_owned());
        assert_eq!(boards.take_copy().as_deref(), Some("nasha-lab-byd.12"));
        assert_eq!(boards.take_copy(), None, "one click is one copy");
    }

    /// Names taken from the Beads projects on the machine this was written on:
    /// a median epic is one word and 21 characters, and the long tail runs to
    /// 72. Both shapes have to survive — a sentence and a slug.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn short_epic_cuts_long_names_at_a_word_boundary() {
        // Under the cap, nothing happens: half of the real names are here.
        for name in ["dark-mode", "google-auth", "e2e-sandbox-gaps", "Beads integration"] {
            assert_eq!(short_epic(name), name);
        }

        // Sentences break at a space.
        assert_eq!(
            short_epic("Replace hook-derived subagent tracking with snapshots"),
            "Replace hook-derived\u{2026}"
        );
        assert_eq!(
            short_epic("Window resume does not honor screen or minimized state"),
            "Window resume does not\u{2026}"
        );
        // Slugs break at a hyphen.
        assert_eq!(short_epic("client-empty-error-state-parity"), "client-empty-error\u{2026}");
        // A boundary too near the start would throw away more than it saves,
        // so the cut is hard instead.
        assert_eq!(
            short_epic("Antidisestablishmentarianism review"),
            "Antidisestablishmentaria\u{2026}"
        );
        // The ellipsis never lands on a boundary character.
        assert_eq!(
            short_epic("Coordinate legacy - runtime retirement"),
            "Coordinate legacy\u{2026}"
        );
        // Multi-byte names are cut by character, not by byte.
        assert_eq!(short_epic(&"é".repeat(40)), format!("{}\u{2026}", "é".repeat(24)));
    }

    /// The card shows the issue, not the project: every card on a board
    /// carries the same prefix, and the space is the title's.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn short_id_drops_the_project_and_nothing_else() {
        assert_eq!(short_id("nasha-lab-byd.12"), "byd.12");
        assert_eq!(short_id("scribe-3j2y"), "3j2y");
        assert_eq!(short_id("sc-70"), "70");
        // A child issue keeps the parent it hangs off.
        assert_eq!(short_id("scribe-aq1.23"), "aq1.23");
        // Nothing to drop, or nothing left after dropping: keep the whole id,
        // because half an id is worse than a long one.
        assert_eq!(short_id("70"), "70");
        assert_eq!(short_id("scribe-"), "scribe-");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn fresh_ready_snapshot_does_not_refresh_on_hover() {
        let workspace = WorkspaceId::new();
        let now: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis()
            .try_into()
            .expect("epoch fits");
        let mut boards = BeadsBoards::default();
        boards.update(
            workspace,
            BeadsBoardState::Ready {
                snapshot: BeadsBoardSnapshot { refreshed_at_epoch_ms: now, ..Default::default() },
                stale: false,
                refresh_error: None,
            },
        );

        assert!(!boards.needs_refresh(workspace, Duration::from_secs(30)));
    }

    #[test]
    fn unavailable_board_schedules_one_retry_per_interval() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.update(workspace, BeadsBoardState::Unavailable { message: "missing bd".into() });

        assert_eq!(boards.due_retry(Duration::from_secs(30)), Some(workspace));
        assert_eq!(boards.due_retry(Duration::from_secs(30)), None);
    }
}
