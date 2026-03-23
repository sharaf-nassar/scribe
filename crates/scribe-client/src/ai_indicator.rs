//! AI state tracking and visual indicator generation.
//!
//! Maintains per-session [`AiProcessState`] and produces [`CellInstance`]
//! quads for pane border overlays and tab-bar badges.

use std::collections::HashMap;

use scribe_common::ai_state::{AiProcessState, AiState};
use scribe_common::ids::SessionId;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// Width of the animated border overlay in pixels.
const BORDER_WIDTH: f32 = 2.0;

/// Animation pulse frequency for idle/permission states (Hz).
const PULSE_HZ: f32 = 1.0;

/// Minimum alpha for the pulsing border.
const PULSE_ALPHA_MIN: f32 = 0.3;

/// Maximum alpha for the pulsing border.
const PULSE_ALPHA_MAX: f32 = 0.8;

/// How long the error flash lasts before fully decaying (seconds).
const ERROR_DECAY_SECS: f32 = 0.5;

/// Wrap period for animation time to prevent f32 precision loss.
/// 100 full sine cycles at TAU ~ 628 seconds of continuous animation.
const ANIMATION_WRAP_PERIOD: f32 = std::f32::consts::TAU * 100.0;

/// Tracks AI state for all sessions and drives border / badge colours.
pub struct AiStateTracker {
    states: HashMap<SessionId, AiProcessState>,
    /// Monotonically increasing time in seconds, used for pulse animation.
    animation_time: f32,
    /// Time of most recent error state per session (for decay animation).
    error_times: HashMap<SessionId, f32>,
}

impl AiStateTracker {
    /// Create a new tracker with no sessions.
    pub fn new() -> Self {
        Self { states: HashMap::new(), animation_time: 0.0, error_times: HashMap::new() }
    }

    /// Store the latest AI state for a session.
    pub fn update(&mut self, session_id: SessionId, ai_state: AiProcessState) {
        if matches!(ai_state.state, AiState::Error) {
            self.error_times.insert(session_id, self.animation_time);
        } else {
            // Clear stale error timestamp when state leaves Error.
            self.error_times.remove(&session_id);
        }
        self.states.insert(session_id, ai_state);
    }

    /// Get the current AI state for a session.
    #[allow(dead_code, reason = "public API for external state queries")]
    pub fn get(&self, session_id: SessionId) -> Option<&AiProcessState> {
        self.states.get(&session_id)
    }

    /// Advance the animation clock by `dt` seconds.
    ///
    /// The time is wrapped modulo a large period (100 full sine cycles at TAU)
    /// to prevent f32 precision degradation after long uptime.
    /// 100 * TAU ~ 628 s, far beyond any perceptible phase jump.
    pub fn tick(&mut self, dt: f32) {
        self.animation_time = (self.animation_time + dt) % ANIMATION_WRAP_PERIOD;
    }

    /// Returns `true` if any session has an animated (pulsing) state.
    pub fn needs_animation(&self) -> bool {
        self.states.values().any(|s| requires_animation(&s.state))
            || self.error_times.values().any(|&t| {
                let elapsed = self.animation_time - t;
                elapsed < ERROR_DECAY_SECS
            })
    }

    /// Compute the animated border colour for a session.
    ///
    /// `ansi_colors` is the theme's 16-colour ANSI palette and `accent` is the
    /// chrome accent colour.  Returns `None` when there is no active AI state
    /// or the state has fully decayed.
    pub fn border_color(
        &self,
        session_id: SessionId,
        ansi_colors: &[[f32; 4]; 16],
        accent: [f32; 4],
    ) -> Option<[f32; 4]> {
        let state = self.states.get(&session_id)?;
        Some(self.state_to_border_color(session_id, state, ansi_colors, accent))
    }

    /// Compute the badge colour for a session (no alpha animation).
    ///
    /// `ansi_colors` is the theme's 16-colour ANSI palette and `accent` is the
    /// chrome accent colour.  Returns `None` when there is no active AI state.
    #[allow(dead_code, reason = "used by tab_bar badge rendering in future integration")]
    pub fn badge_color(
        &self,
        session_id: SessionId,
        ansi_colors: &[[f32; 4]; 16],
        accent: [f32; 4],
    ) -> Option<[f32; 4]> {
        let state = self.states.get(&session_id)?;
        Some(base_color_full_alpha(&state.state, ansi_colors, accent))
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    #[allow(
        clippy::indexing_slicing,
        reason = "fixed-size [f32; 4] arrays, indices 0-2 always valid"
    )]
    fn state_to_border_color(
        &self,
        session_id: SessionId,
        state: &AiProcessState,
        ansi_colors: &[[f32; 4]; 16],
        accent: [f32; 4],
    ) -> [f32; 4] {
        match &state.state {
            AiState::IdlePrompt => {
                let alpha = pulse_alpha(self.animation_time, PULSE_HZ);
                let base = ansi_green(ansi_colors);
                [base[0], base[1], base[2], alpha]
            }
            AiState::PermissionPrompt => {
                let alpha = pulse_alpha(self.animation_time, PULSE_HZ);
                let base = ansi_yellow(ansi_colors);
                [base[0], base[1], base[2], alpha]
            }
            AiState::Processing => [accent[0], accent[1], accent[2], 0.4],
            AiState::Error => {
                let alpha = self.error_times.get(&session_id).map_or(0.0, |&t| {
                    let elapsed = self.animation_time - t;
                    let remaining = (ERROR_DECAY_SECS - elapsed) / ERROR_DECAY_SECS;
                    (remaining * 0.8).clamp(0.0, 0.8)
                });
                let base = ansi_red(ansi_colors);
                [base[0], base[1], base[2], alpha]
            }
        }
    }
}

impl Default for AiStateTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Return `true` if the given state requires continuous animation updates.
fn requires_animation(state: &AiState) -> bool {
    matches!(state, AiState::IdlePrompt | AiState::PermissionPrompt)
}

/// Compute a pulsing alpha value between [`PULSE_ALPHA_MIN`] and
/// [`PULSE_ALPHA_MAX`] using a sine wave at `hz` cycles per second.
fn pulse_alpha(t: f32, hz: f32) -> f32 {
    let mid = f32::midpoint(PULSE_ALPHA_MIN, PULSE_ALPHA_MAX);
    let amp = (PULSE_ALPHA_MAX - PULSE_ALPHA_MIN) / 2.0;
    mid + amp * (t * std::f32::consts::TAU * hz).sin()
}

// ---------------------------------------------------------------------------
// ANSI palette helpers — safe `.get()` with sensible fallbacks
// ---------------------------------------------------------------------------

/// Fallback green if the palette is somehow missing index 2.
const FALLBACK_GREEN: [f32; 4] = [0.4, 0.9, 0.5, 1.0];
/// Fallback yellow if the palette is somehow missing index 3.
const FALLBACK_YELLOW: [f32; 4] = [1.0, 0.75, 0.2, 1.0];
/// Fallback red if the palette is somehow missing index 1.
const FALLBACK_RED: [f32; 4] = [1.0, 0.2, 0.2, 1.0];

/// ANSI red (index 1) with fallback.
fn ansi_red(ansi_colors: &[[f32; 4]; 16]) -> [f32; 4] {
    ansi_colors.get(1).copied().unwrap_or(FALLBACK_RED)
}

/// ANSI green (index 2) with fallback.
fn ansi_green(ansi_colors: &[[f32; 4]; 16]) -> [f32; 4] {
    ansi_colors.get(2).copied().unwrap_or(FALLBACK_GREEN)
}

/// ANSI yellow (index 3) with fallback.
fn ansi_yellow(ansi_colors: &[[f32; 4]; 16]) -> [f32; 4] {
    ansi_colors.get(3).copied().unwrap_or(FALLBACK_YELLOW)
}

/// Return the base colour for an AI state at full opacity (for badges).
///
/// Uses the theme's ANSI palette and chrome accent colour.
#[allow(dead_code, reason = "called by badge_color which is API for tab_bar badge rendering")]
#[allow(clippy::indexing_slicing, reason = "fixed-size [f32; 4] arrays, indices 0-2 always valid")]
fn base_color_full_alpha(
    state: &AiState,
    ansi_colors: &[[f32; 4]; 16],
    accent: [f32; 4],
) -> [f32; 4] {
    match state {
        AiState::IdlePrompt => {
            let c = ansi_green(ansi_colors);
            [c[0], c[1], c[2], 1.0]
        }
        AiState::PermissionPrompt => {
            let c = ansi_yellow(ansi_colors);
            [c[0], c[1], c[2], 1.0]
        }
        AiState::Processing => [accent[0], accent[1], accent[2], 1.0],
        AiState::Error => {
            let c = ansi_red(ansi_colors);
            [c[0], c[1], c[2], 1.0]
        }
    }
}

// ---------------------------------------------------------------------------
// Border instance generation
// ---------------------------------------------------------------------------

/// Build four thin border quads around a pane's full area (including tab bar).
///
/// Each of the four sides is rendered as one solid-colour [`CellInstance`]
/// (no glyph: `uv_min == uv_max == [0,0]`).  The `color` comes from
/// [`AiStateTracker::border_color`].
#[allow(
    clippy::many_single_char_names,
    reason = "x/y/w/h/bw are conventional 2-D geometry shorthands"
)]
pub fn build_border_instances(pane_rect: Rect, color: [f32; 4]) -> [CellInstance; 4] {
    // The border wraps the entire pane rect including the tab bar area.
    let x = pane_rect.x;
    let y = pane_rect.y;
    let w = pane_rect.width;
    let h = pane_rect.height;
    let bw = BORDER_WIDTH;

    // Top edge
    let top = solid_quad(x, y, color);
    // Bottom edge
    let bottom = solid_quad(x, y + h - bw, color);
    // Left edge (excluding corners already covered by top/bottom)
    let left = solid_quad(x, y + bw, color);
    // Right edge (excluding corners already covered by top/bottom)
    let right = solid_quad(x + w - bw, y + bw, color);

    [top, bottom, left, right]
}

/// Create one solid-colour [`CellInstance`] quad at the given position.
fn solid_quad(x: f32, y: f32, color: [f32; 4]) -> CellInstance {
    CellInstance {
        pos: [x, y],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
    }
}

// ---------------------------------------------------------------------------
// Badge instance generation
// ---------------------------------------------------------------------------

/// Parameters for building a tab-bar AI badge.
#[allow(dead_code, reason = "public API for tab_bar badge rendering, used in future phases")]
pub struct BadgeParams<'a> {
    /// The full pane rect; the badge is placed in the tab bar area.
    pub pane_rect: Rect,
    /// Cell size `(width, height)` from the font.
    pub cell_size: (f32, f32),
    /// Badge dot colour (from [`AiStateTracker::badge_color`]).
    pub color: [f32; 4],
    /// Optional tool name to display after the badge dot (e.g. "Bash").
    pub tool_name: Option<&'a str>,
    /// Closure that resolves a `char` to atlas UV coordinates `(uv_min, uv_max)`.
    pub resolve_glyph: &'a dyn Fn(char) -> ([f32; 2], [f32; 2]),
}

/// Build cell instances for an AI state badge at the start of a tab bar.
///
/// Renders a filled dot (●) followed by an optional tool name.
#[allow(dead_code, reason = "public API for tab_bar badge rendering, used in future phases")]
pub fn build_badge_instances(params: &BadgeParams<'_>) -> Vec<CellInstance> {
    let (cell_w, _cell_h) = params.cell_size;
    if cell_w <= 0.0 {
        return Vec::new();
    }

    // The badge sits in the tab bar, which starts at the top of pane_rect.
    let bar_y = params.pane_rect.y;
    let max_cols = columns_in_width(params.pane_rect.width, cell_w);

    let mut instances = Vec::new();
    let mut col: usize = 0;

    // Dot character.
    col = emit_badge_char(&mut instances, '●', col, max_cols, params, bar_y);

    // Space after dot.
    if col < max_cols {
        col = emit_badge_char(&mut instances, ' ', col, max_cols, params, bar_y);
    }

    // Optional tool name.
    if let Some(tool) = params.tool_name {
        for ch in tool.chars() {
            if col >= max_cols {
                break;
            }
            col = emit_badge_char(&mut instances, ch, col, max_cols, params, bar_y);
        }
    }

    let _ = col; // col is advanced by side-effecting calls; final value not needed
    instances
}

/// Emit a single badge character at the given column.
#[allow(dead_code, reason = "called by build_badge_instances")]
#[allow(
    clippy::too_many_arguments,
    reason = "helper that needs all render context: instances, char, col, max, params, y"
)]
#[allow(
    clippy::cast_precision_loss,
    reason = "column index is a small positive integer fitting in f32"
)]
fn emit_badge_char(
    instances: &mut Vec<CellInstance>,
    ch: char,
    col: usize,
    max_cols: usize,
    params: &BadgeParams<'_>,
    bar_y: f32,
) -> usize {
    if col >= max_cols {
        return col;
    }

    let (cell_w, _cell_h) = params.cell_size;
    let x = params.pane_rect.x + col as f32 * cell_w;
    let (uv_min, uv_max) = (params.resolve_glyph)(ch);

    instances.push(CellInstance {
        pos: [x, bar_y],
        uv_min,
        uv_max,
        fg_color: params.color,
        bg_color: [0.0, 0.0, 0.0, 0.0], // transparent background
    });

    col + 1
}

/// How many cell-width columns fit in a pixel width.
#[allow(dead_code, reason = "called by build_badge_instances")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "width / cell_w yields a small positive value fitting in usize"
)]
fn columns_in_width(width: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 { 0 } else { (width / cell_w) as usize }
}
