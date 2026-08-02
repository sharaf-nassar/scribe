use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use nix::unistd::{Uid, User};

/// Source selected while resolving the host login shell for an AI launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiLoginShellSource {
    /// Current account shell from the passwd database.
    Passwd,
    /// `SHELL` inherited by the server daemon.
    DaemonEnvironment,
    /// Portable fallback when neither host source names a shell.
    Fallback,
}

impl AiLoginShellSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passwd => "passwd",
            Self::DaemonEnvironment => "daemon_shell",
            Self::Fallback => "fallback_sh",
        }
    }
}

/// Host login shell and the fallback tier that selected it for an AI launch.
pub struct AiLoginShell {
    pub program: String,
    pub source: AiLoginShellSource,
}

/// Resolve the shell Scribe should use for new sessions and shell-wrapped
/// commands.
#[must_use]
pub fn default_shell_path() -> Option<PathBuf> {
    resolve_default_shell_path(
        std::env::var_os("SHELL").as_deref(),
        account_shell_path().as_deref(),
    )
}

/// Resolve the shell binary string, falling back to `"sh"` when neither the
/// process environment nor the account database provides one.
#[must_use]
pub fn default_shell_program() -> String {
    default_shell_path().unwrap_or_else(|| PathBuf::from("sh")).to_string_lossy().into_owned()
}

/// Resolve an AI launch shell from live host identity before daemon state.
///
/// Unlike [`default_shell_program`], this is deliberately passwd-first so a
/// host-side `chsh` takes effect without waiting for the daemon to restart.
#[must_use]
pub fn ai_login_shell() -> AiLoginShell {
    if let Some(program) = account_shell_path() {
        return AiLoginShell {
            program: program.to_string_lossy().into_owned(),
            source: AiLoginShellSource::Passwd,
        };
    }

    if let Some(program) = std::env::var_os("SHELL").filter(|shell| !shell.is_empty()) {
        return AiLoginShell {
            program: PathBuf::from(program).to_string_lossy().into_owned(),
            source: AiLoginShellSource::DaemonEnvironment,
        };
    }

    AiLoginShell { program: String::from("sh"), source: AiLoginShellSource::Fallback }
}

#[must_use]
fn account_shell_path() -> Option<PathBuf> {
    User::from_uid(Uid::current())
        .ok()
        .flatten()
        .map(|user| user.shell)
        .filter(|shell| !shell.as_os_str().is_empty())
}

#[must_use]
fn resolve_default_shell_path(
    shell_env: Option<&OsStr>,
    account_shell: Option<&Path>,
) -> Option<PathBuf> {
    shell_env
        .filter(|shell| !shell.is_empty())
        .map(PathBuf::from)
        .or_else(|| account_shell.map(Path::to_path_buf))
}

#[cfg(test)]
mod tests {
    use super::resolve_default_shell_path;
    use std::ffi::OsStr;
    use std::path::Path;

    #[test]
    fn prefers_shell_env_when_present() {
        let resolved = resolve_default_shell_path(
            Some(OsStr::new("/opt/homebrew/bin/bash")),
            Some(Path::new("/bin/zsh")),
        );

        assert_eq!(resolved.as_deref(), Some(Path::new("/opt/homebrew/bin/bash")));
    }

    #[test]
    fn falls_back_to_account_shell_when_shell_env_missing() {
        let resolved = resolve_default_shell_path(None, Some(Path::new("/bin/bash")));

        assert_eq!(resolved.as_deref(), Some(Path::new("/bin/bash")));
    }

    #[test]
    fn ignores_empty_shell_env() {
        let resolved =
            resolve_default_shell_path(Some(OsStr::new("")), Some(Path::new("/bin/bash")));

        assert_eq!(resolved.as_deref(), Some(Path::new("/bin/bash")));
    }
}
