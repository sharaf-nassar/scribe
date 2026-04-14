//! Right-click context menu overlay rendered as GPU instances.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// Action triggered by selecting a context menu item.
#[derive(Clone)]
pub enum ContextMenuAction {
    /// Copy selection to clipboard.
    Copy,
    /// Paste from clipboard.
    Paste,
    /// Select all text in the pane.
    SelectAll,
    /// Open the given URL.
    OpenUrl(String),
    /// Open the given file path.
    OpenFile(String),
}

/// A single item in the context menu.
pub struct MenuItem {
    pub label: String,
    pub action: ContextMenuAction,
    /// If `false`, the item is greyed out and not clickable.
    pub enabled: bool,
}

/// State for the right-click context menu overlay.
pub struct ContextMenu {
    /// Pixel position of the top-left corner of the menu.
    pub x: f32,
    pub y: f32,
    /// Index of the currently hovered item.
    hovered: Option<usize>,
    /// Menu items to render and respond to.
    pub items: Vec<MenuItem>,
    /// Cached hit rects from the last render (viewport-pixel coords).
    pub item_rects: Vec<Rect>,
}

pub struct ContextMenuBuildContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub viewport: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub resolve_glyph: &'a mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
}

fn menu_grid_units(units: usize) -> u16 {
    u16::try_from(units).unwrap_or(u16::MAX)
}

fn menu_grid_x(origin: f32, col: usize, cell_w: f32) -> f32 {
    origin + f32::from(menu_grid_units(col)) * cell_w
}

fn menu_grid_y(origin: f32, row: usize, cell_h: f32) -> f32 {
    origin + f32::from(menu_grid_units(row)) * cell_h
}

fn menu_grid_width(cols: usize, cell_w: f32) -> f32 {
    f32::from(menu_grid_units(cols)) * cell_w
}

fn menu_grid_height(rows: usize, cell_h: f32) -> f32 {
    f32::from(menu_grid_units(rows)) * cell_h
}

impl ContextMenu {
    /// Build a context menu at `(x, y)`.
    ///
    /// Items are populated based on the current state: `has_selection`
    /// enables Copy, `url` appends an "Open URL" item when present, and
    /// `file_path` appends an "Open File" item when present.
    pub fn new(
        x: f32,
        y: f32,
        has_selection: bool,
        url: Option<String>,
        file_path: Option<String>,
    ) -> Self {
        let mut items = vec![
            MenuItem {
                label: String::from("Copy"),
                action: ContextMenuAction::Copy,
                enabled: has_selection,
            },
            MenuItem {
                label: String::from("Paste"),
                action: ContextMenuAction::Paste,
                enabled: true,
            },
            MenuItem {
                label: String::from("Select All"),
                action: ContextMenuAction::SelectAll,
                enabled: true,
            },
        ];

        if let Some(u) = url {
            items.push(MenuItem {
                label: String::from("Open URL"),
                action: ContextMenuAction::OpenUrl(u),
                enabled: true,
            });
        }

        if let Some(p) = file_path {
            items.push(MenuItem {
                label: String::from("Open File"),
                action: ContextMenuAction::OpenFile(p),
                enabled: true,
            });
        }

        let item_count = items.len();
        let zero_rect = Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
        Self { x, y, hovered: None, items, item_rects: vec![zero_rect; item_count] }
    }

    /// Update hover state from cursor position. Returns `true` if state changed.
    pub fn update_hover(&mut self, x: f32, y: f32) -> bool {
        let prev = self.hovered;
        self.hovered = self.item_rects.iter().position(|r| r.contains(x, y));
        self.hovered != prev
    }

    /// Return the action for the clicked item, or `None` if miss or disabled.
    pub fn click(&self, x: f32, y: f32) -> Option<ContextMenuAction> {
        let idx = self.item_rects.iter().position(|r| r.contains(x, y))?;
        let item = self.items.get(idx)?;
        if item.enabled { Some(item.action.clone()) } else { None }
    }

    /// Return `true` if `(x, y)` falls within the rendered menu bounds.
    ///
    /// Used to decide whether a click outside the menu should dismiss it.
    pub fn click_is_inside(&self, x: f32, y: f32) -> bool {
        self.item_rects.iter().any(|r| r.contains(x, y))
    }

    /// Build GPU instances for the context menu overlay.
    ///
    /// Appends a near-invisible click-capture backdrop (full viewport),
    /// a dark menu box with border, and individual item rows into `out`.
    pub fn build_instances(&mut self, context: ContextMenuBuildContext<'_>) {
        let ContextMenuBuildContext { out, viewport, cell_size, chrome, resolve_glyph } = context;
        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }

        let colors = MenuColors::from_chrome(chrome);
        push_solid_rect(out, viewport, colors.backdrop);

        let Some(layout) = MenuLayout::new(self, viewport, cell_size, &colors) else {
            return;
        };

        push_solid_rect(out, layout.menu_rect, colors.menu_bg);
        draw_menu_frame(out, &layout, colors.border);
        self.item_rects.resize(self.items.len(), Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 });
        let mut renderer =
            MenuRenderer { out, layout: &layout, colors: &colors, cell_size, resolve_glyph };
        renderer.render_items(self);
    }
}

struct MenuLayout {
    menu_rect: Rect,
    menu_cols: usize,
    item_rows: usize,
    has_open_item: bool,
}

#[derive(Clone, Copy)]
struct MenuTextLine<'a> {
    text: &'a str,
    row: usize,
    start_col: usize,
    max_cols: usize,
    fg: [f32; 4],
    bg: [f32; 4],
}

struct MenuRenderer<'a> {
    out: &'a mut Vec<CellInstance>,
    layout: &'a MenuLayout,
    colors: &'a MenuColors,
    cell_size: (f32, f32),
    resolve_glyph: &'a mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
}

impl MenuRenderer<'_> {
    fn render_items(&mut self, menu: &mut ContextMenu) {
        let (cell_w, cell_h) = self.cell_size;
        let mut row = 0usize;

        for (idx, item) in menu.items.iter().enumerate() {
            let is_open_item = matches!(
                item.action,
                ContextMenuAction::OpenUrl(_) | ContextMenuAction::OpenFile(_)
            );
            if is_open_item && self.layout.has_open_item {
                let sep_y = menu_grid_y(self.layout.menu_rect.y, row, cell_h) + cell_h / 2.0;
                push_solid_rect(
                    self.out,
                    Rect {
                        x: self.layout.menu_rect.x + cell_w,
                        y: sep_y,
                        width: self.layout.menu_rect.width - 2.0 * cell_w,
                        height: 1.0,
                    },
                    self.colors.separator,
                );
                row += 1;
            }

            let item_rect = Rect {
                x: self.layout.menu_rect.x,
                y: menu_grid_y(self.layout.menu_rect.y, row, cell_h),
                width: self.layout.menu_rect.width,
                height: menu_grid_height(self.layout.item_rows, cell_h),
            };
            let hovered = menu.hovered == Some(idx);
            if hovered && item.enabled {
                push_solid_rect(self.out, item_rect, self.colors.item_hover_bg);
            }

            let fg = if item.enabled { self.colors.item_fg } else { self.colors.item_disabled_fg };
            let bg = if hovered && item.enabled {
                self.colors.item_hover_bg
            } else {
                self.colors.menu_bg
            };
            self.emit_text_line(MenuTextLine {
                text: &item.label,
                row: row + 1,
                start_col: 2,
                max_cols: self.layout.menu_cols,
                fg,
                bg,
            });

            if let Some(rect) = menu.item_rects.get_mut(idx) {
                *rect = item_rect;
            }
            row += self.layout.item_rows;
        }
    }

    fn emit_text_line(&mut self, line: MenuTextLine<'_>) {
        let (cell_w, cell_h) = self.cell_size;
        let y = menu_grid_y(self.layout.menu_rect.y, line.row, cell_h);

        for (i, ch) in line.text.chars().enumerate() {
            let col = line.start_col + i;
            if col >= line.max_cols {
                break;
            }
            let x = menu_grid_x(self.layout.menu_rect.x, col, cell_w);
            let (uv_min, uv_max) = (self.resolve_glyph)(ch);
            self.out.push(CellInstance {
                pos: [x, y],
                size: [0.0, 0.0],
                uv_min,
                uv_max,
                fg_color: line.fg,
                bg_color: line.bg,
                corner_radius: 0.0,
            });
        }
    }
}

impl MenuLayout {
    fn new(
        menu: &ContextMenu,
        viewport: Rect,
        cell_size: (f32, f32),
        _colors: &MenuColors,
    ) -> Option<Self> {
        let (cell_w, cell_h) = cell_size;
        if menu.items.is_empty() || cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }

        let label_max = menu.items.iter().map(|item| item.label.len()).max().unwrap_or(0);
        let menu_cols = label_max + 4;
        let item_rows = 2;
        let has_open_item = menu.items.iter().any(|item| {
            matches!(item.action, ContextMenuAction::OpenUrl(_) | ContextMenuAction::OpenFile(_))
        });
        let total_rows = menu.items.len() * item_rows + usize::from(has_open_item);

        let menu_w = menu_grid_width(menu_cols, cell_w);
        let menu_h = menu_grid_height(total_rows, cell_h);

        let menu_x = menu.x.min(viewport.x + viewport.width - menu_w).max(viewport.x);
        let menu_y = menu.y.min(viewport.y + viewport.height - menu_h).max(viewport.y);

        Some(Self {
            menu_rect: Rect { x: menu_x, y: menu_y, width: menu_w, height: menu_h },
            menu_cols,
            item_rows,
            has_open_item,
        })
    }
}

fn draw_menu_frame(out: &mut Vec<CellInstance>, layout: &MenuLayout, border: [f32; 4]) {
    let menu = layout.menu_rect;
    push_solid_rect(out, Rect { x: menu.x, y: menu.y, width: menu.width, height: 1.0 }, border);
    push_solid_rect(
        out,
        Rect { x: menu.x, y: menu.y + menu.height - 1.0, width: menu.width, height: 1.0 },
        border,
    );
    push_solid_rect(out, Rect { x: menu.x, y: menu.y, width: 1.0, height: menu.height }, border);
    push_solid_rect(
        out,
        Rect { x: menu.x + menu.width - 1.0, y: menu.y, width: 1.0, height: menu.height },
        border,
    );
}

/// Pre-computed linear-RGB colors for context menu rendering.
struct MenuColors {
    backdrop: [f32; 4],
    menu_bg: [f32; 4],
    border: [f32; 4],
    separator: [f32; 4],
    item_fg: [f32; 4],
    item_disabled_fg: [f32; 4],
    item_hover_bg: [f32; 4],
}

impl MenuColors {
    fn from_chrome(chrome: &ChromeColors) -> Self {
        Self {
            backdrop: [0.0, 0.0, 0.0, 0.01],
            menu_bg: srgb_to_linear_rgba(lighten(chrome.tab_bar_bg, 0.04)),
            border: srgb_to_linear_rgba(with_alpha(chrome.tab_text, 0.20)),
            separator: srgb_to_linear_rgba(with_alpha(chrome.tab_text, 0.12)),
            item_fg: srgb_to_linear_rgba(chrome.tab_text_active),
            item_disabled_fg: srgb_to_linear_rgba(with_alpha(chrome.tab_text, 0.35)),
            item_hover_bg: srgb_to_linear_rgba(with_alpha(chrome.tab_text, 0.12)),
        }
    }
}

/// Push a solid-color rectangle as a single `CellInstance`.
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

/// Lighten an sRGB color by adding `amount` to each RGB channel, clamped to 1.0.
fn lighten(color: [f32; 4], amount: f32) -> [f32; 4] {
    [
        (color.first().copied().unwrap_or(0.0) + amount).min(1.0),
        (color.get(1).copied().unwrap_or(0.0) + amount).min(1.0),
        (color.get(2).copied().unwrap_or(0.0) + amount).min(1.0),
        color.get(3).copied().unwrap_or(1.0),
    ]
}

/// Return a copy of `color` with a new alpha value.
fn with_alpha(color: [f32; 4], new_alpha: f32) -> [f32; 4] {
    [
        color.first().copied().unwrap_or(0.0),
        color.get(1).copied().unwrap_or(0.0),
        color.get(2).copied().unwrap_or(0.0),
        new_alpha,
    ]
}
