//! In-app GPU-rendered update confirmation dialog overlay.
//!
//! Follows the same pattern as `close_dialog.rs`: renders as
//! [`CellInstance`] quads in the same GPU pass as the terminal.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// What the user chose in the update dialog.
#[derive(Clone, Copy)]
pub enum UpdateAction {
    /// Install the update now.
    Confirm,
    /// Dismiss the notification.
    Dismiss,
}

/// Index of the currently focused button.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonIndex {
    UpdateNow = 0,
    Later = 1,
}

impl ButtonIndex {
    fn next(self) -> Self {
        match self {
            Self::UpdateNow => Self::Later,
            Self::Later => Self::UpdateNow,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::UpdateNow => Self::Later,
            Self::Later => Self::UpdateNow,
        }
    }

    fn to_action(self) -> UpdateAction {
        match self {
            Self::UpdateNow => UpdateAction::Confirm,
            Self::Later => UpdateAction::Dismiss,
        }
    }
}

const BUTTON_COUNT: usize = 2;

/// Labels for each button, in order matching [`ButtonIndex`].
const BUTTON_LABELS: [&str; BUTTON_COUNT] = ["Update Now", "Later"];

/// Minimum number of columns the dialog can shrink to (ensures buttons fit).
const MIN_DIALOG_COLS: usize = 46;

/// Horizontal padding inside the dialog (columns from each edge).
const PADDING: usize = 3;

/// Height of each button in cell rows (top pad + label + bottom pad).
const BUTTON_HEIGHT_ROWS: usize = 3;

/// State for the in-app update dialog overlay.
pub struct UpdateDialog {
    /// Version string of the available update.
    version: String,
    #[allow(dead_code, reason = "release_url stored for future browser-open action")]
    release_url: String,
    /// Currently keyboard-focused button.
    focused: ButtonIndex,
    /// Button the mouse is currently hovering (if any).
    hovered: Option<usize>,
    /// Cached button hit rects from the last render (viewport-pixel coords).
    button_rects: [Rect; BUTTON_COUNT],
}

impl UpdateDialog {
    pub fn new(version: String, release_url: String) -> Self {
        Self {
            version,
            release_url,
            focused: ButtonIndex::UpdateNow,
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
    pub fn confirm(&self) -> UpdateAction {
        self.focused.to_action()
    }

    /// Update hover state from cursor position. Returns `true` if the state changed.
    pub fn update_hover(&mut self, x: f32, y: f32) -> bool {
        let prev = self.hovered;
        self.hovered = self.button_rects.iter().position(|r| r.contains(x, y));
        self.hovered != prev
    }

    /// Handle a mouse click at `(x, y)`. Returns `Some(action)` if a button was clicked.
    pub fn click(&self, x: f32, y: f32) -> Option<UpdateAction> {
        let idx = self.button_rects.iter().position(|r| r.contains(x, y))?;
        let button = match idx {
            0 => ButtonIndex::UpdateNow,
            1 => ButtonIndex::Later,
            _ => return None,
        };
        Some(button.to_action())
    }

    /// Build GPU instances for the dialog overlay.
    ///
    /// Appends a full-viewport backdrop and a centered dialog box with title,
    /// description, separator, and buttons into `out`.
    #[allow(
        clippy::too_many_lines,
        reason = "single render builder: backdrop, border, box, title, separator, body, buttons"
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

        let colors = DialogColors::from_chrome(chrome);

        // -- Backdrop --
        push_solid_rect(out, viewport, colors.backdrop);

        // -- Dialog dimensions --
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "viewport.width / cell_w yields a small positive value fitting in usize"
        )]
        let max_cols = (viewport.width / cell_w) as usize;
        let dialog_cols: usize = 50_usize.min(max_cols.max(MIN_DIALOG_COLS));

        let body_lines = self.body_lines();

        // Vertical layout (row indices relative to dialog top):
        //   0           top border (1px line)
        //   1           blank
        //   2           title
        //   3           blank
        //   4..4+body   body lines
        //   4+body      blank
        //   4+body+1    separator line
        //   4+body+2    blank
        //   4+body+3    button row (3 rows tall: pad + label + pad)
        //   4+body+5+1  blank (bottom padding)
        //   4+body+5+2  bottom border
        let body_count = body_lines.len();
        let content_rows = 2 + 1 + 1 + body_count + 1 + 1 + 1 + BUTTON_HEIGHT_ROWS + 1;

        #[allow(
            clippy::cast_precision_loss,
            reason = "dialog_cols and content_rows are small computed values fitting in f32"
        )]
        let (dialog_w, dialog_h) = (dialog_cols as f32 * cell_w, content_rows as f32 * cell_h);

        let dialog_x = viewport.x + (viewport.width - dialog_w).max(0.0) / 2.0;
        let dialog_y = viewport.y + (viewport.height - dialog_h).max(0.0) / 2.0;
        let dialog_rect = Rect { x: dialog_x, y: dialog_y, width: dialog_w, height: dialog_h };

        // -- Dialog background --
        push_solid_rect(out, dialog_rect, colors.dialog_bg);

        // -- Border (1px lines on top and bottom edges) --
        let border_rect_top = Rect { x: dialog_x, y: dialog_y, width: dialog_w, height: 1.0 };
        let border_rect_bottom =
            Rect { x: dialog_x, y: dialog_y + dialog_h - 1.0, width: dialog_w, height: 1.0 };
        push_solid_rect(out, border_rect_top, colors.border);
        push_solid_rect(out, border_rect_bottom, colors.border);

        // -- Title (centered) --
        let title = "Update Available";
        let title_row = 2;
        emit_text_centered(
            out,
            title,
            dialog_x,
            dialog_y,
            title_row,
            dialog_cols,
            cell_size,
            colors.title_fg,
            colors.dialog_bg,
            resolve_glyph,
        );

        // -- Body text --
        let body_start_row = 4;
        for (i, line) in body_lines.iter().enumerate() {
            emit_text_line(
                out,
                line,
                dialog_x,
                dialog_y,
                body_start_row + i,
                PADDING,
                dialog_cols,
                cell_size,
                colors.body_fg,
                colors.dialog_bg,
                resolve_glyph,
            );
        }

        // -- Separator line between body and buttons --
        let sep_row = body_start_row + body_count + 1;
        #[allow(
            clippy::cast_precision_loss,
            reason = "sep_row is a small computed value fitting in f32"
        )]
        let sep_y = dialog_y + sep_row as f32 * cell_h + cell_h / 2.0;
        #[allow(
            clippy::cast_precision_loss,
            reason = "PADDING is a small constant (3) fitting in f32"
        )]
        let sep_inset = PADDING as f32 * cell_w;
        let sep_rect = Rect {
            x: dialog_x + sep_inset,
            y: sep_y,
            width: dialog_w - sep_inset * 2.0,
            height: 1.0,
        };
        push_solid_rect(out, sep_rect, colors.separator);

        // -- Buttons --
        let button_row = sep_row + 2;
        self.build_buttons(
            out,
            dialog_x,
            dialog_y,
            button_row,
            dialog_cols,
            cell_size,
            &colors,
            resolve_glyph,
        );
    }

    /// Build the two action buttons with proper padding and per-button colors.
    #[allow(
        clippy::too_many_arguments,
        reason = "needs dialog origin, row, column count, cell size, colors, and glyph resolver"
    )]
    fn build_buttons(
        &mut self,
        out: &mut Vec<CellInstance>,
        dialog_x: f32,
        dialog_y: f32,
        button_row: usize,
        dialog_cols: usize,
        cell_size: (f32, f32),
        colors: &DialogColors,
        resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    ) {
        let (cell_w, cell_h) = cell_size;

        // Each button: 2 padding + label + 2 padding.
        let btn_col_widths: Vec<usize> = BUTTON_LABELS.iter().map(|l| l.len() + 4).collect();
        let total_btn_cols: usize = btn_col_widths.iter().sum();
        let usable = dialog_cols.saturating_sub(PADDING * 2);
        let remaining = usable.saturating_sub(total_btn_cols);
        let gap = if BUTTON_COUNT > 1 { remaining / (BUTTON_COUNT - 1) } else { 0 };

        #[allow(
            clippy::cast_precision_loss,
            reason = "button_row is a small computed value fitting in f32"
        )]
        let button_y = dialog_y + button_row as f32 * cell_h;
        #[allow(
            clippy::cast_precision_loss,
            reason = "BUTTON_HEIGHT_ROWS is a small constant (3) fitting in f32"
        )]
        let button_h = BUTTON_HEIGHT_ROWS as f32 * cell_h;

        let mut col = PADDING;
        for (btn_idx, label) in BUTTON_LABELS.iter().enumerate() {
            #[allow(
                clippy::indexing_slicing,
                reason = "btn_idx < BUTTON_COUNT (2), btn_col_widths has 2 elements"
            )]
            let btn_w_cols = btn_col_widths[btn_idx];
            let is_focused = self.focused as usize == btn_idx;
            let is_hovered = self.hovered == Some(btn_idx);
            let active = is_focused || is_hovered;

            // Per-button color scheme: UpdateNow = accent, Later = subtle.
            let (fg, bg) = button_colors(btn_idx, active, colors);

            // Button background rect (spans BUTTON_HEIGHT_ROWS).
            #[allow(
                clippy::cast_precision_loss,
                reason = "col and btn_w_cols are small positive integers fitting in f32"
            )]
            let btn_rect = Rect {
                x: dialog_x + col as f32 * cell_w,
                y: button_y,
                width: btn_w_cols as f32 * cell_w,
                height: button_h,
            };
            push_solid_rect(out, btn_rect, bg);

            // Label (vertically centered in the button — middle row of 3).
            let label_col = col + 2;
            let label_row = button_row + 1; // middle row
            emit_text_line(
                out,
                label,
                dialog_x,
                dialog_y,
                label_row,
                label_col,
                dialog_cols,
                cell_size,
                fg,
                bg,
                resolve_glyph,
            );

            #[allow(
                clippy::indexing_slicing,
                reason = "btn_idx is always < BUTTON_COUNT (2), array has 2 elements"
            )]
            {
                self.button_rects[btn_idx] = btn_rect;
            }

            col += btn_w_cols + gap;
        }
    }

    /// Build the body text lines for the dialog.
    fn body_lines(&self) -> Vec<String> {
        vec![
            format!("Version {} is ready to install.", self.version),
            String::new(),
            String::from("On Linux, your sessions will be preserved"),
            String::from("via hot-reload. On macOS, you'll need to"),
            String::from("restart after the update completes."),
        ]
    }
}

/// Per-button color selection.
///
/// - **Update Now** (idx 0): accent — themed accent color highlight
/// - **Later** (idx 1): subtle — low-contrast background, normal text
fn button_colors(btn_idx: usize, active: bool, colors: &DialogColors) -> ([f32; 4], [f32; 4]) {
    if active {
        match btn_idx {
            0 => (colors.button_active_fg, colors.button_accent_bg),
            _ => (colors.button_active_fg, colors.button_active_bg),
        }
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
    button_accent_bg: [f32; 4],
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
            button_accent_bg: srgb_to_linear_rgba(chrome.accent),
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
        _pad: 0.0,
    });
}

/// Emit centered text on a given row.
#[allow(
    clippy::too_many_arguments,
    reason = "needs dialog origin, row, column count, cell size, colors, and glyph resolver"
)]
fn emit_text_centered(
    out: &mut Vec<CellInstance>,
    text: &str,
    dialog_x: f32,
    dialog_y: f32,
    row: usize,
    dialog_cols: usize,
    cell_size: (f32, f32),
    fg: [f32; 4],
    bg: [f32; 4],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let start_col = dialog_cols.saturating_sub(text.len()) / 2;
    emit_text_line(
        out,
        text,
        dialog_x,
        dialog_y,
        row,
        start_col,
        dialog_cols,
        cell_size,
        fg,
        bg,
        resolve_glyph,
    );
}

/// Emit a line of text as individual character instances.
#[allow(
    clippy::too_many_arguments,
    reason = "needs dialog origin, row, column, cell size, colors, and glyph resolver"
)]
#[allow(
    clippy::cast_precision_loss,
    reason = "row/col indices are small positive integers fitting in f32"
)]
fn emit_text_line(
    out: &mut Vec<CellInstance>,
    text: &str,
    dialog_x: f32,
    dialog_y: f32,
    row: usize,
    start_col: usize,
    max_cols: usize,
    cell_size: (f32, f32),
    fg: [f32; 4],
    bg: [f32; 4],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let (cell_w, cell_h) = cell_size;
    let y = dialog_y + row as f32 * cell_h;

    for (i, ch) in text.chars().enumerate() {
        let col = start_col + i;
        if col >= max_cols {
            break;
        }
        let x = dialog_x + col as f32 * cell_w;
        let (uv_min, uv_max) = resolve_glyph(ch);
        out.push(CellInstance {
            pos: [x, y],
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
