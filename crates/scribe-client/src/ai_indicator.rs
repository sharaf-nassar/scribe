//! AI state tracking and visual indicator generation.
//!
//! Maintains per-session [`AiProcessState`] and produces [`CellInstance`]
//! quads for pane border overlays and tab-bar indicator bars.
//!
//! Colours, per-state enable flags, and auto-clear timeouts are driven by
//! [`ClaudeStatesConfig`] rather than compile-time constants.

use std::collections::HashMap;

use scribe_common::ai_state::{AiProcessState, AiState};
use scribe_common::config::{AiStateEntry, ClaudeStatesConfig};
use scribe_common::ids::SessionId;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// Width of the animated border overlay in pixels.
const BORDER_WIDTH: f32 = 2.0;

/// Default pulse frequency when `pulse_ms` is 0 (static display).
const DEFAULT_PULSE_HZ: f32 = 1.0;

/// Minimum alpha for the pulsing border.
const PULSE_ALPHA_MIN: f32 = 0.3;

/// Maximum alpha for the pulsing border.
const PULSE_ALPHA_MAX: f32 = 0.8;

/// Wrap period for animation time to prevent f32 precision loss.
/// 100 full sine cycles at TAU ~ 628 seconds of continuous animation.
const ANIMATION_WRAP_PERIOD: f32 = std::f32::consts::TAU * 100.0;

/// Tracks AI state for all sessions and drives border / indicator colours.
pub struct AiStateTracker {
    states: HashMap<SessionId, AiProcessState>,
    /// Monotonically increasing time in seconds, used for pulse animation.
    animation_time: f32,
    /// Time each session entered its current state, for timeout expiry.
    state_enter_times: HashMap<SessionId, f32>,
    /// Per-state configuration (colours, enabled, timeouts).
    config: ClaudeStatesConfig,
}

impl AiStateTracker {
    /// Create a new tracker with no sessions.
    #[must_use]
    pub fn new(config: ClaudeStatesConfig) -> Self {
        Self {
            states: HashMap::new(),
            animation_time: 0.0,
            state_enter_times: HashMap::new(),
            config,
        }
    }

    /// Replace the per-state configuration snapshot (called on config reload).
    pub fn reconfigure(&mut self, config: ClaudeStatesConfig) {
        self.config = config;
    }

    /// Store the latest AI state for a session.
    ///
    /// States whose per-state `enabled` flag is `false` are silently ignored.
    pub fn update(&mut self, session_id: SessionId, ai_state: AiProcessState) {
        let entry = self.entry_for(&ai_state.state);
        if !entry.tab_indicator && !entry.pane_border {
            return;
        }
        self.state_enter_times.insert(session_id, self.animation_time);
        self.states.insert(session_id, ai_state);
    }

    /// Get the current AI state for a session.
    pub fn get(&self, session_id: SessionId) -> Option<&AiProcessState> {
        self.states.get(&session_id)
    }

    /// Clear attention states (`IdlePrompt` / `WaitingForInput` /
    /// `PermissionPrompt`) for a session, typically in response to user
    /// keystrokes. Other states (`Processing`, `Error`) are left untouched.
    pub fn clear_attention_states(&mut self, session_id: SessionId) {
        if let Some(state) = self.states.get(&session_id) {
            if matches!(
                state.state,
                AiState::IdlePrompt | AiState::WaitingForInput | AiState::PermissionPrompt
            ) {
                self.states.remove(&session_id);
                self.state_enter_times.remove(&session_id);
            }
        }
    }

    /// Advance the animation clock by `dt` seconds and expire timed-out states.
    ///
    /// The time is wrapped modulo a large period (100 full sine cycles at TAU)
    /// to prevent f32 precision degradation after long uptime.
    pub fn tick(&mut self, dt: f32) {
        self.animation_time = (self.animation_time + dt) % ANIMATION_WRAP_PERIOD;

        // Expire states whose configured timeout has elapsed.
        let now = self.animation_time;
        let config = &self.config;
        self.states.retain(|sid, ps| {
            let timeout = entry_for_config(config, &ps.state).timeout_secs;
            if timeout <= 0.0 {
                return true; // no timeout
            }
            let entered = self.state_enter_times.get(sid).copied().unwrap_or(now);
            let elapsed = (now - entered).max(0.0);
            elapsed < timeout
        });
        // Clean up orphaned enter-times.
        self.state_enter_times.retain(|sid, _| self.states.contains_key(sid));
    }

    /// Returns `true` if any session has an animated (pulsing or decaying)
    /// state that requires continuous redraw.
    pub fn needs_animation(&self) -> bool {
        self.states.values().any(|s| {
            if matches!(s.state, AiState::Error) {
                // Error decays over timeout_secs; animate while decay is active.
                self.config.error.timeout_secs > 0.0
            } else {
                requires_animation(&s.state)
            }
        })
    }

    /// Remove all tracked state for a session (e.g. on session exit).
    pub fn remove(&mut self, session_id: SessionId) {
        self.states.remove(&session_id);
        self.state_enter_times.remove(&session_id);
    }

    /// Compute the tab-bar indicator colour for a session.
    ///
    /// Returns the full-alpha base colour for the session's AI state, or
    /// `None` when the state is inactive or `tab_indicator` is disabled.
    pub fn tab_indicator_color(
        &self,
        session_id: SessionId,
        ansi_colors: &[[f32; 4]; 16],
    ) -> Option<[f32; 4]> {
        let state = self.states.get(&session_id)?;
        if !self.entry_for(&state.state).tab_indicator {
            return None;
        }
        Some(self.base_color_full_alpha(&state.state, ansi_colors))
    }

    /// Compute the highest-priority animated border colour across a set of
    /// sessions (for workspace-level aggregation).
    ///
    /// Priority: `PermissionPrompt > WaitingForInput > IdlePrompt > Error > Processing`.
    pub fn workspace_border_color(
        &self,
        session_ids: &[SessionId],
        ansi_colors: &[[f32; 4]; 16],
    ) -> Option<[f32; 4]> {
        let mut best: Option<(u8, [f32; 4])> = None;

        for &sid in session_ids {
            let Some(state) = self.states.get(&sid) else { continue };
            if !self.entry_for(&state.state).pane_border {
                continue;
            }
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

    /// Look up the config entry for a given AI state.
    fn entry_for(&self, state: &AiState) -> &AiStateEntry {
        entry_for_config(&self.config, state)
    }

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
        let entry = self.entry_for(&state.state);
        let base = entry.color.resolve(ansi_colors);
        let alpha = match &state.state {
            AiState::Processing
            | AiState::IdlePrompt
            | AiState::WaitingForInput
            | AiState::PermissionPrompt => {
                let hz = pulse_hz(entry.pulse_ms);
                pulse_alpha(self.animation_time, hz)
            }
            AiState::Error => {
                let timeout = self.config.error.timeout_secs;
                if timeout <= 0.0 {
                    return [base[0], base[1], base[2], PULSE_ALPHA_MAX];
                }
                self.state_enter_times.get(&session_id).map_or(0.0, |&t| {
                    let elapsed = (self.animation_time - t).max(0.0);
                    let remaining = (timeout - elapsed) / timeout;
                    (remaining * PULSE_ALPHA_MAX).clamp(0.0, PULSE_ALPHA_MAX)
                })
            }
        };
        [base[0], base[1], base[2], alpha]
    }

    /// Return the base colour for an AI state at full opacity (for tab indicators).
    #[allow(
        clippy::indexing_slicing,
        reason = "fixed-size [f32; 4] arrays, indices 0-2 always valid"
    )]
    fn base_color_full_alpha(&self, state: &AiState, ansi_colors: &[[f32; 4]; 16]) -> [f32; 4] {
        let c = self.entry_for(state).color.resolve(ansi_colors);
        [c[0], c[1], c[2], 1.0]
    }
}

impl Default for AiStateTracker {
    fn default() -> Self {
        Self::new(ClaudeStatesConfig::default())
    }
}

/// Look up the config entry for a given AI state.
fn entry_for_config<'a>(config: &'a ClaudeStatesConfig, state: &AiState) -> &'a AiStateEntry {
    match state {
        AiState::Processing => &config.processing,
        AiState::IdlePrompt => &config.idle_prompt,
        AiState::WaitingForInput => &config.waiting_for_input,
        AiState::PermissionPrompt => &config.permission_prompt,
        AiState::Error => &config.error,
    }
}

/// Return `true` if the given state requires continuous animation updates.
fn requires_animation(state: &AiState) -> bool {
    matches!(
        state,
        AiState::Processing
            | AiState::IdlePrompt
            | AiState::WaitingForInput
            | AiState::PermissionPrompt
    )
}

/// Numeric priority for workspace-level aggregation.
/// Higher value = more urgent.
fn state_priority(state: &AiState) -> u8 {
    match state {
        AiState::PermissionPrompt => 4,
        AiState::WaitingForInput => 3,
        AiState::IdlePrompt => 2,
        AiState::Error => 1,
        AiState::Processing => 0,
    }
}

/// Convert a `pulse_ms` config value to Hz. Returns [`DEFAULT_PULSE_HZ`]
/// when `pulse_ms` is 0 (no pulsing → static at max alpha).
fn pulse_hz(pulse_ms: u32) -> f32 {
    if pulse_ms == 0 {
        return DEFAULT_PULSE_HZ;
    }
    #[allow(clippy::cast_precision_loss, reason = "pulse_ms is a small integer")]
    let secs = pulse_ms as f32 / 1000.0;
    1.0 / secs
}

/// Compute a pulsing alpha value between [`PULSE_ALPHA_MIN`] and
/// [`PULSE_ALPHA_MAX`] using a sine wave at `hz` cycles per second.
fn pulse_alpha(t: f32, hz: f32) -> f32 {
    let mid = f32::midpoint(PULSE_ALPHA_MIN, PULSE_ALPHA_MAX);
    let amp = (PULSE_ALPHA_MAX - PULSE_ALPHA_MIN) / 2.0;
    mid + amp * (t * std::f32::consts::TAU * hz).sin()
}

// ---------------------------------------------------------------------------
// Border instance generation
// ---------------------------------------------------------------------------

/// Build four thin border quads around a pane's terminal content area
/// (excluding the tab bar).
///
/// Each of the four sides is rendered as one solid-colour [`CellInstance`]
/// (no glyph: `uv_min == uv_max == [0,0]`).  The `color` comes from
/// [`AiStateTracker::workspace_border_color`].
#[allow(
    clippy::many_single_char_names,
    reason = "x/y/w/h/bw are conventional 2-D geometry shorthands"
)]
pub fn build_border_instances(
    pane_rect: Rect,
    color: [f32; 4],
    tab_bar_height: f32,
) -> [CellInstance; 4] {
    let x = pane_rect.x;
    let y = pane_rect.y + tab_bar_height;
    let w = pane_rect.width;
    let h = pane_rect.height - tab_bar_height;
    let bw = BORDER_WIDTH;

    let top = solid_quad(x, y, w, bw, color);
    let bottom = solid_quad(x, y + h - bw, w, bw, color);
    let left = solid_quad(x, y + bw, bw, h - 2.0 * bw, color);
    let right = solid_quad(x + w - bw, y + bw, bw, h - 2.0 * bw, color);

    [top, bottom, left, right]
}

/// Create one solid-colour [`CellInstance`] quad with explicit pixel dimensions.
#[allow(
    clippy::many_single_char_names,
    reason = "x/y/w/h are conventional 2-D geometry shorthands"
)]
fn solid_quad(x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) -> CellInstance {
    CellInstance {
        pos: [x, y],
        size: [w, h],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
    }
}
