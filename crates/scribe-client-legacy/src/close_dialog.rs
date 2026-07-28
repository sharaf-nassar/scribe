//! In-app GPU-rendered close dialog overlay.
//!
//! Replaces the external zenity-based dialog with a themed modal that
//! renders as [`CellInstance`] quads in the same GPU pass as the terminal.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// What the user chose in the close dialog.
#[derive(Clone, Copy)]
pub enum CloseAction {
    /// Quit Scribe (all windows close, sessions preserved).
    QuitAll,
    /// Kill this window only (sessions terminated).
    CloseWindow,
    /// Dismiss the dialog.
    Cancel,
}

/// Index of the currently focused button.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonIndex {
    QuitScribe = 0,
    KillWindow = 1,
    Cancel = 2,
}

impl ButtonIndex {
    fn next(self) -> Self {
        match self {
            Self::QuitScribe => Self::KillWindow,
            Self::KillWindow => Self::Cancel,
            Self::Cancel => Self::QuitScribe,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::QuitScribe => Self::Cancel,
            Self::KillWindow => Self::QuitScribe,
            Self::Cancel => Self::KillWindow,
        }
    }

    fn to_action(self) -> CloseAction {
        match self {
            Self::QuitScribe => CloseAction::QuitAll,
            Self::KillWindow => CloseAction::CloseWindow,
            Self::Cancel => CloseAction::Cancel,
        }
    }
}

const BUTTON_COUNT: usize = 3;
type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

/// Labels for each button, in order matching [`ButtonIndex`].
const BUTTON_LABELS: [&str; BUTTON_COUNT] = ["Quit Scribe", "Kill Window", "Cancel"];

/// Minimum number of columns the dialog can shrink to (ensures buttons fit).
const MIN_DIALOG_COLS: usize = 36;

/// Horizontal padding inside the dialog (columns from each edge).
const PADDING: usize = 3;

/// Height of each button in cell rows (top pad + label + bottom pad).
const BUTTON_HEIGHT_ROWS: usize = 3;
/// Dialog layout never needs more than this many grid units, which keeps the
/// integer-to-float conversion exact for pixel placement.
const MAX_DIALOG_GRID_UNITS: usize = 65_535;

pub struct CloseDialogBuildContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub viewport: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

fn dialog_grid_units(units: usize) -> u16 {
    u16::try_from(units).unwrap_or(u16::MAX)
}

fn dialog_grid_x(origin: f32, col: usize, cell_w: f32) -> f32 {
    origin + f32::from(dialog_grid_units(col)) * cell_w
}

fn dialog_grid_y(origin: f32, row: usize, cell_h: f32) -> f32 {
    origin + f32::from(dialog_grid_units(row)) * cell_h
}

fn dialog_grid_width(cols: usize, cell_w: f32) -> f32 {
    f32::from(dialog_grid_units(cols)) * cell_w
}

fn dialog_grid_height(rows: usize, cell_h: f32) -> f32 {
    f32::from(dialog_grid_units(rows)) * cell_h
}

fn dialog_units_in_extent(extent: f32, unit: f32) -> usize {
    if unit <= 0.0 || !extent.is_finite() || extent <= 0.0 {
        return 0;
    }

    let mut low = 0usize;
    let mut high = 1usize;
    while high < MAX_DIALOG_GRID_UNITS && dialog_grid_width(high, unit) <= extent {
        low = high;
        high = high.saturating_mul(2).min(MAX_DIALOG_GRID_UNITS);
        if high == low {
            break;
        }
    }

    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if dialog_grid_width(mid, unit) <= extent {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    low
}

/// State for the in-app close dialog overlay.
pub struct CloseDialog {
    /// Number of active sessions (shown in the warning message).
    session_count: usize,
    /// Currently keyboard-focused button.
    focused: ButtonIndex,
    /// Button the mouse is currently hovering (if any).
    hovered: Option<usize>,
    /// Cached button hit rects from the last render (viewport-pixel coords).
    button_rects: [Rect; BUTTON_COUNT],
}

impl CloseDialog {
    pub fn new(session_count: usize) -> Self {
        Self {
            session_count,
            focused: ButtonIndex::Cancel,
            hovered: None,
            button_rects: [Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }; BUTTON_COUNT],
        }
    }

    /// Cycle focus to the next button.
    pub fn focus_next(&mut self) {
        self.focused = self.focused.next();
    }

    /// Cycle focus to the previous button.
    pub fn focus_prev(&mut self) {
        self.focused = self.focused.prev();
    }

    /// Confirm the currently focused button.
    pub fn confirm(&self) -> CloseAction {
        self.focused.to_action()
    }

    /// Update hover state from cursor position. Returns `true` if the state changed.
    pub fn update_hover(&mut self, x: f32, y: f32) -> bool {
        let prev = self.hovered;
        self.hovered = self.button_rects.iter().position(|r| r.contains(x, y));
        self.hovered != prev
    }

    /// Handle a mouse click at `(x, y)`. Returns `Some(action)` if a button was clicked.
    pub fn click(&self, x: f32, y: f32) -> Option<CloseAction> {
        let idx = self.button_rects.iter().position(|r| r.contains(x, y))?;
        let button = match idx {
            0 => ButtonIndex::QuitScribe,
            1 => ButtonIndex::KillWindow,
            2 => ButtonIndex::Cancel,
            _ => return None,
        };
        Some(button.to_action())
    }

    /// Build GPU instances for the dialog overlay.
    ///
    /// Appends a full-viewport backdrop and a centered dialog box with title,
    /// description, separator, and buttons into `out`.
    pub fn build_instances(&mut self, ctx: CloseDialogBuildContext<'_>) {
        let CloseDialogBuildContext { out, viewport, cell_size, chrome, resolve_glyph } = ctx;
        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }

        let colors = DialogColors::from_chrome(chrome);
        let layout = DialogLayout::new(viewport, cell_size, self.body_lines(MIN_DIALOG_COLS));
        let mut renderer = DialogRenderer::new(out, &layout, cell_size, resolve_glyph);

        renderer.push_solid_rect(viewport, colors.backdrop);
        renderer.push_solid_rect(layout.dialog_rect, colors.dialog_bg);
        renderer.draw_frame(colors.border);
        renderer
            .draw_title("Close Scribe", TextColors { fg: colors.title_fg, bg: colors.dialog_bg });
        renderer.draw_body(TextColors { fg: colors.body_fg, bg: colors.dialog_bg });
        renderer.draw_separator(colors.separator);
        self.build_buttons(&mut renderer, &colors);
    }

    /// Build the three action buttons with proper padding and per-button colors.
    fn build_buttons(&mut self, renderer: &mut DialogRenderer<'_, '_>, colors: &DialogColors) {
        let (cell_w, cell_h) = renderer.cell_size;
        let dialog_x = renderer.layout.dialog_rect.x;
        let dialog_y = renderer.layout.dialog_rect.y;
        let button_row = renderer.layout.button_row;
        let dialog_cols = renderer.layout.dialog_cols;

        // Each button: 2 padding + label + 2 padding.
        let btn_col_widths: Vec<usize> = BUTTON_LABELS.iter().map(|l| l.len() + 4).collect();
        let total_btn_cols: usize = btn_col_widths.iter().sum();
        let usable = dialog_cols.saturating_sub(PADDING * 2);
        let remaining = usable.saturating_sub(total_btn_cols);
        let gap = if BUTTON_COUNT > 1 { remaining / (BUTTON_COUNT - 1) } else { 0 };

        let button_y = dialog_grid_y(dialog_y, button_row, cell_h);
        let button_h = dialog_grid_height(BUTTON_HEIGHT_ROWS, cell_h);

        let mut col = PADDING;
        for (btn_idx, label) in BUTTON_LABELS.iter().enumerate() {
            let Some(btn_w_cols) = btn_col_widths.get(btn_idx).copied() else {
                continue;
            };
            let is_focused = self.focused as usize == btn_idx;
            let is_hovered = self.hovered == Some(btn_idx);
            let active = is_focused || is_hovered;

            // Per-button color scheme: Cancel = subtle, Quit = accent, Kill = destructive.
            let (fg, bg) = button_colors(btn_idx, active, colors);

            // Button background rect (spans BUTTON_HEIGHT_ROWS).
            let btn_rect = Rect {
                x: dialog_grid_x(dialog_x, col, cell_w),
                y: button_y,
                width: dialog_grid_width(btn_w_cols, cell_w),
                height: button_h,
            };
            renderer.push_solid_rect(btn_rect, bg);

            // Label (vertically centered in the button — middle row of 3).
            let label_col = col + 2;
            let label_row = button_row + 1; // middle row
            renderer.emit_text_line(label, label_row, label_col, TextColors { fg, bg });

            if let Some(rect) = self.button_rects.get_mut(btn_idx) {
                *rect = btn_rect;
            }

            col += btn_w_cols + gap;
        }
    }

    /// Build the body text lines for the dialog.
    fn body_lines(&self, _dialog_cols: usize) -> Vec<String> {
        let mut lines = vec![
            String::from("Quit Scribe"),
            String::from("  Close all windows. Sessions are"),
            String::from("  preserved and can be reattached."),
            String::new(),
            String::from("Kill Window"),
            String::from("  Close this window only. Its"),
            String::from("  sessions will be terminated."),
        ];

        if self.session_count > 0 {
            lines.push(String::new());
            lines.push(format!("  {} active session(s) will be lost.", self.session_count));
        }

        lines
    }
}

struct DialogLayout {
    dialog_rect: Rect,
    dialog_cols: usize,
    body_lines: Vec<String>,
    body_count: usize,
    button_row: usize,
}

impl DialogLayout {
    fn new(viewport: Rect, cell_size: (f32, f32), body_lines: Vec<String>) -> Self {
        let (cell_w, cell_h) = cell_size;
        let max_cols = dialog_units_in_extent(viewport.width, cell_w);
        let dialog_cols = 46_usize.min(max_cols.max(MIN_DIALOG_COLS));
        let body_count = body_lines.len();
        let content_rows = 2 + 1 + 1 + body_count + 1 + 1 + 1 + BUTTON_HEIGHT_ROWS + 1;
        let dialog_w = dialog_grid_width(dialog_cols, cell_w);
        let dialog_h = dialog_grid_height(content_rows, cell_h);
        let dialog_x = viewport.x + (viewport.width - dialog_w).max(0.0) / 2.0;
        let dialog_y = viewport.y + (viewport.height - dialog_h).max(0.0) / 2.0;

        Self {
            dialog_rect: Rect { x: dialog_x, y: dialog_y, width: dialog_w, height: dialog_h },
            dialog_cols,
            body_lines,
            body_count,
            button_row: 4 + body_count + 3,
        }
    }
}

#[derive(Clone, Copy)]
struct TextColors {
    fg: [f32; 4],
    bg: [f32; 4],
}

struct DialogRenderer<'a, 'layout> {
    out: &'a mut Vec<CellInstance>,
    layout: &'layout DialogLayout,
    cell_size: (f32, f32),
    resolve_glyph: &'a mut GlyphResolver<'a>,
}

impl<'a, 'layout> DialogRenderer<'a, 'layout> {
    fn new(
        out: &'a mut Vec<CellInstance>,
        layout: &'layout DialogLayout,
        cell_size: (f32, f32),
        resolve_glyph: &'a mut GlyphResolver<'a>,
    ) -> Self {
        Self { out, layout, cell_size, resolve_glyph }
    }

    fn push_solid_rect(&mut self, rect: Rect, color: [f32; 4]) {
        push_solid_rect(self.out, rect, color);
    }

    fn draw_frame(&mut self, border: [f32; 4]) {
        let rect = self.layout.dialog_rect;
        self.push_solid_rect(Rect { x: rect.x, y: rect.y, width: rect.width, height: 1.0 }, border);
        self.push_solid_rect(
            Rect { x: rect.x, y: rect.y + rect.height - 1.0, width: rect.width, height: 1.0 },
            border,
        );
    }

    fn draw_title(&mut self, title: &str, colors: TextColors) {
        self.emit_text_centered(title, 2, colors);
    }

    fn draw_body(&mut self, colors: TextColors) {
        for (i, line) in self.layout.body_lines.iter().enumerate() {
            self.emit_text_line(line, 4 + i, PADDING, colors);
        }
    }

    fn draw_separator(&mut self, color: [f32; 4]) {
        let (cell_w, cell_h) = self.cell_size;
        let sep_row = 4 + self.layout.body_count + 1;
        let sep_y = dialog_grid_y(self.layout.dialog_rect.y, sep_row, cell_h) + cell_h / 2.0;
        let sep_inset = dialog_grid_width(PADDING, cell_w);
        self.push_solid_rect(
            Rect {
                x: self.layout.dialog_rect.x + sep_inset,
                y: sep_y,
                width: self.layout.dialog_rect.width - sep_inset * 2.0,
                height: 1.0,
            },
            color,
        );
    }

    fn emit_text_centered(&mut self, text: &str, row: usize, colors: TextColors) {
        let start_col = self.layout.dialog_cols.saturating_sub(text.len()) / 2;
        self.emit_text_line(text, row, start_col, colors);
    }

    fn emit_text_line(&mut self, text: &str, row: usize, start_col: usize, colors: TextColors) {
        let (cell_w, cell_h) = self.cell_size;
        let y = dialog_grid_y(self.layout.dialog_rect.y, row, cell_h);

        for (i, ch) in text.chars().enumerate() {
            let col = start_col + i;
            if col >= self.layout.dialog_cols {
                break;
            }
            let x = dialog_grid_x(self.layout.dialog_rect.x, col, cell_w);
            let (uv_min, uv_max) = (self.resolve_glyph)(ch);
            self.out.push(CellInstance {
                pos: [x, y],
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

/// Per-button color selection.
///
/// - **Quit Scribe** (idx 0): accent — themed accent color highlight
/// - **Kill Window** (idx 1): destructive — ANSI red tint
/// - **Cancel** (idx 2): subtle — low-contrast background, normal text
fn button_colors(btn_idx: usize, active: bool, colors: &DialogColors) -> ([f32; 4], [f32; 4]) {
    if active {
        match btn_idx {
            0 => (colors.button_active_fg, colors.button_accent_bg),
            1 => (colors.button_active_fg, colors.button_danger_bg),
            _ => (colors.button_active_fg, colors.button_active_bg),
        }
    } else {
        match btn_idx {
            1 => (colors.button_danger_fg, colors.button_bg),
            _ => (colors.button_fg, colors.button_bg),
        }
    }
}

/// Pre-computed linear-RGB colors for dialog rendering.
struct DialogColors {
    backdrop: [f32; 4],
    dialog_bg: [f32; 4],
    border: [f32; 4],
    separator: [f32; 4],
    title_fg: [f32; 4],
    body_fg: [f32; 4],
    button_fg: [f32; 4],
    button_bg: [f32; 4],
    button_active_fg: [f32; 4],
    button_active_bg: [f32; 4],
    button_accent_bg: [f32; 4],
    button_danger_fg: [f32; 4],
    button_danger_bg: [f32; 4],
}

impl DialogColors {
    fn from_chrome(chrome: &ChromeColors) -> Self {
        // ANSI red (index 1 in the palette) for destructive actions.
        // We don't have direct access to ansi_colors here, so derive from
        // chrome colors: darken the accent for danger, use a muted red fallback.
        let danger_red = [0.85, 0.25, 0.25, 1.0]; // warm red

        Self {
            backdrop: [0.0, 0.0, 0.0, 0.55],
            dialog_bg: srgb_to_linear_rgba(lighten(chrome.tab_bar_bg, 0.04)),
            border: srgb_to_linear_rgba(with_alpha(chrome.tab_text, 0.15)),
            separator: srgb_to_linear_rgba(with_alpha(chrome.tab_text, 0.10)),
            title_fg: srgb_to_linear_rgba(chrome.tab_text_active),
            body_fg: srgb_to_linear_rgba(chrome.tab_text),
            button_fg: srgb_to_linear_rgba(chrome.tab_text),
            button_bg: srgb_to_linear_rgba(lighten(chrome.tab_bar_bg, 0.02)),
            button_active_fg: srgb_to_linear_rgba(chrome.tab_bar_bg),
            button_active_bg: srgb_to_linear_rgba(with_alpha(chrome.tab_text, 0.85)),
            button_accent_bg: srgb_to_linear_rgba(chrome.accent),
            button_danger_fg: srgb_to_linear_rgba(danger_red),
            button_danger_bg: srgb_to_linear_rgba(danger_red),
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
