//! AI state tracking and visual indicator generation.
//!
//! Maintains per-session [`AiProcessState`] and produces [`CellInstance`]
//! quads for pane border overlays and tab-bar indicator bars.

use std::collections::HashMap;

use scribe_common::ai_state::{AiProcessState, AiState};
use scribe_common::ids::SessionId;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// Width of the animated border overlay in pixels.
const BORDER_WIDTH: f32 = 2.0;

/// Animation pulse frequency for processing and idle-prompt states (Hz).
const SLOW_PULSE_HZ: f32 = 1.0;

/// Animation pulse frequency for the permission-prompt state (Hz).
const FAST_PULSE_HZ: f32 = 2.0;

/// Minimum alpha for the pulsing border.
const PULSE_ALPHA_MIN: f32 = 0.3;

/// Maximum alpha for the pulsing border.
const PULSE_ALPHA_MAX: f32 = 0.8;

/// How long the error indicator lasts before fully fading out (seconds).
const ERROR_DECAY_SECS: f32 = 3.0;

/// Wrap period for animation time to prevent f32 precision loss.
/// 100 full sine cycles at TAU ~ 628 seconds of continuous animation.
const ANIMATION_WRAP_PERIOD: f32 = std::f32::consts::TAU * 100.0;

/// Tracks AI state for all sessions and drives border / indicator colours.
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
    pub fn get(&self, session_id: SessionId) -> Option<&AiProcessState> {
        self.states.get(&session_id)
    }

    /// Clear attention states (`IdlePrompt` / `PermissionPrompt`) for a
    /// session, typically in response to user keystrokes. Other states
    /// (`Processing`, `Error`) are left untouched.
    pub fn clear_attention_states(&mut self, session_id: SessionId) {
        if let Some(state) = self.states.get(&session_id) {
            if matches!(state.state, AiState::IdlePrompt | AiState::PermissionPrompt) {
                self.states.remove(&session_id);
            }
        }
    }

    /// Advance the animation clock by `dt` seconds.
    ///
    /// The time is wrapped modulo a large period (100 full sine cycles at TAU)
    /// to prevent f32 precision degradation after long uptime.
    /// 100 * TAU ~ 628 s, far beyond any perceptible phase jump.
    pub fn tick(&mut self, dt: f32) {
        self.animation_time = (self.animation_time + dt) % ANIMATION_WRAP_PERIOD;
    }

    /// Returns `true` if any session has an animated (pulsing or decaying)
    /// state that requires continuous redraw.
    pub fn needs_animation(&self) -> bool {
        self.states.values().any(|s| requires_animation(&s.state))
            || self.error_times.values().any(|&t| {
                // Guard against negative elapsed from animation_time wrapping.
                let elapsed = (self.animation_time - t).max(0.0);
                elapsed < ERROR_DECAY_SECS
            })
    }

    /// Remove all tracked state for a session (e.g. on session exit).
    pub fn remove(&mut self, session_id: SessionId) {
        self.states.remove(&session_id);
        self.error_times.remove(&session_id);
    }

    /// Compute the tab-bar indicator colour for a session.
    ///
    /// Returns the full-alpha base colour for the session's AI state, or
    /// `None` when there is no active AI state.
    pub fn tab_indicator_color(
        &self,
        session_id: SessionId,
        ansi_colors: &[[f32; 4]; 16],
    ) -> Option<[f32; 4]> {
        let state = self.states.get(&session_id)?;
        Some(base_color_full_alpha(&state.state, ansi_colors))
    }

    /// Compute the highest-priority animated border colour across a set of
    /// sessions (for workspace-level aggregation).
    ///
    /// Priority: `PermissionPrompt > IdlePrompt > Error > Processing`.
    pub fn workspace_border_color(
        &self,
        session_ids: &[SessionId],
        ansi_colors: &[[f32; 4]; 16],
    ) -> Option<[f32; 4]> {
        let mut best: Option<(u8, [f32; 4])> = None;

        for &sid in session_ids {
            let Some(state) = self.states.get(&sid) else { continue };
            let priority = state_priority(&state.state);
            let color = self.animated_color(sid, state, ansi_colors);
            // Skip fully-transparent (decayed error).
            if color[3] <= 0.0 {
                continue;
            }
            if best.as_ref().is_none_or(|(bp, _)| priority > *bp) {
                best = Some((priority, color));
            }
        }

        best.map(|(_, color)| color)
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    #[allow(
        clippy::indexing_slicing,
        reason = "fixed-size [f32; 4] arrays, indices 0-2 always valid"
    )]
    fn animated_color(
        &self,
        session_id: SessionId,
        state: &AiProcessState,
        ansi_colors: &[[f32; 4]; 16],
    ) -> [f32; 4] {
        match &state.state {
            AiState::Processing => {
                let alpha = pulse_alpha(self.animation_time, SLOW_PULSE_HZ);
                let base = ansi_green(ansi_colors);
                [base[0], base[1], base[2], alpha]
            }
            AiState::IdlePrompt => {
                let alpha = pulse_alpha(self.animation_time, SLOW_PULSE_HZ);
                let base = AMBER;
                [base[0], base[1], base[2], alpha]
            }
            AiState::PermissionPrompt => {
                let alpha = pulse_alpha(self.animation_time, FAST_PULSE_HZ);
                let base = ansi_red(ansi_colors);
                [base[0], base[1], base[2], alpha]
            }
            AiState::Error => {
                let alpha = self.error_times.get(&session_id).map_or(0.0, |&t| {
                    let elapsed = self.animation_time - t;
                    let remaining = (ERROR_DECAY_SECS - elapsed) / ERROR_DECAY_SECS;
                    (remaining * 0.8).clamp(0.0, 0.8)
                });
                let base = PURPLE;
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
    matches!(state, AiState::Processing | AiState::IdlePrompt | AiState::PermissionPrompt)
}

/// Numeric priority for workspace-level aggregation.
/// Higher value = more urgent.
fn state_priority(state: &AiState) -> u8 {
    match state {
        AiState::PermissionPrompt => 3,
        AiState::IdlePrompt => 2,
        AiState::Error => 1,
        AiState::Processing => 0,
    }
}

/// Compute a pulsing alpha value between [`PULSE_ALPHA_MIN`] and
/// [`PULSE_ALPHA_MAX`] using a sine wave at `hz` cycles per second.
fn pulse_alpha(t: f32, hz: f32) -> f32 {
    let mid = f32::midpoint(PULSE_ALPHA_MIN, PULSE_ALPHA_MAX);
    let amp = (PULSE_ALPHA_MAX - PULSE_ALPHA_MIN) / 2.0;
    mid + amp * (t * std::f32::consts::TAU * hz).sin()
}

// ---------------------------------------------------------------------------
// Colour helpers
// ---------------------------------------------------------------------------

/// Fallback green if the palette is somehow missing index 2.
const FALLBACK_GREEN: [f32; 4] = [0.4, 0.9, 0.5, 1.0];
/// Fallback red if the palette is somehow missing index 1.
const FALLBACK_RED: [f32; 4] = [1.0, 0.2, 0.2, 1.0];
/// Amber / orange for the idle-prompt state (no standard ANSI slot).
const FALLBACK_AMBER: [f32; 4] = [1.0, 0.65, 0.1, 1.0];
/// Purple for the error state (no standard ANSI slot).
const FALLBACK_PURPLE: [f32; 4] = [0.6, 0.2, 0.8, 1.0];

/// ANSI red (index 1) with fallback.
fn ansi_red(ansi_colors: &[[f32; 4]; 16]) -> [f32; 4] {
    ansi_colors.get(1).copied().unwrap_or(FALLBACK_RED)
}

/// ANSI green (index 2) with fallback.
fn ansi_green(ansi_colors: &[[f32; 4]; 16]) -> [f32; 4] {
    ansi_colors.get(2).copied().unwrap_or(FALLBACK_GREEN)
}

/// Amber / orange for the idle-prompt state.
const AMBER: [f32; 4] = FALLBACK_AMBER;
/// Purple for the error state.
const PURPLE: [f32; 4] = FALLBACK_PURPLE;

/// Return the base colour for an AI state at full opacity (for tab indicators).
#[allow(clippy::indexing_slicing, reason = "fixed-size [f32; 4] arrays, indices 0-2 always valid")]
fn base_color_full_alpha(state: &AiState, ansi_colors: &[[f32; 4]; 16]) -> [f32; 4] {
    match state {
        AiState::Processing => {
            let c = ansi_green(ansi_colors);
            [c[0], c[1], c[2], 1.0]
        }
        AiState::IdlePrompt => {
            let c = AMBER;
            [c[0], c[1], c[2], 1.0]
        }
        AiState::PermissionPrompt => {
            let c = ansi_red(ansi_colors);
            [c[0], c[1], c[2], 1.0]
        }
        AiState::Error => {
            let c = PURPLE;
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
/// [`AiStateTracker::workspace_border_color`].
#[allow(
    clippy::many_single_char_names,
    reason = "x/y/w/h/bw are conventional 2-D geometry shorthands"
)]
pub fn build_border_instances(pane_rect: Rect, color: [f32; 4]) -> [CellInstance; 4] {
    let x = pane_rect.x;
    let y = pane_rect.y;
    let w = pane_rect.width;
    let h = pane_rect.height;
    let bw = BORDER_WIDTH;

    let top = solid_quad(x, y, color);
    let bottom = solid_quad(x, y + h - bw, color);
    let left = solid_quad(x, y + bw, color);
    let right = solid_quad(x + w - bw, y + bw, color);

    [top, bottom, left, right]
}

/// Create one solid-colour [`CellInstance`] quad at the given position.
fn solid_quad(x: f32, y: f32, color: [f32; 4]) -> CellInstance {
    CellInstance {
        pos: [x, y],
        size: [0.0, 0.0],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
    }
}
