//! Typed classification for failed link opens.
//!
//! This module receives only a spawn error kind or exit code. It never reads
//! child stdout or stderr.

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};

/// Result reported by an attempted system-link opener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenOutcome {
    /// The opener command could not be spawned.
    SpawnError(ErrorKind),
    /// The opener exited; `None` means it ended without an exit code.
    Exited { code: Option<i32> },
}

/// Origin of the target sent to the system opener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenTargetKind {
    /// A URL found by heuristic detection.
    Url,
    /// A file-system path found by heuristic detection.
    Path,
    /// A URI supplied by an OSC 8 hyperlink.
    Osc8,
}

/// Target metadata available to the failed-open classifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenTarget {
    /// Whether the target came from a URL, path, or OSC 8 span.
    pub kind: OpenTargetKind,
    /// URI scheme, when the target has one.
    pub scheme: Option<String>,
    /// Resolved file-system path for path and `file:` targets.
    pub resolved_path: Option<PathBuf>,
}

/// Return the user message for an unsuccessful link open.
///
/// The branches deliberately follow the normative precedence: a missing
/// command, mailto, a missing path/file, a safe non-file scheme, then the
/// exit-code and code-less fallbacks. A successful exit returns no message.
#[must_use]
pub fn classify_open_failure(
    command: &str,
    outcome: OpenOutcome,
    target: &OpenTarget,
) -> Option<String> {
    match outcome {
        OpenOutcome::SpawnError(ErrorKind::NotFound) => Some(format!("{command} is not installed")),
        OpenOutcome::SpawnError(_) => Some(format!("{command} failed")),
        OpenOutcome::Exited { code: Some(0) } => None,
        OpenOutcome::Exited { code } => Some(classify_exit_failure(command, code, target)),
    }
}

fn classify_exit_failure(command: &str, code: Option<i32>, target: &OpenTarget) -> String {
    let Some(code) = code else {
        return format!("{command} failed");
    };
    if target.is_mailto() {
        return "no mail client configured".to_owned();
    }
    if target.is_path_or_file() {
        if target.resolved_path.as_deref().is_some_and(path_is_missing) {
            return "file no longer exists".to_owned();
        }
    } else if let Some(scheme) = safe_scheme(target.scheme.as_deref()) {
        return format!("no application handles {scheme} links");
    }
    format!("{command} exited {code}")
}

impl OpenTarget {
    fn is_mailto(&self) -> bool {
        self.scheme.as_deref().is_some_and(|scheme| scheme.eq_ignore_ascii_case("mailto"))
    }

    fn is_path_or_file(&self) -> bool {
        self.kind == OpenTargetKind::Path
            || self.scheme.as_deref().is_some_and(|scheme| scheme.eq_ignore_ascii_case("file"))
    }
}

fn path_is_missing(path: &Path) -> bool {
    std::fs::metadata(path).is_err()
}

/// Return a scheme that is safe to interpolate into a user-facing message.
fn safe_scheme(scheme: Option<&str>) -> Option<String> {
    let scheme = scheme?;
    if scheme.is_empty() || scheme.len() > 16 || !scheme.is_ascii() {
        return None;
    }
    let mut bytes = scheme.bytes();
    if !bytes.next()?.is_ascii_alphabetic()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::{io::ErrorKind, path::PathBuf};

    use super::{OpenOutcome, OpenTarget, OpenTargetKind, classify_open_failure};

    fn target(
        kind: OpenTargetKind,
        scheme: Option<&str>,
        resolved_path: Option<PathBuf>,
    ) -> OpenTarget {
        OpenTarget { kind, scheme: scheme.map(str::to_owned), resolved_path }
    }

    fn uri(scheme: &str) -> OpenTarget {
        target(OpenTargetKind::Url, Some(scheme), None)
    }

    // @lat: [[client#Client#URL Detection#Failed link opens]]
    #[test]
    fn classifier_truth_table_covers_all_six_messages() {
        let missing_path = PathBuf::from("/scribe-link-feedback-file-does-not-exist");
        let existing_path = std::env::current_exe().expect("test executable exists");
        let cases = [
            (
                "open",
                OpenOutcome::SpawnError(ErrorKind::NotFound),
                uri("ssh"),
                "open is not installed",
            ),
            (
                "xdg-open",
                OpenOutcome::Exited { code: Some(3) },
                uri("mailto"),
                "no mail client configured",
            ),
            (
                "xdg-open",
                OpenOutcome::Exited { code: Some(4) },
                target(OpenTargetKind::Path, None, Some(missing_path)),
                "file no longer exists",
            ),
            (
                "xdg-open",
                OpenOutcome::Exited { code: Some(5) },
                uri("ssh"),
                "no application handles ssh links",
            ),
            (
                "code",
                OpenOutcome::Exited { code: Some(6) },
                target(OpenTargetKind::Path, None, Some(existing_path)),
                "code exited 6",
            ),
            ("open", OpenOutcome::Exited { code: None }, uri("https"), "open failed"),
        ];

        for (command, outcome, target, expected) in cases {
            assert_eq!(classify_open_failure(command, outcome, &target).as_deref(), Some(expected));
        }
    }

    #[test]
    fn classifier_preserves_precedence_and_stats_file_targets() {
        let missing_file_uri = target(
            OpenTargetKind::Url,
            Some("file"),
            Some(PathBuf::from("/scribe-link-feedback-file-uri-does-not-exist")),
        );
        assert_eq!(
            classify_open_failure(
                "xdg-open",
                OpenOutcome::Exited { code: Some(7) },
                &missing_file_uri
            ),
            Some("file no longer exists".to_owned())
        );

        let existing_file_uri = target(
            OpenTargetKind::Url,
            Some("file"),
            Some(std::env::current_exe().expect("test executable exists")),
        );
        assert_eq!(
            classify_open_failure(
                "xdg-open",
                OpenOutcome::Exited { code: Some(7) },
                &existing_file_uri
            ),
            Some("xdg-open exited 7".to_owned())
        );

        assert_eq!(
            classify_open_failure(
                "xdg-open",
                OpenOutcome::Exited { code: Some(7) },
                &uri("mailto")
            ),
            Some("no mail client configured".to_owned())
        );
    }

    #[test]
    fn classifier_validates_and_caps_scheme() {
        assert_eq!(
            classify_open_failure(
                "xdg-open",
                OpenOutcome::Exited { code: Some(8) },
                &uri("SSH+V2")
            ),
            Some("no application handles ssh+v2 links".to_owned())
        );
        assert_eq!(
            classify_open_failure(
                "xdg-open",
                OpenOutcome::Exited { code: Some(8) },
                &uri("abcdefghijklmnop")
            ),
            Some("no application handles abcdefghijklmnop links".to_owned())
        );
        for scheme in ["abcdefghijklmnopq", "ssh\nwarning", "sshé"] {
            assert_eq!(
                classify_open_failure(
                    "xdg-open",
                    OpenOutcome::Exited { code: Some(8) },
                    &uri(scheme)
                ),
                Some("xdg-open exited 8".to_owned())
            );
        }
    }

    #[test]
    fn classifier_templates_the_final_command_name() {
        let no_scheme = target(OpenTargetKind::Url, None, None);
        assert_eq!(
            classify_open_failure("code", OpenOutcome::SpawnError(ErrorKind::NotFound), &no_scheme),
            Some("code is not installed".to_owned())
        );
        assert_eq!(
            classify_open_failure("code", OpenOutcome::Exited { code: Some(9) }, &no_scheme),
            Some("code exited 9".to_owned())
        );
        assert_eq!(
            classify_open_failure(
                "code",
                OpenOutcome::SpawnError(ErrorKind::PermissionDenied),
                &no_scheme
            ),
            Some("code failed".to_owned())
        );
        assert_eq!(
            classify_open_failure("code", OpenOutcome::Exited { code: None }, &uri("ssh")),
            Some("code failed".to_owned())
        );
        assert_eq!(
            classify_open_failure("code", OpenOutcome::Exited { code: Some(0) }, &no_scheme),
            None
        );
    }
}
