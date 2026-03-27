use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::ai_state::AiProvider;

use crate::error::ScribeError;
use crate::theme::{self, Theme, ThemeColors, hex_to_rgba, rgba_to_hex};

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
    #[serde(default)]
    pub update: UpdateConfig,
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
    /// Override color for active pane and workspace focus borders (`#rrggbb`).
    /// Falls back to the theme accent color when `None`.
    #[serde(default)]
    pub focus_border_color: Option<String>,
    #[serde(default = "default_focus_border_width")]
    pub focus_border_width: f32,
    /// Vertical padding added to `tab_height` for the effective tab bar row height.
    #[serde(default = "default_tab_bar_padding")]
    pub tab_bar_padding: f32,
    #[serde(default = "default_tab_width")]
    pub tab_width: u16,
    #[serde(default = "default_status_bar_height")]
    pub status_bar_height: f32,
    #[serde(default = "default_tab_height")]
    pub tab_height: f32,
    #[serde(default)]
    pub content_padding: ContentPadding,
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
            focus_border_color: None,
            focus_border_width: default_focus_border_width(),
            tab_bar_padding: default_tab_bar_padding(),
            tab_width: default_tab_width(),
            status_bar_height: default_status_bar_height(),
            tab_height: default_tab_height(),
            content_padding: ContentPadding::default(),
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

fn default_focus_border_width() -> f32 {
    2.0
}

fn default_tab_bar_padding() -> f32 {
    8.0
}

fn default_tab_width() -> u16 {
    20
}

fn default_status_bar_height() -> f32 {
    24.0
}

fn default_tab_height() -> f32 {
    28.0
}

fn default_content_padding_side() -> f32 {
    8.0
}

// ---------------------------------------------------------------------------
// Content padding

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPadding {
    #[serde(default = "default_content_padding_side")]
    pub top: f32,
    #[serde(default = "default_content_padding_side")]
    pub right: f32,
    #[serde(default = "default_content_padding_side")]
    pub bottom: f32,
    #[serde(default = "default_content_padding_side")]
    pub left: f32,
}

impl ContentPadding {
    /// Clamp all sides to the valid range `0.0..=50.0`.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            top: self.top.clamp(0.0, 50.0),
            right: self.right.clamp(0.0, 50.0),
            bottom: self.bottom.clamp(0.0, 50.0),
            left: self.left.clamp(0.0, 50.0),
        }
    }
}

impl Default for ContentPadding {
    fn default() -> Self {
        Self {
            top: default_content_padding_side(),
            right: default_content_padding_side(),
            bottom: default_content_padding_side(),
            left: default_content_padding_side(),
        }
    }
}

impl AppearanceConfig {
    /// Return a copy of this config with all float fields clamped to valid ranges.
    ///
    /// - `font_size`: clamped to `[4.0, 72.0]`
    /// - `opacity`: clamped to `[0.0, 1.0]`
    /// - `scrollbar_width`: clamped to `[0.0, 20.0]`
    /// - `content_padding`: each side clamped to `[0.0, 50.0]`
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            font_size: self.font_size.clamp(4.0, 72.0),
            opacity: self.opacity.clamp(0.0, 1.0),
            scrollbar_width: self.scrollbar_width.clamp(0.0, 20.0),
            focus_border_width: self.focus_border_width.clamp(1.0, 10.0),
            content_padding: self.content_padding.clamped(),
            ..self
        }
    }
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
// AI State Colors
// ---------------------------------------------------------------------------

/// A color reference that can be either a fixed hex colour or an ANSI palette
/// index (0–15) that adapts to the active theme.
///
/// TOML format: `"#rrggbb"` for hex, `"ansi:N"` for palette index.
#[derive(Debug, Clone, PartialEq)]
pub enum AiColor {
    /// Fixed sRGB colour parsed from `#rrggbb`.
    /// Note: alpha is not preserved through serialization (hex is RGB only).
    Hex([f32; 4]),
    /// ANSI palette index (0–15), resolved at render time.
    Ansi(u8),
}

impl AiColor {
    /// Resolve to a concrete `[f32; 4]` colour given the current ANSI palette.
    #[must_use]
    pub fn resolve(&self, ansi_colors: &[[f32; 4]; 16]) -> [f32; 4] {
        match self {
            Self::Hex(c) => *c,
            Self::Ansi(idx) => {
                ansi_colors.get(usize::from(*idx)).copied().unwrap_or([1.0, 1.0, 1.0, 1.0])
            }
        }
    }
}

impl Serialize for AiColor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Hex(c) => serializer.serialize_str(&rgba_to_hex(*c)),
            Self::Ansi(idx) => serializer.serialize_str(&format!("ansi:{idx}")),
        }
    }
}

impl<'de> Deserialize<'de> for AiColor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if let Some(rest) = s.strip_prefix("ansi:") {
            let idx: u8 = rest.parse().map_err(serde::de::Error::custom)?;
            if idx > 15 {
                return Err(serde::de::Error::custom("ANSI index must be 0–15"));
            }
            Ok(Self::Ansi(idx))
        } else {
            hex_to_rgba(&s).map(Self::Hex).map_err(serde::de::Error::custom)
        }
    }
}

/// Per-state configuration for a single AI indicator state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiStateEntry {
    /// Show a coloured bar under the tab label.
    #[serde(default = "default_true")]
    pub tab_indicator: bool,
    /// Show a coloured border around the pane.
    #[serde(default = "default_true")]
    pub pane_border: bool,
    /// Indicator colour (hex or ANSI palette index).
    pub color: AiColor,
    /// Pulse animation duration in milliseconds. `0` means no pulsing.
    #[serde(default = "default_pulse_duration")]
    pub pulse_ms: u32,
    /// Auto-clear timeout in seconds. `0` means the state persists until
    /// explicitly replaced by another state.
    #[serde(default)]
    pub timeout_secs: f32,
}

fn default_pulse_duration() -> u32 {
    1000
}

/// Configuration for the four Claude Code AI indicator states.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeStatesConfig {
    #[serde(default = "default_processing_entry")]
    pub processing: AiStateEntry,
    #[serde(default = "default_waiting_for_input_entry")]
    pub waiting_for_input: AiStateEntry,
    #[serde(default = "default_permission_prompt_entry")]
    pub permission_prompt: AiStateEntry,
    #[serde(default = "default_error_entry")]
    pub error: AiStateEntry,
}

impl Default for ClaudeStatesConfig {
    fn default() -> Self {
        Self {
            processing: default_processing_entry(),
            waiting_for_input: default_waiting_for_input_entry(),
            permission_prompt: default_permission_prompt_entry(),
            error: default_error_entry(),
        }
    }
}

fn default_processing_entry() -> AiStateEntry {
    AiStateEntry {
        tab_indicator: true,
        pane_border: true,
        color: AiColor::Ansi(2),
        pulse_ms: 1400,
        timeout_secs: 0.0,
    }
}

fn default_waiting_for_input_entry() -> AiStateEntry {
    AiStateEntry {
        tab_indicator: true,
        pane_border: true,
        color: AiColor::Hex([1.0, 0.55, 0.0, 1.0]),
        pulse_ms: 2000,
        timeout_secs: 0.0,
    }
}

fn default_permission_prompt_entry() -> AiStateEntry {
    AiStateEntry {
        tab_indicator: true,
        pane_border: true,
        color: AiColor::Ansi(1),
        pulse_ms: 1500,
        timeout_secs: 0.0,
    }
}

fn default_error_entry() -> AiStateEntry {
    AiStateEntry {
        tab_indicator: true,
        pane_border: true,
        color: AiColor::Hex([0.6, 0.2, 0.8, 1.0]),
        pulse_ms: 0,
        timeout_secs: 3.0,
    }
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "status bar stats config has independent boolean feature flags, not a state machine"
)]
pub struct StatusBarStatsConfig {
    #[serde(default = "default_true")]
    pub cpu: bool,
    #[serde(default = "default_true")]
    pub memory: bool,
    #[serde(default = "default_true")]
    pub gpu: bool,
    #[serde(default = "default_true")]
    pub network: bool,
}

impl Default for StatusBarStatsConfig {
    fn default() -> Self {
        Self { cpu: true, memory: true, gpu: true, network: true }
    }
}

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
    #[serde(default = "default_true")]
    pub codex_code_integration: bool,
    #[serde(default)]
    pub hide_codex_hook_logs: bool,
    /// Which AI CLI the AI tab keybindings launch.
    #[serde(default = "default_ai_tab_provider")]
    pub ai_tab_provider: AiProvider,
    /// When `true`, the OS-reported scroll direction is used as-is (natural
    /// scrolling).  When `false` (default), the scroll delta is inverted for
    /// traditional terminal behaviour.
    #[serde(default)]
    pub natural_scroll: bool,
    /// Per-state configuration for Claude Code AI indicators.
    #[serde(default)]
    pub claude_states: ClaudeStatesConfig,
    /// Height of the AI state indicator bar in pixels.
    #[serde(default = "default_indicator_height")]
    pub indicator_height: f32,
    /// Which system stats are shown in the status bar.
    #[serde(default)]
    pub status_bar_stats: StatusBarStatsConfig,
    #[serde(default)]
    pub shell_integration: ShellIntegrationConfig,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback_lines: default_scrollback_lines(),
            copy_on_select: true,
            claude_copy_cleanup: true,
            claude_code_integration: true,
            codex_code_integration: true,
            hide_codex_hook_logs: false,
            ai_tab_provider: default_ai_tab_provider(),
            natural_scroll: false,
            claude_states: ClaudeStatesConfig::default(),
            indicator_height: default_indicator_height(),
            status_bar_stats: StatusBarStatsConfig::default(),
            shell_integration: ShellIntegrationConfig::default(),
        }
    }
}

impl TerminalConfig {
    #[must_use]
    pub fn ai_provider_enabled(&self, provider: AiProvider) -> bool {
        match provider {
            AiProvider::ClaudeCode => self.claude_code_integration,
            AiProvider::CodexCode => self.codex_code_integration,
        }
    }
}

// ---------------------------------------------------------------------------
// Shell integration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellIntegrationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ShellIntegrationConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_scrollback_lines() -> u32 {
    10_000
}

fn default_ai_tab_provider() -> AiProvider {
    AiProvider::ClaudeCode
}

fn default_indicator_height() -> f32 {
    2.0
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
    #[serde(default = "default_workspace_focus_left")]
    pub workspace_focus_left: KeyComboList,
    #[serde(default = "default_workspace_focus_right")]
    pub workspace_focus_right: KeyComboList,
    #[serde(default = "default_workspace_focus_up")]
    pub workspace_focus_up: KeyComboList,
    #[serde(default = "default_workspace_focus_down")]
    pub workspace_focus_down: KeyComboList,

    // Tabs
    #[serde(default = "default_new_tab")]
    pub new_tab: KeyComboList,
    #[serde(default = "default_new_claude_tab")]
    pub new_claude_tab: KeyComboList,
    #[serde(default = "default_new_claude_resume_tab")]
    pub new_claude_resume_tab: KeyComboList,
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
    #[serde(default = "default_prompt_jump_up")]
    pub prompt_jump_up: KeyComboList,
    #[serde(default = "default_prompt_jump_down")]
    pub prompt_jump_down: KeyComboList,

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
    #[serde(default = "default_command_palette")]
    pub command_palette: KeyComboList,
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
            workspace_focus_left: default_workspace_focus_left(),
            workspace_focus_right: default_workspace_focus_right(),
            workspace_focus_up: default_workspace_focus_up(),
            workspace_focus_down: default_workspace_focus_down(),
            new_tab: default_new_tab(),
            new_claude_tab: default_new_claude_tab(),
            new_claude_resume_tab: default_new_claude_resume_tab(),
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
            prompt_jump_up: default_prompt_jump_up(),
            prompt_jump_down: default_prompt_jump_down(),
            zoom_in: default_zoom_in(),
            zoom_out: default_zoom_out(),
            zoom_reset: default_zoom_reset(),
            new_window: default_new_window(),
            command_palette: default_command_palette(),
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

/// Return a [`KeyComboList`] with the macOS combo on macOS, otherwise the
/// other combo.  Evaluated entirely at compile time — the optimizer removes
/// the dead branch.
fn platform_combo(macos: &str, other: &str) -> KeyComboList {
    if cfg!(target_os = "macos") {
        KeyComboList::single(macos)
    } else {
        KeyComboList::single(other)
    }
}

fn default_split_vertical() -> KeyComboList {
    platform_combo("cmd+d", "ctrl+shift+\\")
}

fn default_split_horizontal() -> KeyComboList {
    platform_combo("cmd+shift+d", "ctrl+shift+-")
}

fn default_close_pane() -> KeyComboList {
    platform_combo("cmd+w", "ctrl+shift+w")
}

fn default_cycle_pane() -> KeyComboList {
    KeyComboList::single("ctrl+tab")
}

fn default_focus_left() -> KeyComboList {
    platform_combo("cmd+alt+left", "shift+ctrl+alt+left")
}

fn default_focus_right() -> KeyComboList {
    platform_combo("cmd+alt+right", "shift+ctrl+alt+right")
}

fn default_focus_up() -> KeyComboList {
    platform_combo("cmd+alt+up", "shift+ctrl+alt+up")
}

fn default_focus_down() -> KeyComboList {
    platform_combo("cmd+alt+down", "shift+ctrl+alt+down")
}

fn default_workspace_split_vertical() -> KeyComboList {
    platform_combo("cmd+ctrl+\\", "ctrl+alt+\\")
}

fn default_workspace_split_horizontal() -> KeyComboList {
    platform_combo("cmd+ctrl+-", "ctrl+alt+-")
}

fn default_workspace_focus_left() -> KeyComboList {
    KeyComboList::single("ctrl+alt+left")
}

fn default_workspace_focus_right() -> KeyComboList {
    KeyComboList::single("ctrl+alt+right")
}

fn default_workspace_focus_up() -> KeyComboList {
    KeyComboList::single("ctrl+alt+up")
}

fn default_workspace_focus_down() -> KeyComboList {
    KeyComboList::single("ctrl+alt+down")
}

fn default_new_tab() -> KeyComboList {
    platform_combo("cmd+t", "ctrl+shift+t")
}

fn default_new_claude_tab() -> KeyComboList {
    KeyComboList::single("ctrl+alt+c")
}

fn default_new_claude_resume_tab() -> KeyComboList {
    KeyComboList::single("ctrl+alt+r")
}

fn default_close_tab() -> KeyComboList {
    platform_combo("cmd+shift+w", "ctrl+shift+q")
}

fn default_next_tab() -> KeyComboList {
    platform_combo("cmd+shift+]", "ctrl+pagedown")
}

fn default_prev_tab() -> KeyComboList {
    platform_combo("cmd+shift+[", "ctrl+pageup")
}

fn default_select_tab_1() -> KeyComboList {
    platform_combo("cmd+1", "alt+1")
}

fn default_select_tab_2() -> KeyComboList {
    platform_combo("cmd+2", "alt+2")
}

fn default_select_tab_3() -> KeyComboList {
    platform_combo("cmd+3", "alt+3")
}

fn default_select_tab_4() -> KeyComboList {
    platform_combo("cmd+4", "alt+4")
}

fn default_select_tab_5() -> KeyComboList {
    platform_combo("cmd+5", "alt+5")
}

fn default_select_tab_6() -> KeyComboList {
    platform_combo("cmd+6", "alt+6")
}

fn default_select_tab_7() -> KeyComboList {
    platform_combo("cmd+7", "alt+7")
}

fn default_select_tab_8() -> KeyComboList {
    platform_combo("cmd+8", "alt+8")
}

fn default_select_tab_9() -> KeyComboList {
    platform_combo("cmd+9", "alt+9")
}

fn default_copy() -> KeyComboList {
    platform_combo("cmd+c", "ctrl+shift+c")
}

fn default_paste() -> KeyComboList {
    platform_combo("cmd+v", "ctrl+shift+v")
}

fn default_scroll_up() -> KeyComboList {
    KeyComboList::single("shift+pageup")
}

fn default_scroll_down() -> KeyComboList {
    KeyComboList::single("shift+pagedown")
}

fn default_scroll_top() -> KeyComboList {
    platform_combo("cmd+home", "shift+home")
}

fn default_scroll_bottom() -> KeyComboList {
    platform_combo("cmd+end", "shift+end")
}

fn default_prompt_jump_up() -> KeyComboList {
    KeyComboList::single("ctrl+shift+z")
}

fn default_prompt_jump_down() -> KeyComboList {
    KeyComboList::single("ctrl+shift+x")
}

fn default_find() -> KeyComboList {
    platform_combo("cmd+f", "ctrl+shift+f")
}

fn default_zoom_in() -> KeyComboList {
    platform_combo("cmd+=", "ctrl+=")
}

fn default_zoom_out() -> KeyComboList {
    platform_combo("cmd+-", "ctrl+-")
}

fn default_zoom_reset() -> KeyComboList {
    platform_combo("cmd+0", "ctrl+0")
}

fn default_new_window() -> KeyComboList {
    platform_combo("cmd+n", "ctrl+shift+n")
}

fn default_settings() -> KeyComboList {
    platform_combo("cmd+,", "ctrl+,")
}

fn default_command_palette() -> KeyComboList {
    platform_combo("cmd+shift+p", "ctrl+shift+p")
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

fn default_badge_colors() -> Vec<String> {
    vec![
        "#a78bfa".to_owned(),
        "#38bdf8".to_owned(),
        "#6ee7b7".to_owned(),
        "#fb7185".to_owned(),
        "#fbbf24".to_owned(),
        "#a3e635".to_owned(),
        "#f472b6".to_owned(),
        "#22d3ee".to_owned(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacesConfig {
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default = "default_badge_colors")]
    pub badge_colors: Vec<String>,
}

impl Default for WorkspacesConfig {
    fn default() -> Self {
        Self { roots: Vec::new(), badge_colors: default_badge_colors() }
    }
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    #[default]
    Stable,
    Beta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    #[serde(default = "default_update_enabled")]
    pub enabled: bool,
    #[serde(default = "default_update_check_interval_secs")]
    pub check_interval_secs: u64,
    #[serde(default)]
    pub channel: UpdateChannel,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            enabled: default_update_enabled(),
            check_interval_secs: default_update_check_interval_secs(),
            channel: UpdateChannel::default(),
        }
    }
}

fn default_update_enabled() -> bool {
    true
}

fn default_update_check_interval_secs() -> u64 {
    86_400
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

    let mut config: ScribeConfig = toml::from_str(&content)
        .map_err(|e| ScribeError::ConfigError { reason: format!("config parse error: {e}") })?;

    config.appearance = config.appearance.clamped();

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

    Ok(Theme::from_colors(&ThemeColors {
        name: Cow::Owned(tc.name.clone()),
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
