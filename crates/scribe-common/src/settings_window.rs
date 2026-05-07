//! Shared settings-window launch and focus metadata.

/// Environment variable used to pass the launcher window rectangle to a newly
/// spawned settings process.
pub const SETTINGS_WINDOW_ANCHOR_ENV: &str = "SCRIBE_SETTINGS_ANCHOR";

/// Screen-space rectangle for the Scribe terminal that opened Settings.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct SettingsWindowAnchor {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl SettingsWindowAnchor {
    /// Encode for `SCRIBE_SETTINGS_ANCHOR`.
    #[must_use]
    pub fn to_env_value(self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.width, self.height)
    }

    /// Decode from `SCRIBE_SETTINGS_ANCHOR`.
    #[must_use]
    pub fn from_env_value(value: &str) -> Option<Self> {
        let mut parts = value.split(',');
        let anchor = Self {
            x: parts.next()?.parse().ok()?,
            y: parts.next()?.parse().ok()?,
            width: parts.next()?.parse().ok()?,
            height: parts.next()?.parse().ok()?,
        };
        if parts.next().is_some() || !anchor.is_sane() {
            return None;
        }
        Some(anchor)
    }

    /// Return true when dimensions are plausible for a launcher window.
    #[must_use]
    pub fn is_sane(self) -> bool {
        self.width > 0 && self.height > 0 && self.width <= 16384 && self.height <= 16384
    }
}

/// Singleton socket command for the settings process.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettingsWindowCommand {
    pub cmd: String,
    #[serde(default)]
    pub anchor: Option<SettingsWindowAnchor>,
}

impl SettingsWindowCommand {
    #[must_use]
    pub fn focus(anchor: Option<SettingsWindowAnchor>) -> Self {
        Self { cmd: String::from("focus"), anchor }
    }

    #[must_use]
    pub fn quit() -> Self {
        Self { cmd: String::from("quit"), anchor: None }
    }
}

/// Compute a top-left position that centers a window over the launcher.
#[must_use]
pub fn centered_settings_position(
    anchor: SettingsWindowAnchor,
    window_width: i32,
    window_height: i32,
) -> (i32, i32) {
    let width = window_width.max(1);
    let height = window_height.max(1);
    let x = i64::from(anchor.x) + (i64::from(anchor.width) - i64::from(width)) / 2;
    let y = i64::from(anchor.y) + (i64::from(anchor.height) - i64::from(height)) / 2;
    (i64_to_i32_saturating(x), i64_to_i32_saturating(y))
}

fn i64_to_i32_saturating(value: i64) -> i32 {
    i32::try_from(value).unwrap_or_else(|_| if value.is_negative() { i32::MIN } else { i32::MAX })
}
