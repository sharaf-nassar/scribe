//! Feature 013 (T017): the displaced-client "lost control" state.
//!
//! When another controller claims a window this client was driving, the server
//! sends [`ServerMessage::WindowTakenOver`](scribe_common::protocol::ServerMessage::WindowTakenOver).
//! The client freezes its last frame (it expects no further `PtyOutput`),
//! suppresses ALL input for that window, and renders a dimmed backdrop under a
//! centered banner naming the new controller, offering one-action reclaim
//! (Enter or click) that reconnects with `Hello { takeover: true }`.
//!
//! The same state drives BOTH a local client displaced by a remote peer and a
//! remote client displaced by a reclaim — the displaced-client obligations in
//! contracts/remote-protocol.md are transport-agnostic, so the rendering and
//! input suppression live here once and the app layer wires the reclaim to the
//! transport it already speaks.
//!
//! Rendering mirrors the sibling overlay chrome (`remote_connect.rs`,
//! `command_palette.rs`): a full-viewport dim backdrop, a centered bordered
//! box, and text drawn as [`CellInstance`] quads in the terminal GPU pass.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// Overlay layout never needs more than this many grid units, keeping the
/// integer-to-float conversion exact for pixel placement (mirrors the sibling
/// overlays' precision-lint-clean approach).
const MAX_GRID_UNITS: usize = 65_535;
/// Minimum banner width in grid columns so the box never collapses around a
/// short controller name.
const MIN_COLS: usize = 32;
/// Maximum banner width in grid columns so an unusually long device/account name
/// cannot grow the box without bound (the headline is truncated to fit).
const MAX_COLS: usize = 78;

type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

/// What a displaced client renders after `WindowTakenOver` (data-model
/// `LostControlState`). Holds only the new controller's identity strings; the
/// frozen grid content is the panes' own last-rendered state, which the app
/// stops advancing while this is set.
#[derive(Debug, Clone)]
pub struct LostControlState {
    /// New controller's device name (or "this machine" for a local reclaim).
    device_name: String,
    /// New controller's tailnet account display name.
    login_name: String,
}

impl LostControlState {
    #[must_use]
    pub fn new(device_name: String, login_name: String) -> Self {
        Self { device_name, login_name }
    }

    /// Banner headline per FR-009b / settings-and-config.md:
    /// `Controlled by <device> (<account>)`.
    #[must_use]
    fn headline(&self) -> String {
        format!("Controlled by {} ({})", self.device_name, self.login_name)
    }

    /// Append the displaced overlay (dim backdrop + centered banner) to `out`.
    pub fn build_instances(&self, ctx: LostControlBuildContext<'_>) {
        let LostControlBuildContext { out, viewport, cell_size, chrome, resolve_glyph } = ctx;
        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }

        let colors = BannerColors::from_chrome(chrome);
        // Dim the whole window so the frozen frame reads as "not yours right
        // now" (clarification: dimmed frozen view).
        push_solid_rect(out, viewport, colors.backdrop);

        let headline = self.headline();
        let hint = String::from("Press Enter or click to take back control");
        let cols = (longest_line(&[&headline, &hint]) + 2).clamp(MIN_COLS, MAX_COLS);
        let content_width = cols.saturating_sub(2); // exclude the 1-col side pads
        let headline = truncate_to_width(&headline, content_width);
        let hint = truncate_to_width(&hint, content_width);
        let rows = 4; // top pad + headline + hint + bottom pad
        let box_w = grid_extent(cols, cell_w);
        let box_h = grid_extent(rows, cell_h);
        let box_rect = Rect {
            x: viewport.x + ((viewport.width - box_w) / 2.0).max(0.0),
            y: viewport.y + ((viewport.height - box_h) / 2.0).max(0.0),
            width: box_w,
            height: box_h,
        };

        push_solid_rect(out, box_rect, colors.bg);
        draw_border(out, box_rect, colors.border);

        let text_x = box_rect.x + cell_w;
        let mut row_y = box_rect.y + cell_h;
        emit_line(
            out,
            resolve_glyph,
            EmitLine {
                text: &headline,
                start_x: text_x,
                y: row_y,
                cell_w,
                colors: TextColors { fg: colors.headline_fg, bg: colors.bg },
            },
        );
        row_y += cell_h;
        emit_line(
            out,
            resolve_glyph,
            EmitLine {
                text: &hint,
                start_x: text_x,
                y: row_y,
                cell_w,
                colors: TextColors { fg: colors.hint_fg, bg: colors.bg },
            },
        );
    }
}

/// Build-time context handed to [`LostControlState::build_instances`], mirroring
/// the sibling overlay build contexts.
pub struct LostControlBuildContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub viewport: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

/// Resolved colors for the displaced banner, derived from the active theme
/// chrome so it matches the sibling overlays.
struct BannerColors {
    backdrop: [f32; 4],
    bg: [f32; 4],
    border: [f32; 4],
    headline_fg: [f32; 4],
    hint_fg: [f32; 4],
}

impl BannerColors {
    fn from_chrome(chrome: &ChromeColors) -> Self {
        let mut bg = srgb_to_linear_rgba(chrome.tab_bar_active_bg);
        bg[3] = 0.98;
        let border = srgb_to_linear_rgba(chrome.accent);
        let headline_fg = srgb_to_linear_rgba(chrome.tab_text_active);
        let mut hint_fg = srgb_to_linear_rgba(chrome.status_bar_text);
        hint_fg[3] *= 0.85;
        Self {
            // A heavier dim than the picker's transient backdrop: this state is
            // persistent and the window is genuinely not interactive.
            backdrop: [0.0, 0.0, 0.0, 0.55],
            bg,
            border,
            headline_fg,
            hint_fg,
        }
    }
}

fn push_solid_rect(out: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4]) {
    out.push(scribe_renderer::chrome::solid_quad(rect.x, rect.y, rect.width, rect.height, color));
}

fn draw_border(out: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4]) {
    push_solid_rect(out, Rect { x: rect.x, y: rect.y, width: rect.width, height: 1.0 }, color);
    push_solid_rect(
        out,
        Rect { x: rect.x, y: rect.y + rect.height - 1.0, width: rect.width, height: 1.0 },
        color,
    );
    push_solid_rect(out, Rect { x: rect.x, y: rect.y, width: 1.0, height: rect.height }, color);
    push_solid_rect(
        out,
        Rect { x: rect.x + rect.width - 1.0, y: rect.y, width: 1.0, height: rect.height },
        color,
    );
}

/// Foreground/background pair for one rendered text line.
#[derive(Clone, Copy)]
struct TextColors {
    fg: [f32; 4],
    bg: [f32; 4],
}

/// Placement + styling for one call to [`emit_line`], bundled so the helper
/// stays within the argument-count budget.
#[derive(Clone, Copy)]
struct EmitLine<'a> {
    text: &'a str,
    start_x: f32,
    y: f32,
    cell_w: f32,
    colors: TextColors,
}

fn emit_line(
    out: &mut Vec<CellInstance>,
    resolve_glyph: &mut GlyphResolver<'_>,
    line: EmitLine<'_>,
) {
    let EmitLine { text, start_x, y, cell_w, colors } = line;
    for (idx, ch) in text.chars().enumerate() {
        let (uv_min, uv_max) = resolve_glyph(ch);
        out.push(CellInstance {
            pos: [start_x + grid_extent(idx, cell_w), y],
            size: [0.0, 0.0],
            uv_min,
            uv_max,
            fg_color: colors.fg,
            bg_color: colors.bg,
            corner_radius: 0.0,
        });
    }
}

/// Longest line length in chars across `lines`, used to size the banner box.
fn longest_line(lines: &[&str]) -> usize {
    lines.iter().map(|line| line.chars().count()).max().unwrap_or(0)
}

/// Truncate `text` to at most `max` display columns, appending an ellipsis when
/// it does not fit so a long device/account name never spills past the banner.
fn truncate_to_width(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    if max <= 1 {
        return "…".chars().take(max).collect();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Pixel extent of `units` grid cells, converting through `u16` so the
/// `usize`→`f32` step is exact (matches the sibling overlays' lint-clean math).
fn grid_extent(units: usize, cell: f32) -> f32 {
    f32::from(u16::try_from(units.min(MAX_GRID_UNITS)).unwrap_or(u16::MAX)) * cell
}
