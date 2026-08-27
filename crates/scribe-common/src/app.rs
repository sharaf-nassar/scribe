use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Runtime install flavor inferred from the current executable path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppFlavor {
    Stable,
    Dev,
}

/// Names and directories that define one install flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppIdentity {
    flavor: AppFlavor,
}

impl AppIdentity {
    #[must_use]
    pub const fn stable() -> Self {
        Self { flavor: AppFlavor::Stable }
    }

    #[must_use]
    pub const fn dev() -> Self {
        Self { flavor: AppFlavor::Dev }
    }

    #[must_use]
    pub fn detect_current() -> Self {
        std::env::current_exe().ok().as_deref().map_or_else(Self::stable, Self::detect_from_path)
    }

    #[must_use]
    pub fn detect_from_path(path: &Path) -> Self {
        let stem = path.file_stem().and_then(OsStr::to_str).unwrap_or_default();
        if matches!(stem, "scribe-dev" | "scribe-dev-cli" | "scribe-dev-server")
            || path.ancestors().any(|ancestor| {
                ancestor
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name == "Scribe Dev.app")
            })
        {
            Self::dev()
        } else {
            Self::stable()
        }
    }

    #[must_use]
    pub const fn is_dev(self) -> bool {
        matches!(self.flavor, AppFlavor::Dev)
    }

    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self.flavor {
            AppFlavor::Stable => "scribe",
            AppFlavor::Dev => "scribe-dev",
        }
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self.flavor {
            AppFlavor::Stable => "Scribe",
            AppFlavor::Dev => "Scribe Dev",
        }
    }

    #[must_use]
    pub const fn window_title_name(self) -> &'static str {
        match self.flavor {
            AppFlavor::Stable => "Scribe",
            AppFlavor::Dev => "devScribe",
        }
    }

    #[must_use]
    pub const fn client_binary_name(self) -> &'static str {
        match self.flavor {
            AppFlavor::Stable => "scribe-client",
            AppFlavor::Dev => "scribe-dev",
        }
    }

    #[must_use]
    pub const fn server_binary_name(self) -> &'static str {
        match self.flavor {
            AppFlavor::Stable => "scribe-server",
            AppFlavor::Dev => "scribe-dev-server",
        }
    }

    #[must_use]
    pub const fn runtime_dir_name(self) -> &'static str {
        self.slug()
    }

    #[must_use]
    pub const fn config_dir_name(self) -> &'static str {
        self.slug()
    }

    #[must_use]
    pub const fn state_dir_name(self) -> &'static str {
        self.slug()
    }

    #[must_use]
    pub const fn share_dir_name(self) -> &'static str {
        self.slug()
    }

    #[must_use]
    pub const fn systemd_service_name(self) -> &'static str {
        self.server_binary_name()
    }

    #[must_use]
    pub const fn launchd_label(self) -> &'static str {
        match self.flavor {
            AppFlavor::Stable => "com.scribe.server",
            AppFlavor::Dev => "com.scribe.dev.server",
        }
    }

    #[must_use]
    pub const fn launchd_plist_name(self) -> &'static str {
        match self.flavor {
            AppFlavor::Stable => "com.scribe.server.plist",
            AppFlavor::Dev => "com.scribe.dev.server.plist",
        }
    }

    #[must_use]
    pub const fn app_bundle_name(self) -> &'static str {
        match self.flavor {
            AppFlavor::Stable => "Scribe.app",
            AppFlavor::Dev => "Scribe Dev.app",
        }
    }

    #[must_use]
    pub fn config_dir(self) -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join(self.config_dir_name()))
    }

    #[must_use]
    pub fn state_dir(self) -> Option<PathBuf> {
        // `dirs::state_dir()` follows the XDG base-directory spec and is only
        // defined on Linux and other non-Apple unixes; on macOS and Windows it
        // returns `None`. Fall back to the platform data directory there
        // (`~/Library/Application Support` on macOS, `%APPDATA%` on Windows) so
        // persisted state — session restore, window geometry, LAN trusted
        // networks/devices, and the device identity cert — has a real home on
        // every platform instead of silently failing closed. Mirrors how
        // `config_dir` already resolves on macOS.
        dirs::state_dir().or_else(dirs::data_dir).map(|dir| dir.join(self.state_dir_name()))
    }

    #[must_use]
    pub fn macos_support_dir(self, home: &Path) -> PathBuf {
        home.join("Library/Application Support").join(self.display_name())
    }
}

static CURRENT_IDENTITY: OnceLock<AppIdentity> = OnceLock::new();

#[must_use]
pub fn current_identity() -> AppIdentity {
    *CURRENT_IDENTITY.get_or_init(AppIdentity::detect_current)
}

#[must_use]
pub fn current_config_dir() -> Option<PathBuf> {
    current_identity().config_dir()
}

#[must_use]
pub fn current_state_dir() -> Option<PathBuf> {
    current_identity().state_dir()
}

/// Shared size cap for state-directory diagnostic logs.
pub const STATE_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Rename an oversized state-directory log to its single `.log.1` rotation.
///
/// Best-effort by design: callers can still open a fresh log when rotation
/// cannot preserve the previous one.
pub fn rotate_log_if_oversized(path: &Path, max_bytes: u64) {
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > max_bytes) {
        drop(std::fs::rename(path, path.with_extension("log.1")));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::AppIdentity;

    #[test]
    fn dev_cli_uses_the_dev_install_identity() {
        assert_eq!(
            AppIdentity::detect_from_path(Path::new("/usr/bin/scribe-dev-cli")),
            AppIdentity::dev()
        );
    }
}
