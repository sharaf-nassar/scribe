use std::sync::atomic::{AtomicBool, Ordering};

/// Live GitHub CI eligibility. Enabling it performs no process or network I/O;
/// a later qualifying push owns prerequisite checks and any GitHub request.
static GITHUB_CI_ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether GitHub CI tracking is currently eligible to react to a local push.
#[must_use]
pub fn github_ci_enabled() -> bool {
    GITHUB_CI_ENABLED.load(Ordering::Relaxed)
}

/// Apply `github_ci.enabled` live, returning its previous value.
pub fn set_github_ci_enabled(enabled: bool) -> bool {
    GITHUB_CI_ENABLED.swap(enabled, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{github_ci_enabled, set_github_ci_enabled};

    // @lat: [[test#GitHub CI Opt-in#Live projection]]
    #[test]
    fn live_projection_follows_config_changes() {
        set_github_ci_enabled(false);
        assert!(!github_ci_enabled());

        set_github_ci_enabled(true);
        assert!(github_ci_enabled());

        set_github_ci_enabled(false);
    }
}
