//! IME composition state and per-frame overlay description.
//!
//! Holds the transient preedit string published by the OS IME via
//! `winit::event::Ime::Preedit` together with the cursor cell where the
//! composition began. Both records are pure data — owned by `App`, populated
//! by the `WindowEvent::Ime` arm, and read by the renderer hook per frame.
//!
//! Design references:
//! - `specs/008-ime-composition/data-model.md#PreeditState`
//! - `specs/008-ime-composition/contracts/ime-pipeline.md#Preedit overlay request`
//! - `specs/008-ime-composition/research.md#R4` (rendering approach)
//!
//! No PTY bytes flow through this module; preedit text is never written to
//! scrollback or persisted.

/// In-progress IME composition for the focused pane.
///
/// At most one instance lives on `App` at a time. Created on the first
/// non-empty `Ime::Preedit` after a clear (the cursor cell at that moment
/// is captured as the anchor); updated on subsequent non-empty `Preedit`
/// events; dropped on empty `Preedit`, `Commit`, `Disabled`, focus loss,
/// or focused-pane change.
///
/// `Debug` is implemented by hand to redact `text` — preedit content is
/// transient user input from the OS IME and must not leak into tracing,
/// panic backtraces, or log output.
#[derive(Clone)]
pub struct PreeditState {
    /// Current preedit text from the most recent `Ime::Preedit` event (UTF-8).
    pub text: String,
    /// Byte-range caret hint reported by the IME (active segment in
    /// multi-segment composition), when present. Captured for data-model
    /// alignment; the v1 Alacritty-minimal renderer ignores it.
    pub caret: Option<(usize, usize)>,
    /// Absolute scrollback row where composition began. Held stable for the
    /// lifetime of the `PreeditState` so the overlay does not shift if the
    /// grid scrolls underneath the composition.
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

/// Per-frame description of the preedit overlay handed to the renderer hook.
///
/// Recomputed every frame from `PreeditState` and the focused pane's layout;
/// no caching across frames. Empty `Option<PreeditOverlay>` means the overlay
/// must not be drawn this frame (no composition, focus elsewhere, anchor
/// scrolled off, etc.).
///
/// `Debug` is implemented by hand to redact `text` — the overlay carries the
/// same transient preedit content as `PreeditState` and must not leak into
/// tracing, panic backtraces, or log output.
#[derive(Clone)]
pub struct PreeditOverlay {
    /// Window-space origin of the first preedit cell (top-left), in the
    /// same coordinate space the terminal renderer uses for grid cells.
    pub origin_px: [f32; 2],
    /// Cell size of the focused pane in points (`(width, height)`).
    pub cell_px: [f32; 2],
    /// The preedit string to shape via cosmic-text exactly like normal
    /// grid cells.
    pub text: String,
    /// How many cells the preedit clips to before being truncated (single
    /// row). Truncation drops trailing cells; underline width matches the
    /// rendered glyphs.
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
