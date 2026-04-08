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
        if matches!(stem, "scribe-dev" | "scribe-dev-server" | "scribe-dev-settings")
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
    pub const fn settings_binary_name(self) -> &'static str {
        match self.flavor {
            AppFlavor::Stable => "scribe-settings",
            AppFlavor::Dev => "scribe-dev-settings",
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
        dirs::state_dir().map(|dir| dir.join(self.state_dir_name()))
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
