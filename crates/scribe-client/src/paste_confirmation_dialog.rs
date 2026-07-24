//! In-app GPU-rendered confirmation dialog that gates a "risky" paste before
//! any byte reaches the PTY (spec 011).
//!
//! A paste is risky when it contains a line break or a non-tab control/escape
//! byte AND the focused application has not enabled bracketed paste. The
//! classifier ([`classify_paste`]) is a pure, allocation-free function; the
//! dialog itself renders as [`CellInstance`] quads in the same GPU pass as the
//! terminal, cloning the chrome conventions of [`crate::disallowed_scheme_dialog`]
//! and [`crate::clipboard_dialog`]: title + body + separator + two buttons,
//! **Cancel default focus**, Esc cancels, Tab cycles focus, Enter activates the
//! focused button, mouse click activates the clicked button.
//!
//! The parked paste content is rendered into the body in caret notation so a
//! malicious/accidental control sequence in the preview can never drive the
//! terminal or the dialog (contract C5 / FR-005); delivery on confirm is
//! byte-identical to the disabled path (the preview is display-only).

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::PasteTarget;
use crate::layout::Rect;

// ---------------------------------------------------------------------------
// Classifier (pure)
// ---------------------------------------------------------------------------

/// Result of classifying paste content for the confirmation gate.
///
/// At least one flag is always set when this is produced (see
/// [`classify_paste`]); a value with both flags `false` is never returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteRisk {
    /// Content contains `\n` or `\r` (including a single trailing newline).
    pub has_line_break: bool,
    /// Content contains a control/escape character that is NOT `\t`/`\n`/`\r`
    /// (C0 except tab/LF/CR, DEL, or C1).
    pub has_control: bool,
}

/// Classify paste `text`, returning `Some(risk)` iff it should be gated.
///
/// A line break is `'\n'` or `'\r'`. A control character is any
/// [`char::is_control`] other than `'\t'`, `'\n'`, or `'\r'`. Returns `Some`
/// iff `has_line_break || has_control`, else `None`. Pure and allocation-free;
/// O(n) in the char length of `text`.
//
// @lat: [[client#Dialogs#Paste Confirmation Dialog]]
#[must_use]
pub fn classify_paste(text: &str) -> Option<PasteRisk> {
    let mut has_line_break = false;
    let mut has_control = false;
    for ch in text.chars() {
        match ch {
            '\n' | '\r' => has_line_break = true,
            '\t' => {}
            c if c.is_control() => has_control = true,
            _ => {}
        }
    }
    if has_line_break || has_control {
        Some(PasteRisk { has_line_break, has_control })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Caret-escaped preview helper
// ---------------------------------------------------------------------------

/// Maximum number of preview lines shown in the dialog body; extra lines are
/// summarised by a `… (+N more lines)` trailer.
const MAX_PREVIEW_LINES: usize = 8;

/// Maximum number of columns each preview line is truncated to (matches the
/// disallowed-scheme dialog's body width).
const MAX_PREVIEW_COLS: usize = 56;

/// Number of spaces a tab is rendered as in the preview (legible, never a raw
/// control byte).
const TAB_PREVIEW: &str = "  ";

/// Render `content` as a caret-escaped, per-line-truncated preview suitable for
/// the dialog body.
///
/// Splits on `'\n'`, takes at most [`MAX_PREVIEW_LINES`] lines (appending a
/// `… (+N more lines)` summary when there are more), replaces every
/// control/escape byte with caret notation, renders tabs as spaces, and
/// truncates each rendered line to [`MAX_PREVIEW_COLS`] columns via the same
/// head/tail-ellipsis pattern used by [`truncate_for_display`]. A raw control
/// byte is never emitted into the returned strings (FR-005 / SC-008).
fn caret_preview(content: &str) -> Vec<String> {
    // `split('\n')` yields one trailing empty segment for content ending in a
    // newline; that faithfully shows the trailing blank line in the count.
    let raw_lines: Vec<&str> = content.split('\n').collect();
    let total = raw_lines.len();
    let shown = total.min(MAX_PREVIEW_LINES);

    let mut out: Vec<String> = Vec::with_capacity(shown + 1);
    for line in raw_lines.iter().take(shown) {
        // `\n` is already consumed by the split; a `\r` left at a line end (or
        // a bare `\r` line separator) is rendered in caret notation like any
        // other control byte so it is visible rather than silently dropped.
        out.push(truncate_for_display(&escape_line(line), MAX_PREVIEW_COLS));
    }
    if total > shown {
        let more = total - shown;
        let plural = if more == 1 { "line" } else { "lines" };
        out.push(format!("… (+{more} more {plural})"));
    }
    out
}

/// Replace control/escape characters in a single line with caret notation,
/// leaving printable characters untouched. Never emits a raw control byte.
fn escape_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for ch in line.chars() {
        match ch {
            '\t' => out.push_str(TAB_PREVIEW),
            c if c.is_control() => out.push_str(&caret_escape(c)),
            c => out.push(c),
        }
    }
    out
}

/// Render a single control character in caret / unicode-escape notation.
///
/// - DEL (`0x7F`) → `^?`.
/// - C0 (`0x00..=0x1F`) → `^` + `(byte ^ 0x40)` (e.g. `ESC` → `^[`, `CR` →
///   `^M`, NUL → `^@`).
/// - C1 (`0x80..=0x9F`) and any other control char → `\u{NN}` so it is shown
///   without emitting a raw byte.
fn caret_escape(c: char) -> String {
    let code = c as u32;
    match code {
        0x7F => String::from("^?"),
        0x00..=0x1F => {
            // `byte ^ 0x40` maps C0 onto the printable caret range `@A..._`.
            let caret = u8::try_from(code).map_or(b'?', |b| b ^ 0x40);
            format!("^{}", caret as char)
        }
        _ => format!("\\u{{{code:02X}}}"),
    }
}

// ---------------------------------------------------------------------------
// Dialog action + focus
// ---------------------------------------------------------------------------

/// What the user chose in the paste-confirmation dialog.
#[derive(Clone, Copy)]
pub enum PasteConfirmationAction {
    /// Deliver the parked paste to the parked target (byte-identical).
    Paste,
    /// Drop the parked paste; send nothing (default).
    Cancel,
}

/// Index of the currently focused button. `Cancel` is index 0 and the default
/// focus so the safe choice is pre-selected (mirrors the disallowed-scheme and
/// clipboard dialogs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonIndex {
    Cancel = 0,
    Paste = 1,
}

impl ButtonIndex {
    fn next(self) -> Self {
        match self {
            Self::Cancel => Self::Paste,
            Self::Paste => Self::Cancel,
        }
    }

    fn prev(self) -> Self {
        // Two buttons: previous == next.
        self.next()
    }

    fn from_index(idx: usize) -> Option<Self> {
        match idx {
            0 => Some(Self::Cancel),
            1 => Some(Self::Paste),
            _ => None,
        }
    }

    fn to_action(self) -> PasteConfirmationAction {
        match self {
            Self::Cancel => PasteConfirmationAction::Cancel,
            Self::Paste => PasteConfirmationAction::Paste,
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

/// Dialog layout never needs more than this many grid units, which keeps the
/// integer-to-float conversion exact for pixel placement.
const MAX_DIALOG_GRID_UNITS: usize = 65_535;

pub struct PasteConfirmationDialogBuildContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub viewport: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

// ---------------------------------------------------------------------------
// Dialog state
// ---------------------------------------------------------------------------

/// State for the in-app paste-confirmation overlay. Parks the paste content and
/// its resolved target while awaiting the user's choice (research R2).
pub struct PasteConfirmationDialog {
    /// Raw, unmodified paste text delivered verbatim on confirm (contract C5).
    content: String,
    /// Parked destination + bracketed flag, snapshotted at request time.
    target: PasteTarget,
    /// Classification driving the reason line.
    risk: PasteRisk,
    /// Currently keyboard-focused button. Defaults to `Cancel` (index 0).
    focused: ButtonIndex,
    /// Button the mouse is currently hovering (if any).
    hovered: Option<usize>,
    /// Cached button hit rects from the last render (viewport-pixel coords).
    button_rects: [Rect; BUTTON_COUNT],
}

impl PasteConfirmationDialog {
    /// Construct a new paste-confirmation dialog parking `content` for the
    /// resolved `target`.
    pub fn new(content: String, target: PasteTarget, risk: PasteRisk) -> Self {
        Self {
            content,
            target,
            risk,
            focused: ButtonIndex::Cancel,
            hovered: None,
            button_rects: [Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }; BUTTON_COUNT],
        }
    }

    /// Consume the dialog, returning the parked content and target so the
    /// caller can resume delivery on confirm.
    pub fn into_parked_paste(self) -> (String, PasteTarget) {
        (self.content, self.target)
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
    pub fn confirm(&self) -> PasteConfirmationAction {
        self.focused.to_action()
    }

    /// Update hover state from cursor position. Returns `true` if it changed.
    pub fn update_hover(&mut self, x: f32, y: f32) -> bool {
        let prev = self.hovered;
        self.hovered = self.button_rects.iter().position(|r| r.contains(x, y));
        self.hovered != prev
    }

    /// Handle a mouse click at `(x, y)`. Returns `Some(action)` if a button was
    /// clicked.
    pub fn click(&self, x: f32, y: f32) -> Option<PasteConfirmationAction> {
        let idx = self.button_rects.iter().position(|r| r.contains(x, y))?;
        Some(ButtonIndex::from_index(idx)?.to_action())
    }

    /// Build GPU instances for the dialog overlay.
    pub fn build_instances(&mut self, ctx: PasteConfirmationDialogBuildContext<'_>) {
        let PasteConfirmationDialogBuildContext { out, viewport, cell_size, chrome, resolve_glyph } =
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

            let (fg, bg) = button_colors(active, colors);

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

    /// Build the body text lines: a reason line derived from `risk`, a blank,
    /// then the caret-escaped preview of the parked content (research R4).
    fn body_lines(&self) -> Vec<String> {
        let mut lines = vec![self.reason_line(), String::new()];
        lines.extend(caret_preview(&self.content));
        lines
    }

    /// Derive the human-readable reason line from `risk`, distinguishing the
    /// multiline-only, control-only, and both cases (T012).
    fn reason_line(&self) -> String {
        // Count `\n`-delimited segments so the number matches the preview,
        // which also splits on `'\n'`. A bare `\r` (old-Mac line ending) still
        // sets `has_line_break` and renders as `^M` in the preview; it is not
        // separately counted as a line boundary here.
        let line_count = self.content.matches('\n').count() + 1;
        let control_count = if self.risk.has_control {
            self.content
                .chars()
                .filter(|c| c.is_control() && *c != '\t' && *c != '\n' && *c != '\r')
                .count()
        } else {
            0
        };

        match (self.risk.has_line_break, self.risk.has_control) {
            (true, true) => {
                let lines_word = if line_count == 1 { "line" } else { "lines" };
                let ctrl_word =
                    if control_count == 1 { "control character" } else { "control characters" };
                format!("{line_count} {lines_word} · {control_count} {ctrl_word}")
            }
            (true, false) => {
                let lines_word = if line_count == 1 { "line" } else { "lines" };
                format!("{line_count} {lines_word}")
            }
            (false, true) => {
                if control_count == 1 {
                    String::from("contains a control character")
                } else {
                    String::from("contains control characters")
                }
            }
            // `body_lines` is only reached for a gated paste, so at least one
            // flag is set; this arm is unreachable but kept total.
            (false, false) => String::from("risky paste"),
        }
    }

    fn title() -> &'static str {
        "Confirm Paste"
    }

    fn button_labels() -> [&'static str; BUTTON_COUNT] {
        ["Cancel", "Paste"]
    }
}

// ---------------------------------------------------------------------------
// Shared dialog chrome (replicated per the established sibling-dialog pattern:
// `disallowed_scheme_dialog.rs` and `clipboard_dialog.rs` each keep a private
// copy rather than a shared module).
// ---------------------------------------------------------------------------

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

/// Shrink `text` to fit `max_cols` columns by keeping a head and tail slice
/// with `...` between them.
///
/// Showing both halves (rather than head-only) keeps the start and the end of
/// a long line legible — important for a paste preview where a trailing control
/// sequence would otherwise be hidden.
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
/// Both **Cancel** (idx 0, default focus) and **Paste** (idx 1) use the subtle
/// visual language: pasting confirmed content is an ordinary action, not a
/// destructive trust-boundary crossing, so neither button uses the warm-red
/// "danger" treatment that the disallowed-scheme / clipboard dialogs reserve
/// for their proceed-anyway buttons. The focused/hovered button is highlighted.
fn button_colors(active: bool, colors: &DialogColors) -> ([f32; 4], [f32; 4]) {
    if active {
        (colors.button_active_fg, colors.button_active_bg)
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
}

impl DialogColors {
    fn from_chrome(chrome: &ChromeColors) -> Self {
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
