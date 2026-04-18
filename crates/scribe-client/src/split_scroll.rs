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
/// Extra live rows above the cursor to keep visible in the pinned region.
///
/// Claude/Codex can draw prompt chrome one row above the input itself, so the
/// old `+ 2` margin could clip the waiting-for-input block when the cursor sat
/// on the last visible row.
const CURSOR_CONTEXT_ROWS: usize = 3;

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

/// Compute the number of rows to pin at the bottom, based on cursor position.
///
/// `cursor_line` is 0-indexed from the top of the visible screen (at
/// `display_offset = 0`).  The result is clamped to `[MIN_PIN_ROWS, max_rows]`.
pub fn compute_pin_rows(cursor_line: usize, screen_lines: usize) -> usize {
    let max_rows = screen_lines / 2;
    // Rows from cursor to bottom, plus a small context margin above the
    // cursor so live prompt chrome stays intact when split-scroll is active.
    let rows = screen_lines.saturating_sub(cursor_line) + CURSOR_CONTEXT_ROWS;
    rows.clamp(MIN_PIN_ROWS, max_rows.max(MIN_PIN_ROWS))
}

/// Compute the number of rows needed to keep the active prompt block intact.
///
/// `history_size` is the number of scrollback rows preceding the live viewport
/// at `display_offset = 0`. `prompt_start_abs` and `input_start_abs` are
/// absolute row indices from the top of scrollback (`0 = oldest line`).
///
/// When a live prompt is active, we prefer its `PromptStart` mark so the
/// pinned region includes the full prompt chrome above the user's input. If
/// that mark is unavailable, fall back to the input start row so at least the
/// editable input block remains visible.
pub fn compute_active_prompt_pin_rows(
    history_size: usize,
    screen_lines: usize,
    prompt_start_abs: Option<usize>,
    input_start_abs: Option<usize>,
) -> Option<usize> {
    if screen_lines == 0 {
        return None;
    }

    let prompt_top_abs = prompt_start_abs.or(input_start_abs)?;
    let live_bottom_abs = history_size.saturating_add(screen_lines.saturating_sub(1));
    let max_rows = screen_lines.saturating_sub(MIN_PIN_ROWS).max(MIN_PIN_ROWS);
    let rows = live_bottom_abs.saturating_sub(prompt_top_abs) + 1;
    Some(rows.clamp(MIN_PIN_ROWS, max_rows))
}

/// Expand the pinned bottom upward so the split never starts mid-way through
/// a soft-wrapped logical line, while still leaving room for the top portion.
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

    let jump_btn_rect = Rect {
        x: top_rect.x + top_rect.width - JUMP_BTN_SIZE - JUMP_BTN_INSET,
        y: top_rect.y + top_rect.height - JUMP_BTN_SIZE - JUMP_BTN_INSET,
        width: JUMP_BTN_SIZE,
        height: JUMP_BTN_SIZE,
    };

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
    pub resolve_glyph: &'a mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
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

    // Jump-to-bottom button background (rounded).
    let btn_bg = if request.jump_button_hovered {
        [request.accent_color[0], request.accent_color[1], request.accent_color[2], 0.35]
    } else {
        [request.accent_color[0], request.accent_color[1], request.accent_color[2], 0.18]
    };
    out.push(CellInstance {
        pos: [geo.jump_button.x, geo.jump_button.y],
        size: [geo.jump_button.width, geo.jump_button.height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: btn_bg,
        bg_color: btn_bg,
        corner_radius: 4.0,
    });

    // Jump icon glyph centered in button.
    let (uv_min, uv_max) = (request.resolve_glyph)(JUMP_ICON);
    let icon_color =
        if request.jump_button_hovered { [1.0, 1.0, 1.0, 0.9] } else { [1.0, 1.0, 1.0, 0.55] };
    // Center glyph in the button.
    let glyph_x = geo.jump_button.x + 3.0;
    let glyph_y = geo.jump_button.y + 1.0;
    out.push(CellInstance {
        pos: [glyph_x, glyph_y],
        size: [0.0, 0.0], // use uniform cell size
        uv_min,
        uv_max,
        fg_color: icon_color,
        bg_color: btn_bg,
        corner_radius: 0.0,
    });
}

/// Hit-test the jump-to-bottom button.
pub fn hit_test_jump_btn(geo: &SplitScrollGeometry, x: f32, y: f32) -> bool {
    geo.jump_button.contains(x, y)
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
    });
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
