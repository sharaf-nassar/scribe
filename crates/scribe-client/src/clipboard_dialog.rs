//! In-app GPU-rendered confirmation dialog for OSC 52 clipboard read/write
//! requests issued by PTY-side programs (spec 010 FR-005, FR-006).
//!
//! Modeled on [`crate::disallowed_scheme_dialog`]: title + body + separator +
//! buttons rendered as [`scribe_renderer::types::CellInstance`] quads in the
//! same GPU pass as the terminal. Reuses the same Esc-cancels / Tab-cycles /
//! Enter-activates conventions and default-focus-on-deny discipline. Wave 4
//! ships the four-button layout: `Deny once` (default focus), `Always deny`,
//! `Allow once`, `Always allow`. The two `Always*` variants persist a policy
//! change on the matching axis (see `main.rs#handle_clipboard_dialog_action`).
//
// @lat: [[client#URL Detection#Clipboard Dialog]]

use scribe_common::ids::SessionId;
use scribe_common::protocol::{ClipboardOp, ClipboardSelection, PromptId};
use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// User decision returned from the OSC 52 confirmation dialog. Wave 4
/// exposes all four variants; the `Always*` pair persists the matching
/// axis of `terminal.clipboard.*` to disk in addition to resolving the
/// single in-flight prompt.
#[derive(Clone, Copy, Debug)]
pub enum ClipboardDialogAction {
    /// Allow this single request through (PTY-side program receives the
    /// requested clipboard content or write).
    AllowOnce,
    /// Deny this single request silently (PTY-side program sees no
    /// reply / clipboard mutation).
    DenyOnce,
    /// Allow this request AND persist `terminal.clipboard.{read,write}_mode`
    /// (matching the request's op) to `"allow"` in the user config.
    AlwaysAllow,
    /// Deny this request AND persist `terminal.clipboard.{read,write}_mode`
    /// (matching the request's op) to `"deny"` in the user config.
    AlwaysDeny,
}

/// Index of the currently focused button. Tab cycles across all four
/// variants left-to-right; Shift+Tab reverses. Layout order matches the
/// dialog's left-to-right rendering order: `Deny once`, `Always deny`,
/// `Allow once`, `Always allow`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonIndex {
    DenyOnce = 0,
    AlwaysDeny = 1,
    AllowOnce = 2,
    AlwaysAllow = 3,
}

impl ButtonIndex {
    fn next(self) -> Self {
        match self {
            Self::DenyOnce => Self::AlwaysDeny,
            Self::AlwaysDeny => Self::AllowOnce,
            Self::AllowOnce => Self::AlwaysAllow,
            Self::AlwaysAllow => Self::DenyOnce,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::DenyOnce => Self::AlwaysAllow,
            Self::AlwaysDeny => Self::DenyOnce,
            Self::AllowOnce => Self::AlwaysDeny,
            Self::AlwaysAllow => Self::AllowOnce,
        }
    }

    fn to_action(self) -> ClipboardDialogAction {
        match self {
            Self::DenyOnce => ClipboardDialogAction::DenyOnce,
            Self::AlwaysDeny => ClipboardDialogAction::AlwaysDeny,
            Self::AllowOnce => ClipboardDialogAction::AllowOnce,
            Self::AlwaysAllow => ClipboardDialogAction::AlwaysAllow,
        }
    }

    fn from_index(idx: usize) -> Option<Self> {
        match idx {
            0 => Some(Self::DenyOnce),
            1 => Some(Self::AlwaysDeny),
            2 => Some(Self::AllowOnce),
            3 => Some(Self::AlwaysAllow),
            _ => None,
        }
    }
}

const BUTTON_COUNT: usize = 4;
type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

/// Minimum number of columns the dialog can shrink to (ensures four
/// buttons fit). Each label (the longest is "Always allow" at 12 chars)
/// needs 16 cols with padding; four buttons plus three gaps plus inner
/// padding lands around 76 cols.
const MIN_DIALOG_COLS: usize = 76;

/// Horizontal padding inside the dialog (columns from each edge).
const PADDING: usize = 3;

/// Height of each button in cell rows (top pad + label + bottom pad).
const BUTTON_HEIGHT_ROWS: usize = 3;

/// Dialog layout never needs more than this many grid units, which keeps
/// the integer-to-float conversion exact for pixel placement.
const MAX_DIALOG_GRID_UNITS: usize = 65_535;

/// Maximum body-row width for the truncated write payload preview. The
/// full preview is built server-side by `clipboard_write_preview` and is
/// already head-and-tail truncated; this cap is the row width inside the
/// dialog body.
const BODY_PREVIEW_MAX_COLS: usize = 56;

pub struct ClipboardDialogBuildContext<'a> {
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

/// State for the in-app OSC 52 confirmation overlay (spec 010 data-model E4).
pub struct ClipboardDialog {
    /// Echoed back to the server as the response's `request_id`.
    request_id: PromptId,
    /// Session whose PTY issued the OSC 52 request. Stored so the dialog
    /// resolves against the originating pane even when focus has moved
    /// elsewhere by the time the user picks an action. Wave 2 records but
    /// does not yet route on this; Wave 3 ties dialog visibility to the
    /// originating pane (FR-016 / per-pane scoping). Exposed via
    /// [`Self::session_id`] for tracing in the action-dispatch path.
    session_id: SessionId,
    /// Whether the request is a read or write. Drives title + body copy.
    op: ClipboardOp,
    /// Selection target (clipboard vs. primary). Mentioned in the body
    /// alongside the op so the user can tell `c` from `p` requests apart.
    selection: ClipboardSelection,
    /// Truncated write payload preview (FR-006). `None` for reads.
    preview: Option<String>,
    /// Currently keyboard-focused button. Defaults to `DenyOnce` (FR-005).
    focused: ButtonIndex,
    /// Button the mouse is currently hovering (if any).
    hovered: Option<usize>,
    /// Cached button hit rects from the last render (viewport-pixel coords).
    button_rects: [Rect; BUTTON_COUNT],
}

impl ClipboardDialog {
    /// Construct a new clipboard confirmation dialog for the given OSC 52
    /// request.
    #[must_use]
    pub fn new(
        request_id: PromptId,
        session_id: SessionId,
        op: ClipboardOp,
        selection: ClipboardSelection,
        preview: Option<String>,
    ) -> Self {
        Self {
            request_id,
            session_id,
            op,
            selection,
            preview,
            focused: ButtonIndex::DenyOnce,
            hovered: None,
            button_rects: [Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }; BUTTON_COUNT],
        }
    }

    /// Wire-side `request_id` the response must echo back to the server.
    #[must_use]
    pub fn request_id(&self) -> PromptId {
        self.request_id
    }

    /// Session that issued the prompt. Used by the activation path to
    /// anchor any subsequent UI to the originating pane. Wave 3 wires
    /// per-pane scoping; Wave 2 reads it only via tracing.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
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
    #[must_use]
    pub fn confirm(&self) -> ClipboardDialogAction {
        self.focused.to_action()
    }

    /// Update hover state from cursor position. Returns `true` if the state
    /// changed and the caller should request a redraw.
    pub fn update_hover(&mut self, x: f32, y: f32) -> bool {
        let prev = self.hovered;
        self.hovered = self.button_rects.iter().position(|r| r.contains(x, y));
        self.hovered != prev
    }

    /// Handle a mouse click at `(x, y)`. Returns `Some(action)` if a
    /// button was clicked.
    #[must_use]
    pub fn click(&self, x: f32, y: f32) -> Option<ClipboardDialogAction> {
        let idx = self.button_rects.iter().position(|r| r.contains(x, y))?;
        let button = ButtonIndex::from_index(idx)?;
        Some(button.to_action())
    }

    /// The OSC 52 op (read or write) this prompt represents. Wave 4 uses
    /// this from the activation path to decide which `terminal.clipboard.*`
    /// axis to persist when the user picks `Always allow` / `Always deny`.
    #[must_use]
    pub fn op(&self) -> ClipboardOp {
        self.op
    }

    /// Build GPU instances for the dialog overlay.
    pub fn build_instances(&mut self, ctx: ClipboardDialogBuildContext<'_>) {
        let ClipboardDialogBuildContext { out, viewport, cell_size, chrome, resolve_glyph } = ctx;
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
        renderer.draw_title(self.title(), TextColors { fg: colors.title_fg, bg: colors.dialog_bg });
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

    /// Build the body text lines for the dialog. Write requests include a
    /// truncated preview of the payload per FR-006; reads do not.
    fn body_lines(&self) -> Vec<String> {
        let selection_word = match self.selection {
            ClipboardSelection::Clipboard => "clipboard",
            ClipboardSelection::Primary => "primary selection",
        };
        let intro = match self.op {
            ClipboardOp::Read => {
                format!("A program in this terminal wants to read the {selection_word}.")
            }
            ClipboardOp::Write => {
                format!("A program in this terminal wants to overwrite the {selection_word}.")
            }
        };

        let mut lines =
            vec![intro, String::new(), String::from("Allow only if you recognise this action.")];

        if let Some(preview) = self.preview.as_deref() {
            lines.push(String::new());
            lines.push(String::from("Payload preview:"));
            lines.push(truncate_for_display(preview, BODY_PREVIEW_MAX_COLS));
        }

        lines
    }

    fn title(&self) -> &'static str {
        match self.op {
            ClipboardOp::Read => "Allow clipboard read?",
            ClipboardOp::Write => "Allow clipboard write?",
        }
    }

    fn button_labels() -> [&'static str; BUTTON_COUNT] {
        ["Deny once", "Always deny", "Allow once", "Always allow"]
    }
}

/// Shrink `text` to fit `max_cols` columns by keeping a head and tail
/// slice with `...` between them. Mirrors the disallowed-scheme dialog's
/// same-name helper so both surfaces present a consistent truncation
/// shape; the server may already have truncated the payload further.
fn truncate_for_display(text: &str, max_cols: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_cols {
        return text.to_owned();
    }
    if max_cols <= 3 {
        return chars.into_iter().take(max_cols).collect();
    }
    let budget = max_cols.saturating_sub(3);
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
        // Wave 4 prefers ~84 cols so the four-button row breathes; clamp
        // down to MIN_DIALOG_COLS when the viewport is narrower.
        let dialog_cols = 84_usize.min(max_cols.max(MIN_DIALOG_COLS));
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
/// - **Deny once** (idx 0) and **Always deny** (idx 1): subtle — low-
///   contrast background, normal text (default focus per FR-005). The
///   "Always" variant shares the deny visual language because it is the
///   safe direction.
/// - **Allow once** (idx 2) and **Always allow** (idx 3): warning —
///   warm-red tint when active to signal that allowing the operation
///   crosses a trust boundary. The "Always" variant is the loudest of
///   the four because it both allows AND persists the choice.
fn button_colors(btn_idx: usize, active: bool, colors: &DialogColors) -> ([f32; 4], [f32; 4]) {
    let is_allow = btn_idx == 2 || btn_idx == 3;
    if active {
        if is_allow {
            (colors.button_active_fg, colors.button_danger_bg)
        } else {
            (colors.button_active_fg, colors.button_active_bg)
        }
    } else if is_allow {
        (colors.button_danger_fg, colors.button_bg)
    } else {
        (colors.button_fg, colors.button_bg)
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
        // "Allow once" button. Matches the disallowed-scheme dialog so
        // both consent surfaces share one visual idiom.
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
