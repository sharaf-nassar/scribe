//! launchd service targets for the in-place server restart.
//!
//! The shipped updater built one target, `user/<uid>/com.scribe.server`, and
//! treated a failure as "launchd unavailable". It is not unavailable — the
//! target was simply wrong. A `LaunchAgent` loaded into a desktop session lives
//! in the **`gui/<uid>`** domain; `user/<uid>` only resolves for agents loaded
//! outside a GUI session. Resolving both on a real install shows it plainly:
//!
//! ```text
//! launchctl print user/501/com.scribe.server  → no such service
//! launchctl print gui/501/com.scribe.server   → resolves
//! ```
//!
//! So the kickstart silently failed on every desktop install and every macOS
//! upgrade took the direct `--upgrade` spawn fallback instead. The fallback
//! works, which is why nothing ever looked broken.
//!
//! The label is a parameter rather than a literal so the Dev flavour targets
//! its own service, and so a test can point at a service of its own instead of
//! restarting the developer's live server.

/// Candidate launchd targets for `label`, most likely first.
///
/// Both domains are tried because which one a service lives in depends on how
/// it was loaded, and guessing wrong is indistinguishable from launchd being
/// absent.
#[must_use]
pub fn service_targets(uid: u32, label: &str) -> Vec<String> {
    vec![format!("gui/{uid}/{label}"), format!("user/{uid}/{label}")]
}

/// Command line that restarts `target` in place.
#[must_use]
pub fn kickstart_args(target: &str) -> Vec<String> {
    vec!["kickstart".to_owned(), "-k".to_owned(), target.to_owned()]
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#Test Harness#launchd service targets#GUI domain is tried before the user domain]]
    #[test]
    fn gui_domain_is_tried_before_the_user_domain() {
        let targets = service_targets(501, "com.scribe.server");

        assert_eq!(
            targets,
            vec!["gui/501/com.scribe.server".to_owned(), "user/501/com.scribe.server".to_owned()],
            "a desktop LaunchAgent lives in gui/<uid>; trying only user/<uid> silently never works"
        );
    }

    // @lat: [[test#Test Harness#launchd service targets#Label selects the flavour service]]
    #[test]
    fn label_selects_the_flavour_service() {
        let targets = service_targets(501, "com.scribe.dev.server");

        assert!(
            targets.iter().all(|t| t.ends_with("com.scribe.dev.server")),
            "the Dev flavour must never kickstart the stable service"
        );
    }

    // @lat: [[test#Test Harness#launchd service targets#Kickstart forces a restart]]
    #[test]
    fn kickstart_forces_a_restart() {
        assert_eq!(
            kickstart_args("gui/501/com.scribe.server"),
            vec!["kickstart".to_owned(), "-k".to_owned(), "gui/501/com.scribe.server".to_owned()],
            "-k kills the running instance first; without it launchd leaves the old binary up"
        );
    }
}
