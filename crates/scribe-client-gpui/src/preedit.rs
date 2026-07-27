//! IME preedit composition state, overlay geometry, and the GPUI input handler.
//!
//! Ports the winit client's [`crate`](../../scribe-client/src/preedit.rs)
//! preedit semantics onto GPUI's IME plumbing. The pure data ([`PreeditState`],
//! [`PreeditOverlay`]) and the two pure helpers ([`compute_overlay`],
//! [`preedit_cell_width`]) are display-independent and unit-tested. The state
//! machine ([`PreeditMachine`]) mirrors the winit `WindowEvent::Ime` arm without
//! any GPUI dependency, so its transitions are testable off a window.
//!
//! [`Ime`] wraps the machine in a `gpui::Entity` and implements
//! [`gpui::EntityInputHandler`]: GPUI delivers marked (composing) text through
//! `replace_and_mark_text_in_range` and committed text through
//! `replace_text_in_range`, exactly the events winit surfaced as `Ime::Preedit`
//! and `Ime::Commit`. Committed text is re-emitted as [`ImeEvent::Commit`] so the
//! view routes it through the normal `ClientMessage::KeyInput` path — no PTY
//! bytes ever flow through this module and preedit text is never persisted.
//!
//! The live window registers the [`Ime`] entity with the platform on every
//! painted frame through `Window::handle_input`, from the focused pane's paint
//! pass, so the handler's element bounds are the cursor cell the OS candidate
//! window anchors on.
//!
//! Because IME needs a real compositor (ibus/fcitx on X11, a text-input protocol
//! on Wayland) it is verified by the parity procedure documented in
//! `lat.md/client.md` under "GPUI IME Composition" and by the ibus-driven visual
//! E2E oracle, not by a `#[gpui::test]`.

use std::ops::Range;

use gpui::{
    Bounds, Context, EntityInputHandler, EventEmitter, Pixels, Point, UTF16Selection, Window,
};
use unicode_width::UnicodeWidthChar;

/// In-progress IME composition for the focused pane.
///
/// Created on the first non-empty marked-text update after a clear (the cursor
/// cell at that moment is captured as the anchor); updated on subsequent marked
/// updates; dropped on an empty update, commit, unmark, focus loss, or a
/// focused-pane change.
///
/// `Debug` is implemented by hand to redact `text` — preedit content is
/// transient user input from the OS IME and must not leak into tracing, panic
/// backtraces, or log output.
#[derive(Clone)]
pub struct PreeditState {
    /// Current preedit text from the most recent marked-text update (UTF-8).
    pub text: String,
    /// Byte-range caret hint reported by the IME (active segment in a
    /// multi-segment composition), when present. Captured for data-model
    /// alignment; the minimal underline renderer ignores it.
    pub caret: Option<(usize, usize)>,
    /// Absolute scrollback row where composition began. Held stable for the
    /// lifetime of the `PreeditState` so the overlay does not shift if the grid
    /// scrolls underneath the composition.
    pub start_row: usize,
    /// Column at composition start.
    pub start_col: usize,
}

impl std::fmt::Debug for PreeditState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreeditState")
            .field("text", &format_args!("<redacted, {} bytes>", self.text.len()))
            .field("caret", &self.caret)
            .field("start_row", &self.start_row)
            .field("start_col", &self.start_col)
            .finish()
    }
}

impl PreeditState {
    /// Construct a fresh composition anchored at the supplied cursor cell.
    #[must_use]
    pub const fn new(
        text: String,
        caret: Option<(usize, usize)>,
        start_row: usize,
        start_col: usize,
    ) -> Self {
        Self { text, caret, start_row, start_col }
    }
}

/// Per-frame description of the preedit overlay handed to the paint path.
///
/// Recomputed every frame by [`compute_overlay`] from a [`PreeditState`] and the
/// focused pane's live grid geometry; no caching across frames. An empty
/// `Option<PreeditOverlay>` means the overlay must not be drawn this frame (no
/// composition, focus elsewhere, anchor scrolled off, etc.).
///
/// `Debug` is hand-written to redact `text` for the same reason as
/// [`PreeditState`].
#[derive(Clone)]
pub struct PreeditOverlay {
    /// Window-space origin of the first preedit cell (top-left), in the same
    /// coordinate space the terminal renderer uses for grid cells.
    pub origin_px: [f32; 2],
    /// Cell size of the focused pane in points (`(width, height)`).
    pub cell_px: [f32; 2],
    /// The preedit string to shape and paint exactly like normal grid cells.
    pub text: String,
    /// How many cells the preedit clips to before being truncated (single row).
    /// Truncation drops trailing cells; the underline width matches the rendered
    /// glyph advances.
    pub max_cells: u16,
}

impl std::fmt::Debug for PreeditOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreeditOverlay")
            .field("origin_px", &self.origin_px)
            .field("cell_px", &self.cell_px)
            .field("text", &format_args!("<redacted, {} bytes>", self.text.len()))
            .field("max_cells", &self.max_cells)
            .finish()
    }
}

/// Live grid geometry needed to place the preedit overlay for one frame.
///
/// `viewport_top_abs_row` is the absolute scrollback row index of the topmost
/// visible line; combined with the anchor's absolute [`PreeditState::start_row`]
/// it resolves the on-screen line, so terminal scroll keeps the underline pinned
/// to the originating line. `display_offset > 0` means the viewport is scrolled
/// up into scrollback, where the overlay must not render.
#[derive(Clone, Copy, Debug)]
pub struct PreeditGeometry {
    /// Window-space top-left of the grid's first cell.
    pub grid_origin_px: [f32; 2],
    /// Cell size in points (`(width, height)`).
    pub cell_px: [f32; 2],
    /// Grid width in columns.
    pub columns: u16,
    /// Number of visible rows.
    pub screen_lines: usize,
    /// Rows the viewport is scrolled up into scrollback (0 when at the bottom).
    pub display_offset: usize,
    /// Absolute scrollback row of the topmost visible line.
    pub viewport_top_abs_row: usize,
}

/// Compute the overlay for the current composition, or `None` if it must not be
/// drawn this frame.
///
/// Returns `None` when the viewport is scrolled into scrollback
/// (`display_offset > 0`), when the anchor row is above or below the visible
/// window, or when the anchor column leaves no visible cells. Otherwise the
/// origin is the anchor cell's window-space top-left and `max_cells` is the
/// column budget from the anchor to the grid's right edge.
#[must_use]
pub fn compute_overlay(state: &PreeditState, geom: PreeditGeometry) -> Option<PreeditOverlay> {
    if geom.display_offset > 0 {
        return None;
    }
    let visible_line = state.start_row.checked_sub(geom.viewport_top_abs_row)?;
    if visible_line >= geom.screen_lines {
        return None;
    }
    let max_cells = geom.columns.checked_sub(u16::try_from(state.start_col).ok()?)?;
    if max_cells == 0 {
        return None;
    }
    let start_col = u32_to_f32(u32::try_from(state.start_col).ok()?);
    let visible_line = u32_to_f32(u32::try_from(visible_line).ok()?);
    Some(PreeditOverlay {
        origin_px: [
            geom.grid_origin_px[0] + start_col * geom.cell_px[0],
            geom.grid_origin_px[1] + visible_line * geom.cell_px[1],
        ],
        cell_px: geom.cell_px,
        text: state.text.clone(),
        max_cells,
    })
}

/// Width, in terminal cells, the preedit `text` occupies on its single row.
///
/// Matches the renderer's styled-run advance accumulator: wide glyphs (CJK)
/// reserve two cells, zero-width combining marks ride the previous base glyph,
/// and a leading combining mark with no base is skipped. The result sizes the
/// underline the paint path draws under the composition.
#[must_use]
pub fn preedit_cell_width(text: &str) -> u16 {
    let mut cells: u16 = 0;
    for ch in text.chars() {
        // Zero-width marks ride the previous base glyph (a leading mark has no
        // base and is dropped); an implausible width degrades to nothing.
        if let Ok(w @ 1..) = u16::try_from(UnicodeWidthChar::width(ch).unwrap_or(0)) {
            cells = cells.saturating_add(w);
        }
    }
    cells
}

/// Convert a small unsigned magnitude to `f32` without a lossy `as` cast.
///
/// Column and row indices are bounded by the terminal grid, so the `u32 -> f32`
/// widening is always exact here; the helper keeps the strict cast lints happy.
fn u32_to_f32(value: u32) -> f32 {
    // u16::MAX fits exactly in f32; grid indices never exceed that in practice,
    // but clamp defensively so an absurd value degenerates instead of wrapping.
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

/// Pure IME preedit state machine, independent of GPUI.
///
/// Mirrors the winit client's `WindowEvent::Ime` handling: [`Self::mark`] with
/// non-empty text arms/updates the composition anchored at the last
/// [`Self::set_anchor`] cell, empty text clears it, and [`Self::commit`] /
/// [`Self::clear`] retire it. Callers read [`Self::preedit`] to paint the
/// overlay and act on the committed string [`Self::commit`] returns.
#[derive(Debug, Default)]
pub struct PreeditMachine {
    preedit: Option<PreeditState>,
    anchor: (usize, usize),
}

impl PreeditMachine {
    /// Create an idle machine anchored at the grid origin.
    #[must_use]
    pub const fn new() -> Self {
        Self { preedit: None, anchor: (0, 0) }
    }

    /// Record the focused pane's current cursor cell as the composition anchor.
    ///
    /// Applied to the next composition only; an in-flight `PreeditState` keeps
    /// its captured anchor so the overlay stays pinned while composing.
    pub const fn set_anchor(&mut self, row: usize, col: usize) {
        self.anchor = (row, col);
    }

    /// The active composition, if any.
    #[must_use]
    pub const fn preedit(&self) -> Option<&PreeditState> {
        self.preedit.as_ref()
    }

    /// Whether a composition is currently in flight.
    #[must_use]
    pub const fn is_composing(&self) -> bool {
        self.preedit.is_some()
    }

    /// Apply a marked-text update. Non-empty text arms or updates the
    /// composition (anchored at the last [`Self::set_anchor`] on first arm);
    /// empty text clears it. Returns `true` when the state changed.
    pub fn mark(&mut self, text: &str, caret: Option<(usize, usize)>) -> bool {
        if text.is_empty() {
            return self.clear();
        }
        match self.preedit.as_mut() {
            Some(state) => {
                state.text.clear();
                state.text.push_str(text);
                state.caret = caret;
            }
            None => {
                self.preedit =
                    Some(PreeditState::new(text.to_owned(), caret, self.anchor.0, self.anchor.1));
            }
        }
        true
    }

    /// Commit the composition: clear any in-flight preedit and return the
    /// committed text so the caller can send it through the PTY path. The
    /// committed string comes from the IME event, not the preedit buffer, so a
    /// commit with no active composition still forwards the text.
    pub fn commit(&mut self, text: String) -> String {
        self.preedit = None;
        text
    }

    /// Drop any in-flight composition. Returns `true` if one was present.
    pub fn clear(&mut self) -> bool {
        self.preedit.take().is_some()
    }
}

/// Event emitted by the [`Ime`] entity when the OS IME commits text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImeEvent {
    /// Composition finished; the string must be sent to the focused pane.
    Commit(String),
}

/// GPUI entity wrapping a [`PreeditMachine`] and bridging GPUI's IME plumbing.
///
/// Implements [`gpui::EntityInputHandler`] so the platform layer routes IME
/// marked and committed text here; committed text is re-emitted as
/// [`ImeEvent::Commit`] for the view to send over IPC. The terminal owns no
/// editable buffer, so the text-query methods report an empty document and IME
/// candidate placement is driven purely by the composition anchor.
pub struct Ime {
    machine: PreeditMachine,
}

impl EventEmitter<ImeEvent> for Ime {}

impl Default for Ime {
    fn default() -> Self {
        Self::new()
    }
}

impl Ime {
    /// Create an idle IME entity.
    #[must_use]
    pub const fn new() -> Self {
        Self { machine: PreeditMachine::new() }
    }

    /// Record the focused pane's cursor cell as the next composition anchor.
    pub const fn set_anchor(&mut self, row: usize, col: usize) {
        self.machine.set_anchor(row, col);
    }

    /// The active composition, if any.
    #[must_use]
    pub const fn preedit(&self) -> Option<&PreeditState> {
        self.machine.preedit()
    }

    /// Whether a composition is currently in flight.
    #[must_use]
    pub const fn is_composing(&self) -> bool {
        self.machine.is_composing()
    }

    /// Drop any in-flight composition, returning `true` if one was present.
    ///
    /// The window calls this on focus loss and on a keystroke that reached the
    /// PTY byte encoder: both mean the input method is no longer the thing
    /// producing text, so an overlay left on screen would be stale. GPUI's
    /// xkb-compose path in particular arms a preedit for a dead key and then
    /// delivers the composed character as an ordinary `KeyDown` without ever
    /// retracting the mark, so nothing else would ever clear it.
    pub fn clear(&mut self) -> bool {
        self.machine.clear()
    }
}

impl EntityInputHandler for Ime {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        // The terminal exposes no editable document to the IME.
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // The terminal exposes no editable document, so there is no selection
        // to report — but the X11 backend's candidate-window placement
        // (`get_ime_area`) asks for a selection *first* and gives up on the
        // rect when the answer is `None`, leaving the OS candidate list parked
        // at the window origin. An empty selection at the composition point is
        // what makes the popup follow the cursor cell instead.
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        // The marked (composing) text spans the whole preedit buffer.
        self.machine.preedit().map(|state| 0..state.text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.machine.clear() {
            tracing::info!("IME preedit unmarked");
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A committed edit: clear the composition and forward the bytes.
        let committed = self.machine.commit(text.to_owned());
        if !committed.is_empty() {
            // Size only — the composed text itself is redacted everywhere, so
            // the log records that a commit arrived, never what it said.
            tracing::info!(bytes = committed.len(), "IME committed text");
            cx.emit(ImeEvent::Commit(committed));
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A marked (in-progress) edit: update the preedit overlay in place.
        if self.machine.mark(new_text, None) {
            // Size only, for the same reason the commit line is: this is the
            // one place an outside observer can see that the platform reached
            // the handler at all, which is what the IME oracle asserts on.
            tracing::info!(bytes = new_text.len(), "IME preedit updated");
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // `element_bounds` is the focused pane's *cursor cell* rect: the paint
        // path builds the `ElementInputHandler` from the live grid geometry
        // rather than from the whole grid, so the origin here already is the
        // composition point. Zero width plus the cell height puts the platform
        // spot (origin + size) at the cell's bottom-left corner, which is where
        // an X11 candidate list wants to hang.
        Some(Bounds {
            origin: element_bounds.origin,
            size: gpui::size(Pixels::ZERO, element_bounds.size.height),
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PreeditGeometry, PreeditMachine, PreeditState, compute_overlay, preedit_cell_width,
    };

    fn geometry() -> PreeditGeometry {
        PreeditGeometry {
            grid_origin_px: [10.0, 20.0],
            cell_px: [8.0, 18.0],
            columns: 80,
            screen_lines: 24,
            display_offset: 0,
            viewport_top_abs_row: 100,
        }
    }

    // @lat: [[client#GPUI Client Spike#GPUI IME Composition#Preedit Overlay Geometry]]
    #[test]
    fn overlay_places_origin_at_anchor_cell() {
        let state = PreeditState::new("あ".to_owned(), None, 105, 3);
        let overlay = compute_overlay(&state, geometry()).expect("visible anchor yields overlay");
        // visible line = 105 - 100 = 5; origin = grid_origin + (col*w, line*h).
        assert!((overlay.origin_px[0] - (10.0 + 3.0 * 8.0)).abs() < f32::EPSILON);
        assert!((overlay.origin_px[1] - (20.0 + 5.0 * 18.0)).abs() < f32::EPSILON);
        assert_eq!(overlay.max_cells, 80 - 3);
        assert_eq!(overlay.text, "あ");
    }

    // @lat: [[client#GPUI Client Spike#GPUI IME Composition#Preedit Overlay Geometry]]
    #[test]
    fn overlay_hidden_while_scrolled_into_scrollback() {
        let state = PreeditState::new("x".to_owned(), None, 105, 3);
        let geom = PreeditGeometry { display_offset: 4, ..geometry() };
        assert!(compute_overlay(&state, geom).is_none());
    }

    // @lat: [[client#GPUI Client Spike#GPUI IME Composition#Preedit Overlay Geometry]]
    #[test]
    fn overlay_hidden_when_anchor_row_scrolled_above_viewport() {
        // Anchor row 90 is above the topmost visible row 100.
        let state = PreeditState::new("x".to_owned(), None, 90, 0);
        assert!(compute_overlay(&state, geometry()).is_none());
    }

    // @lat: [[client#GPUI Client Spike#GPUI IME Composition#Preedit Overlay Geometry]]
    #[test]
    fn preedit_cell_width_counts_wide_and_combining() {
        // Two ASCII cells.
        assert_eq!(preedit_cell_width("ab"), 2);
        // CJK wide glyph reserves two cells.
        assert_eq!(preedit_cell_width("漢"), 2);
        // Base 'e' + combining acute rides the base: still one cell.
        assert_eq!(preedit_cell_width("e\u{0301}"), 1);
        // A leading combining mark with no base is skipped: zero cells.
        assert_eq!(preedit_cell_width("\u{0301}"), 0);
    }

    // @lat: [[client#GPUI Client Spike#GPUI IME Composition]]
    #[test]
    fn machine_arms_updates_and_clears_on_empty_mark() {
        let mut machine = PreeditMachine::new();
        machine.set_anchor(105, 3);
        assert!(machine.mark("k", None));
        let state = machine.preedit().expect("armed");
        assert_eq!((state.start_row, state.start_col), (105, 3));
        assert_eq!(state.text, "k");
        // A later anchor move does not shift the in-flight composition.
        machine.set_anchor(200, 40);
        assert!(machine.mark("ka", None));
        let updated = machine.preedit().expect("still composing");
        assert_eq!((updated.start_row, updated.start_col), (105, 3));
        assert_eq!(updated.text, "ka");
        // Empty mark clears.
        assert!(machine.mark("", None));
        assert!(machine.preedit().is_none());
        // Clearing an idle machine reports no change.
        assert!(!machine.mark("", None));
    }

    // @lat: [[client#GPUI Client Spike#GPUI IME Composition]]
    #[test]
    fn machine_commit_clears_and_returns_text() {
        let mut machine = PreeditMachine::new();
        machine.set_anchor(105, 3);
        machine.mark("か", None);
        let committed = machine.commit("課".to_owned());
        assert_eq!(committed, "課");
        assert!(machine.preedit().is_none());
    }
}
