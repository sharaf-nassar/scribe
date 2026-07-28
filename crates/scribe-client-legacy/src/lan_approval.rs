//! Feature 014 (T018): the owning-side LAN device-approval prompt.
//!
//! When an unknown LAN device completes the mutual-TLS handshake, the owning
//! server holds it pending (revealing NO window or session data) and pushes a
//! [`ServerMessage::LanApprovalRequest`](scribe_common::protocol::ServerMessage::LanApprovalRequest)
//! to its OWN local client over the Unix socket. This module renders that
//! request as a GPU-overlay dialog — the requesting device's name, the trusted
//! network it arrived on, and its identity fingerprint words — with
//! equally-prominent Approve / Decline actions (UX-002). The user's choice
//! becomes a
//! [`ClientMessage::LanApprovalDecision`](scribe_common::protocol::ClientMessage::LanApprovalDecision):
//! Approve writes a `TrustedDevice` and proceeds into the 013 attach flow;
//! Decline refuses ([`LanRefusal::Declined`](scribe_common::protocol::LanRefusal::Declined))
//! and reveals nothing (FR-004/006, SEC-001/002).
//!
//! The dialog itself renders as [`CellInstance`] quads in the same GPU pass as
//! the terminal, cloning the chrome conventions of
//! [`crate::paste_confirmation_dialog`] and [`crate::disallowed_scheme_dialog`]:
//! title + body + separator + two buttons, **Decline default focus** (the safe
//! choice — no data flows until Approve), Esc declines, Tab cycles focus, Enter
//! activates the focused button, mouse click activates the clicked button. While
//! it is open the app layer intercepts every window event, so no keystroke
//! reaches the PTY (input capture, mirroring the sibling dialogs).

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

// ---------------------------------------------------------------------------
// Dialog action + focus
// ---------------------------------------------------------------------------

/// What the owning user chose on the approval prompt.
#[derive(Clone, Copy)]
pub enum LanApprovalAction {
    /// Trust this device: write a `TrustedDevice` and let it attach.
    Approve,
    /// Refuse this device: reveal nothing and remember nothing (default).
    Decline,
}

/// Index of the currently focused button. `Decline` is index 0 and the default
/// focus so the safe choice is pre-selected — pressing Enter on an unexpected
/// prompt never silently grants trust (mirrors the deny-default of the paste,
/// disallowed-scheme, and clipboard dialogs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonIndex {
    Decline = 0,
    Approve = 1,
}

impl ButtonIndex {
    fn next(self) -> Self {
        match self {
            Self::Decline => Self::Approve,
            Self::Approve => Self::Decline,
        }
    }

    fn prev(self) -> Self {
        // Two buttons: previous == next.
        self.next()
    }

    fn from_index(idx: usize) -> Option<Self> {
        match idx {
            0 => Some(Self::Decline),
            1 => Some(Self::Approve),
            _ => None,
        }
    }

    fn to_action(self) -> LanApprovalAction {
        match self {
            Self::Decline => LanApprovalAction::Decline,
            Self::Approve => LanApprovalAction::Approve,
        }
    }
}

const BUTTON_COUNT: usize = 2;
type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

/// Minimum number of columns the dialog can shrink to (ensures buttons fit).
const MIN_DIALOG_COLS: usize = 48;

/// Preferred dialog width in columns; clamped down to the viewport when narrow.
const DIALOG_COLS: usize = 60;

/// Horizontal padding inside the dialog (columns from each edge).
const PADDING: usize = 3;

/// Width the body prose is word-wrapped to (dialog interior minus both pads).
const BODY_WRAP_COLS: usize = DIALOG_COLS - PADDING * 2;

/// Height of each button in cell rows (top pad + label + bottom pad).
const BUTTON_HEIGHT_ROWS: usize = 3;

/// Dialog layout never needs more than this many grid units, which keeps the
/// integer-to-float conversion exact for pixel placement.
const MAX_DIALOG_GRID_UNITS: usize = 65_535;

pub struct LanApprovalDialogBuildContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub viewport: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

// ---------------------------------------------------------------------------
// Dialog state
// ---------------------------------------------------------------------------

/// State for the in-app LAN device-approval overlay. Holds the pending request's
/// display fields plus the `request_id` that correlates the user's decision back
/// to the held connection (data-model `ApprovalRequest`).
pub struct LanApprovalDialog {
    /// Correlates the [`ClientMessage::LanApprovalDecision`](scribe_common::protocol::ClientMessage::LanApprovalDecision)
    /// reply with the held connection.
    request_id: u64,
    /// Requesting device's advertised name (display only; never a trust key).
    device_name: String,
    /// The peer's identity fingerprint words (research D8).
    fingerprint_words: String,
    /// The trusted network the request arrived on.
    network_label: String,
    /// `true` when an already-trusted device shares this advertised name — an
    /// informational hint only, added as an extra body line.
    name_collision: bool,
    /// Currently keyboard-focused button. Defaults to `Decline` (index 0).
    focused: ButtonIndex,
    /// Button the mouse is currently hovering (if any).
    hovered: Option<usize>,
    /// Cached button hit rects from the last render (viewport-pixel coords).
    button_rects: [Rect; BUTTON_COUNT],
}

impl LanApprovalDialog {
    /// Construct a new approval dialog for a pending LAN device.
    #[must_use]
    pub fn new(
        request_id: u64,
        device_name: String,
        fingerprint_words: String,
        network_label: String,
        name_collision: bool,
    ) -> Self {
        Self {
            request_id,
            device_name,
            fingerprint_words,
            network_label,
            name_collision,
            focused: ButtonIndex::Decline,
            hovered: None,
            button_rects: [Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }; BUTTON_COUNT],
        }
    }

    /// The `request_id` this prompt answers, echoed in the decision reply.
    #[must_use]
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    /// Cycle focus to the next button.
    pub fn focus_next(&mut self) {
        self.focused = self.focused.next();
    }

    /// Cycle focus to the previous button.
    pub fn focus_prev(&mut self) {
        self.focused = self.focused.prev();
    }

    /// The action for the currently focused button (Enter / activate).
    #[must_use]
    pub fn confirm(&self) -> LanApprovalAction {
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
    #[must_use]
    pub fn click(&self, x: f32, y: f32) -> Option<LanApprovalAction> {
        let idx = self.button_rects.iter().position(|r| r.contains(x, y))?;
        Some(ButtonIndex::from_index(idx)?.to_action())
    }

    /// Build GPU instances for the dialog overlay.
    pub fn build_instances(&mut self, ctx: LanApprovalDialogBuildContext<'_>) {
        let LanApprovalDialogBuildContext { out, viewport, cell_size, chrome, resolve_glyph } = ctx;
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

    /// Build the two action buttons with equal width and equal (neutral) color
    /// weight — neither Approve nor Decline is visually dominant (UX-002).
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

    /// Build the body text lines: the primary "who wants control" sentence, the
    /// device's fingerprint words, and — only when the advertised name collides
    /// with an already-trusted device — an informational collision hint. All
    /// prose is word-wrapped so a long name or fingerprint stays inside the box.
    fn body_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();

        // Primary message (settings-and-config.md approval-prompt copy).
        let headline = format!(
            "{} on {} wants to control this machine.",
            self.device_name, self.network_label
        );
        lines.extend(wrap_text(&headline, BODY_WRAP_COLS));

        // Identity fingerprint the user MAY compare out of band (FR-006).
        lines.push(String::new());
        lines.push(String::from("Fingerprint:"));
        lines.extend(wrap_text(&self.fingerprint_words, BODY_WRAP_COLS));

        // Informational hint only — never a trust decision (data-model
        // `name_collision`, spec US2 #4 / FR-005).
        if self.name_collision {
            lines.push(String::new());
            let hint = format!(
                "You already trust a different device named {} — approve only if you recognize this one.",
                self.device_name
            );
            lines.extend(wrap_text(&hint, BODY_WRAP_COLS));
        }

        lines
    }

    fn title() -> &'static str {
        "Approve device?"
    }

    fn button_labels() -> [&'static str; BUTTON_COUNT] {
        ["Decline", "Approve"]
    }
}

// ---------------------------------------------------------------------------
// Prose wrapping (variable-length device name / fingerprint)
// ---------------------------------------------------------------------------

/// Word-wrap `text` to at most `max_cols` display columns per line, breaking on
/// whitespace and hard-splitting any single token longer than `max_cols`, so a
/// long device name or fingerprint word list can never overflow the dialog box.
/// Char-based throughout, so a multibyte name never splits mid-codepoint.
fn wrap_text(text: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for word in text.split_whitespace() {
        let word_len = word.chars().count();

        // A token that cannot fit on a line by itself is hard-split onto its own
        // lines (after flushing any partial line first).
        if word_len > max_cols {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_len = 0;
            }
            lines.extend(split_long_token(word, max_cols));
            continue;
        }

        let sep = usize::from(!current.is_empty());
        if current_len + sep + word_len > max_cols {
            lines.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_len += 1;
        }
        current.push_str(word);
        current_len += word_len;
    }

    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Split a single overlong token into `max_cols`-column, codepoint-aligned
/// chunks. `max_cols` is always non-zero at the call site.
fn split_long_token(token: &str, max_cols: usize) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    chars.chunks(max_cols).map(|chunk| chunk.iter().collect()).collect()
}

// ---------------------------------------------------------------------------
// Shared dialog chrome (replicated per the established sibling-dialog pattern:
// `paste_confirmation_dialog.rs` and `disallowed_scheme_dialog.rs` each keep a
// private copy rather than a shared module).
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
        let dialog_cols = DIALOG_COLS.min(max_cols.max(MIN_DIALOG_COLS));
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
/// Both **Decline** (idx 0, default focus) and **Approve** (idx 1) use the same
/// neutral visual language so neither choice is visually dominant (UX-002):
/// the trust decision must be made deliberately, not nudged. The focused /
/// hovered button is highlighted.
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
