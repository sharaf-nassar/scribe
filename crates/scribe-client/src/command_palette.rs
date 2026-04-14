//! Command palette overlay state.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

const COMMAND_PALETTE_MIN_COLS: usize = 32;
const COMMAND_PALETTE_MAX_COLS: usize = 72;
const COMMAND_PALETTE_MAX_ITEMS: usize = 8;
/// Overlay layout never needs more than this many grid units, which keeps the
/// integer-to-float conversion exact for pixel placement.
const MAX_OVERLAY_GRID_UNITS: usize = 65_535;
type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

pub struct CommandPaletteBuildContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub viewport: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub items: &'a [String],
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

fn overlay_grid_units(units: usize) -> u16 {
    u16::try_from(units.min(MAX_OVERLAY_GRID_UNITS)).unwrap_or(u16::MAX)
}

fn overlay_grid_width(cols: usize, cell_w: f32) -> f32 {
    f32::from(overlay_grid_units(cols)) * cell_w
}

fn overlay_grid_height(rows: usize, cell_h: f32) -> f32 {
    f32::from(overlay_grid_units(rows)) * cell_h
}

fn overlay_grid_y(origin: f32, row: usize, cell_h: f32) -> f32 {
    origin + overlay_grid_height(row, cell_h)
}

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

    pub fn build_instances(&self, ctx: CommandPaletteBuildContext<'_>) {
        let CommandPaletteBuildContext { out, viewport, cell_size, chrome, items, resolve_glyph } =
            ctx;
        if !self.active {
            return;
        }

        let colors = CommandPaletteColors::from_chrome(chrome);
        let Some(layout) = CommandPaletteLayout::new(self, viewport, cell_size, items) else {
            return;
        };

        let mut renderer = CommandPaletteRenderer::new(out, cell_size.0, resolve_glyph);
        push_solid_rect(renderer.out, viewport, colors.backdrop);
        draw_command_palette_frame(renderer.out, &layout, &colors);
        draw_command_palette_text(&mut renderer, &layout, &colors, cell_size.1, items);
    }
}

struct CommandPaletteLayout {
    overlay: Rect,
    visible_items: usize,
    query_text: String,
    selected: usize,
}

impl CommandPaletteLayout {
    fn new(
        palette: &CommandPalette,
        viewport: Rect,
        cell_size: (f32, f32),
        items: &[String],
    ) -> Option<Self> {
        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }

        let visible_items = items.len().min(COMMAND_PALETTE_MAX_ITEMS);
        let query_text = if palette.query.is_empty() {
            String::from("Type a command or profile name")
        } else {
            palette.query.clone()
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
        let overlay_width = overlay_grid_width(overlay_cols, cell_w);
        let overlay_height = overlay_grid_height(overlay_rows, cell_h);
        let overlay = Rect {
            x: viewport.x + ((viewport.width - overlay_width) / 2.0).max(0.0),
            y: viewport.y + ((viewport.height - overlay_height) / 4.0).max(0.0),
            width: overlay_width,
            height: overlay_height,
        };

        Some(Self { overlay, visible_items, query_text, selected: palette.selected })
    }
}

fn draw_command_palette_frame(
    out: &mut Vec<CellInstance>,
    layout: &CommandPaletteLayout,
    colors: &CommandPaletteColors,
) {
    push_solid_rect(out, layout.overlay, colors.bg);
    push_solid_rect(
        out,
        Rect { x: layout.overlay.x, y: layout.overlay.y, width: layout.overlay.width, height: 1.0 },
        colors.border,
    );
    push_solid_rect(
        out,
        Rect {
            x: layout.overlay.x,
            y: layout.overlay.y + layout.overlay.height - 1.0,
            width: layout.overlay.width,
            height: 1.0,
        },
        colors.border,
    );
    push_solid_rect(
        out,
        Rect {
            x: layout.overlay.x,
            y: layout.overlay.y,
            width: 1.0,
            height: layout.overlay.height,
        },
        colors.border,
    );
    push_solid_rect(
        out,
        Rect {
            x: layout.overlay.x + layout.overlay.width - 1.0,
            y: layout.overlay.y,
            width: 1.0,
            height: layout.overlay.height,
        },
        colors.border,
    );
    push_solid_rect(
        out,
        Rect {
            x: layout.overlay.x + 1.0,
            y: layout.overlay.y + (layout.overlay.height / 2.0),
            width: (layout.overlay.width - 2.0).max(0.0),
            height: (layout.overlay.height / 2.0 - 1.0).max(0.0),
        },
        colors.input_bg,
    );
}

fn draw_command_palette_text(
    renderer: &mut CommandPaletteRenderer<'_>,
    layout: &CommandPaletteLayout,
    colors: &CommandPaletteColors,
    cell_h: f32,
    items: &[String],
) {
    let cell_w = renderer.cell_w;
    renderer.emit_text_line(
        "Command Palette",
        layout.overlay.x + cell_w,
        layout.overlay.y,
        TextColors { fg: colors.header_fg, bg: colors.bg },
    );
    renderer.emit_text_line(
        ">",
        layout.overlay.x + cell_w,
        layout.overlay.y + cell_h,
        TextColors { fg: colors.border, bg: colors.input_bg },
    );
    renderer.emit_text_line(
        &layout.query_text,
        layout.overlay.x + 2.0 * cell_w,
        layout.overlay.y + cell_h,
        TextColors {
            fg: if layout.query_text.is_empty() { colors.placeholder_fg } else { colors.query_fg },
            bg: colors.input_bg,
        },
    );

    for (index, item) in items.iter().take(COMMAND_PALETTE_MAX_ITEMS).enumerate() {
        let row_y = overlay_grid_y(layout.overlay.y, index + 2, cell_h);
        let selected = index == layout.selected;
        if selected {
            push_solid_rect(
                renderer.out,
                Rect {
                    x: layout.overlay.x + 1.0,
                    y: row_y,
                    width: (layout.overlay.width - 2.0).max(0.0),
                    height: cell_h,
                },
                colors.selection_bg,
            );
        }
        renderer.emit_text_line(
            item,
            layout.overlay.x + cell_w,
            row_y,
            TextColors {
                fg: if selected { colors.selection_fg } else { colors.item_fg },
                bg: if selected { colors.selection_bg } else { colors.bg },
            },
        );
    }

    if layout.visible_items == 0 {
        renderer.emit_text_line(
            "No matching commands",
            layout.overlay.x + cell_w,
            layout.overlay.y + 2.0 * cell_h,
            TextColors { fg: colors.placeholder_fg, bg: colors.bg },
        );
    }
}

#[derive(Clone, Copy)]
struct TextColors {
    fg: [f32; 4],
    bg: [f32; 4],
}

struct CommandPaletteRenderer<'a> {
    out: &'a mut Vec<CellInstance>,
    cell_w: f32,
    resolve_glyph: &'a mut GlyphResolver<'a>,
}

impl<'a> CommandPaletteRenderer<'a> {
    fn new(
        out: &'a mut Vec<CellInstance>,
        cell_w: f32,
        resolve_glyph: &'a mut GlyphResolver<'a>,
    ) -> Self {
        Self { out, cell_w, resolve_glyph }
    }

    fn emit_text_line(&mut self, text: &str, start_x: f32, y: f32, colors: TextColors) {
        for (idx, ch) in text.chars().enumerate() {
            let (uv_min, uv_max) = (self.resolve_glyph)(ch);
            self.out.push(CellInstance {
                pos: [start_x + overlay_grid_width(idx, self.cell_w), y],
                size: [0.0, 0.0],
                uv_min,
                uv_max,
                fg_color: colors.fg,
                bg_color: colors.bg,
                corner_radius: 0.0,
            });
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
