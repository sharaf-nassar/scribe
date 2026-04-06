//! Split-scroll: pin the live terminal bottom while scrolled up in AI panes.
//!
//! When the user scrolls up in a pane running Claude Code or Codex, the
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

/// Size of the jump-to-bottom button (pixels).
const JUMP_BTN_SIZE: f32 = 22.0;

/// Inset from the bottom-right corner of the top portion.
const JUMP_BTN_INSET: f32 = 8.0;

/// Divider thickness (pixels).
const DIVIDER_H: f32 = 1.0;

/// Jump-to-bottom icon glyph.
const JUMP_ICON: char = '↓';

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
#[allow(clippy::struct_field_names, reason = "rects are semantically distinct regions")]
pub struct SplitScrollGeometry {
    /// Full content area of the pane (below tab bar / prompt bar).
    #[allow(dead_code, reason = "kept for future scrollbar confinement")]
    pub content_rect: Rect,
    /// The top portion showing scrollback.
    pub top_rect: Rect,
    /// The 1px divider line.
    pub divider_rect: Rect,
    /// The bottom portion showing live terminal.
    pub bottom_rect: Rect,
    /// The jump-to-bottom button rect.
    pub jump_btn_rect: Rect,
}

/// Compute the number of rows to pin at the bottom, based on cursor position.
///
/// `cursor_line` is 0-indexed from the top of the visible screen (at
/// `display_offset = 0`).  The result is clamped to `[MIN_PIN_ROWS, max_rows]`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "screen_lines is a small terminal dimension fitting in usize"
)]
pub fn compute_pin_rows(cursor_line: usize, screen_lines: usize) -> usize {
    let max_rows = screen_lines / 2;
    // Rows from cursor to bottom, plus 2 rows of margin above cursor.
    let rows = screen_lines.saturating_sub(cursor_line) + 2;
    rows.clamp(MIN_PIN_ROWS, max_rows.max(MIN_PIN_ROWS))
}

/// Expand the pinned bottom upward so the split never starts mid-way through
/// a soft-wrapped logical line, while still leaving room for the top portion.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    reason = "screen row indices are bounded by terminal dimensions and fit in i32"
)]
pub fn align_pin_rows_to_logical_lines(
    term: &Term<VoidListener>,
    pin_rows: usize,
    screen_lines: usize,
) -> usize {
    if screen_lines <= MIN_PIN_ROWS {
        return pin_rows.min(screen_lines);
    }

    let max_pin_rows = screen_lines.saturating_sub(MIN_PIN_ROWS).max(MIN_PIN_ROWS);
    let last_col = Column(term.grid().columns().saturating_sub(1));
    let mut aligned_pin_rows = pin_rows.min(max_pin_rows);
    let mut boundary_row = screen_lines.saturating_sub(aligned_pin_rows);

    while boundary_row > 0
        && aligned_pin_rows < max_pin_rows
        && term.grid()[Line(boundary_row as i32 - 1)][last_col].flags.contains(Flags::WRAPLINE)
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

    let jump_btn_rect = Rect {
        x: top_rect.x + top_rect.width - JUMP_BTN_SIZE - JUMP_BTN_INSET,
        y: top_rect.y + top_rect.height - JUMP_BTN_SIZE - JUMP_BTN_INSET,
        width: JUMP_BTN_SIZE,
        height: JUMP_BTN_SIZE,
    };

    SplitScrollGeometry { content_rect, top_rect, divider_rect, bottom_rect, jump_btn_rect }
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
#[allow(
    clippy::too_many_arguments,
    reason = "chrome rendering needs geometry, colors, hover state, and glyph resolver"
)]
pub fn render_chrome(
    out: &mut Vec<CellInstance>,
    geo: &SplitScrollGeometry,
    divider_color: [f32; 4],
    jump_btn_hovered: bool,
    accent_color: [f32; 4],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    // Divider line.
    push_solid_rect(out, geo.divider_rect, divider_color);

    // Jump-to-bottom button background (rounded).
    let btn_bg = if jump_btn_hovered {
        [accent_color[0], accent_color[1], accent_color[2], 0.35]
    } else {
        [accent_color[0], accent_color[1], accent_color[2], 0.18]
    };
    out.push(CellInstance {
        pos: [geo.jump_btn_rect.x, geo.jump_btn_rect.y],
        size: [geo.jump_btn_rect.width, geo.jump_btn_rect.height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: btn_bg,
        bg_color: btn_bg,
        corner_radius: 4.0,
        _pad: 0.0,
    });

    // Jump icon glyph centered in button.
    let (uv_min, uv_max) = resolve_glyph(JUMP_ICON);
    let icon_color = if jump_btn_hovered { [1.0, 1.0, 1.0, 0.9] } else { [1.0, 1.0, 1.0, 0.55] };
    // Center glyph in the button.
    let glyph_x = geo.jump_btn_rect.x + 3.0;
    let glyph_y = geo.jump_btn_rect.y + 1.0;
    out.push(CellInstance {
        pos: [glyph_x, glyph_y],
        size: [0.0, 0.0], // use uniform cell size
        uv_min,
        uv_max,
        fg_color: icon_color,
        bg_color: btn_bg,
        corner_radius: 0.0,
        _pad: 0.0,
    });
}

/// Hit-test the jump-to-bottom button.
pub fn hit_test_jump_btn(geo: &SplitScrollGeometry, x: f32, y: f32) -> bool {
    geo.jump_btn_rect.contains(x, y)
}

/// Push a solid-color rectangle.
fn push_solid_rect(out: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4]) {
    out.push(CellInstance {
        pos: [rect.x, rect.y],
        size: [rect.width, rect.height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
        corner_radius: 0.0,
        _pad: 0.0,
    });
}
