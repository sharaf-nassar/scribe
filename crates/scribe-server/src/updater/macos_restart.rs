//! Command contract for a warm macOS server replacement.
//!
//! The updater has already swapped the app bundle before it reaches this step,
//! so it must execute the server from the replacement bundle with `--upgrade`.
//! A launchd kickstart is not a warm restart: it kills the predecessor without
//! giving the successor upgrade mode, so there is no process left to hand off
//! live PTYs.

use std::path::{Path, PathBuf};

/// Arguments that put a replacement server into handoff receiver mode.
pub const UPGRADE_ARGS: &[&str] = &["--upgrade"];

/// The newly installed server executable inside `app_bundle`.
#[must_use]
pub fn server_executable(app_bundle: &Path, binary_name: &str) -> PathBuf {
    app_bundle.join("Contents/MacOS").join(binary_name)
}

/// Arguments for the warm replacement process.
#[must_use]
pub const fn upgrade_args() -> &'static [&'static str] {
    UPGRADE_ARGS
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#Test Harness#macOS updater handoff command#Uses the replacement bundle server]]
    #[test]
    fn uses_the_replacement_bundle_server() {
        assert_eq!(
            server_executable(Path::new("/Applications/Scribe.app"), "scribe-server"),
            PathBuf::from("/Applications/Scribe.app/Contents/MacOS/scribe-server")
        );
    }

    // @lat: [[test#Test Harness#macOS updater handoff command#Starts the successor in upgrade mode]]
    #[test]
    fn starts_the_successor_in_upgrade_mode() {
        assert_eq!(upgrade_args(), ["--upgrade"]);
    }
}
