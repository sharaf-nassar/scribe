//! Motion policy for the GPUI client: capped, interruptible UI transitions
//! plus the sanctioned `animations` off switch.
//!
//! US3 polish wants smooth tab/focus/overlay transitions and pixel-smooth
//! scrolling, but latency purists and deterministic visual-E2E screenshots
//! need a hard off switch. This module centralises that policy so every future
//! shell surface (tab bar, focus ring, overlays, scroll views) resolves motion
//! from one place.
//!
//! Two inputs decide whether motion runs, resolved by [`AnimationSettings`]:
//! the `appearance.animations` config bool (default `true`, doubling as the
//! reduce-motion user setting) and the [`DISABLE_ANIMATIONS_ENV`] environment
//! override that force-disables motion for E2E determinism. The env override
//! always wins.
//!
//! When motion is enabled, [`AnimationSettings::transition`] builds a
//! `gpui::Animation` capped at [`MAX_TRANSITION`] (150 ms) with an ease-out
//! curve; GPUI's `AnimationElement` re-reads the animation on interruption, so
//! a new transition started mid-flight simply retargets. When motion is
//! disabled, [`AnimationSettings::apply_to_app`] flips GPUI's global
//! [`App::set_reduce_motion`](gpui::App::set_reduce_motion) so every
//! `with_animation` in the tree renders its static end state and schedules no
//! frames — the property the byte-identical-screenshot acceptance relies on.

use std::ffi::OsStr;
use std::time::Duration;

use gpui::{Animation, App, ease_out_quint};
use scribe_common::config::ScribeConfig;

/// Environment variable that force-disables all client animations regardless of
/// the `animations` config key. Set to a truthy value (`1`, `true`, `yes`, `on`)
/// for deterministic visual-E2E runs; unset or falsy leaves the config in
/// charge.
pub const DISABLE_ANIMATIONS_ENV: &str = "SCRIBE_DISABLE_ANIMATIONS";

/// Hard ceiling on any single UI transition (tab, focus, overlay). No animation
/// may exceed this per the plan's ≤150 ms interruptible-easing budget; the
/// duration builders clamp to it.
pub const MAX_TRANSITION: Duration = Duration::from_millis(150);

/// Resolved motion policy for the running client.
///
/// Constructed from the `appearance.animations` config bool and the
/// [`DISABLE_ANIMATIONS_ENV`] override. Cheap to copy; recompute it whenever the
/// config reloads so a saved edit to `animations` takes effect live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnimationSettings {
    enabled: bool,
}

impl AnimationSettings {
    /// Resolve the policy from the config bool and the process environment.
    ///
    /// The environment override wins: a truthy [`DISABLE_ANIMATIONS_ENV`]
    /// force-disables motion even when `config_enabled` is `true`.
    #[must_use]
    pub fn resolve(config_enabled: bool) -> Self {
        Self::resolve_with_env(config_enabled, std::env::var_os(DISABLE_ANIMATIONS_ENV).as_deref())
    }

    /// Resolve the policy straight from a parsed [`ScribeConfig`].
    #[must_use]
    pub fn from_config(config: &ScribeConfig) -> Self {
        Self::resolve(config.appearance.animations)
    }

    /// Resolve against an explicit override value (the raw env string, or
    /// `None` when unset). Splitting this out keeps the precedence rule unit
    /// testable without mutating shared process env in parallel test threads.
    #[must_use]
    pub fn resolve_with_env(config_enabled: bool, raw_override: Option<&OsStr>) -> Self {
        let forced_off = raw_override.is_some_and(env_is_truthy);
        Self { enabled: config_enabled && !forced_off }
    }

    /// Whether motion is enabled for this client.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }

    /// The effective duration for a UI transition of the requested length.
    ///
    /// Returns [`Duration::ZERO`] when motion is disabled — callers then paint
    /// the end state directly — and otherwise clamps `requested` to
    /// [`MAX_TRANSITION`].
    #[must_use]
    pub fn duration(self, requested: Duration) -> Duration {
        if self.enabled { requested.min(MAX_TRANSITION) } else { Duration::ZERO }
    }

    /// Build a capped, ease-out `gpui::Animation` for a UI transition.
    ///
    /// The duration is clamped by [`duration`](Self::duration); when motion is
    /// disabled the animation has zero duration so GPUI resolves it to its end
    /// state on the first frame. Motion also stays globally suppressed whenever
    /// [`apply_to_app`](Self::apply_to_app) has flipped reduce-motion on.
    #[must_use]
    pub fn transition(self, requested: Duration) -> Animation {
        Animation::new(self.duration(requested)).with_easing(ease_out_quint())
    }

    /// Mirror this policy onto GPUI's global reduce-motion flag.
    ///
    /// With motion disabled this sets `reduce_motion`, which makes every
    /// `AnimationExt::with_animation` in the tree render its static end state
    /// and schedule no frames — the deterministic path that keeps repeated
    /// screenshots byte-identical. Call it at startup and after any config
    /// reload so the flag tracks the live `animations` value.
    pub fn apply_to_app(self, cx: &mut App) {
        cx.set_reduce_motion(!self.enabled);
    }
}

/// Whether a raw environment value counts as "on".
///
/// Accepts the common truthy spellings (`1`, `true`, `yes`, `on`, case
/// insensitive); everything else — including the empty string and unparseable
/// bytes — is treated as falsy so a stray `SCRIBE_DISABLE_ANIMATIONS=` does not
/// silently kill motion.
fn env_is_truthy(value: &OsStr) -> bool {
    value
        .to_str()
        .map(str::trim)
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

#[cfg(test)]
mod tests;
