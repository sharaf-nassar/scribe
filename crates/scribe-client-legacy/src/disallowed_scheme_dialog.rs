//! In-app GPU-rendered confirmation dialog for OSC 8 URIs whose scheme is
//! outside the existing outbound URL allowlist (spec 009 FR-015).
//!
//! Renders as [`CellInstance`] quads in the same GPU pass as the terminal,
//! following the conventions established by [`crate::close_dialog`] and
//! [`crate::update_dialog`]: title + body + separator + two buttons,
//! Cancel default focus, Esc cancels, Tab cycles focus, Enter activates the
//! focused button, mouse click activates the clicked button.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// What the user chose in the disallowed-scheme confirmation dialog.
#[derive(Clone, Copy)]
pub enum DisallowedSchemeAction {
    /// Activate the dialog's primary action — proceed with opening the URI.
    OpenAnyway,
    /// Dismiss the dialog without opening anything (default).
    Cancel,
}

/// Index of the currently focused button.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonIndex {
    Cancel = 0,
    OpenAnyway = 1,
}

impl ButtonIndex {
    fn next(self) -> Self {
        match self {
            Self::Cancel => Self::OpenAnyway,
            Self::OpenAnyway => Self::Cancel,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Cancel => Self::OpenAnyway,
            Self::OpenAnyway => Self::Cancel,
        }
    }

    fn to_action(self) -> DisallowedSchemeAction {
        match self {
            Self::Cancel => DisallowedSchemeAction::Cancel,
            Self::OpenAnyway => DisallowedSchemeAction::OpenAnyway,
        }
    }
}

const BUTTON_COUNT: usize = 2;
type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

/// Minimum number of columns the dialog can shrink to (ensures buttons fit).
const MIN_DIALOG_COLS: usize = 46;

/// Horizontal padding inside the dialog (columns from each edge).
const PADDING: usize = 3;

/// Height of each button in cell rows (top pad + label + bottom pad).
const BUTTON_HEIGHT_ROWS: usize = 3;

/// Dialog layout never needs more than this many grid units, which keeps
/// the integer-to-float conversion exact for pixel placement.
const MAX_DIALOG_GRID_UNITS: usize = 65_535;

/// Body-text URI rendering is truncated to this many columns when the URI
/// alone would exceed the dialog body width. The full URI is still
/// preserved on the dialog state for activation.
const BODY_URI_MAX_COLS: usize = 56;

pub struct DisallowedSchemeDialogBuildContext<'a> {
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

/// State for the in-app disallowed-scheme confirmation overlay.
pub struct DisallowedSchemeDialog {
    /// Verbatim URI awaiting confirmation (returned to the activation path
    /// when the user picks "Open Anyway"). Preserved at full length even
    /// when the body-row display is truncated.
    pending_uri: String,
    /// Extracted scheme name (everything up to the first `:`).
    scheme: String,
    /// Currently keyboard-focused button. Defaults to `Cancel` (FR-015).
    focused: ButtonIndex,
    /// Button the mouse is currently hovering (if any).
    hovered: Option<usize>,
    /// Cached button hit rects from the last render (viewport-pixel coords).
    button_rects: [Rect; BUTTON_COUNT],
}

impl DisallowedSchemeDialog {
    /// Construct a new disallowed-scheme dialog.
    ///
    /// `scheme` should be the substring of `uri` up to (but not including)
    /// the first `:`. The caller is expected to perform that extraction and
    /// route allowed-scheme URIs through `url_detect::open_url` directly.
    pub fn new(uri: String, scheme: String) -> Self {
        Self {
            pending_uri: uri,
            scheme,
            focused: ButtonIndex::Cancel,
            hovered: None,
            button_rects: [Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }; BUTTON_COUNT],
        }
    }

    /// Take ownership of the verbatim URI, consuming the dialog state.
    pub fn into_pending_uri(self) -> String {
        self.pending_uri
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
    pub fn confirm(&self) -> DisallowedSchemeAction {
        self.focused.to_action()
    }

    /// Update hover state from cursor position. Returns `true` if the state changed.
    pub fn update_hover(&mut self, x: f32, y: f32) -> bool {
        let prev = self.hovered;
        self.hovered = self.button_rects.iter().position(|r| r.contains(x, y));
        self.hovered != prev
    }

    /// Handle a mouse click at `(x, y)`. Returns `Some(action)` if a button was clicked.
    pub fn click(&self, x: f32, y: f32) -> Option<DisallowedSchemeAction> {
        let idx = self.button_rects.iter().position(|r| r.contains(x, y))?;
        let button = match idx {
            0 => ButtonIndex::Cancel,
            1 => ButtonIndex::OpenAnyway,
            _ => return None,
        };
        Some(button.to_action())
    }

    /// Build GPU instances for the dialog overlay.
    pub fn build_instances(&mut self, ctx: DisallowedSchemeDialogBuildContext<'_>) {
        let DisallowedSchemeDialogBuildContext { out, viewport, cell_size, chrome, resolve_glyph } =
            ctx;
        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }

        let colors = DialogColors::from_chrome(chrome);
        let layout = DialogLayout::new(viewport, cell_size, self.body_lines());
        let mut renderer = DialogRenderer::new(out, &layout, cell_size, resolve_glyph);

        renderer.push_solid_rect(viewport, colors.backdrop);
        renderer.push_solid_rect(layout.dialog_rect, colors.dialog_bg);
        renderer.draw_frame(colors.border);
        renderer
            .draw_title(Self::title(), TextColors { fg: colors.title_fg, bg: colors.dialog_bg });
        renderer.draw_body(TextColors { fg: colors.body_fg, bg: colors.dialog_bg });
        renderer.draw_separator(colors.separator);
        self.build_buttons(&mut renderer, &colors);
    }

    /// Build the two action buttons with proper padding and per-button colors.
    fn build_buttons(&mut self, renderer: &mut DialogRenderer<'_, '_>, colors: &DialogColors) {
        let (cell_w, cell_h) = renderer.cell_size;
        let dialog_x = renderer.layout.dialog_rect.x;
        let dialog_y = renderer.layout.dialog_rect.y;
        let button_row = renderer.layout.button_row;
        let dialog_cols = renderer.layout.dialog_cols;

        let button_labels = Self::button_labels();
        let btn_col_widths: Vec<usize> = button_labels.iter().map(|l| l.len() + 4).collect();
        let total_btn_cols: usize = btn_col_widths.iter().sum();
        let usable = dialog_cols.saturating_sub(PADDING * 2);
        let remaining = usable.saturating_sub(total_btn_cols);
        let gap = if BUTTON_COUNT > 1 { remaining / (BUTTON_COUNT - 1) } else { 0 };

        let button_y = dialog_grid_y(dialog_y, button_row, cell_h);
        let button_h = dialog_grid_height(BUTTON_HEIGHT_ROWS, cell_h);

        let mut col = PADDING;
        for (btn_idx, label) in button_labels.iter().enumerate() {
            let Some(btn_w_cols) = btn_col_widths.get(btn_idx).copied() else {
                continue;
            };
            let is_focused = self.focused as usize == btn_idx;
            let is_hovered = self.hovered == Some(btn_idx);
            let active = is_focused || is_hovered;

            let (fg, bg) = button_colors(btn_idx, active, colors);

            let btn_rect = Rect {
                x: dialog_grid_x(dialog_x, col, cell_w),
                y: button_y,
                width: dialog_grid_width(btn_w_cols, cell_w),
                height: button_h,
            };
            renderer.push_solid_rect(btn_rect, bg);

            let label_col = col + 2;
            let label_row = button_row + 1; // middle row of 3
            renderer.emit_text_line(label, label_row, label_col, TextColors { fg, bg });

            if let Some(rect) = self.button_rects.get_mut(btn_idx) {
                *rect = btn_rect;
            }

            col += btn_w_cols + gap;
        }
    }

    /// Build the body text lines for the dialog. Truncates the URI when it
    /// exceeds the body width so the dialog renders within the viewport;
    /// the full URI is still preserved on `pending_uri` for activation.
    fn body_lines(&self) -> Vec<String> {
        vec![
            format!("Scheme `{}:` is normally blocked.", self.scheme),
            String::new(),
            String::from("Open the following URI anyway?"),
            String::new(),
            truncate_for_display(&self.pending_uri, BODY_URI_MAX_COLS),
        ]
    }

    fn title() -> &'static str {
        "Unsafe URI Scheme"
    }

    fn button_labels() -> [&'static str; BUTTON_COUNT] {
        ["Cancel", "Open Anyway"]
    }
}

/// Shrink `text` to fit `max_cols` columns by keeping a head and tail
/// slice with `...` between them.
///
/// For URIs this matters because head-only truncation can hide
/// domain-confusion suffixes (e.g. `https://github.com.evil.com/...`
/// truncated to `https://github.com...`). Showing both halves makes the
/// tail visible to the user before they activate.
fn truncate_for_display(text: &str, max_cols: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_cols {
        return text.to_owned();
    }
    if max_cols <= 3 {
        return chars.into_iter().take(max_cols).collect();
    }
    let budget = max_cols.saturating_sub(3);
    // Bias the head slightly so the scheme + start of the path remain
    // visible; give the tail the remainder so the final path segment /
    // query stays legible. Round head up so odd budgets favour the head.
    let head_chars = budget.div_ceil(2);
    let tail_chars = budget - head_chars;
    let mut out: String = chars.iter().take(head_chars).collect();
    out.push_str("...");
    out.extend(chars.iter().skip(chars.len() - tail_chars));
    out
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
        let dialog_cols = 64_usize.min(max_cols.max(MIN_DIALOG_COLS));
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
/// - **Cancel** (idx 0): subtle — low-contrast background, normal text
///   (default focus per FR-015).
/// - **Open Anyway** (idx 1): warning — warm-red tint when active to
///   signal proceeding despite the disallowed-scheme blocker.
fn button_colors(btn_idx: usize, active: bool, colors: &DialogColors) -> ([f32; 4], [f32; 4]) {
    if active {
        match btn_idx {
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
    button_danger_fg: [f32; 4],
    button_danger_bg: [f32; 4],
}

impl DialogColors {
    fn from_chrome(chrome: &ChromeColors) -> Self {
        // ANSI red (index 1) — warm-red signal for the destructive
        // "Open Anyway" button, matching `close_dialog`'s convention.
        let danger_red = [0.85, 0.25, 0.25, 1.0];

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
