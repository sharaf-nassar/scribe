//! AI state tracking and pane-border indicator for the GPUI client.
//!
//! Ports the winit client's `AiStateTracker` state machine byte-for-byte: the
//! per-session pulse envelope, the Layer-2 stale-`Processing` clear, the
//! attention-state keystroke clear, the workspace-level priority ordering, and
//! the decoupled context-window store. The winit client emitted the pulsing
//! pane border as GPU quads through the legacy renderer; the GPUI
//! rebuild keeps the same geometry pure — [`pane_border_edges`] returns the
//! four edge [`Rect`]s and the GPUI paint path fills them with
//! [`AiStateTracker::workspace_border_color`], mirroring [`crate::focus_border`].
//!
//! The tab context-% suffix banding now lives in [`crate::tab_bar`]; this module
//! only owns the per-session context store that feeds it ([`AiStateTracker::context_for`])
//! and the pulse-suppression predicate ([`AiStateTracker::context_suffix_suppressed`]).
//!
//! Colours, per-state enable flags, and auto-clear timeouts are driven by
//! [`AiStateStylesConfig`] rather than compile-time constants.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
use scribe_common::config::{AiStateEntry, AiStateStylesConfig, TerminalConfig};
use scribe_common::ids::SessionId;

use crate::focus_border::border_edges;
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

// --- Pulse envelope policy (Layer 1 GPU-drain fix) -------------------------
//
// The pulse is an *attention* affordance, not a permanent state display.
// Decoupling its lifetime from AI-state lifetime is what lets the shared
// redraw loop retire when a session is stuck/idle — see `pulse_is_active`.

/// How long an attention state (`IdlePrompt` / `WaitingForInput` /
/// `PermissionPrompt`) keeps actively pulsing after it is entered before
/// it rests at a steady colour. The state stays tracked and visible; only
/// the animation (and the redraw loop it pins) stops.
const ATTENTION_PULSE_SECS: f32 = 12.0;

/// How long `Processing` keeps pulsing after the last sign of life — a
/// state edge or fresh PTY output — before it is treated as stale and
/// rests. A genuinely-working session keeps refreshing this; a hung or
/// dead AI on a still-open PTY goes silent and the pulse retires.
const PROCESSING_IDLE_PULSE_SECS: f32 = 8.0;

/// Layer 2 (correctness defence-in-depth): how long a `Processing` state
/// may go with zero liveness (no AI hook edge, no PTY output) before the
/// indicator is *cleared* entirely, not merely rested. A killed/crashed AI
/// can never fire its own terminal hook and the server only supervises the
/// shell, not the AI subprocess — so without this a dead AI shows a stale
/// "working" colour forever. Wall-clock, evaluated lazily — see
/// [`AiStateTracker::clear_stale_processing`].
const STALE_PROCESSING_CLEAR: Duration = Duration::from_mins(5);

/// Tracks AI state for all sessions and drives border / indicator colours.
pub struct AiStateTracker {
    states: HashMap<SessionId, AiProcessState>,
    /// Providers detected per session.
    ///
    /// Unlike `states`, this is not cleared by timeouts or keystrokes — only
    /// by an explicit `AiStateCleared` / session removal.
    detected_providers: HashMap<SessionId, AiProvider>,
    /// Most recent context-window % per session, decoupled from `states`.
    ///
    /// The pane border can rest or be pruned (stale-Processing clear,
    /// attention-state keystroke clear, Error decay) without dropping the
    /// percent — those events don't change how full the LLM's window is.
    /// Cleared on session removal, explicit conversation change, and
    /// explicit `AiStateCleared`. Mirrors the `detected_providers` lifetime.
    last_contexts: HashMap<SessionId, u8>,
    /// Monotonically increasing time in seconds, used for pulse animation.
    animation_time: f32,
    /// Time each session entered its current state, for timeout expiry.
    state_enter_times: HashMap<SessionId, f32>,
    /// Last time (in `animation_time` units) a session showed liveness:
    /// an `AiStateChanged` edge or fresh PTY output. Drives the
    /// `Processing` pulse envelope so a hung AI stops pinning the redraw
    /// loop while a genuinely-working one keeps animating across long,
    /// hook-silent tool calls. See [`Self::pulse_is_active`].
    last_activity_times: HashMap<SessionId, f32>,
    /// Wall-clock counterpart of `last_activity_times`, used solely by
    /// [`Self::clear_stale_processing`]. Kept separate from the f32
    /// animation clock because that clock freezes once the redraw loop
    /// retires (Layer 1) — exactly the stuck-`Processing` case Layer 2
    /// must still detect. Write-only outside that method, so the tracker
    /// stays deterministic for unit tests.
    last_activity_instant: HashMap<SessionId, Instant>,
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
            last_contexts: HashMap::new(),
            animation_time: 0.0,
            state_enter_times: HashMap::new(),
            last_activity_times: HashMap::new(),
            last_activity_instant: HashMap::new(),
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
        // Persist the percent independently of `states` so it survives
        // pulse pruning. `None` doesn't overwrite — a state edge without
        // a fresh Context=NN hook keeps the prior value visible.
        if let Some(ctx) = ai_state.context {
            self.last_contexts.insert(session_id, ctx);
        }
        let entry = self.entry_for(&ai_state.state);
        if !entry.tab_indicator && !entry.pane_border {
            return;
        }
        self.state_enter_times.insert(session_id, self.animation_time);
        // A state edge is a sign of life — re-arm the Processing envelope
        // (animation clock) and the Layer 2 staleness clock (wall clock).
        self.last_activity_times.insert(session_id, self.animation_time);
        self.last_activity_instant.insert(session_id, Instant::now());
        self.states.insert(session_id, ai_state);
    }

    /// Record that a session is alive *right now* because it produced fresh
    /// PTY output. This re-arms the `Processing` pulse envelope so a
    /// genuinely-working session keeps animating even through long tool
    /// calls that emit no AI hook edges. Cheap (one map insert); safe to
    /// call on every output chunk.
    pub fn note_activity(&mut self, session_id: SessionId) {
        if self.states.contains_key(&session_id) {
            self.last_activity_times.insert(session_id, self.animation_time);
            self.last_activity_instant.insert(session_id, Instant::now());
        }
    }

    /// Layer 2 defence-in-depth: clear any `Processing` state that has had
    /// zero liveness (no hook edge, no PTY output) for
    /// [`STALE_PROCESSING_CLEAR`] — a crashed/killed AI that can never send
    /// its own terminal hook. Only `Processing` is cleared: attention
    /// states legitimately persist until the human acts, and clearing a
    /// "waiting for you" indicator because the user stepped away would
    /// defeat its purpose. `detected_providers` is intentionally preserved
    /// so provider-aware clipboard cleanup survives, mirroring reconnect.
    /// Evaluated lazily by the client (cheap; no work when no session is
    /// stuck). Returns `true` if anything was cleared so the caller can
    /// repaint.
    pub fn clear_stale_processing(&mut self) -> bool {
        let stale: Vec<SessionId> = self
            .states
            .iter()
            .filter(|(sid, ps)| {
                matches!(ps.state, AiState::Processing)
                    && self
                        .last_activity_instant
                        .get(*sid)
                        .is_some_and(|seen| seen.elapsed() >= STALE_PROCESSING_CLEAR)
            })
            .map(|(sid, _)| *sid)
            .collect();
        for sid in &stale {
            self.states.remove(sid);
            self.state_enter_times.remove(sid);
            self.last_activity_times.remove(sid);
            self.last_activity_instant.remove(sid);
        }
        !stale.is_empty()
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
        if let Some(state) = self.states.get(&session_id)
            && matches!(
                state.state,
                AiState::IdlePrompt | AiState::WaitingForInput | AiState::PermissionPrompt
            )
        {
            self.states.remove(&session_id);
            self.state_enter_times.remove(&session_id);
            self.last_activity_times.remove(&session_id);
            self.last_activity_instant.remove(&session_id);
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
        // Clean up orphaned enter-times and activity-times.
        self.state_enter_times.retain(|sid, _| self.states.contains_key(sid));
        self.last_activity_times.retain(|sid, _| self.states.contains_key(sid));
        self.last_activity_instant.retain(|sid, _| self.states.contains_key(sid));
    }

    /// Returns `true` if any session has an animated (pulsing or decaying)
    /// state that requires continuous redraw.
    pub fn needs_animation(&self, terminal: &TerminalConfig) -> bool {
        self.states.iter().any(|(sid, s)| {
            if !terminal.ai_provider_enabled(s.provider) {
                return false;
            }
            if matches!(s.state, AiState::Error) {
                // Error decays over timeout_secs; animate while decay is active.
                self.config.error.timeout_secs > 0.0
            } else {
                // Only keep the redraw loop alive while the pulse is within
                // its envelope. Once stale it rests statically (see
                // `animated_color`) and contributes no animation, letting
                // the shared redraw loop retire.
                requires_animation(&s.state) && self.pulse_is_active(*sid, &s.state)
            }
        })
    }

    /// Policy predicate: should this session's state still be *actively
    /// pulsing* right now (vs. resting at a steady colour)?
    ///
    /// This is the heart of the GPU-drain fix. The pulse is an attention
    /// affordance with diminishing returns — it must not run forever just
    /// because the underlying AI state is long-lived. Returning `false`
    /// here both (a) stops the pulse rendering (`animated_color` falls back
    /// to a steady alpha) and (b) lets `needs_animation` report idle so the
    /// shared redraw loop retires and GPU use drops to zero.
    ///
    /// `state` is guaranteed to satisfy `requires_animation` (i.e. one of
    /// `Processing` / `IdlePrompt` / `WaitingForInput` / `PermissionPrompt`)
    /// — `Error` never reaches here.
    fn pulse_is_active(&self, session_id: SessionId, state: &AiState) -> bool {
        let now = self.animation_time;
        // `.max(0.0)` mirrors the wrap handling in `tick` / `animated_color`:
        // across the ~628 s `animation_time` wrap a stale delta clamps to 0,
        // erring toward "still pulsing" for one cycle — never toward a
        // wrongly-frozen indicator.
        match state {
            // Attention states block on the human; the pulse is a bounded
            // attention grab measured from when the state was entered. After
            // it, rest (still tracked + visible); a keystroke still clears
            // instantly via `clear_attention_states`.
            AiState::IdlePrompt | AiState::WaitingForInput | AiState::PermissionPrompt => {
                let entered = self.state_enter_times.get(&session_id).copied().unwrap_or(now);
                (now - entered).max(0.0) < ATTENTION_PULSE_SECS
            }
            // Processing pulses only while alive. `last_activity_times` is
            // refreshed by AI state edges and PTY output, so a working
            // session keeps re-arming across hook-silent tool calls while a
            // hung AI on a still-open PTY falls silent and rests.
            AiState::Processing => {
                let last = self.last_activity_times.get(&session_id).copied().unwrap_or(now);
                (now - last).max(0.0) < PROCESSING_IDLE_PULSE_SECS
            }
            // `Error` is gated by its own decay before this point and never
            // reaches here; keep prior behaviour if it ever does.
            AiState::Error => true,
        }
    }

    /// Remove all tracked state for a session (e.g. on session exit).
    pub fn remove(&mut self, session_id: SessionId) {
        self.states.remove(&session_id);
        self.state_enter_times.remove(&session_id);
        self.last_activity_times.remove(&session_id);
        self.last_activity_instant.remove(&session_id);
        self.detected_providers.remove(&session_id);
        self.last_contexts.remove(&session_id);
    }

    /// Drop the stored context % for a session without touching its state.
    ///
    /// Called on conversation change: a new conversation starts a fresh
    /// window, so the prior conversation's percent would be misleading.
    /// Border lifetime is unaffected.
    pub fn clear_context(&mut self, session_id: SessionId) {
        self.last_contexts.remove(&session_id);
    }

    /// Whether Claude Code has been detected in this session.
    ///
    /// Unlike the visible state, this returns `true` even after the visual
    /// indicator has timed out or been cleared by a keystroke. It is only
    /// reset when the session is removed or explicitly cleared.
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
    ///
    /// Reads from `last_contexts`, so the percent survives any path that
    /// prunes `states` (stale-Processing clear, attention-state keystroke
    /// clear, Error decay). Feeds [`crate::tab_bar::context_suffix`].
    #[must_use]
    pub fn context_for(&self, session: SessionId) -> Option<u8> {
        self.last_contexts.get(&session).copied()
    }

    /// Whether the tab context-% suffix must be suppressed for `session`
    /// because a pulsing attention state (`PermissionPrompt` /
    /// `WaitingForInput`) owns the UX. The suffix reappears on the same
    /// percent once that state clears or rests. This is the `pulsing`
    /// argument for [`crate::tab_bar::context_suffix`].
    #[must_use]
    pub fn context_suffix_suppressed(&self, session: SessionId) -> bool {
        self.states.get(&session).is_some_and(|ps| {
            matches!(ps.state, AiState::PermissionPrompt | AiState::WaitingForInput)
        })
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
                if self.pulse_is_active(session_id, &state.state) {
                    let hz = pulse_hz(entry.pulse_ms);
                    pulse_alpha(self.animation_time, hz)
                } else {
                    // Envelope elapsed: rest at a steady, fully-visible colour
                    // instead of freezing at a random mid-pulse alpha. The
                    // indicator stays informative at zero GPU.
                    PULSE_ALPHA_MAX
                }
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
    fn base_color_full_alpha(&self, state: &AiState, ansi_colors: &[[f32; 4]; 16]) -> [f32; 4] {
        let c = self.entry_for(state).color.resolve(ansi_colors);
        [c[0], c[1], c[2], 1.0]
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
    let secs = Duration::from_millis(u64::from(pulse_ms)).as_secs_f32();
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
// Border geometry
// ---------------------------------------------------------------------------

/// Compute the four edge rects of the AI pane-border overlay (top, bottom,
/// left, right) around a pane's terminal content area, excluding the tab bar.
///
/// The winit client emitted these as four solid renderer quads; the GPUI paint
/// path fills the returned rects with the colour from
/// [`AiStateTracker::workspace_border_color`], reusing the shared
/// [`crate::focus_border::border_edges`] corner-safe strip math.
#[must_use]
pub fn pane_border_edges(pane_rect: Rect, tab_bar_height: f32) -> [Rect; 4] {
    let content = Rect {
        x: pane_rect.x,
        y: pane_rect.y + tab_bar_height,
        width: pane_rect.width,
        height: (pane_rect.height - tab_bar_height).max(0.0),
    };
    border_edges(content, BORDER_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::{AiStateTracker, STALE_PROCESSING_CLEAR, pane_border_edges};
    use crate::layout::Rect;
    use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
    use scribe_common::config::TerminalConfig;
    use scribe_common::ids::SessionId;
    use std::time::{Duration, Instant};

    const ANSI_COLORS: [[f32; 4]; 16] = [[0.25, 0.5, 0.75, 1.0]; 16];

    fn make_state_with_ctx(state: AiState, ctx: u8) -> AiProcessState {
        AiProcessState { context: Some(ctx), ..AiProcessState::new(state) }
    }

    // --- Provider gating ---------------------------------------------------

    // @lat: [[client#GPUI AI Indicator#Provider toggle gates the indicator]]
    #[gpui::test]
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

    // @lat: [[client#GPUI AI Indicator#Provider memory survives clears]]
    #[gpui::test]
    fn codex_sessions_do_not_enable_claude_cleanup() {
        let mut tracker = AiStateTracker::default();
        let session_id = SessionId::new();
        tracker.update(
            session_id,
            AiProcessState::new_with_provider(AiProvider::CodexCode, AiState::Processing),
        );
        assert!(!tracker.has_claude_session(session_id));
    }

    // --- Pulse envelope (Layer 1) + stale clear (Layer 2) ------------------

    // @lat: [[client#GPUI AI Indicator#Processing pulse rests after idle window]]
    #[gpui::test]
    fn processing_pulse_rests_after_idle_window() {
        let mut tracker = AiStateTracker::default();
        let terminal = TerminalConfig::default();
        let sid = SessionId::new();
        tracker.update(sid, AiProcessState::new(AiState::Processing));
        assert!(tracker.needs_animation(&terminal), "fresh Processing must pulse");
        tracker.tick(super::PROCESSING_IDLE_PULSE_SECS + 1.0);
        assert!(
            !tracker.needs_animation(&terminal),
            "stuck Processing must stop pinning the redraw loop (the GPU bug)"
        );
    }

    // @lat: [[client#GPUI AI Indicator#Activity re-arms the processing pulse]]
    #[gpui::test]
    fn processing_activity_rearms_pulse() {
        let mut tracker = AiStateTracker::default();
        let terminal = TerminalConfig::default();
        let sid = SessionId::new();
        tracker.update(sid, AiProcessState::new(AiState::Processing));
        tracker.tick(super::PROCESSING_IDLE_PULSE_SECS + 1.0);
        assert!(!tracker.needs_animation(&terminal), "rested before re-arm");
        tracker.note_activity(sid);
        assert!(
            tracker.needs_animation(&terminal),
            "PTY-output activity must re-arm a rested Processing pulse"
        );
        tracker.tick(super::PROCESSING_IDLE_PULSE_SECS + 1.0);
        assert!(!tracker.needs_animation(&terminal), "must rest again after renewed silence");
    }

    // @lat: [[client#GPUI AI Indicator#A state edge re-arms the pulse]]
    #[gpui::test]
    fn state_edge_rearms_pulse() {
        let mut tracker = AiStateTracker::default();
        let terminal = TerminalConfig::default();
        let sid = SessionId::new();
        tracker.update(sid, AiProcessState::new(AiState::Processing));
        tracker.tick(super::PROCESSING_IDLE_PULSE_SECS + 1.0);
        assert!(!tracker.needs_animation(&terminal), "rested before re-arm");
        tracker.update(sid, AiProcessState::new(AiState::Processing));
        assert!(
            tracker.needs_animation(&terminal),
            "a Processing state edge must re-arm the pulse"
        );
    }

    // @lat: [[client#GPUI AI Indicator#Attention pulse rests after its window]]
    #[gpui::test]
    fn attention_pulse_rests_after_window() {
        let mut tracker = AiStateTracker::default();
        let terminal = TerminalConfig::default();
        let sid = SessionId::new();
        tracker.update(sid, AiProcessState::new(AiState::WaitingForInput));
        assert!(tracker.needs_animation(&terminal), "fresh attention state must pulse");
        tracker.tick(super::ATTENTION_PULSE_SECS + 1.0);
        assert!(
            !tracker.needs_animation(&terminal),
            "attention pulse must rest after its bounded window"
        );
    }

    // @lat: [[client#GPUI AI Indicator#Stale processing is cleared]]
    #[gpui::test]
    fn stale_processing_is_cleared() {
        let mut tracker = AiStateTracker::default();
        let terminal = TerminalConfig::default();
        let sid = SessionId::new();
        tracker.update(sid, AiProcessState::new(AiState::Processing));
        tracker.last_activity_instant.insert(
            sid,
            Instant::now().checked_sub(STALE_PROCESSING_CLEAR + Duration::from_secs(1)).unwrap(),
        );
        assert!(tracker.clear_stale_processing(), "must report a clear");
        assert!(
            !tracker.needs_animation(&terminal),
            "a dead Processing state must be removed, not shown forever"
        );
        assert_eq!(
            tracker.provider_for_session(sid),
            Some(AiProvider::ClaudeCode),
            "provider memory must survive the clear (clipboard cleanup)"
        );
    }

    // @lat: [[client#GPUI AI Indicator#Fresh processing is not cleared]]
    #[gpui::test]
    fn fresh_processing_not_cleared() {
        let mut tracker = AiStateTracker::default();
        let terminal = TerminalConfig::default();
        let sid = SessionId::new();
        tracker.update(sid, AiProcessState::new(AiState::Processing));
        assert!(!tracker.clear_stale_processing(), "a just-updated Processing state is not stale");
        assert!(tracker.needs_animation(&terminal), "fresh Processing must still be tracked");
    }

    // @lat: [[client#GPUI AI Indicator#Only processing is hard-cleared]]
    #[gpui::test]
    fn stale_attention_state_not_cleared() {
        let mut tracker = AiStateTracker::default();
        let terminal = TerminalConfig::default();
        let sid = SessionId::new();
        tracker.update(sid, AiProcessState::new(AiState::WaitingForInput));
        tracker.last_activity_instant.insert(
            sid,
            Instant::now().checked_sub(STALE_PROCESSING_CLEAR + Duration::from_secs(1)).unwrap(),
        );
        assert!(!tracker.clear_stale_processing(), "only Processing is hard-cleared");
        assert!(
            tracker.needs_animation(&terminal),
            "an attention state must persist until the human acts, even if idle"
        );
    }

    // @lat: [[client#GPUI AI Indicator#Activity re-arms the stale-clear timer]]
    #[gpui::test]
    fn activity_rearms_stale_processing() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, AiProcessState::new(AiState::Processing));
        tracker.last_activity_instant.insert(
            sid,
            Instant::now().checked_sub(STALE_PROCESSING_CLEAR + Duration::from_secs(1)).unwrap(),
        );
        tracker.note_activity(sid);
        assert!(
            !tracker.clear_stale_processing(),
            "activity must reset the wall-clock staleness timer"
        );
    }

    // --- Priority ordering -------------------------------------------------

    // @lat: [[client#GPUI AI Indicator#Workspace border takes the highest-priority state]]
    #[gpui::test]
    fn workspace_border_takes_highest_priority_state() {
        let mut tracker = AiStateTracker::default();
        let terminal = TerminalConfig::default();
        let processing = SessionId::new();
        let permission = SessionId::new();
        let waiting = SessionId::new();
        tracker.update(processing, AiProcessState::new(AiState::Processing));
        tracker.update(waiting, AiProcessState::new(AiState::WaitingForInput));
        tracker.update(permission, AiProcessState::new(AiState::PermissionPrompt));

        // PermissionPrompt (priority 4) wins over WaitingForInput (3) and
        // Processing (0), so the aggregated border colour is the permission
        // entry's colour.
        let sessions = [processing, waiting, permission];
        let border = tracker
            .workspace_border_color(&sessions, &ANSI_COLORS, &terminal)
            .expect("some session drives the border");
        let permission_only = tracker
            .workspace_border_color(&[permission], &ANSI_COLORS, &terminal)
            .expect("permission drives its own border");
        assert_eq!(
            border[0..3],
            permission_only[0..3],
            "the highest-priority state must own the workspace border colour"
        );
    }

    // @lat: [[client#GPUI AI Indicator#Border colour drops decayed sessions]]
    #[gpui::test]
    fn workspace_border_none_without_active_sessions() {
        let tracker = AiStateTracker::default();
        let terminal = TerminalConfig::default();
        let sid = SessionId::new(); // never inserted
        assert_eq!(tracker.workspace_border_color(&[sid], &ANSI_COLORS, &terminal), None);
    }

    // --- Context store survives clears -------------------------------------

    // @lat: [[client#GPUI AI Indicator#Context survives the stale-processing clear]]
    #[gpui::test]
    fn context_survives_stale_processing_clear() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, make_state_with_ctx(AiState::Processing, 85));
        tracker.last_activity_instant.insert(
            sid,
            Instant::now().checked_sub(STALE_PROCESSING_CLEAR + Duration::from_secs(1)).unwrap(),
        );
        assert!(tracker.clear_stale_processing(), "stale Processing must clear");
        assert_eq!(
            tracker.context_for(sid),
            Some(85),
            "context must survive stale-state clear so percent stays visible"
        );
        assert!(
            !tracker.context_suffix_suppressed(sid),
            "with no state left, the suffix is no longer suppressed"
        );
    }

    // @lat: [[client#GPUI AI Indicator#Context suffix suppressed during attention pulse]]
    #[gpui::test]
    fn context_suffix_suppressed_during_attention_states() {
        let mut tracker = AiStateTracker::default();
        let permission = SessionId::new();
        let waiting = SessionId::new();
        let processing = SessionId::new();
        tracker.update(permission, make_state_with_ctx(AiState::PermissionPrompt, 85));
        tracker.update(waiting, make_state_with_ctx(AiState::WaitingForInput, 85));
        tracker.update(processing, make_state_with_ctx(AiState::Processing, 85));
        assert!(tracker.context_suffix_suppressed(permission));
        assert!(tracker.context_suffix_suppressed(waiting));
        assert!(
            !tracker.context_suffix_suppressed(processing),
            "Processing does not suppress the tab context suffix"
        );
    }

    // @lat: [[client#GPUI AI Indicator#Conversation change wipes the context]]
    #[gpui::test]
    fn clear_context_wipes_for_conversation_change() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, make_state_with_ctx(AiState::Processing, 85));
        tracker.clear_context(sid);
        assert_eq!(tracker.context_for(sid), None);
    }

    // @lat: [[client#GPUI AI Indicator#Session removal drops the context]]
    #[gpui::test]
    fn context_remove_clears_last_context() {
        let mut tracker = AiStateTracker::default();
        let sid = SessionId::new();
        tracker.update(sid, make_state_with_ctx(AiState::Processing, 85));
        tracker.remove(sid);
        assert_eq!(tracker.context_for(sid), None);
    }

    // --- Border geometry ---------------------------------------------------

    // @lat: [[client#GPUI AI Indicator#Pane border edges exclude the tab bar]]
    #[gpui::test]
    fn pane_border_edges_exclude_tab_bar() {
        let pane = Rect { x: 10.0, y: 20.0, width: 200.0, height: 150.0 };
        let tab_bar_height = 24.0;
        let [top, bottom, left, right] = pane_border_edges(pane, tab_bar_height);
        // Top strip starts below the tab bar.
        assert!((top.y - (20.0 + 24.0)).abs() < f32::EPSILON);
        assert!((top.width - 200.0).abs() < f32::EPSILON);
        // Bottom strip sits at the pane's lower edge.
        assert!((bottom.y - (20.0 + 150.0 - 2.0)).abs() < f32::EPSILON);
        // Side strips are 2px wide and inset between top and bottom.
        assert!((left.width - 2.0).abs() < f32::EPSILON);
        assert!((right.x - (10.0 + 200.0 - 2.0)).abs() < f32::EPSILON);
    }
}
