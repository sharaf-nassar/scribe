//! Unit coverage for the motion-policy resolver: config/env precedence, the
//! 150 ms cap, and the disabled-path zero duration that underpins deterministic
//! screenshots.

use std::ffi::OsStr;
use std::time::Duration;

use super::{AnimationSettings, MAX_TRANSITION};

// @lat: [[test#GPUI Animation Policy#Config default enables motion]]
// @lat: [[test#GPUI Animation Policy#Config false disables motion]]
#[test]
fn config_controls_motion_without_an_override() {
    assert!(AnimationSettings::resolve_with_env(true, None).enabled());
    assert!(!AnimationSettings::resolve_with_env(false, None).enabled());
}

// @lat: [[test#GPUI Animation Policy#Truthy env override forces motion off]]
#[test]
fn truthy_env_override_beats_config_true() {
    for raw in ["1", "true", "TRUE", "Yes", "on", " on "] {
        let settings = AnimationSettings::resolve_with_env(true, Some(OsStr::new(raw)));
        assert!(!settings.enabled(), "override {raw:?} should force motion off");
    }
}

// @lat: [[test#GPUI Animation Policy#Falsy env value leaves config in charge]]
#[test]
fn falsy_or_empty_env_leaves_config_in_charge() {
    for raw in ["0", "false", "no", "off", "", "garbage"] {
        let settings = AnimationSettings::resolve_with_env(true, Some(OsStr::new(raw)));
        assert!(settings.enabled(), "override {raw:?} should not disable motion");
    }
}

// @lat: [[test#GPUI Animation Policy#Enabled duration clamps to 150 ms]]
// @lat: [[test#GPUI Animation Policy#Disabled duration is zero]]
#[test]
fn duration_follows_motion_policy() {
    let enabled = AnimationSettings::resolve_with_env(true, None);
    assert_eq!(enabled.duration(Duration::from_millis(400)), MAX_TRANSITION);
    assert_eq!(enabled.duration(Duration::from_millis(80)), Duration::from_millis(80));

    let disabled = AnimationSettings::resolve_with_env(false, None);
    assert_eq!(disabled.duration(Duration::from_millis(120)), Duration::ZERO);
    assert_eq!(disabled.transition(Duration::from_millis(120)).duration, Duration::ZERO);
}
