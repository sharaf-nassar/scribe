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

impl ContextMenu {
    /// Build a context menu at `(x, y)`.
    ///
    /// Items are populated based on the current state: `has_selection`
    /// enables Copy, and `url` appends an "Open URL" item when present.
    pub fn new(x: f32, y: f32, has_selection: bool, url: Option<String>) -> Self {
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
    #[allow(
        clippy::too_many_lines,
        reason = "single render builder: backdrop, box, border, items, hover highlight, separator"
    )]
    #[allow(
        clippy::too_many_arguments,
        reason = "needs output vec, viewport, cell size, chrome colors, and glyph resolver"
    )]
    pub fn build_instances(
        &mut self,
        out: &mut Vec<CellInstance>,
        viewport: Rect,
        cell_size: (f32, f32),
        chrome: &ChromeColors,
        resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    ) {
        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }

        let colors = MenuColors::from_chrome(chrome);

        // -- Near-invisible backdrop (captures clicks outside the menu) --
        push_solid_rect(out, viewport, colors.backdrop);

        if self.items.is_empty() {
            return;
        }

        // -- Menu dimensions --
        // Width: widest label + 4 columns padding (2 each side).
        let label_max = self.items.iter().map(|item| item.label.len()).max().unwrap_or(0);
        let menu_cols = label_max + 4;

        // Height: each item is 2 cell rows (0.5 top pad + 1 label + 0.5 bottom pad).
        // We approximate this as 2 rows per item.
        let item_rows: usize = 2;
        let has_url_item =
            self.items.iter().any(|item| matches!(item.action, ContextMenuAction::OpenUrl(_)));
        // Extra row for separator before URL item.
        let separator_rows: usize = usize::from(has_url_item);
        let total_rows = self.items.len() * item_rows + separator_rows;

        #[allow(
            clippy::cast_precision_loss,
            reason = "menu_cols and total_rows are small positive integers fitting in f32"
        )]
        let menu_w = menu_cols as f32 * cell_w;
        #[allow(
            clippy::cast_precision_loss,
            reason = "total_rows is a small positive integer fitting in f32"
        )]
        let menu_h = total_rows as f32 * cell_h;

        // Clamp menu position so it stays within the viewport.
        let menu_x = self.x.min(viewport.x + viewport.width - menu_w).max(viewport.x);
        let menu_y = self.y.min(viewport.y + viewport.height - menu_h).max(viewport.y);

        let menu_rect = Rect { x: menu_x, y: menu_y, width: menu_w, height: menu_h };

        // -- Menu background --
        push_solid_rect(out, menu_rect, colors.menu_bg);

        // -- Border (1px on all four sides) --
        push_solid_rect(
            out,
            Rect { x: menu_x, y: menu_y, width: menu_w, height: 1.0 },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect { x: menu_x, y: menu_y + menu_h - 1.0, width: menu_w, height: 1.0 },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect { x: menu_x, y: menu_y, width: 1.0, height: menu_h },
            colors.border,
        );
        push_solid_rect(
            out,
            Rect { x: menu_x + menu_w - 1.0, y: menu_y, width: 1.0, height: menu_h },
            colors.border,
        );

        // -- Items --
        // Ensure item_rects is the right size.
        self.item_rects.resize(self.items.len(), Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 });

        let mut row: usize = 0;
        for (idx, item) in self.items.iter().enumerate() {
            // Insert separator before the URL item.
            let is_url = matches!(item.action, ContextMenuAction::OpenUrl(_));
            if is_url && has_url_item {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "row is a small positive integer fitting in f32"
                )]
                let sep_y = menu_y + row as f32 * cell_h + cell_h / 2.0;
                let sep_rect = Rect {
                    x: menu_x + cell_w,
                    y: sep_y,
                    width: menu_w - 2.0 * cell_w,
                    height: 1.0,
                };
                push_solid_rect(out, sep_rect, colors.separator);
                row += 1;
            }

            let is_hovered = self.hovered == Some(idx);

            #[allow(
                clippy::cast_precision_loss,
                reason = "row and item_rows are small positive integers fitting in f32"
            )]
            let item_rect = Rect {
                x: menu_x,
                y: menu_y + row as f32 * cell_h,
                width: menu_w,
                height: item_rows as f32 * cell_h,
            };

            // Hover highlight.
            if is_hovered && item.enabled {
                push_solid_rect(out, item_rect, colors.item_hover_bg);
            }

            // Text — vertically centered in the item row (middle of the 2 rows).
            let text_row = row + 1;
            let fg = if item.enabled { colors.item_fg } else { colors.item_disabled_fg };
            let bg = if is_hovered && item.enabled { colors.item_hover_bg } else { colors.menu_bg };

            emit_text_line(
                out,
                &item.label,
                menu_x,
                menu_y,
                text_row,
                2,
                menu_cols,
                cell_size,
                fg,
                bg,
                resolve_glyph,
            );

            #[allow(
                clippy::indexing_slicing,
                reason = "idx < self.items.len() and item_rects was resized to match"
            )]
            {
                self.item_rects[idx] = item_rect;
            }

            row += item_rows;
        }
    }
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
    });
}

/// Emit a line of text as individual character instances.
#[allow(
    clippy::too_many_arguments,
    reason = "needs menu origin, row, column, cell size, colors, and glyph resolver"
)]
#[allow(
    clippy::cast_precision_loss,
    reason = "row/col indices are small positive integers fitting in f32"
)]
fn emit_text_line(
    out: &mut Vec<CellInstance>,
    text: &str,
    menu_x: f32,
    menu_y: f32,
    row: usize,
    start_col: usize,
    max_cols: usize,
    cell_size: (f32, f32),
    fg: [f32; 4],
    bg: [f32; 4],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let (cell_w, cell_h) = cell_size;
    let y = menu_y + row as f32 * cell_h;

    for (i, ch) in text.chars().enumerate() {
        let col = start_col + i;
        if col >= max_cols {
            break;
        }
        let x = menu_x + col as f32 * cell_w;
        let (uv_min, uv_max) = resolve_glyph(ch);
        out.push(CellInstance {
            pos: [x, y],
            size: [0.0, 0.0],
            uv_min,
            uv_max,
            fg_color: fg,
            bg_color: bg,
        });
    }
}

/// Lighten an sRGB color by adding `amount` to each RGB channel, clamped to 1.0.
fn lighten(color: [f32; 4], amount: f32) -> [f32; 4] {
    #[allow(
        clippy::indexing_slicing,
        reason = "fixed-size [f32; 4] array, indices 0-3 always valid"
    )]
    [
        (color[0] + amount).min(1.0),
        (color[1] + amount).min(1.0),
        (color[2] + amount).min(1.0),
        color[3],
    ]
}

/// Return a copy of `color` with a new alpha value.
fn with_alpha(color: [f32; 4], new_alpha: f32) -> [f32; 4] {
    #[allow(
        clippy::indexing_slicing,
        reason = "fixed-size [f32; 4] array, indices 0-2 always valid"
    )]
    [color[0], color[1], color[2], new_alpha]
}
