//! Feature 015 (T015/T020): the connecting/owning client's collaborative
//! window-sharing surfaces.
//!
//! When a window is shared across machines in
//! [`SharingMode::SharedSingleTypist`](scribe_common::config::SharingMode::SharedSingleTypist)
//! the server broadcasts a full-state [`ShareRoster`](scribe_common::protocol::ServerMessage::ShareRoster)
//! on every membership / control change. The client mirrors the latest roster in
//! a [`ShareState`] and derives whether this machine is the current input holder
//! or a **viewer**. A viewer renders the live terminal exactly as normal (never
//! the frozen dimmed [`crate::lost_control::LostControlState`], which stays
//! reserved for `SingleController` displacement) but suppresses its own
//! keystrokes locally — the server drops them anyway — and instead shows a
//! non-intrusive [`ControlHint`] telling it who holds control and how to take it.
//!
//! The current holder (or the owner, when control is unheld) answers an incoming
//! request-and-grant [`ControlRequested`](scribe_common::protocol::ServerMessage::ControlRequested)
//! through a small [`ControlRequestPrompt`] modal. Both overlays render as
//! [`CellInstance`] quads in the terminal GPU pass, cloning the chrome
//! conventions of the sibling overlays ([`crate::lost_control`],
//! [`crate::paste_confirmation_dialog`]): a bordered box drawn from the active
//! theme chrome, keyboard-driven, with a dim backdrop only for the modal prompt.

use std::time::{Duration, Instant};

use scribe_common::config::SharingMode;
use scribe_common::ids::WindowId;
use scribe_common::protocol::ParticipantInfo;
use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// How long a transient control hint / denied notice stays on screen before the
/// idle-wake loop clears it.
pub const HINT_DURATION: Duration = Duration::from_secs(5);

/// Overlay layout never needs more than this many grid units, keeping the
/// integer-to-float conversion exact for pixel placement (mirrors the sibling
/// overlays' precision-lint-clean approach).
const MAX_GRID_UNITS: usize = 65_535;
/// Minimum banner width in grid columns so a box never collapses around short
/// text.
const MIN_COLS: usize = 32;
/// Maximum banner width in grid columns so an unusually long device/account name
/// cannot grow a box without bound (the text is truncated to fit).
const MAX_COLS: usize = 78;

type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

/// The latest [`ShareRoster`](scribe_common::protocol::ServerMessage::ShareRoster)
/// for the client's window, mirrored so the UI can derive roles and render the
/// presence badge. `None` on the client whenever the window is not part of a
/// broadcasting share (`SingleController` / solo).
#[derive(Debug, Clone)]
pub struct ShareState {
    /// The shared window's id.
    pub window_id: WindowId,
    /// The complete current participant roster (server order, by join).
    pub participants: Vec<ParticipantInfo>,
    /// The window's active sharing mode.
    pub mode: SharingMode,
    /// The current input-control holder's participant id, or `None` when unheld.
    pub holder: Option<u64>,
}

impl ShareState {
    /// Number of attached participants (owner + remotes).
    #[must_use]
    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }

    /// Whether more than one machine is attached — the trigger for the presence
    /// badge (T024) and the viewer affordances (T015/T020).
    #[must_use]
    pub fn is_multi(&self) -> bool {
        self.participants.len() > 1
    }

    /// The current control holder's roster entry, if any.
    #[must_use]
    pub fn holder_entry(&self) -> Option<&ParticipantInfo> {
        let holder = self.holder?;
        self.participants.iter().find(|p| p.participant_id == holder)
    }

    /// Display label for the current control holder, or `None` when unheld.
    #[must_use]
    pub fn holder_label(&self) -> Option<String> {
        self.holder_entry().map(participant_label)
    }
}

/// Human-readable label for a roster entry: `device (login)`, or just the device
/// name when the account is unknown (the owner's `Local` entry is named
/// `this machine` server-side, with an empty login).
#[must_use]
pub fn participant_label(p: &ParticipantInfo) -> String {
    if p.login_name.is_empty() {
        p.device_name.clone()
    } else {
        format!("{} ({})", p.device_name, p.login_name)
    }
}

/// A transient, non-intrusive hint shown to a viewer that just pressed a
/// (suppressed) key: it names the control holder and how to take control. Unlike
/// the lost-control banner it does NOT dim or freeze the window — output keeps
/// streaming live behind it (T015/T020).
#[derive(Debug, Clone)]
pub struct ControlHint {
    text: String,
    expires_at: Instant,
}

impl ControlHint {
    /// Build a hint that expires after [`HINT_DURATION`].
    #[must_use]
    pub fn new(text: String) -> Self {
        Self { text, expires_at: Instant::now() + HINT_DURATION }
    }

    /// The instant this hint should be cleared (for the idle-wake deadline).
    #[must_use]
    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    /// Whether this hint has outlived [`HINT_DURATION`].
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    /// Append the hint (a small bordered strip at the top-center, no backdrop) to
    /// `out`.
    pub fn build_instances(&self, ctx: ShareOverlayContext<'_>) {
        let ShareOverlayContext { out, viewport, cell_size, chrome, resolve_glyph } = ctx;
        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }
        let colors = BannerColors::from_chrome(chrome);
        let cols = (self.text.chars().count() + 2).clamp(MIN_COLS, MAX_COLS);
        let content_width = cols.saturating_sub(2);
        let text = truncate_to_width(&self.text, content_width);
        let rows = 3; // top pad + text + bottom pad
        let box_w = grid_extent(cols, cell_w);
        let box_h = grid_extent(rows, cell_h);
        // Anchored near the top so it never sits over the terminal cursor / active
        // work, staying non-intrusive while output streams live behind it.
        let box_rect = Rect {
            x: viewport.x + ((viewport.width - box_w) / 2.0).max(0.0),
            y: viewport.y + cell_h,
            width: box_w,
            height: box_h,
        };
        push_solid_rect(out, box_rect, colors.bg);
        draw_border(out, box_rect, colors.border);
        emit_line(
            out,
            resolve_glyph,
            EmitLine {
                text: &text,
                start_x: box_rect.x + cell_w,
                y: box_rect.y + cell_h,
                cell_w,
                colors: TextColors { fg: colors.headline_fg, bg: colors.bg },
            },
        );
    }
}

/// A pending incoming control request that this client — the current holder, or
/// the owner when control is unheld — must grant or deny (request-and-grant
/// acquisition, T020). Modal while set: `Enter` grants, `Esc` denies.
#[derive(Debug, Clone)]
pub struct ControlRequestPrompt {
    window_id: WindowId,
    requester_id: u64,
    requester_label: String,
}

impl ControlRequestPrompt {
    /// Build the prompt from the `from` participant of a `ControlRequested`.
    #[must_use]
    pub fn new(window_id: WindowId, from: &ParticipantInfo) -> Self {
        Self {
            window_id,
            requester_id: from.participant_id,
            requester_label: participant_label(from),
        }
    }

    /// The shared window this request targets.
    #[must_use]
    pub fn window_id(&self) -> WindowId {
        self.window_id
    }

    /// The requesting participant's id — the [`ControlGrant`] target.
    #[must_use]
    pub fn requester_id(&self) -> u64 {
        self.requester_id
    }

    /// Append the prompt (dim backdrop + centered bordered box) to `out`.
    pub fn build_instances(&self, ctx: ShareOverlayContext<'_>) {
        let ShareOverlayContext { out, viewport, cell_size, chrome, resolve_glyph } = ctx;
        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }
        let colors = BannerColors::from_chrome(chrome);
        push_solid_rect(out, viewport, colors.backdrop);

        let headline = format!("{} wants control", self.requester_label);
        let hint = String::from("Press Enter to grant \u{00B7} Esc to deny");
        let cols = (longest_line(&[&headline, &hint]) + 2).clamp(MIN_COLS, MAX_COLS);
        let content_width = cols.saturating_sub(2);
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

/// Build-time context handed to the share overlays, mirroring the sibling
/// overlay build contexts.
pub struct ShareOverlayContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub viewport: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

/// Resolved colors for the share overlays, derived from the active theme chrome
/// so they match the sibling overlays.
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
            // Lighter than the lost-control dim: the window is still live and
            // interactive behind the modal, only the input decision is pending.
            backdrop: [0.0, 0.0, 0.0, 0.4],
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

/// Longest line length in chars across `lines`, used to size a box.
fn longest_line(lines: &[&str]) -> usize {
    lines.iter().map(|line| line.chars().count()).max().unwrap_or(0)
}

/// Truncate `text` to at most `max` display columns, appending an ellipsis when
/// it does not fit so a long device/account name never spills past the box.
fn truncate_to_width(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    if max <= 1 {
        return "\u{2026}".chars().take(max).collect();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('\u{2026}');
    out
}

/// Pixel extent of `units` grid cells, converting through `u16` so the
/// `usize`→`f32` step is exact (matches the sibling overlays' lint-clean math).
fn grid_extent(units: usize, cell: f32) -> f32 {
    f32::from(u16::try_from(units.min(MAX_GRID_UNITS)).unwrap_or(u16::MAX)) * cell
}
