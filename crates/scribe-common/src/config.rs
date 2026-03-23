use serde::{Deserialize, Serialize};

use crate::error::ScribeError;
use crate::theme::{self, Theme, ThemeColors, hex_to_rgba};

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// Unified configuration for Scribe, shared between server and client.
///
/// Deserialized from `~/.config/scribe/config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScribeConfig {
    #[serde(default)]
    pub appearance: AppearanceConfig,
    #[serde(default)]
    pub theme: Option<ThemeConfig>,
    #[serde(default)]
    pub terminal: TerminalConfig,
    #[serde(default)]
    pub keybindings: KeybindingsConfig,
    #[serde(default)]
    pub workspaces: WorkspacesConfig,
}

// ---------------------------------------------------------------------------
// Appearance
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppearanceConfig {
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default = "default_font_weight")]
    pub font_weight: u16,
    #[serde(default = "default_font_weight_bold")]
    pub font_weight_bold: u16,
    #[serde(default)]
    pub ligatures: bool,
    #[serde(default)]
    pub line_padding: u16,
    #[serde(default)]
    pub cursor_shape: CursorShape,
    #[serde(default = "default_true")]
    pub cursor_blink: bool,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    #[serde(default = "default_theme_name")]
    pub theme: String,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            font: default_font(),
            font_size: default_font_size(),
            font_weight: default_font_weight(),
            font_weight_bold: default_font_weight_bold(),
            ligatures: false,
            line_padding: 0,
            cursor_shape: CursorShape::default(),
            cursor_blink: true,
            opacity: default_opacity(),
            theme: default_theme_name(),
        }
    }
}

fn default_font() -> String {
    String::from("JetBrains Mono")
}

fn default_font_size() -> f32 {
    14.0
}

fn default_font_weight() -> u16 {
    400
}

fn default_font_weight_bold() -> u16 {
    700
}

fn default_true() -> bool {
    true
}

fn default_opacity() -> f32 {
    1.0
}

fn default_theme_name() -> String {
    String::from("minimal-dark")
}

// ---------------------------------------------------------------------------
// Cursor shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorShape {
    #[default]
    Block,
    Beam,
    Underline,
}

// ---------------------------------------------------------------------------
// Theme (inline custom definition)
// ---------------------------------------------------------------------------

/// Optional inline theme definition in the config file.
///
/// When `appearance.theme == "custom"`, these values are used to build a
/// runtime `Theme`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    pub name: String,
    pub foreground: String,
    pub background: String,
    pub cursor: String,
    pub cursor_accent: String,
    pub selection: String,
    pub selection_foreground: String,
    pub colors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfig {
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: u32,
    #[serde(default)]
    pub shell: String,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self { scrollback_lines: default_scrollback_lines(), shell: String::new() }
    }
}

fn default_scrollback_lines() -> u32 {
    10_000
}

// ---------------------------------------------------------------------------
// Keybindings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindingsConfig {
    #[serde(default = "default_split_vertical")]
    pub split_vertical: String,
    #[serde(default = "default_split_horizontal")]
    pub split_horizontal: String,
    #[serde(default = "default_close_pane")]
    pub close_pane: String,
    #[serde(default = "default_cycle_pane")]
    pub cycle_pane: String,
    #[serde(default = "default_settings")]
    pub settings: String,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            split_vertical: default_split_vertical(),
            split_horizontal: default_split_horizontal(),
            close_pane: default_close_pane(),
            cycle_pane: default_cycle_pane(),
            settings: default_settings(),
        }
    }
}

fn default_split_vertical() -> String {
    String::from("ctrl+shift+\\")
}

fn default_split_horizontal() -> String {
    String::from("ctrl+shift+-")
}

fn default_close_pane() -> String {
    String::from("ctrl+shift+w")
}

fn default_cycle_pane() -> String {
    String::from("ctrl+tab")
}

fn default_settings() -> String {
    String::from("ctrl+,")
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspacesConfig {
    #[serde(default)]
    pub roots: Vec<String>,
}

// ---------------------------------------------------------------------------
// Load / Save
// ---------------------------------------------------------------------------

/// Load the Scribe config from `~/.config/scribe/config.toml`.
///
/// Returns `ScribeConfig::default()` if the file does not exist.
pub fn load_config() -> Result<ScribeConfig, ScribeError> {
    let Some(config_dir) = dirs::config_dir() else {
        tracing::info!("no config directory found, using defaults");
        return Ok(ScribeConfig::default());
    };

    let config_path = config_dir.join("scribe").join("config.toml");

    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(?config_path, "no config file found, using defaults");
            return Ok(ScribeConfig::default());
        }
        Err(e) => {
            return Err(ScribeError::ConfigError {
                reason: format!("failed to read {}: {e}", config_path.display()),
            });
        }
    };

    tracing::info!(?config_path, "loading config");

    let config: ScribeConfig = toml::from_str(&content)
        .map_err(|e| ScribeError::ConfigError { reason: format!("config parse error: {e}") })?;

    Ok(config)
}

/// Serialize the config to TOML and write it to `~/.config/scribe/config.toml`.
///
/// Creates parent directories if they do not exist.
pub fn save_config(config: &ScribeConfig) -> Result<(), ScribeError> {
    let Some(config_dir) = dirs::config_dir() else {
        return Err(ScribeError::ConfigError {
            reason: String::from("could not determine config directory"),
        });
    };

    let scribe_dir = config_dir.join("scribe");
    std::fs::create_dir_all(&scribe_dir).map_err(|e| ScribeError::ConfigError {
        reason: format!("failed to create config directory {}: {e}", scribe_dir.display()),
    })?;

    let config_path = scribe_dir.join("config.toml");
    let content = toml::to_string_pretty(config)
        .map_err(|e| ScribeError::ConfigError { reason: format!("TOML serialize error: {e}") })?;

    std::fs::write(&config_path, content).map_err(|e| ScribeError::ConfigError {
        reason: format!("failed to write {}: {e}", config_path.display()),
    })?;

    tracing::info!(?config_path, "config saved");
    Ok(())
}

// ---------------------------------------------------------------------------
// Theme resolution
// ---------------------------------------------------------------------------

/// Resolve the active `Theme` from the config.
///
/// Resolution order:
/// 1. If `appearance.theme` matches a built-in preset name, use that preset.
/// 2. If `appearance.theme == "custom"`, parse the inline `[theme]` section.
/// 3. Otherwise, attempt to load `~/.config/scribe/themes/{name}.toml`.
/// 4. On any failure, log a warning and fall back to `minimal-dark`.
pub fn resolve_theme(config: &ScribeConfig) -> Theme {
    let name = &config.appearance.theme;

    // Reject path-traversal attempts in theme names.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        tracing::warn!(theme = %name, "theme name contains invalid characters, using default");
        return theme::minimal_dark();
    }

    // 1. Built-in presets
    if let Some(preset) = theme::resolve_preset(name) {
        return preset;
    }

    // 2. Inline custom theme
    if name == "custom" {
        return config.theme.as_ref().map_or_else(
            || {
                tracing::warn!("theme set to 'custom' but no [theme] section found");
                theme::minimal_dark()
            },
            build_theme_from_config,
        );
    }

    // 3. External theme file
    load_theme_file(name)
}

/// Build a `Theme` from an inline `ThemeConfig`.
fn build_theme_from_config(tc: &ThemeConfig) -> Theme {
    match try_build_theme_from_config(tc) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse inline theme, using default");
            theme::minimal_dark()
        }
    }
}

/// Fallible conversion from `ThemeConfig` to `Theme`.
fn try_build_theme_from_config(tc: &ThemeConfig) -> Result<Theme, ScribeError> {
    let foreground = hex_to_rgba(&tc.foreground)?;
    let background = hex_to_rgba(&tc.background)?;
    let cursor = hex_to_rgba(&tc.cursor)?;
    let cursor_accent = hex_to_rgba(&tc.cursor_accent)?;
    let selection = hex_to_rgba(&tc.selection)?;
    let selection_foreground = hex_to_rgba(&tc.selection_foreground)?;

    if tc.colors.len() != 16 {
        return Err(ScribeError::ThemeParse {
            reason: format!("expected 16 ANSI colors, got {}", tc.colors.len()),
        });
    }

    let mut ansi_colors = [[0.0_f32; 4]; 16];
    for (idx, hex) in tc.colors.iter().enumerate() {
        if let Some(slot) = ansi_colors.get_mut(idx) {
            *slot = hex_to_rgba(hex)?;
        }
    }

    // Leak the name string to get a `&'static str` that `Theme` requires.
    let name: &'static str = Box::leak(tc.name.clone().into_boxed_str());

    Ok(Theme::from_colors(&ThemeColors {
        name,
        foreground,
        background,
        cursor,
        cursor_accent,
        selection,
        selection_foreground,
        ansi_colors,
    }))
}

/// Try to load a theme from `~/.config/scribe/themes/{name}.toml`.
fn load_theme_file(name: &str) -> Theme {
    let result = try_load_theme_file(name);
    match result {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(theme = %name, error = %e, "failed to load theme file, using default");
            theme::minimal_dark()
        }
    }
}

/// Fallible theme file loading.
fn try_load_theme_file(name: &str) -> Result<Theme, ScribeError> {
    let config_dir = dirs::config_dir().ok_or_else(|| ScribeError::ConfigError {
        reason: String::from("could not determine config directory"),
    })?;

    let theme_path = config_dir.join("scribe").join("themes").join(format!("{name}.toml"));

    let content = std::fs::read_to_string(&theme_path).map_err(|e| ScribeError::ConfigError {
        reason: format!("failed to read {}: {e}", theme_path.display()),
    })?;

    let tc: ThemeConfig = toml::from_str(&content)
        .map_err(|e| ScribeError::ConfigError { reason: format!("theme parse error: {e}") })?;

    try_build_theme_from_config(&tc)
}
