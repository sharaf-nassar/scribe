use serde::{Deserialize, Serialize};

use crate::error::ScribeError;
use crate::theme::{self, Theme, ThemeColors, hex_to_rgba};

// ---------------------------------------------------------------------------
// KeyComboList — a keybinding field that holds one or more key combos
// ---------------------------------------------------------------------------

/// Maximum number of key combos allowed per action.
pub const MAX_BINDINGS: usize = 5;

/// A list of key combo strings for a single keybinding action.
///
/// Deserializes from either a bare TOML string (`"ctrl+shift+w"`) for backward
/// compatibility, or a TOML array (`["ctrl+shift+w", "ctrl+w"]`).  Always
/// serializes as an array.
#[derive(Debug, Clone)]
pub struct KeyComboList(pub Vec<String>);

impl KeyComboList {
    /// Create a list containing a single combo.
    pub fn single(s: &str) -> Self {
        Self(vec![String::from(s)])
    }

    /// Create from a vec, clamping to [`MAX_BINDINGS`].
    pub fn from_vec(mut v: Vec<String>) -> Self {
        v.truncate(MAX_BINDINGS);
        Self(v)
    }

    /// Borrow the underlying combo strings.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

impl Serialize for KeyComboList {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

/// Visitor for deserializing a [`KeyComboList`] from either a string or array.
struct KeyComboVisitor;

impl<'de> serde::de::Visitor<'de> for KeyComboVisitor {
    type Value = KeyComboList;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a string or array of strings")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(KeyComboList(vec![v.to_owned()]))
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut v = Vec::with_capacity(seq.size_hint().unwrap_or(1));
        while let Some(s) = seq.next_element::<String>()? {
            v.push(s);
        }
        v.truncate(MAX_BINDINGS);
        Ok(KeyComboList(v))
    }
}

impl<'de> Deserialize<'de> for KeyComboList {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(KeyComboVisitor)
    }
}

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
    #[serde(default = "default_true")]
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
    #[serde(default = "default_scrollbar_width")]
    pub scrollbar_width: f32,
    #[serde(default)]
    pub scrollbar_color: Option<String>,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            font: default_font(),
            font_size: default_font_size(),
            font_weight: default_font_weight(),
            font_weight_bold: default_font_weight_bold(),
            ligatures: true,
            line_padding: 0,
            cursor_shape: CursorShape::default(),
            cursor_blink: true,
            opacity: default_opacity(),
            theme: default_theme_name(),
            scrollbar_width: default_scrollbar_width(),
            scrollbar_color: None,
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

fn default_scrollbar_width() -> f32 {
    6.0
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
#[allow(
    clippy::struct_excessive_bools,
    reason = "terminal config has independent boolean feature flags, not a state machine"
)]
pub struct TerminalConfig {
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: u32,
    #[serde(default = "default_true")]
    pub copy_on_select: bool,
    #[serde(default = "default_true")]
    pub claude_copy_cleanup: bool,
    #[serde(default = "default_true")]
    pub claude_code_integration: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback_lines: default_scrollback_lines(),
            copy_on_select: true,
            claude_copy_cleanup: true,
            claude_code_integration: true,
        }
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
    // Panes
    #[serde(default = "default_split_vertical")]
    pub split_vertical: KeyComboList,
    #[serde(default = "default_split_horizontal")]
    pub split_horizontal: KeyComboList,
    #[serde(default = "default_close_pane")]
    pub close_pane: KeyComboList,
    #[serde(default = "default_cycle_pane")]
    pub cycle_pane: KeyComboList,
    #[serde(default = "default_focus_left")]
    pub focus_left: KeyComboList,
    #[serde(default = "default_focus_right")]
    pub focus_right: KeyComboList,
    #[serde(default = "default_focus_up")]
    pub focus_up: KeyComboList,
    #[serde(default = "default_focus_down")]
    pub focus_down: KeyComboList,

    // Workspaces
    #[serde(default = "default_workspace_split_vertical")]
    pub workspace_split_vertical: KeyComboList,
    #[serde(default = "default_workspace_split_horizontal")]
    pub workspace_split_horizontal: KeyComboList,
    #[serde(default = "default_cycle_workspace")]
    pub cycle_workspace: KeyComboList,

    // Tabs
    #[serde(default = "default_new_tab")]
    pub new_tab: KeyComboList,
    #[serde(default = "default_close_tab")]
    pub close_tab: KeyComboList,
    #[serde(default = "default_next_tab")]
    pub next_tab: KeyComboList,
    #[serde(default = "default_prev_tab")]
    pub prev_tab: KeyComboList,
    #[serde(default = "default_select_tab_1")]
    pub select_tab_1: KeyComboList,
    #[serde(default = "default_select_tab_2")]
    pub select_tab_2: KeyComboList,
    #[serde(default = "default_select_tab_3")]
    pub select_tab_3: KeyComboList,
    #[serde(default = "default_select_tab_4")]
    pub select_tab_4: KeyComboList,
    #[serde(default = "default_select_tab_5")]
    pub select_tab_5: KeyComboList,
    #[serde(default = "default_select_tab_6")]
    pub select_tab_6: KeyComboList,
    #[serde(default = "default_select_tab_7")]
    pub select_tab_7: KeyComboList,
    #[serde(default = "default_select_tab_8")]
    pub select_tab_8: KeyComboList,
    #[serde(default = "default_select_tab_9")]
    pub select_tab_9: KeyComboList,

    // Clipboard
    #[serde(default = "default_copy")]
    pub copy: KeyComboList,
    #[serde(default = "default_paste")]
    pub paste: KeyComboList,

    // Navigation
    #[serde(default = "default_scroll_up")]
    pub scroll_up: KeyComboList,
    #[serde(default = "default_scroll_down")]
    pub scroll_down: KeyComboList,
    #[serde(default = "default_scroll_top")]
    pub scroll_top: KeyComboList,
    #[serde(default = "default_scroll_bottom")]
    pub scroll_bottom: KeyComboList,
    #[serde(default = "default_find")]
    pub find: KeyComboList,

    // View
    #[serde(default = "default_zoom_in")]
    pub zoom_in: KeyComboList,
    #[serde(default = "default_zoom_out")]
    pub zoom_out: KeyComboList,
    #[serde(default = "default_zoom_reset")]
    pub zoom_reset: KeyComboList,

    // Window
    #[serde(default = "default_new_window")]
    pub new_window: KeyComboList,

    // General
    #[serde(default = "default_settings")]
    pub settings: KeyComboList,

    // Terminal shortcuts (send escape sequences to PTY)
    #[serde(default = "default_word_left")]
    pub word_left: KeyComboList,
    #[serde(default = "default_word_right")]
    pub word_right: KeyComboList,
    #[serde(default = "default_delete_word_backward")]
    pub delete_word_backward: KeyComboList,
    #[serde(default = "default_delete_word_backward_ctrl")]
    pub delete_word_backward_ctrl: KeyComboList,
    #[serde(default = "default_delete_word_forward")]
    pub delete_word_forward: KeyComboList,
    #[serde(default = "default_line_start")]
    pub line_start: KeyComboList,
    #[serde(default = "default_line_end")]
    pub line_end: KeyComboList,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            split_vertical: default_split_vertical(),
            split_horizontal: default_split_horizontal(),
            close_pane: default_close_pane(),
            cycle_pane: default_cycle_pane(),
            focus_left: default_focus_left(),
            focus_right: default_focus_right(),
            focus_up: default_focus_up(),
            focus_down: default_focus_down(),
            workspace_split_vertical: default_workspace_split_vertical(),
            workspace_split_horizontal: default_workspace_split_horizontal(),
            cycle_workspace: default_cycle_workspace(),
            new_tab: default_new_tab(),
            close_tab: default_close_tab(),
            next_tab: default_next_tab(),
            prev_tab: default_prev_tab(),
            select_tab_1: default_select_tab_1(),
            select_tab_2: default_select_tab_2(),
            select_tab_3: default_select_tab_3(),
            select_tab_4: default_select_tab_4(),
            select_tab_5: default_select_tab_5(),
            select_tab_6: default_select_tab_6(),
            select_tab_7: default_select_tab_7(),
            select_tab_8: default_select_tab_8(),
            select_tab_9: default_select_tab_9(),
            copy: default_copy(),
            paste: default_paste(),
            scroll_up: default_scroll_up(),
            scroll_down: default_scroll_down(),
            scroll_top: default_scroll_top(),
            scroll_bottom: default_scroll_bottom(),
            find: default_find(),
            zoom_in: default_zoom_in(),
            zoom_out: default_zoom_out(),
            zoom_reset: default_zoom_reset(),
            new_window: default_new_window(),
            settings: default_settings(),
            word_left: default_word_left(),
            word_right: default_word_right(),
            delete_word_backward: default_delete_word_backward(),
            delete_word_backward_ctrl: default_delete_word_backward_ctrl(),
            delete_word_forward: default_delete_word_forward(),
            line_start: default_line_start(),
            line_end: default_line_end(),
        }
    }
}

fn default_split_vertical() -> KeyComboList {
    KeyComboList::single("ctrl+shift+\\")
}

fn default_split_horizontal() -> KeyComboList {
    KeyComboList::single("ctrl+shift+-")
}

fn default_close_pane() -> KeyComboList {
    KeyComboList::single("ctrl+shift+w")
}

fn default_cycle_pane() -> KeyComboList {
    KeyComboList::single("ctrl+tab")
}

fn default_focus_left() -> KeyComboList {
    KeyComboList::single("ctrl+alt+left")
}

fn default_focus_right() -> KeyComboList {
    KeyComboList::single("ctrl+alt+right")
}

fn default_focus_up() -> KeyComboList {
    KeyComboList::single("ctrl+alt+up")
}

fn default_focus_down() -> KeyComboList {
    KeyComboList::single("ctrl+alt+down")
}

fn default_workspace_split_vertical() -> KeyComboList {
    KeyComboList::single("ctrl+alt+\\")
}

fn default_workspace_split_horizontal() -> KeyComboList {
    KeyComboList::single("ctrl+alt+-")
}

fn default_cycle_workspace() -> KeyComboList {
    KeyComboList::single("ctrl+alt+tab")
}

fn default_new_tab() -> KeyComboList {
    KeyComboList::single("ctrl+shift+t")
}

fn default_close_tab() -> KeyComboList {
    KeyComboList::single("ctrl+shift+q")
}

fn default_next_tab() -> KeyComboList {
    KeyComboList::single("ctrl+pagedown")
}

fn default_prev_tab() -> KeyComboList {
    KeyComboList::single("ctrl+pageup")
}

fn default_select_tab_1() -> KeyComboList {
    KeyComboList::single("ctrl+1")
}

fn default_select_tab_2() -> KeyComboList {
    KeyComboList::single("ctrl+2")
}

fn default_select_tab_3() -> KeyComboList {
    KeyComboList::single("ctrl+3")
}

fn default_select_tab_4() -> KeyComboList {
    KeyComboList::single("ctrl+4")
}

fn default_select_tab_5() -> KeyComboList {
    KeyComboList::single("ctrl+5")
}

fn default_select_tab_6() -> KeyComboList {
    KeyComboList::single("ctrl+6")
}

fn default_select_tab_7() -> KeyComboList {
    KeyComboList::single("ctrl+7")
}

fn default_select_tab_8() -> KeyComboList {
    KeyComboList::single("ctrl+8")
}

fn default_select_tab_9() -> KeyComboList {
    KeyComboList::single("ctrl+9")
}

fn default_copy() -> KeyComboList {
    KeyComboList::single("ctrl+shift+c")
}

fn default_paste() -> KeyComboList {
    KeyComboList::single("ctrl+shift+v")
}

fn default_scroll_up() -> KeyComboList {
    KeyComboList::single("shift+pageup")
}

fn default_scroll_down() -> KeyComboList {
    KeyComboList::single("shift+pagedown")
}

fn default_scroll_top() -> KeyComboList {
    KeyComboList::single("shift+home")
}

fn default_scroll_bottom() -> KeyComboList {
    KeyComboList::single("shift+end")
}

fn default_find() -> KeyComboList {
    KeyComboList::single("ctrl+shift+f")
}

fn default_zoom_in() -> KeyComboList {
    KeyComboList::single("ctrl+=")
}

fn default_zoom_out() -> KeyComboList {
    KeyComboList::single("ctrl+-")
}

fn default_zoom_reset() -> KeyComboList {
    KeyComboList::single("ctrl+0")
}

fn default_new_window() -> KeyComboList {
    KeyComboList::single("ctrl+shift+n")
}

fn default_settings() -> KeyComboList {
    KeyComboList::single("ctrl+,")
}

fn default_word_left() -> KeyComboList {
    KeyComboList::single("ctrl+left")
}

fn default_word_right() -> KeyComboList {
    KeyComboList::single("ctrl+right")
}

fn default_delete_word_backward() -> KeyComboList {
    KeyComboList::single("alt+backspace")
}

fn default_delete_word_backward_ctrl() -> KeyComboList {
    KeyComboList::single("ctrl+backspace")
}

fn default_delete_word_forward() -> KeyComboList {
    KeyComboList::single("ctrl+delete")
}

fn default_line_start() -> KeyComboList {
    KeyComboList::single("ctrl+home")
}

fn default_line_end() -> KeyComboList {
    KeyComboList::single("ctrl+end")
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
