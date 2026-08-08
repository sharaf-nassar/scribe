use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use nix::unistd::{Uid, User};

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
