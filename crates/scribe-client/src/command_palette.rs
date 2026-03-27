//! Command palette overlay state.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

const COMMAND_PALETTE_MIN_COLS: usize = 32;
const COMMAND_PALETTE_MAX_COLS: usize = 72;
const COMMAND_PALETTE_MAX_ITEMS: usize = 8;

pub struct CommandPalette {
    active: bool,
    query: String,
    selected: usize,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self { active: false, query: String::new(), selected: 0 }
    }

    pub fn open(&mut self) {
        self.active = true;
        self.query.clear();
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.selected = 0;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    pub fn clear_query(&mut self) {
        self.query.clear();
        self.selected = 0;
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn clamp_selection(&mut self, count: usize) {
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    pub fn next_item(&mut self, count: usize) {
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected + 1) % count;
        }
    }

    pub fn prev_item(&mut self, count: usize) {
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = (self.selected + count - 1) % count;
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "overlay builder needs output vec, viewport, cell size, chrome colors, and glyph resolver"
    )]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "overlay dimensions are derived from viewport and cell sizes and fit within usize/f32 bounds"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "overlay builder emits multiple rows of text, background, border, and selection states"
    )]
    pub fn build_instances(
        &self,
        out: &mut Vec<CellInstance>,
        viewport: Rect,
        cell_size: (f32, f32),
        chrome: &ChromeColors,
        items: &[String],
        resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    ) {
        if !self.active {
            return;
        }

        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }

        let colors = CommandPaletteColors::from_chrome(chrome);
        let visible_items = items.len().min(COMMAND_PALETTE_MAX_ITEMS);
        let query_text = if self.query.is_empty() {
            String::from("Type a command or profile name")
        } else {
            self.query.clone()
        };

        let max_text_cols = items
            .iter()
            .take(COMMAND_PALETTE_MAX_ITEMS)
            .map(|item| item.chars().count())
            .chain(std::iter::once(query_text.chars().count() + 2))
            .max()
            .unwrap_or(COMMAND_PALETTE_MIN_COLS);
        let overlay_cols = max_text_cols.clamp(COMMAND_PALETTE_MIN_COLS, COMMAND_PALETTE_MAX_COLS);
        let overlay_rows = visible_items + 2;
        let overlay_width = overlay_cols as f32 * cell_w;
        let overlay_height = overlay_rows as f32 * cell_h;
        let overlay = Rect {
            x: viewport.x + ((viewport.width - overlay_width) / 2.0).max(0.0),
            y: viewport.y + ((viewport.height - overlay_height) / 4.0).max(0.0),
            width: overlay_width,
            height: overlay_height,
        };

        push_solid_rect(out, viewport, colors.backdrop);
        push_solid_rect(out, overlay, colors.bg);
        push_solid_rect(
            out,
            Rect { x: overlay.x, y: overlay.y, width: overlay.width, height: 1.0 },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect {
                x: overlay.x,
                y: overlay.y + overlay.height - 1.0,
                width: overlay.width,
                height: 1.0,
            },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect { x: overlay.x, y: overlay.y, width: 1.0, height: overlay.height },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect {
                x: overlay.x + overlay.width - 1.0,
                y: overlay.y,
                width: 1.0,
                height: overlay.height,
            },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect {
                x: overlay.x + 1.0,
                y: overlay.y + cell_h,
                width: (overlay.width - 2.0).max(0.0),
                height: (cell_h - 1.0).max(0.0),
            },
            colors.input_bg,
        );

        emit_text_line(
            out,
            "Command Palette",
            overlay.x + cell_w,
            overlay.y,
            colors.header_fg,
            colors.bg,
            cell_w,
            resolve_glyph,
        );
        emit_text_line(
            out,
            ">",
            overlay.x + cell_w,
            overlay.y + cell_h,
            colors.border,
            colors.input_bg,
            cell_w,
            resolve_glyph,
        );
        emit_text_line(
            out,
            &query_text,
            overlay.x + 2.0 * cell_w,
            overlay.y + cell_h,
            if self.query.is_empty() { colors.placeholder_fg } else { colors.query_fg },
            colors.input_bg,
            cell_w,
            resolve_glyph,
        );

        for (index, item) in items.iter().take(COMMAND_PALETTE_MAX_ITEMS).enumerate() {
            let row_y = overlay.y + (index + 2) as f32 * cell_h;
            let selected = index == self.selected;
            if selected {
                push_solid_rect(
                    out,
                    Rect {
                        x: overlay.x + 1.0,
                        y: row_y,
                        width: (overlay.width - 2.0).max(0.0),
                        height: cell_h,
                    },
                    colors.selection_bg,
                );
            }
            emit_text_line(
                out,
                item,
                overlay.x + cell_w,
                row_y,
                if selected { colors.selection_fg } else { colors.item_fg },
                if selected { colors.selection_bg } else { colors.bg },
                cell_w,
                resolve_glyph,
            );
        }

        if visible_items == 0 {
            emit_text_line(
                out,
                "No matching commands",
                overlay.x + cell_w,
                overlay.y + 2.0 * cell_h,
                colors.placeholder_fg,
                colors.bg,
                cell_w,
                resolve_glyph,
            );
        }
    }
}

struct CommandPaletteColors {
    backdrop: [f32; 4],
    bg: [f32; 4],
    input_bg: [f32; 4],
    border: [f32; 4],
    header_fg: [f32; 4],
    placeholder_fg: [f32; 4],
    query_fg: [f32; 4],
    item_fg: [f32; 4],
    selection_bg: [f32; 4],
    selection_fg: [f32; 4],
}

impl CommandPaletteColors {
    fn from_chrome(chrome: &ChromeColors) -> Self {
        let mut bg = srgb_to_linear_rgba(chrome.tab_bar_active_bg);
        bg[3] = 0.94;

        let mut input_bg = srgb_to_linear_rgba(chrome.status_bar_bg);
        input_bg[3] = 0.98;

        let border = srgb_to_linear_rgba(chrome.accent);
        let header_fg = srgb_to_linear_rgba(chrome.tab_text_active);
        let item_fg = srgb_to_linear_rgba(chrome.tab_text_active);
        let query_fg = srgb_to_linear_rgba(chrome.status_bar_text);
        let mut placeholder_fg = query_fg;
        placeholder_fg[3] *= 0.7;
        let mut active_row_bg = srgb_to_linear_rgba(chrome.status_bar_bg);
        active_row_bg[3] = 1.0;
        let active_text = srgb_to_linear_rgba(chrome.tab_text_active);

        Self {
            backdrop: [0.0, 0.0, 0.0, 0.18],
            bg,
            input_bg,
            border,
            header_fg,
            placeholder_fg,
            query_fg,
            item_fg,
            selection_bg: active_row_bg,
            selection_fg: active_text,
        }
    }
}

fn push_solid_rect(out: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4]) {
    out.push(scribe_renderer::chrome::solid_quad(rect.x, rect.y, rect.width, rect.height, color));
}

#[allow(
    clippy::too_many_arguments,
    reason = "text emission needs the output buffer, layout coordinates, colors, and glyph resolver"
)]
#[allow(
    clippy::cast_precision_loss,
    reason = "command palette rows are bounded to a handful of columns and safely fit in f32"
)]
fn emit_text_line(
    out: &mut Vec<CellInstance>,
    text: &str,
    start_x: f32,
    y: f32,
    fg: [f32; 4],
    bg: [f32; 4],
    cell_w: f32,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    for (idx, ch) in text.chars().enumerate() {
        let (uv_min, uv_max) = resolve_glyph(ch);
        out.push(CellInstance {
            pos: [start_x + idx as f32 * cell_w, y],
            size: [0.0, 0.0],
            uv_min,
            uv_max,
            fg_color: fg,
            bg_color: bg,
            corner_radius: 0.0,
            _pad: 0.0,
        });
    }
}
