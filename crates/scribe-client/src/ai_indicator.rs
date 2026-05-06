//! AI state tracking and visual indicator generation.
//!
//! Maintains per-session [`AiProcessState`] and produces [`CellInstance`]
//! quads for pane border overlays and tab-bar indicator bars.
//!
//! Colours, per-state enable flags, and auto-clear timeouts are driven by
//! [`AiStateStylesConfig`] rather than compile-time constants.

use std::collections::HashMap;

use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
use scribe_common::config::{
    AiContextThresholds, AiStateEntry, AiStateStylesConfig, TerminalConfig,
};
use scribe_common::ids::SessionId;
use scribe_common::theme::hex_to_rgba;
use scribe_renderer::chrome::solid_quad;
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
    /// Providers detected per session.
    ///
    /// Unlike `states`, this is not cleared by timeouts or keystrokes — only
    /// by an explicit `AiStateCleared` / session removal.
    detected_providers: HashMap<SessionId, AiProvider>,
    /// Monotonically increasing time in seconds, used for pulse animation.
    animation_time: f32,
    /// Time each session entered its current state, for timeout expiry.
    state_enter_times: HashMap<SessionId, f32>,
    /// Per-state configuration (colours, enabled, timeouts).
    config: AiStateStylesConfig,
}

impl AiStateTracker {
    /// Create a new tracker with no sessions.
    #[must_use]
    pub fn new(config: AiStateStylesConfig) -> Self {
        Self {
            states: HashMap::new(),
            detected_providers: HashMap::new(),
            animation_time: 0.0,
            state_enter_times: HashMap::new(),
            config,
        }
    }

    /// Replace the per-state configuration snapshot (called on config reload).
    pub fn reconfigure(&mut self, config: AiStateStylesConfig) {
        self.config = config;
    }

    /// Store the latest AI state for a session.
    ///
    /// States whose per-state `enabled` flag is `false` are silently ignored.
    pub fn update(&mut self, session_id: SessionId, ai_state: AiProcessState) {
        self.detected_providers.insert(session_id, ai_state.provider);
        let entry = self.entry_for(&ai_state.state);
        if !entry.tab_indicator && !entry.pane_border {
            return;
        }
        self.state_enter_times.insert(session_id, self.animation_time);
        self.states.insert(session_id, ai_state);
    }

    /// Remember the last provider seen for a session without restoring a
    /// visible state.
    pub fn remember_provider(&mut self, session_id: SessionId, provider: AiProvider) {
        self.detected_providers.insert(session_id, provider);
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
    pub fn needs_animation(&self, terminal: &TerminalConfig) -> bool {
        self.states.values().any(|s| {
            if !terminal.ai_provider_enabled(s.provider) {
                return false;
            }
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
        self.detected_providers.remove(&session_id);
    }

    /// Whether Claude Code has been detected in this session.
    ///
    /// Unlike [`get`], this returns `true` even after the visual indicator
    /// has timed out or been cleared by a keystroke.  It is only reset when
    /// the session explicitly sends `ClaudeState=inactive` or is removed.
    #[cfg(test)]
    pub fn has_claude_session(&self, session_id: SessionId) -> bool {
        self.detected_providers.get(&session_id) == Some(&AiProvider::ClaudeCode)
    }

    /// Provider last seen for a session, if any.
    pub fn provider_for_session(&self, session_id: SessionId) -> Option<AiProvider> {
        self.detected_providers.get(&session_id).copied()
    }

    /// Return the latest context-window usage percentage for a session, or
    /// `None` when no context value has been received.
    #[must_use]
    pub fn context_for(&self, session: SessionId) -> Option<u8> {
        self.states.get(&session)?.context
    }

    /// Return a colored context-% suffix to append to a tab label, or `None`
    /// when no suffix should be drawn.
    ///
    /// Returns `Some((" NN%", color))` only when:
    /// - the session has a context value at or above `thresholds.warn`, AND
    /// - the session's AI state is NOT `PermissionPrompt` or `WaitingForInput`
    ///   (those use the existing pulse indicators and must not compete).
    ///
    /// Color is derived from `thresholds.color_for(ctx)` via `hex_to_rgba` →
    /// `srgb_to_linear_rgba`. Falls back to `fallback_color` on parse failure,
    /// matching the symmetry of `build_context_segment` in the status bar.
    #[must_use]
    pub fn tab_context_suffix(
        &self,
        session: SessionId,
        thresholds: &AiContextThresholds,
        fallback_color: [f32; 4],
    ) -> Option<(String, [f32; 4])> {
        let ps = self.states.get(&session)?;
        // Suppress suffix when pulsing/attention states are active.
        if matches!(ps.state, AiState::PermissionPrompt | AiState::WaitingForInput) {
            return None;
        }
        let ctx = ps.context?;
        if ctx < thresholds.warn {
            return None;
        }
        let hex = thresholds.color_for(ctx);
        let color =
            hex_to_rgba(hex).map(scribe_renderer::srgb_to_linear_rgba).unwrap_or(fallback_color);
        Some((format!(" {ctx}%"), color))
    }

    /// Compute the tab-bar indicator colour for a session.
    ///
    /// Returns the full-alpha base colour for the session's AI state, or
    /// `None` when the state is inactive or `tab_indicator` is disabled.
    pub fn tab_indicator_color(
        &self,
        session_id: SessionId,
        ansi_colors: &[[f32; 4]; 16],
        terminal: &TerminalConfig,
    ) -> Option<[f32; 4]> {
        let state = self.states.get(&session_id)?;
        if !terminal.ai_provider_enabled(state.provider) {
            return None;
        }
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
        terminal: &TerminalConfig,
    ) -> Option<[f32; 4]> {
        let mut best: Option<(u8, [f32; 4])> = None;

        for &sid in session_ids {
            let Some(state) = self.states.get(&sid) else { continue };
            if !terminal.ai_provider_enabled(state.provider) {
                continue;
            }
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
                    return [
                        base.first().copied().unwrap_or(0.0),
                        base.get(1).copied().unwrap_or(0.0),
                        base.get(2).copied().unwrap_or(0.0),
                        PULSE_ALPHA_MAX,
                    ];
                }
                self.state_enter_times.get(&session_id).map_or(0.0, |&t| {
                    let elapsed = (self.animation_time - t).max(0.0);
                    let remaining = (timeout - elapsed) / timeout;
                    (remaining * PULSE_ALPHA_MAX).clamp(0.0, PULSE_ALPHA_MAX)
                })
            }
        };
        [
            base.first().copied().unwrap_or(0.0),
            base.get(1).copied().unwrap_or(0.0),
            base.get(2).copied().unwrap_or(0.0),
            alpha,
        ]
    }

    /// Return the base colour for an AI state at full opacity (for tab indicators).
    fn base_color_full_alpha(&self, state: &AiState, ansi_colors: &[[f32; 4]; 16]) -> [f32; 4] {
        let c = self.entry_for(state).color.resolve(ansi_colors);
        [
            c.first().copied().unwrap_or(0.0),
            c.get(1).copied().unwrap_or(0.0),
            c.get(2).copied().unwrap_or(0.0),
            1.0,
        ]
    }
}

impl Default for AiStateTracker {
    fn default() -> Self {
        Self::new(AiStateStylesConfig::default())
    }
}

/// Look up the config entry for a given AI state.
fn entry_for_config<'a>(config: &'a AiStateStylesConfig, state: &AiState) -> &'a AiStateEntry {
    match state {
        AiState::Processing => &config.processing,
        AiState::IdlePrompt | AiState::WaitingForInput => &config.waiting_for_input,
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
    let secs = std::time::Duration::from_millis(u64::from(pulse_ms)).as_secs_f32();
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
pub fn build_border_instances(
    pane_rect: Rect,
    color: [f32; 4],
    tab_bar_height: f32,
) -> [CellInstance; 4] {
    let x_pos = pane_rect.x;
    let y_pos = pane_rect.y + tab_bar_height;
    let width = pane_rect.width;
    let height = pane_rect.height - tab_bar_height;
    let border_width = BORDER_WIDTH;

    let top = solid_quad(x_pos, y_pos, width, border_width, color);
    let bottom = solid_quad(x_pos, y_pos + height - border_width, width, border_width, color);
    let left =
        solid_quad(x_pos, y_pos + border_width, border_width, height - 2.0 * border_width, color);
    let right = solid_quad(
        x_pos + width - border_width,
        y_pos + border_width,
        border_width,
        height - 2.0 * border_width,
        color,
    );

    [top, bottom, left, right]
}

#[cfg(test)]
mod tests {
    use super::AiStateTracker;
    use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
    use scribe_common::config::{AiContextThresholds, TerminalConfig};
    use scribe_common::ids::SessionId;
    use scribe_common::theme::hex_to_rgba;

    const TEST_FALLBACK_COLOR: [f32; 4] = [0.5, 0.5, 0.5, 1.0];

    const ANSI_COLORS: [[f32; 4]; 16] = [[0.25, 0.5, 0.75, 1.0]; 16];

    /// Compare two `[f32; 4]` arrays by bit pattern (deterministic float equality).
    fn colors_eq(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| x.to_bits() == y.to_bits())
    }

    fn make_state_with_ctx(state: AiState, ctx: u8) -> AiProcessState {
        AiProcessState { context: Some(ctx), ..AiProcessState::new(state) }
    }

    // @lat: [[client#Tab Bar#tab_context_suffix_below_warn_returns_none]]
    #[test]
    fn tab_context_suffix_below_warn_returns_none() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, make_state_with_ctx(AiState::Processing, 50));
        let thresholds = AiContextThresholds::default();
        assert!(tracker.tab_context_suffix(sid, &thresholds, TEST_FALLBACK_COLOR).is_none());
    }

    // @lat: [[client#Tab Bar#tab_context_suffix_at_warn_returns_warn_color]]
    #[test]
    fn tab_context_suffix_at_warn_returns_warn_color() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, make_state_with_ctx(AiState::Processing, 70));
        let thresholds = AiContextThresholds::default();
        let result = tracker.tab_context_suffix(sid, &thresholds, TEST_FALLBACK_COLOR);
        assert!(result.is_some(), "expected Some for ctx=70 (warn threshold)");
        let (text, color) = result.unwrap();
        assert_eq!(text, " 70%");
        let expected = scribe_renderer::srgb_to_linear_rgba(hex_to_rgba("#d4a017").unwrap());
        assert!(colors_eq(color, expected), "expected warn color");
    }

    // @lat: [[client#Tab Bar#tab_context_suffix_at_danger_returns_danger_color]]
    #[test]
    fn tab_context_suffix_at_danger_returns_danger_color() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, make_state_with_ctx(AiState::Processing, 92));
        let thresholds = AiContextThresholds::default();
        let result = tracker.tab_context_suffix(sid, &thresholds, TEST_FALLBACK_COLOR);
        assert!(result.is_some(), "expected Some for ctx=92 (danger threshold)");
        let (text, color) = result.unwrap();
        assert_eq!(text, " 92%");
        let expected = scribe_renderer::srgb_to_linear_rgba(hex_to_rgba("#c83030").unwrap());
        assert!(colors_eq(color, expected), "expected danger color");
    }

    // @lat: [[client#Tab Bar#tab_context_suffix_suppressed_when_permission_prompt]]
    #[test]
    fn tab_context_suffix_suppressed_when_permission_prompt() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, make_state_with_ctx(AiState::PermissionPrompt, 85));
        let thresholds = AiContextThresholds::default();
        assert!(tracker.tab_context_suffix(sid, &thresholds, TEST_FALLBACK_COLOR).is_none());
    }

    // @lat: [[client#Tab Bar#tab_context_suffix_suppressed_when_waiting_for_input]]
    #[test]
    fn tab_context_suffix_suppressed_when_waiting_for_input() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, make_state_with_ctx(AiState::WaitingForInput, 85));
        let thresholds = AiContextThresholds::default();
        assert!(tracker.tab_context_suffix(sid, &thresholds, TEST_FALLBACK_COLOR).is_none());
    }

    // @lat: [[client#Tab Bar#tab_context_suffix_present_when_processing]]
    #[test]
    fn tab_context_suffix_present_when_processing() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, make_state_with_ctx(AiState::Processing, 85));
        let thresholds = AiContextThresholds::default();
        let result = tracker.tab_context_suffix(sid, &thresholds, TEST_FALLBACK_COLOR);
        assert!(result.is_some(), "expected Some for Processing + ctx=85");
        let (text, color) = result.unwrap();
        assert_eq!(text, " 85%");
        // 85 is in warn band (>= 70, < 90)
        let expected = scribe_renderer::srgb_to_linear_rgba(hex_to_rgba("#d4a017").unwrap());
        assert!(colors_eq(color, expected), "expected warn color for ctx=85");
    }

    // @lat: [[client#Tab Bar#tab_context_suffix_none_when_no_session]]
    #[test]
    fn tab_context_suffix_none_when_no_session() {
        let tracker = AiStateTracker::default();
        let sid = SessionId::new(); // never inserted
        let thresholds = AiContextThresholds::default();
        assert!(tracker.tab_context_suffix(sid, &thresholds, TEST_FALLBACK_COLOR).is_none());
    }

    // @lat: [[client#Tab Bar#tab_context_suffix_none_when_no_context_value]]
    #[test]
    fn tab_context_suffix_none_when_no_context_value() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, AiProcessState::new(AiState::Processing)); // context = None
        let thresholds = AiContextThresholds::default();
        assert!(tracker.tab_context_suffix(sid, &thresholds, TEST_FALLBACK_COLOR).is_none());
    }

    // @lat: [[client#Tab Bar#tab_context_suffix_falls_back_on_invalid_hex]]
    #[test]
    fn tab_context_suffix_falls_back_on_invalid_hex() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, make_state_with_ctx(AiState::Processing, 75));
        let thresholds = AiContextThresholds {
            warn_color: "not-a-color".into(),
            ..AiContextThresholds::default()
        };
        let result = tracker.tab_context_suffix(sid, &thresholds, TEST_FALLBACK_COLOR);
        assert!(result.is_some(), "expected Some even when hex parse fails");
        let (_, color) = result.unwrap();
        assert!(
            colors_eq(color, TEST_FALLBACK_COLOR),
            "expected fallback color when hex parse fails"
        );
    }

    #[test]
    fn codex_indicator_respects_provider_toggle() {
        let mut tracker = AiStateTracker::default();
        let session_id = SessionId::new();
        let terminal = TerminalConfig {
            ai_integration: scribe_common::config::TerminalAiIntegrationConfig {
                codex_code: scribe_common::config::AiIntegrationToggle::new(false),
                ..scribe_common::config::TerminalAiIntegrationConfig::default()
            },
            ..TerminalConfig::default()
        };

        tracker.update(
            session_id,
            AiProcessState::new_with_provider(AiProvider::CodexCode, AiState::Processing),
        );

        assert_eq!(tracker.tab_indicator_color(session_id, &ANSI_COLORS, &terminal), None);
    }

    #[test]
    fn codex_sessions_do_not_enable_claude_cleanup() {
        let mut tracker = AiStateTracker::default();
        let session_id = SessionId::new();

        tracker.update(
            session_id,
            AiProcessState::new_with_provider(AiProvider::CodexCode, AiState::Processing),
        );

        assert!(!tracker.has_claude_session(session_id));
    }
}
