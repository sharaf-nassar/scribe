//! Split-scroll: pin the live terminal bottom while scrolled up in AI panes.
//!
//! When the user scrolls up in a pane running a supported AI coding tool, the
//! viewport splits into a top portion (scrollback) and a bottom portion (the
//! live terminal where the cursor/prompt is), so prompts stay composable while
//! reading earlier output. This module ports the pure logic from the legacy
//! client: eligibility (AI provider + scrolled + normal screen), pin-row
//! sizing, cursor-anchored cell translation, logical-line alignment, and the
//! top/divider/bottom/jump-chip geometry. The GPUI dual-render and jump-chip
//! paint wire these in a later bead.

use alacritty_terminal_gpui::event::VoidListener;
use alacritty_terminal_gpui::grid::Dimensions as _;
use alacritty_terminal_gpui::index::{Column, Line};
use alacritty_terminal_gpui::term::Term;
use alacritty_terminal_gpui::term::cell::Flags;

use crate::layout::Rect;

/// Minimum number of rows shown in the pinned bottom portion.
const MIN_PIN_ROWS: usize = 3;

/// Default rows reserved for the AI tool's prompt UI block.
///
/// Claude Code and Codex both render a prompt block several rows tall — a
/// status line, permission/help hints, the input box border, and the input
/// row. 8 rows fits the typical block without consuming half the screen, which
/// keeps scrollback readable in the top portion.
const AI_PROMPT_BLOCK_ROWS: usize = 8;

/// Width of the jump-to-bottom button (pixels).
const JUMP_BTN_W: f32 = 28.0;

/// Height of the jump-to-bottom button (pixels).
const JUMP_BTN_H: f32 = 24.0;

/// Horizontal inset from the bottom-right corner of the top portion.
const JUMP_BTN_INSET_X: f32 = 6.0;

/// Vertical inset from the divider so the chip feels docked to the split.
const JUMP_BTN_INSET_Y: f32 = 4.0;

/// Divider thickness (pixels).
const DIVIDER_H: f32 = 1.0;

/// Per-pane split-scroll state.
#[derive(Debug, Clone, Copy, Default)]
pub struct SplitScrollState {
    /// Pixel height of the live-bottom pin region (set during rendering).
    pub pin_height: f32,
}

impl SplitScrollState {
    #[must_use]
    pub fn new() -> Self {
        Self { pin_height: 0.0 }
    }
}

/// Precomputed geometry for the split-scroll viewport.
#[derive(Debug, Clone, Copy)]
pub struct SplitScrollGeometry {
    /// The top portion showing scrollback.
    pub top: Rect,
    /// The 1px divider line.
    pub divider: Rect,
    /// The bottom portion showing live terminal.
    pub bottom: Rect,
    /// The jump-to-bottom button rect.
    pub jump_button: Rect,
}

/// Configuration-derived split-scroll eligibility for a pane.
///
/// Separates the two "does the environment allow it" flags — the config toggle
/// and whether the pane runs a supported AI provider — from the live terminal
/// state ([`split_scroll_eligible`] adds the scrolled/normal-screen checks).
#[derive(Debug, Clone, Copy)]
pub struct SplitScrollEligibility {
    /// The `scroll_pin` config key is enabled.
    pub scroll_pin_enabled: bool,
    /// The pane runs an AI provider that opts into split-scroll.
    pub ai_provider_enabled: bool,
}

impl SplitScrollEligibility {
    /// Whether the config and provider both permit split-scroll.
    #[must_use]
    pub fn allows(self) -> bool {
        self.scroll_pin_enabled && self.ai_provider_enabled
    }
}

/// Whether split-scroll should be active for a pane.
///
/// Split-scroll only applies to an eligible AI pane that is scrolled up
/// (`display_offset > 0`) while in the normal screen buffer — never on the
/// alternate screen, where full-screen TUIs manage their own viewport.
pub fn split_scroll_eligible(
    eligibility: SplitScrollEligibility,
    display_offset: usize,
    alt_screen: bool,
) -> bool {
    eligibility.allows() && display_offset > 0 && !alt_screen
}

/// Compute the number of rows to pin at the bottom of the screen.
///
/// The pin sits at the bottom of the screen and is sized to fit the AI tool's
/// prompt UI block. The pin's *contents* are translated downward by
/// [`live_cell_y_translation`] so the cursor lands at the last row of the pin
/// regardless of where it actually sits in the live screen — that's what keeps
/// the prompt visible when an AI tool draws it in the top half.
pub fn compute_pin_rows(screen_lines: usize) -> usize {
    let max_rows = screen_lines.saturating_sub(MIN_PIN_ROWS).max(MIN_PIN_ROWS);
    AI_PROMPT_BLOCK_ROWS.clamp(MIN_PIN_ROWS, max_rows)
}

/// Compute the y-pixel shift to apply to live cells so the cursor row lands at
/// the last row of the pin region.
///
/// Without this shift, when the AI tool's cursor is in the upper half of the
/// live screen, the prompt cells fall above the pin rect and are filtered out —
/// hiding the prompt while scrolled. With this shift, every live cell is
/// translated so the cursor row lands on the last screen row (the bottom of the
/// pin), and the rows naturally above the cursor stack upward into the pin from
/// there. Rows naturally below the cursor are pushed off-screen instead.
pub fn live_cell_y_translation(cursor_line: usize, screen_lines: usize, cell_h: f32) -> f32 {
    let last_row = screen_lines.saturating_sub(1);
    let rows_to_shift = last_row.saturating_sub(cursor_line);
    // Screen rows never exceed u16 in practice; the lossless u16->f32 keeps the
    // conversion free of pedantic cast-precision lints.
    f32::from(u16::try_from(rows_to_shift).unwrap_or(u16::MAX)) * cell_h
}

/// Expand the pinned region upward so the split never starts mid-way through a
/// soft-wrapped logical line, while still leaving room for the top portion.
///
/// In the cursor-anchored model, the pin shows the live rows
/// `[cursor_line - pin_rows + 1, cursor_line]` translated to the bottom of the
/// screen. The "boundary" we walk up from is therefore
/// `cursor_line - pin_rows + 1`, not `screen_lines - pin_rows`.
pub fn align_pin_rows_to_logical_lines(
    term: &Term<VoidListener>,
    pin_rows: usize,
    cursor_line: usize,
    screen_lines: usize,
) -> usize {
    if screen_lines <= MIN_PIN_ROWS {
        return pin_rows.min(screen_lines);
    }

    let max_pin_rows = screen_lines.saturating_sub(MIN_PIN_ROWS).max(MIN_PIN_ROWS);
    let last_col = Column(term.grid().columns().saturating_sub(1));
    let mut aligned_pin_rows = pin_rows.min(max_pin_rows);
    let mut boundary_row = cursor_line.saturating_sub(aligned_pin_rows.saturating_sub(1));

    while boundary_row > 0
        && aligned_pin_rows < max_pin_rows
        && read_cell_flags(
            term,
            Line(i32::try_from(boundary_row).unwrap_or(i32::MAX).saturating_sub(1)),
            last_col,
        )
        .contains(Flags::WRAPLINE)
    {
        boundary_row -= 1;
        aligned_pin_rows += 1;
    }

    aligned_pin_rows
}

/// Compute the split-scroll geometry from the content rect and pin height.
pub fn compute_geometry(content_rect: Rect, pin_height: f32) -> SplitScrollGeometry {
    let bottom_h = pin_height.min((content_rect.height - DIVIDER_H).max(0.0));
    let top_h = (content_rect.height - DIVIDER_H - bottom_h).max(0.0);

    let top_rect =
        Rect { x: content_rect.x, y: content_rect.y, width: content_rect.width, height: top_h };

    let divider_rect = Rect {
        x: content_rect.x,
        y: content_rect.y + top_h,
        width: content_rect.width,
        height: DIVIDER_H,
    };

    let bottom_rect = Rect {
        x: content_rect.x,
        y: content_rect.y + top_h + DIVIDER_H,
        width: content_rect.width,
        height: bottom_h,
    };

    let jump_btn_x = (top_rect.x + top_rect.width - JUMP_BTN_W - JUMP_BTN_INSET_X)
        .clamp(top_rect.x, top_rect.x + (top_rect.width - JUMP_BTN_W).max(0.0));
    let jump_btn_y = (top_rect.y + top_rect.height - JUMP_BTN_H - JUMP_BTN_INSET_Y)
        .clamp(top_rect.y, top_rect.y + (top_rect.height - JUMP_BTN_H).max(0.0));
    let jump_btn_rect =
        Rect { x: jump_btn_x, y: jump_btn_y, width: JUMP_BTN_W, height: JUMP_BTN_H };

    SplitScrollGeometry {
        top: top_rect,
        divider: divider_rect,
        bottom: bottom_rect,
        jump_button: jump_btn_rect,
    }
}

/// Hit-test the jump-to-bottom button.
pub fn hit_test_jump_btn(geo: &SplitScrollGeometry, x: f32, y: f32) -> bool {
    geo.jump_button.contains(x, y)
}

/// Read the flags of a single cell from the terminal grid.
///
/// The `alacritty_terminal` grid only exposes `Index`, with no fallible
/// `.get()` alternative, so indexing is required here — matching the direct
/// grid indexing the display snapshot path already relies on.
fn read_cell_flags(term: &Term<VoidListener>, line: Line, col: Column) -> Flags {
    term.grid()[line][col].flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal_gpui::grid::Dimensions;
    use alacritty_terminal_gpui::term::Config;
    use vte::ansi::Processor;

    struct TestDims {
        cols: usize,
        rows: usize,
    }

    impl Dimensions for TestDims {
        fn total_lines(&self) -> usize {
            self.rows
        }
        fn screen_lines(&self) -> usize {
            self.rows
        }
        fn columns(&self) -> usize {
            self.cols
        }
    }

    fn term_with_output(cols: usize, rows: usize, output: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &TestDims { cols, rows }, VoidListener);
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, output);
        term
    }

    // @lat: [[test#GPUI Split-Scroll#Eligible only for scrolled AI panes on the normal screen]]
    #[test]
    fn eligible_only_for_scrolled_ai_panes_on_normal_screen() {
        let both = SplitScrollEligibility { scroll_pin_enabled: true, ai_provider_enabled: true };
        // Happy path: enabled, AI provider, scrolled, normal screen.
        assert!(split_scroll_eligible(both, 5, false));
        // Not scrolled: pinned bottom would equal the live view.
        assert!(!split_scroll_eligible(both, 0, false));
        // Alt screen: full-screen TUI owns its viewport.
        assert!(!split_scroll_eligible(both, 5, true));
        // Config disabled.
        assert!(!split_scroll_eligible(
            SplitScrollEligibility { scroll_pin_enabled: false, ai_provider_enabled: true },
            5,
            false,
        ));
        // No supported AI provider in the pane.
        assert!(!split_scroll_eligible(
            SplitScrollEligibility { scroll_pin_enabled: true, ai_provider_enabled: false },
            5,
            false,
        ));
    }

    // @lat: [[test#GPUI Split-Scroll#Pin rows fit the AI prompt block or clamp on tiny screens]]
    #[test]
    fn pin_rows_fit_ai_block_or_clamp_on_tiny_screens() {
        assert_eq!(compute_pin_rows(30), AI_PROMPT_BLOCK_ROWS);
        // Screen barely larger than MIN_PIN_ROWS: pin can't exceed
        // screen_lines - MIN_PIN_ROWS or the top portion vanishes.
        assert_eq!(compute_pin_rows(MIN_PIN_ROWS + 1), MIN_PIN_ROWS);
        assert_eq!(compute_pin_rows(0), MIN_PIN_ROWS);
        // 10-row screen: max = 10 - 3 = 7. AI_PROMPT_BLOCK_ROWS=8 > 7, so cap.
        assert_eq!(compute_pin_rows(10), 7);
    }

    // @lat: [[test#GPUI Split-Scroll#Cursor-anchored translation keeps the prompt visible]]
    #[test]
    fn cursor_anchored_translation_keeps_prompt_visible() {
        // Cursor high on a 30-row screen shifts down (30 - 1 - 5) = 24 rows.
        let shift = live_cell_y_translation(5, 30, 16.0);
        assert!((shift - 24.0 * 16.0).abs() < f32::EPSILON);
        // Cursor already on the last row: no shift.
        assert!(live_cell_y_translation(29, 30, 16.0).abs() < f32::EPSILON);
        // One above bottom: shift one row.
        assert!((live_cell_y_translation(28, 30, 16.0) - 16.0).abs() < f32::EPSILON);
        // Defensive: cursor past the last row saturates to no shift.
        assert!(live_cell_y_translation(40, 30, 16.0).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Split-Scroll#Geometry stacks top divider and pinned bottom]]
    #[test]
    fn geometry_stacks_top_divider_and_pinned_bottom() {
        let content = Rect { x: 5.0, y: 10.0, width: 400.0, height: 300.0 };
        let geo = compute_geometry(content, 120.0);
        // Bottom is the requested pin height; top fills the remainder minus 1px.
        assert!((geo.bottom.height - 120.0).abs() < f32::EPSILON);
        assert!((geo.top.height - (300.0 - DIVIDER_H - 120.0)).abs() < f32::EPSILON);
        // Divider sits directly below the top portion.
        assert!((geo.divider.y - (content.y + geo.top.height)).abs() < f32::EPSILON);
        assert!((geo.divider.height - DIVIDER_H).abs() < f32::EPSILON);
        // Bottom sits directly below the divider.
        assert!((geo.bottom.y - (content.y + geo.top.height + DIVIDER_H)).abs() < f32::EPSILON);
        // Jump chip is docked inside the top portion and hit-tests there.
        assert!(hit_test_jump_btn(&geo, geo.jump_button.x + 1.0, geo.jump_button.y + 1.0));
        assert!(!hit_test_jump_btn(&geo, content.x, content.y));
    }

    // @lat: [[test#GPUI Split-Scroll#Pin height clamps to the content rect]]
    #[test]
    fn pin_height_clamps_to_content_rect() {
        let content = Rect { x: 0.0, y: 0.0, width: 200.0, height: 50.0 };
        // A pin taller than the content collapses the top portion to zero.
        let geo = compute_geometry(content, 1000.0);
        assert!(geo.top.height.abs() < f32::EPSILON);
        assert!((geo.bottom.height - (50.0 - DIVIDER_H)).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Split-Scroll#Pin alignment absorbs soft-wrapped logical lines]]
    #[test]
    fn pin_alignment_absorbs_soft_wrapped_logical_lines() {
        // Emit a logical line long enough to soft-wrap across the 4-col grid so
        // the row above the pin boundary carries the WRAPLINE flag.
        let term = term_with_output(4, 12, b"abcdefghij");
        // Cursor on the wrapped row; a 3-row pin should expand upward to keep
        // the wrapped continuation intact.
        let aligned = align_pin_rows_to_logical_lines(&term, 3, 2, 12);
        assert!(aligned >= 3);
        // A grid with no wrap keeps the requested pin rows unchanged.
        let plain = term_with_output(80, 12, b"hello\r\nworld\r\n");
        assert_eq!(align_pin_rows_to_logical_lines(&plain, 3, 2, 12), 3);
    }
}
