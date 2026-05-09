//! Split-scroll: pin the live terminal bottom while scrolled up in AI panes.
//!
//! When the user scrolls up in a pane running a supported AI coding tool, the
//! viewport splits into a top portion (scrollback) and a bottom portion
//! (live terminal where the cursor/prompt is). This lets users compose
//! prompts while reading earlier output.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// Minimum number of rows shown in the pinned bottom portion.
const MIN_PIN_ROWS: usize = 3;

/// Default rows reserved for the AI tool's prompt UI block.
///
/// Claude Code, Codex, and Auggie all render a prompt block several rows
/// tall — a status line, permission/help hints, the input box border, and
/// the input row. 8 rows fits the typical block without consuming half the
/// screen, which keeps scrollback readable in the top portion.
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
pub struct SplitScrollState {
    /// Pixel height of the live-bottom pin region (set during rendering).
    pub pin_height: f32,
}

impl SplitScrollState {
    pub fn new() -> Self {
        Self { pin_height: 0.0 }
    }
}

/// Precomputed geometry for the split-scroll viewport.
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

/// Compute the number of rows to pin at the bottom of the screen.
///
/// The pin sits at the bottom of the screen and is sized to fit the AI
/// tool's prompt UI block. The pin's *contents* are translated downward by
/// [`live_cell_y_translation`] so the cursor lands at the last row of the
/// pin regardless of where it actually sits in the live screen — that's
/// what keeps the prompt visible when an AI tool draws it in the top half.
pub fn compute_pin_rows(screen_lines: usize) -> usize {
    let max_rows = screen_lines.saturating_sub(MIN_PIN_ROWS).max(MIN_PIN_ROWS);
    AI_PROMPT_BLOCK_ROWS.clamp(MIN_PIN_ROWS, max_rows)
}

/// Compute the y-pixel shift to apply to live cells so the cursor row
/// lands at the last row of the pin region.
///
/// Without this shift, when the AI tool's cursor is in the upper half of
/// the live screen, the prompt cells fall above the pin rect and get
/// filtered out by [`filter_instances_by_y`] — hiding the prompt entirely
/// while scrolled. With this shift, every live cell is translated so the
/// cursor row lands on the last screen row (the bottom of the pin), and
/// the rows naturally above the cursor stack upward into the pin from
/// there. Rows naturally below the cursor are pushed off-screen and get
/// filtered out instead.
pub fn live_cell_y_translation(cursor_line: usize, screen_lines: usize, cell_h: f32) -> f32 {
    use winit::dpi::Pixel as _;
    let last_row = screen_lines.saturating_sub(1);
    let rows_to_shift = last_row.saturating_sub(cursor_line);
    u32::try_from(rows_to_shift).unwrap_or(u32::MAX).cast::<f32>() * cell_h
}

/// Expand the pinned region upward so the split never starts mid-way through
/// a soft-wrapped logical line, while still leaving room for the top portion.
///
/// In the cursor-anchored model, the pin shows the live rows
/// `[cursor_line - pin_rows + 1, cursor_line]` translated to the bottom of
/// the screen. The "boundary" we walk up from is therefore
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

pub struct SplitScrollChromeRequest<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub geometry: &'a SplitScrollGeometry,
    pub divider_color: [f32; 4],
    pub jump_button_hovered: bool,
    pub accent_color: [f32; 4],
}

/// Filter instances, keeping only those whose `pos[1]` (Y) falls in `[y_min, y_max)`.
pub fn filter_instances_by_y(
    instances: &[CellInstance],
    y_min: f32,
    y_max: f32,
) -> Vec<CellInstance> {
    instances.iter().filter(|inst| inst.pos[1] >= y_min && inst.pos[1] < y_max).copied().collect()
}

/// Render the split-scroll chrome: divider line and jump-to-bottom button.
pub fn render_chrome(request: SplitScrollChromeRequest<'_>) {
    let out = request.out;
    let geo = request.geometry;
    // Divider line.
    push_solid_rect(out, geo.divider, request.divider_color);

    let shadow_rect = inset_rect(geo.jump_button, 0.0, -1.0);
    out.push(rounded_rect(shadow_rect, [0.0, 0.0, 0.0, 0.28], 6.0));

    let frame_color = with_alpha(
        mix_rgb(
            request.divider_color,
            request.accent_color,
            if request.jump_button_hovered { 0.45 } else { 0.26 },
        ),
        if request.jump_button_hovered { 0.98 } else { 0.92 },
    );
    out.push(rounded_rect(geo.jump_button, frame_color, 6.0));

    let body_rect = inset_rect(geo.jump_button, 1.0, 1.0);
    let body_color = with_alpha(
        mix_rgb(
            request.divider_color,
            [0.0, 0.0, 0.0, 1.0],
            if request.jump_button_hovered { 0.82 } else { 0.9 },
        ),
        if request.jump_button_hovered { 0.98 } else { 0.94 },
    );
    out.push(rounded_rect(body_rect, body_color, 5.0));

    let accent_strip = Rect {
        x: body_rect.x + 2.0,
        y: body_rect.y + 2.0,
        width: (body_rect.width - 4.0).max(0.0),
        height: 2.0,
    };
    let accent_strip_color = with_alpha(
        mix_rgb(
            request.accent_color,
            [1.0, 1.0, 1.0, 1.0],
            if request.jump_button_hovered { 0.12 } else { 0.04 },
        ),
        if request.jump_button_hovered { 0.78 } else { 0.52 },
    );
    out.push(rounded_rect(accent_strip, accent_strip_color, 1.0));

    let inner_plate = Rect {
        x: body_rect.x + 2.0,
        y: body_rect.y + 6.0,
        width: (body_rect.width - 4.0).max(0.0),
        height: (body_rect.height - 8.0).max(0.0),
    };
    let inner_plate_color = with_alpha(
        mix_rgb(
            body_color,
            request.accent_color,
            if request.jump_button_hovered { 0.14 } else { 0.08 },
        ),
        if request.jump_button_hovered { 0.98 } else { 0.9 },
    );
    out.push(rounded_rect(inner_plate, inner_plate_color, 4.0));

    let dock_rect = Rect {
        x: geo.jump_button.x + 7.0,
        y: geo.jump_button.y + geo.jump_button.height - 1.0,
        width: 14.0,
        height: 2.0,
    };
    let dock_color =
        with_alpha(request.accent_color, if request.jump_button_hovered { 0.34 } else { 0.2 });
    out.push(rounded_rect(dock_rect, dock_color, 1.0));

    push_jump_arrow(out, geo.jump_button, [0.0, 0.0, 0.0, 0.32], (1.0, 1.0));
    let icon_color = with_alpha(
        mix_rgb(
            request.accent_color,
            [1.0, 1.0, 1.0, 1.0],
            if request.jump_button_hovered { 0.7 } else { 0.54 },
        ),
        if request.jump_button_hovered { 0.98 } else { 0.86 },
    );
    push_jump_arrow(out, geo.jump_button, icon_color, (0.0, 0.0));
}

/// Hit-test the jump-to-bottom button.
pub fn hit_test_jump_btn(geo: &SplitScrollGeometry, x: f32, y: f32) -> bool {
    geo.jump_button.contains(x, y)
}

/// Push a solid-color rectangle.
fn push_solid_rect(out: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4]) {
    out.push(rounded_rect(rect, color, 0.0));
}

fn push_jump_arrow(out: &mut Vec<CellInstance>, button: Rect, color: [f32; 4], offset: (f32, f32)) {
    let (offset_x, offset_y) = offset;
    let baseline = Rect {
        x: button.x + 9.0 + offset_x,
        y: button.y + 17.0 + offset_y,
        width: 10.0,
        height: 2.0,
    };
    let stem = Rect {
        x: button.x + 12.0 + offset_x,
        y: button.y + 5.0 + offset_y,
        width: 4.0,
        height: 8.0,
    };
    let wide_chevron = Rect {
        x: button.x + 8.0 + offset_x,
        y: button.y + 11.0 + offset_y,
        width: 12.0,
        height: 2.0,
    };
    let middle_chevron = Rect {
        x: button.x + 10.0 + offset_x,
        y: button.y + 13.0 + offset_y,
        width: 8.0,
        height: 2.0,
    };
    let arrow_point = Rect {
        x: button.x + 12.0 + offset_x,
        y: button.y + 15.0 + offset_y,
        width: 4.0,
        height: 2.0,
    };

    push_solid_rect(out, baseline, color);
    push_solid_rect(out, stem, color);
    push_solid_rect(out, wide_chevron, color);
    push_solid_rect(out, middle_chevron, color);
    push_solid_rect(out, arrow_point, color);
}

fn rounded_rect(rect: Rect, color: [f32; 4], corner_radius: f32) -> CellInstance {
    CellInstance {
        pos: [rect.x, rect.y],
        size: [rect.width, rect.height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
        corner_radius,
    }
}

fn inset_rect(rect: Rect, inset_x: f32, inset_y: f32) -> Rect {
    Rect {
        x: rect.x + inset_x,
        y: rect.y + inset_y,
        width: (rect.width - inset_x * 2.0).max(0.0),
        height: (rect.height - inset_y * 2.0).max(0.0),
    }
}

fn mix_rgb(a: [f32; 4], b: [f32; 4], amount: f32) -> [f32; 4] {
    let t = amount.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, 1.0]
}

fn with_alpha(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0], color[1], color[2], alpha]
}

fn read_cell_flags(term: &Term<VoidListener>, line: Line, col: Column) -> Flags {
    #[allow(
        clippy::indexing_slicing,
        reason = "alacritty_terminal grid only supports Index trait, no get() alternative"
    )]
    {
        term.grid()[line][col].flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_rows_uses_ai_block_size_when_room() {
        // Comfortable screen: AI_PROMPT_BLOCK_ROWS fits cleanly.
        assert_eq!(compute_pin_rows(30), AI_PROMPT_BLOCK_ROWS);
    }

    #[test]
    fn pin_rows_clamps_when_screen_is_tiny() {
        // Screen barely larger than MIN_PIN_ROWS: pin can't exceed
        // screen_lines - MIN_PIN_ROWS or top portion vanishes.
        assert_eq!(compute_pin_rows(MIN_PIN_ROWS + 1), MIN_PIN_ROWS);
        assert_eq!(compute_pin_rows(0), MIN_PIN_ROWS);
    }

    #[test]
    fn pin_rows_caps_below_screen_minus_min() {
        // 10-row screen: max = 10 - 3 = 7. AI_PROMPT_BLOCK_ROWS=8 > 7, so cap.
        assert_eq!(compute_pin_rows(10), 7);
    }

    // Regression test for the prompt-hidden bug: when the cursor is in the
    // upper half of the live screen, the translation moves the cursor row to
    // the last row of the screen so the prompt stays visible at the bottom of
    // the pin region instead of being filtered out.
    #[test]
    fn translation_moves_cursor_to_last_screen_row_when_cursor_high() {
        // Cursor at line 5 of a 30-row screen, cell_h = 16.0.
        // The cursor row should be shifted by (30 - 1 - 5) = 24 rows.
        let shift = live_cell_y_translation(5, 30, 16.0);
        assert!((shift - 24.0 * 16.0).abs() < f32::EPSILON, "expected 24*16 = 384.0, got {shift}",);
    }

    #[test]
    fn translation_is_zero_when_cursor_already_on_last_row() {
        // Cursor on the last visible row (line 29 of 30) — no shift needed,
        // matches the original "pin shows bottom rows of live screen" behavior.
        let shift = live_cell_y_translation(29, 30, 16.0);
        assert!(shift.abs() < f32::EPSILON, "expected 0.0, got {shift}");
    }

    #[test]
    fn translation_is_one_row_for_cursor_one_above_bottom() {
        // Cursor at line 28 of 30 → shift cells down 1 row so cursor lands
        // at the last screen row.
        let shift = live_cell_y_translation(28, 30, 16.0);
        assert!((shift - 16.0).abs() < f32::EPSILON, "expected one row (16.0), got {shift}",);
    }

    #[test]
    fn translation_saturates_when_cursor_below_screen() {
        // Defensive: cursor_line >= screen_lines should not underflow. The
        // shift should be 0 (treat cursor as already past the last row).
        let shift = live_cell_y_translation(40, 30, 16.0);
        assert!(shift.abs() < f32::EPSILON, "expected 0.0, got {shift}");
    }

    #[test]
    fn translation_handles_cursor_at_top_of_screen() {
        // Cursor at line 0 → shift = 29 rows down.
        let shift = live_cell_y_translation(0, 30, 16.0);
        assert!((shift - 29.0 * 16.0).abs() < f32::EPSILON, "expected 29*16 = 464.0, got {shift}",);
    }
}
