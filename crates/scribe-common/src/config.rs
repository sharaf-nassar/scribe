use std::borrow::Cow;
use std::sync::{Arc, PoisonError, RwLock};

use serde::{Deserialize, Serialize};

use crate::ai_state::AiProvider;
use crate::app::current_config_dir;
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub remote: RemoteConfig,
}

// ---------------------------------------------------------------------------
// Appearance
// ---------------------------------------------------------------------------

#[allow(
    clippy::struct_excessive_bools,
    reason = "Appearance groups independent user-facing on/off toggles (ligatures, cursor blink, animations), not a state machine that would be cleaner as an enum."
)]
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
    /// Whether the GPUI client plays UI transitions and smooth scrolling.
    /// Doubles as the reduce-motion user setting; the GPUI client's
    /// `SCRIBE_DISABLE_ANIMATIONS` env override force-disables it for E2E
    /// determinism. Ignored by the legacy client.
    #[serde(default = "default_true")]
    pub animations: bool,
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
    /// Override color for the second prompt bar row background (`#rrggbb`).
    #[serde(default, alias = "prompt_bar_bg")]
    pub prompt_bar_second_row_bg: Option<String>,
    /// Override color for the first prompt bar row background (`#rrggbb`).
    #[serde(default)]
    pub prompt_bar_first_row_bg: Option<String>,
    /// Override color for prompt bar text (`#rrggbb`).
    #[serde(default)]
    pub prompt_bar_text: Option<String>,
    /// Override color for the first prompt icon (`#rrggbb`).
    #[serde(default)]
    pub prompt_bar_icon_first: Option<String>,
    /// Override color for the latest prompt icon (`#rrggbb`).
    #[serde(default)]
    pub prompt_bar_icon_latest: Option<String>,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            font: default_font(),
            font_size: default_font_size(),
            font_weight: default_font_weight(),
            font_weight_bold: default_font_weight_bold(),
            ligatures: true,
            animations: true,
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
            prompt_bar_second_row_bg: None,
            prompt_bar_first_row_bg: None,
            prompt_bar_text: None,
            prompt_bar_icon_first: None,
            prompt_bar_icon_latest: None,
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

fn default_notification_timeout_secs() -> u32 {
    10
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
// Prompt bar position
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptBarPosition {
    #[default]
    Top,
    Bottom,
}

// ---------------------------------------------------------------------------
// Theme (inline custom definition)
// ---------------------------------------------------------------------------

/// Optional inline theme definition in the config file.
///
/// When `appearance.theme == "custom"`, these values are used to build a
/// runtime `Theme`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Configuration for AI indicator states shared by supported coding tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiStateStylesConfig {
    #[serde(default = "default_processing_entry")]
    pub processing: AiStateEntry,
    #[serde(default = "default_waiting_for_input_entry")]
    pub waiting_for_input: AiStateEntry,
    #[serde(default = "default_permission_prompt_entry")]
    pub permission_prompt: AiStateEntry,
    #[serde(default = "default_error_entry")]
    pub error: AiStateEntry,
}

impl Default for AiStateStylesConfig {
    fn default() -> Self {
        Self {
            processing: default_processing_entry(),
            waiting_for_input: default_waiting_for_input_entry(),
            permission_prompt: default_permission_prompt_entry(),
            error: default_error_entry(),
        }
    }
}

/// Backward-compatible type name retained for downstream code.
pub type ClaudeStatesConfig = AiStateStylesConfig;

// ---------------------------------------------------------------------------
// AI Context Thresholds
// ---------------------------------------------------------------------------

/// Which usage band a context-window percentage falls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextBand {
    /// Below the warn threshold — usage is nominal.
    Ok,
    /// At or above warn, below danger.
    Warn,
    /// At or above the danger threshold.
    Danger,
}

/// Color and threshold configuration for the AI context-window usage indicator.
///
/// Thresholds are percentages (0–100). A percentage at or above `danger` is
/// `Danger`; at or above `warn` is `Warn`; below `warn` is `Ok`.
///
/// Two-boundary band model. If a partial TOML override produces an inverted
/// config (`warn > danger`), the `Warn` band collapses to empty — values at or
/// above `danger` resolve to `Danger`, values below stay `Ok`. Inverted configs
/// are accepted (no parse error) so partial overrides remain forgiving; users
/// who want a Warn band must keep `warn <= danger`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiContextThresholds {
    /// Percentage at which usage enters the Warn band (default: 70).
    #[serde(default = "default_warn_threshold")]
    pub warn: u8,
    /// Percentage at which usage enters the Danger band (default: 90).
    #[serde(default = "default_danger_threshold")]
    pub danger: u8,
    /// Hex color for the Ok band (default: `#5fa05f`).
    #[serde(default = "default_ok_color")]
    pub ok_color: String,
    /// Hex color for the Warn band (default: `#d4a017`).
    #[serde(default = "default_warn_color")]
    pub warn_color: String,
    /// Hex color for the Danger band (default: `#c83030`).
    #[serde(default = "default_danger_color")]
    pub danger_color: String,
}

fn default_warn_threshold() -> u8 {
    70
}
fn default_danger_threshold() -> u8 {
    90
}
fn default_ok_color() -> String {
    "#5fa05f".into()
}
fn default_warn_color() -> String {
    "#d4a017".into()
}
fn default_danger_color() -> String {
    "#c83030".into()
}

impl Default for AiContextThresholds {
    fn default() -> Self {
        Self {
            warn: default_warn_threshold(),
            danger: default_danger_threshold(),
            ok_color: default_ok_color(),
            warn_color: default_warn_color(),
            danger_color: default_danger_color(),
        }
    }
}

impl AiContextThresholds {
    /// Classify a percentage value into a [`ContextBand`].
    ///
    /// Values at or above `danger` resolve to `Danger`; values at or above `warn`
    /// (but below `danger`) resolve to `Warn`; values below `warn` resolve to `Ok`.
    ///
    /// If `warn > danger` is configured (inverted), the `Warn` band is empty: all
    /// values >= `danger` hit `Danger` first, and all values < `danger` are < `warn`
    /// by definition, so they remain `Ok`. This is a conservative failure mode.
    #[must_use]
    pub fn band(&self, pct: u8) -> ContextBand {
        if pct >= self.danger {
            ContextBand::Danger
        } else if pct >= self.warn {
            ContextBand::Warn
        } else {
            ContextBand::Ok
        }
    }

    /// Return the configured hex color string for the band at `pct`.
    #[must_use]
    pub fn color_for(&self, pct: u8) -> &str {
        match self.band(pct) {
            ContextBand::Ok => &self.ok_color,
            ContextBand::Warn => &self.warn_color,
            ContextBand::Danger => &self.danger_color,
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
pub struct StatusBarComputeStatsConfig {
    #[serde(default = "default_true")]
    pub cpu: bool,
    #[serde(default = "default_true")]
    pub gpu: bool,
}

impl Default for StatusBarComputeStatsConfig {
    fn default() -> Self {
        Self { cpu: true, gpu: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBarUsageStatsConfig {
    #[serde(default, flatten)]
    pub compute: StatusBarComputeStatsConfig,
    #[serde(default = "default_true")]
    pub memory: bool,
}

impl Default for StatusBarUsageStatsConfig {
    fn default() -> Self {
        Self { compute: StatusBarComputeStatsConfig::default(), memory: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBarStatsConfig {
    #[serde(default, flatten)]
    pub usage: StatusBarUsageStatsConfig,
    #[serde(default = "default_true")]
    pub network: bool,
}

impl Default for StatusBarStatsConfig {
    fn default() -> Self {
        Self { usage: StatusBarUsageStatsConfig::default(), network: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalClipboardConfig {
    #[serde(default = "default_true")]
    pub copy_on_select: bool,
    #[serde(default = "default_true")]
    pub claude_copy_cleanup: bool,
}

impl Default for TerminalClipboardConfig {
    fn default() -> Self {
        Self { copy_on_select: true, claude_copy_cleanup: true }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartSelectionActivation {
    DoubleClick,
    #[default]
    QuadClick,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartSelectionPrecision {
    VeryLow,
    Low,
    #[default]
    Normal,
    High,
    VeryHigh,
}

impl SmartSelectionPrecision {
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::VeryLow => 0,
            Self::Low => 1,
            Self::Normal => 2,
            Self::High => 3,
            Self::VeryHigh => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartSelectionParameterMode {
    #[default]
    Legacy,
    Interpolated,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartSelectionActionKind {
    OpenFile,
    OpenUrl,
    RunCommand,
    RunCoprocess,
    SendText,
    RunCommandInWindow,
    #[default]
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartSelectionAction {
    #[serde(default)]
    pub kind: SmartSelectionActionKind,
    #[serde(default)]
    pub parameter: String,
    #[serde(default)]
    pub parameter_mode: SmartSelectionParameterMode,
}

impl Default for SmartSelectionAction {
    fn default() -> Self {
        Self {
            kind: SmartSelectionActionKind::Copy,
            parameter: String::new(),
            parameter_mode: SmartSelectionParameterMode::Legacy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartSelectionRule {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub regex: String,
    #[serde(default)]
    pub precision: SmartSelectionPrecision,
    #[serde(default)]
    pub actions: Vec<SmartSelectionAction>,
}

impl Default for SmartSelectionRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            regex: String::new(),
            precision: SmartSelectionPrecision::Normal,
            actions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartSelectionConfig {
    #[serde(
        default = "default_smart_selection_activation",
        deserialize_with = "deserialize_smart_selection_activation"
    )]
    pub activation: SmartSelectionActivation,
    #[serde(default = "default_smart_selection_rules")]
    pub rules: Vec<SmartSelectionRule>,
}

impl Default for SmartSelectionConfig {
    fn default() -> Self {
        Self {
            activation: default_smart_selection_activation(),
            rules: default_smart_selection_rules(),
        }
    }
}

fn default_smart_selection_activation() -> SmartSelectionActivation {
    SmartSelectionActivation::QuadClick
}

fn deserialize_smart_selection_activation<'de, D>(
    deserializer: D,
) -> Result<SmartSelectionActivation, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    Ok(match value.as_str() {
        "double_click" => SmartSelectionActivation::DoubleClick,
        _ => SmartSelectionActivation::QuadClick,
    })
}

fn default_smart_selection_rules() -> Vec<SmartSelectionRule> {
    vec![
        smart_selection_rule(
            "whitespace_word",
            "Whitespace-bounded word",
            r"\S+",
            SmartSelectionPrecision::VeryLow,
            Vec::new(),
        ),
        smart_selection_rule(
            "namespace_identifier",
            "Namespace identifier",
            r"[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+",
            SmartSelectionPrecision::Normal,
            Vec::new(),
        ),
        smart_selection_rule(
            "path",
            "Path",
            r#"(?:~|\.{1,2})?/[^\s"'<>|]+|[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)+"#,
            SmartSelectionPrecision::High,
            vec![smart_selection_action(SmartSelectionActionKind::OpenFile, "")],
        ),
        smart_selection_rule(
            "quoted_string",
            "Quoted string",
            r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'"#,
            SmartSelectionPrecision::Normal,
            Vec::new(),
        ),
        smart_selection_rule(
            "include_path",
            "Java/Python include path",
            r"[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*){2,}",
            SmartSelectionPrecision::High,
            Vec::new(),
        ),
        smart_selection_rule(
            "uri",
            "URI",
            r#"(?:mailto|https?|ssh|telnet):[^\s"'<>|]+"#,
            SmartSelectionPrecision::VeryHigh,
            vec![smart_selection_action(SmartSelectionActionKind::OpenUrl, "")],
        ),
        smart_selection_rule(
            "objective_c_selector",
            "Objective-C selector",
            r"@selector\([A-Za-z_][A-Za-z0-9_]*(?::[A-Za-z_][A-Za-z0-9_]*)*:?\)|[A-Za-z_][A-Za-z0-9_]*(?::[A-Za-z_][A-Za-z0-9_]*)+:",
            SmartSelectionPrecision::High,
            Vec::new(),
        ),
        smart_selection_rule(
            "email",
            "Email address",
            r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
            SmartSelectionPrecision::VeryHigh,
            vec![smart_selection_action(SmartSelectionActionKind::OpenUrl, "mailto:\\0")],
        ),
    ]
}

fn smart_selection_rule(
    id: &str,
    name: &str,
    regex: &str,
    precision: SmartSelectionPrecision,
    actions: Vec<SmartSelectionAction>,
) -> SmartSelectionRule {
    SmartSelectionRule {
        id: id.to_owned(),
        name: name.to_owned(),
        enabled: true,
        regex: regex.to_owned(),
        precision,
        actions,
    }
}

fn smart_selection_action(kind: SmartSelectionActionKind, parameter: &str) -> SmartSelectionAction {
    SmartSelectionAction {
        kind,
        parameter: parameter.to_owned(),
        parameter_mode: SmartSelectionParameterMode::Legacy,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AiIntegrationToggle(bool);

impl AiIntegrationToggle {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self(enabled)
    }

    #[must_use]
    pub fn enabled(self) -> bool {
        self.0
    }
}

impl Default for AiIntegrationToggle {
    fn default() -> Self {
        Self(true)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalAiIntegrationConfig {
    #[serde(default, rename = "claude_code_integration")]
    pub claude_code: AiIntegrationToggle,
    #[serde(default, rename = "codex_code_integration")]
    pub codex_code: AiIntegrationToggle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalAiSessionConfig {
    /// When `true` (default), block `CSI 3 J` (clear scrollback) from AI
    /// sessions so the user's scrollback history is preserved. When `false`,
    /// the sequence is passed through, matching standard terminal behaviour.
    #[serde(default = "default_true")]
    pub preserve_ai_scrollback: bool,
    /// Per-state configuration for AI indicators.
    #[serde(default, alias = "claude_states")]
    pub ai_states: AiStateStylesConfig,
    /// Thresholds and colors for the AI context-window usage indicator.
    #[serde(default)]
    pub context_thresholds: AiContextThresholds,
    /// Height of the AI state indicator bar in pixels.
    #[serde(default = "default_indicator_height")]
    pub indicator_height: f32,
    #[serde(default)]
    pub shell_integration: ShellIntegrationConfig,
}

impl Default for TerminalAiSessionConfig {
    fn default() -> Self {
        Self {
            preserve_ai_scrollback: true,
            ai_states: AiStateStylesConfig::default(),
            context_thresholds: AiContextThresholds::default(),
            indicator_height: default_indicator_height(),
            shell_integration: ShellIntegrationConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalScrollConfig {
    /// When `true`, the OS-reported scroll direction is used as-is (natural
    /// scrolling).  When `false` (default), the scroll delta is inverted for
    /// traditional terminal behaviour.
    #[serde(default)]
    pub natural_scroll: bool,
    /// Optional viewport mode that pins the live terminal bottom while
    /// scrolled up in AI panes so the user can compose prompts while reading
    /// scrollback.
    #[serde(default)]
    pub scroll_pin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalPromptBarConfig {
    /// Show first/latest prompt text in AI terminal panes.
    #[serde(default = "default_true", rename = "prompt_bar")]
    pub enabled: bool,
    /// Font size for prompt bar text. `None` — the default — follows
    /// `appearance.font_size`, so the strip reads at the same size as the
    /// terminal text beside it; a value here is an explicit override.
    #[serde(default, rename = "prompt_bar_font_size")]
    pub font_size: Option<f32>,
    /// Where the prompt bar appears relative to the terminal content.
    #[serde(default, rename = "prompt_bar_position")]
    pub position: PromptBarPosition,
}

impl Default for TerminalPromptBarConfig {
    fn default() -> Self {
        Self { enabled: true, font_size: None, position: PromptBarPosition::default() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalEnvPersistenceConfig {
    /// Persist exported terminal environment variables across Scribe sessions
    /// so new shells inherit them. Disabled by default (FR-009).
    #[serde(default)]
    pub enabled: bool,
}

/// Per-axis OSC 52 clipboard policy mode (spec 010 E1).
///
/// Used by both `read_mode` and `write_mode` of [`ClipboardPolicyConfig`];
/// applies uniformly to the clipboard and primary selection per FR-004.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardMode {
    /// Silently deny the OSC 52 op (no prompt, no host clipboard mutation).
    Deny,
    /// Silently allow the op (no prompt, host clipboard is mutated / read).
    Allow,
    /// Surface a confirmation overlay before honoring the op.
    #[default]
    Prompt,
}

/// OSC 52 clipboard policy (spec 010 E1).
///
/// Two policy axes (`read_mode`, `write_mode`) applied uniformly to the
/// clipboard and primary selection, plus a maximum-write-bytes cap, an
/// opt-in focus-gate-for-writes toggle, and a burst-decision-reuse window.
/// Defaults match kitty's `clipboard_control` posture: read = Prompt,
/// write = Allow, max = 16 MiB, focus-gate = off, burst window = 500 ms.
//
// @lat: [[common#Configuration#Terminal]]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClipboardPolicyConfig {
    pub read_mode: ClipboardMode,
    pub write_mode: ClipboardMode,
    pub max_write_bytes: u64,
    pub focus_gate_writes: bool,
    pub burst_window_ms: u64,
}

impl Default for ClipboardPolicyConfig {
    fn default() -> Self {
        Self {
            read_mode: ClipboardMode::Prompt,
            write_mode: ClipboardMode::Allow,
            max_write_bytes: default_clipboard_max_write_bytes(),
            focus_gate_writes: false,
            burst_window_ms: default_clipboard_burst_window_ms(),
        }
    }
}

/// Maximum permitted value for `max_write_bytes` (512 MiB), matching kitty's
/// `clipboard_max_size` ceiling so a user migrating from kitty can preserve
/// their existing limit verbatim. Per FR-009.
pub const CLIPBOARD_MAX_WRITE_BYTES_CEILING: u64 = 512 * 1024 * 1024;

/// Maximum permitted value for `burst_window_ms` (10 s). Bounds the
/// decision-reuse window to a sane upper limit per data-model E1.
pub const CLIPBOARD_BURST_WINDOW_MS_CEILING: u64 = 10_000;

fn default_clipboard_max_write_bytes() -> u64 {
    16 * 1024 * 1024
}

fn default_clipboard_burst_window_ms() -> u64 {
    500
}

/// Mirror of [`ClipboardPolicyConfig`] used as the serde `from`-target so we
/// can clamp `max_write_bytes` and `burst_window_ms` at deserialize time.
#[derive(Deserialize)]
struct ClipboardPolicyConfigRaw {
    #[serde(default)]
    read_mode: Option<ClipboardMode>,
    #[serde(default)]
    write_mode: Option<ClipboardMode>,
    #[serde(default)]
    max_write_bytes: Option<u64>,
    #[serde(default)]
    focus_gate_writes: Option<bool>,
    #[serde(default)]
    burst_window_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for ClipboardPolicyConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = ClipboardPolicyConfigRaw::deserialize(deserializer)?;
        let defaults = ClipboardPolicyConfig::default();
        let max_write_bytes = raw
            .max_write_bytes
            .unwrap_or(defaults.max_write_bytes)
            .min(CLIPBOARD_MAX_WRITE_BYTES_CEILING);
        let burst_window_ms = raw
            .burst_window_ms
            .unwrap_or(defaults.burst_window_ms)
            .min(CLIPBOARD_BURST_WINDOW_MS_CEILING);
        Ok(Self {
            read_mode: raw.read_mode.unwrap_or(defaults.read_mode),
            write_mode: raw.write_mode.unwrap_or(defaults.write_mode),
            max_write_bytes,
            focus_gate_writes: raw.focus_gate_writes.unwrap_or(defaults.focus_gate_writes),
            burst_window_ms,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfig {
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: u32,
    #[serde(default, flatten)]
    pub clipboard: TerminalClipboardConfig,
    #[serde(default)]
    pub smart_selection: SmartSelectionConfig,
    #[serde(default, flatten)]
    pub ai_integration: TerminalAiIntegrationConfig,
    #[serde(default, flatten)]
    pub ai_session: TerminalAiSessionConfig,
    /// Which system stats are shown in the status bar.
    #[serde(default)]
    pub status_bar_stats: StatusBarStatsConfig,
    #[serde(default, flatten)]
    pub scroll: TerminalScrollConfig,
    #[serde(default)]
    pub env_persistence: TerminalEnvPersistenceConfig,
    #[serde(default, flatten)]
    pub prompt_bar: TerminalPromptBarConfig,
    /// Enable the enhanced (Kitty) keyboard protocol when an application
    /// negotiates it. When `false`, key encoding stays legacy regardless of
    /// negotiation.
    #[serde(default = "default_true")]
    pub keyboard_protocol_enhanced: bool,
    /// OSC 52 clipboard read/write policy (spec 010). Stored under the
    /// `terminal.clipboard` TOML sub-table; distinct from the flattened
    /// `TerminalClipboardConfig` above whose fields live as top-level
    /// `terminal.*` keys for backward compatibility.
    #[serde(default, rename = "clipboard")]
    pub clipboard_policy: ClipboardPolicyConfig,
    /// Require confirmation before sending a risky paste (multi-line or
    /// containing control/escape bytes) to the PTY, but only when the focused
    /// application has not enabled bracketed paste (spec 011). Opt-in; defaults
    /// off, so absent configs keep today's unconditional paste behavior.
    #[serde(default)]
    pub paste_confirmation: bool,
    /// Terminal graphics (Kitty/Sixel) master switch, stored under the
    /// `terminal.images` TOML sub-table (spec 020). Default-on; turning it off
    /// is the rollback path that stops advertising, replying, decoding, and
    /// retaining image data without touching the text pipeline.
    #[serde(default)]
    pub images: TerminalImagesConfig,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback_lines: default_scrollback_lines(),
            clipboard: TerminalClipboardConfig::default(),
            smart_selection: SmartSelectionConfig::default(),
            ai_integration: TerminalAiIntegrationConfig::default(),
            ai_session: TerminalAiSessionConfig::default(),
            status_bar_stats: StatusBarStatsConfig::default(),
            scroll: TerminalScrollConfig::default(),
            env_persistence: TerminalEnvPersistenceConfig::default(),
            prompt_bar: TerminalPromptBarConfig::default(),
            keyboard_protocol_enhanced: true,
            clipboard_policy: ClipboardPolicyConfig::default(),
            paste_confirmation: false,
            images: TerminalImagesConfig::default(),
        }
    }
}

/// Terminal-image master switch (spec 020 rollback control).
///
/// One boolean, deliberately in its own sub-table so the rollback knob is
/// discoverable as `terminal.images.enabled` rather than buried among the
/// flattened legacy `terminal.*` keys.
// @lat: [[terminal-images#Terminal Images#Image Master Switch]]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalImagesConfig {
    /// Whether Scribe may advertise, parse, decode, retain, and render
    /// terminal graphics at all. Off means every session degrades to text.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for TerminalImagesConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl TerminalConfig {
    #[must_use]
    pub fn ai_provider_enabled(&self, provider: AiProvider) -> bool {
        match provider {
            AiProvider::ClaudeCode => self.ai_integration.claude_code.enabled(),
            AiProvider::CodexCode => self.ai_integration.codex_code.enabled(),
            // The synthetic System provider has no AI integration toggle —
            // env-delta is gated on `terminal.env_persistence.enabled` and
            // checked at the hook-ingress call site instead.
            AiProvider::System => false,
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
    #[serde(default = "default_equalize")]
    pub equalize: KeyComboList,

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
    #[serde(default = "default_new_codex_tab")]
    pub new_codex_tab: KeyComboList,
    #[serde(default = "default_new_codex_resume_tab")]
    pub new_codex_resume_tab: KeyComboList,
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
    #[serde(default = "default_jump_to_failure")]
    pub jump_to_failure: KeyComboList,

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
            equalize: default_equalize(),
            workspace_split_vertical: default_workspace_split_vertical(),
            workspace_split_horizontal: default_workspace_split_horizontal(),
            workspace_focus_left: default_workspace_focus_left(),
            workspace_focus_right: default_workspace_focus_right(),
            workspace_focus_up: default_workspace_focus_up(),
            workspace_focus_down: default_workspace_focus_down(),
            new_tab: default_new_tab(),
            new_claude_tab: default_new_claude_tab(),
            new_claude_resume_tab: default_new_claude_resume_tab(),
            new_codex_tab: default_new_codex_tab(),
            new_codex_resume_tab: default_new_codex_resume_tab(),
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
            jump_to_failure: default_jump_to_failure(),
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
    platform_combo("super+ctrl+w", "ctrl+shift+w")
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

/// Same combo on every platform: `ctrl+shift+e` is unclaimed by any other
/// default (and by the shell's overlay chords) on both macOS and Linux, so the
/// action needs no `platform_combo` split.
fn default_equalize() -> KeyComboList {
    KeyComboList::single("ctrl+shift+e")
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

fn default_new_codex_tab() -> KeyComboList {
    platform_combo("cmd+alt+x", "ctrl+alt+x")
}

fn default_new_codex_resume_tab() -> KeyComboList {
    platform_combo("cmd+alt+e", "ctrl+alt+e")
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

fn default_jump_to_failure() -> KeyComboList {
    KeyComboList::single("ctrl+shift+b")
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

#[cfg(target_os = "macos")]
fn migrate_legacy_macos_keybindings(config: &mut ScribeConfig, raw_content: &str) -> bool {
    let legacy_hint = raw_content.contains("\ncycle_workspace") || raw_content.contains("\ndriver");
    let score = legacy_macos_keybinding_score(&config.keybindings);
    if !(score >= 14 || (legacy_hint && score >= 4)) {
        return false;
    }

    let kb = &mut config.keybindings;
    let mut changed = false;

    changed |=
        migrate_keybinding(&mut kb.split_vertical, default_split_vertical(), &["ctrl+shift+\\"]);
    changed |=
        migrate_keybinding(&mut kb.split_horizontal, default_split_horizontal(), &["ctrl+shift+-"]);
    changed |= migrate_keybinding(&mut kb.close_pane, default_close_pane(), &["ctrl+shift+w"]);
    changed |= migrate_keybinding(
        &mut kb.focus_left,
        default_focus_left(),
        &["ctrl+alt+left", "shift+ctrl+alt+left"],
    );
    changed |= migrate_keybinding(
        &mut kb.focus_right,
        default_focus_right(),
        &["ctrl+alt+right", "shift+ctrl+alt+right"],
    );
    changed |= migrate_keybinding(
        &mut kb.focus_up,
        default_focus_up(),
        &["ctrl+alt+up", "shift+ctrl+alt+up"],
    );
    changed |= migrate_keybinding(
        &mut kb.focus_down,
        default_focus_down(),
        &["ctrl+alt+down", "shift+ctrl+alt+down"],
    );
    changed |= migrate_keybinding(
        &mut kb.workspace_split_vertical,
        default_workspace_split_vertical(),
        &["ctrl+alt+\\"],
    );
    changed |= migrate_keybinding(
        &mut kb.workspace_split_horizontal,
        default_workspace_split_horizontal(),
        &["ctrl+alt+-"],
    );
    changed |= migrate_keybinding(&mut kb.new_tab, default_new_tab(), &["ctrl+shift+t"]);
    changed |= migrate_keybinding(&mut kb.close_tab, default_close_tab(), &["ctrl+shift+q"]);
    changed |= migrate_keybinding(&mut kb.next_tab, default_next_tab(), &["ctrl+pagedown"]);
    changed |= migrate_keybinding(&mut kb.prev_tab, default_prev_tab(), &["ctrl+pageup"]);
    changed |=
        migrate_keybinding(&mut kb.select_tab_1, default_select_tab_1(), &["ctrl+1", "alt+1"]);
    changed |=
        migrate_keybinding(&mut kb.select_tab_2, default_select_tab_2(), &["ctrl+2", "alt+2"]);
    changed |=
        migrate_keybinding(&mut kb.select_tab_3, default_select_tab_3(), &["ctrl+3", "alt+3"]);
    changed |=
        migrate_keybinding(&mut kb.select_tab_4, default_select_tab_4(), &["ctrl+4", "alt+4"]);
    changed |=
        migrate_keybinding(&mut kb.select_tab_5, default_select_tab_5(), &["ctrl+5", "alt+5"]);
    changed |=
        migrate_keybinding(&mut kb.select_tab_6, default_select_tab_6(), &["ctrl+6", "alt+6"]);
    changed |=
        migrate_keybinding(&mut kb.select_tab_7, default_select_tab_7(), &["ctrl+7", "alt+7"]);
    changed |=
        migrate_keybinding(&mut kb.select_tab_8, default_select_tab_8(), &["ctrl+8", "alt+8"]);
    changed |=
        migrate_keybinding(&mut kb.select_tab_9, default_select_tab_9(), &["ctrl+9", "alt+9"]);
    changed |= migrate_keybinding(&mut kb.copy, default_copy(), &["ctrl+shift+c"]);
    changed |= migrate_keybinding(&mut kb.paste, default_paste(), &["ctrl+shift+v"]);
    changed |= migrate_keybinding(&mut kb.scroll_top, default_scroll_top(), &["shift+home"]);
    changed |= migrate_keybinding(&mut kb.scroll_bottom, default_scroll_bottom(), &["shift+end"]);
    changed |= migrate_keybinding(&mut kb.find, default_find(), &["ctrl+shift+f"]);
    changed |= migrate_keybinding(&mut kb.zoom_in, default_zoom_in(), &["ctrl+="]);
    changed |= migrate_keybinding(&mut kb.zoom_out, default_zoom_out(), &["ctrl+-"]);
    changed |= migrate_keybinding(&mut kb.zoom_reset, default_zoom_reset(), &["ctrl+0"]);
    changed |= migrate_keybinding(&mut kb.new_window, default_new_window(), &["ctrl+shift+n"]);
    changed |= migrate_keybinding(&mut kb.settings, default_settings(), &["ctrl+,"]);
    changed |=
        migrate_keybinding(&mut kb.command_palette, default_command_palette(), &["ctrl+shift+p"]);

    changed
}

#[cfg(target_os = "macos")]
fn legacy_macos_keybinding_score(kb: &KeybindingsConfig) -> usize {
    usize::from(keybinding_matches_any(&kb.split_vertical, &["ctrl+shift+\\"]))
        + usize::from(keybinding_matches_any(&kb.split_horizontal, &["ctrl+shift+-"]))
        + usize::from(keybinding_matches_any(&kb.close_pane, &["ctrl+shift+w"]))
        + usize::from(keybinding_matches_any(
            &kb.focus_left,
            &["ctrl+alt+left", "shift+ctrl+alt+left"],
        ))
        + usize::from(keybinding_matches_any(
            &kb.focus_right,
            &["ctrl+alt+right", "shift+ctrl+alt+right"],
        ))
        + usize::from(keybinding_matches_any(&kb.focus_up, &["ctrl+alt+up", "shift+ctrl+alt+up"]))
        + usize::from(keybinding_matches_any(
            &kb.focus_down,
            &["ctrl+alt+down", "shift+ctrl+alt+down"],
        ))
        + usize::from(keybinding_matches_any(&kb.workspace_split_vertical, &["ctrl+alt+\\"]))
        + usize::from(keybinding_matches_any(&kb.workspace_split_horizontal, &["ctrl+alt+-"]))
        + usize::from(keybinding_matches_any(&kb.new_tab, &["ctrl+shift+t"]))
        + usize::from(keybinding_matches_any(&kb.close_tab, &["ctrl+shift+q"]))
        + usize::from(keybinding_matches_any(&kb.next_tab, &["ctrl+pagedown"]))
        + usize::from(keybinding_matches_any(&kb.prev_tab, &["ctrl+pageup"]))
        + usize::from(keybinding_matches_any(&kb.select_tab_1, &["ctrl+1", "alt+1"]))
        + usize::from(keybinding_matches_any(&kb.select_tab_2, &["ctrl+2", "alt+2"]))
        + usize::from(keybinding_matches_any(&kb.select_tab_3, &["ctrl+3", "alt+3"]))
        + usize::from(keybinding_matches_any(&kb.select_tab_4, &["ctrl+4", "alt+4"]))
        + usize::from(keybinding_matches_any(&kb.select_tab_5, &["ctrl+5", "alt+5"]))
        + usize::from(keybinding_matches_any(&kb.select_tab_6, &["ctrl+6", "alt+6"]))
        + usize::from(keybinding_matches_any(&kb.select_tab_7, &["ctrl+7", "alt+7"]))
        + usize::from(keybinding_matches_any(&kb.select_tab_8, &["ctrl+8", "alt+8"]))
        + usize::from(keybinding_matches_any(&kb.select_tab_9, &["ctrl+9", "alt+9"]))
        + usize::from(keybinding_matches_any(&kb.copy, &["ctrl+shift+c"]))
        + usize::from(keybinding_matches_any(&kb.paste, &["ctrl+shift+v"]))
        + usize::from(keybinding_matches_any(&kb.scroll_top, &["shift+home"]))
        + usize::from(keybinding_matches_any(&kb.scroll_bottom, &["shift+end"]))
        + usize::from(keybinding_matches_any(&kb.find, &["ctrl+shift+f"]))
        + usize::from(keybinding_matches_any(&kb.zoom_in, &["ctrl+="]))
        + usize::from(keybinding_matches_any(&kb.zoom_out, &["ctrl+-"]))
        + usize::from(keybinding_matches_any(&kb.zoom_reset, &["ctrl+0"]))
        + usize::from(keybinding_matches_any(&kb.new_window, &["ctrl+shift+n"]))
        + usize::from(keybinding_matches_any(&kb.settings, &["ctrl+,"]))
        + usize::from(keybinding_matches_any(&kb.command_palette, &["ctrl+shift+p"]))
}

#[cfg(target_os = "macos")]
fn migrate_keybinding(
    binding: &mut KeyComboList,
    mac_default: KeyComboList,
    legacy: &[&str],
) -> bool {
    if keybinding_matches_any(binding, legacy) {
        *binding = mac_default;
        true
    } else {
        false
    }
}

#[cfg(target_os = "macos")]
fn keybinding_matches_any(binding: &KeyComboList, candidates: &[&str]) -> bool {
    if binding.as_slice().len() != 1 {
        return false;
    }
    let current = &binding.as_slice()[0];
    candidates.iter().any(|candidate| current.eq_ignore_ascii_case(candidate))
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
// Notifications
// ---------------------------------------------------------------------------

/// When to fire desktop notifications for AI session state changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyCondition {
    /// Only fire when the OS window is not focused.
    #[default]
    WhenUnfocused,
    /// Fire even when the session is already visible in the focused window.
    Always,
    /// Also fire when the session is on a background tab.
    WhenUnfocusedOrBackgroundTab,
}

/// How Linux desktop notification expiry should be handled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyTimeoutMode {
    /// Let the desktop notification server choose its default timeout.
    #[default]
    SystemDefault,
    /// Use the configured `timeout_secs` value.
    Custom,
    /// Keep the notification visible until the user dismisses it.
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationsConfig {
    /// Enable desktop notifications when an AI session finishes processing.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When notifications should fire relative to window/tab focus.
    #[serde(default)]
    pub condition: NotifyCondition,
    /// Linux-only timeout mode for desktop notifications.
    #[serde(default)]
    pub timeout_mode: NotifyTimeoutMode,
    /// Timeout used when `timeout_mode` is `Custom`.
    #[serde(default = "default_notification_timeout_secs")]
    pub timeout_secs: u32,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            condition: NotifyCondition::default(),
            timeout_mode: NotifyTimeoutMode::default(),
            timeout_secs: default_notification_timeout_secs(),
        }
    }
}

// ---------------------------------------------------------------------------
// Remote (feature 013 — remote window control over Tailscale)
// ---------------------------------------------------------------------------

/// The `[remote]` TOML table (feature 013). Controls the opt-in Tailscale
/// remote-control listener. A missing table deserializes to these defaults —
/// the feature stays fully off — because the field carries `#[serde(default)]`
/// on [`ScribeConfig`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Whether remote window control is enabled. Default `false`: the TCP
    /// listener exists only while this is `true` (FR-001), and disabling it
    /// severs live remote connections within 2 s (FR-016).
    #[serde(default)]
    pub enabled: bool,
    /// TCP port for the remote listener, bound on the machine's tailnet
    /// addresses only, never `0.0.0.0` (FR-002). Rebound live on change via the
    /// config-reload path; no server restart.
    #[serde(default = "default_remote_port")]
    pub port: u16,
    /// The nested `[remote.lan]` sub-table (feature 014 — LAN remote window
    /// control without Tailscale). A separate opt-in from the tailnet
    /// `enabled`/`port` fields above; a missing `[remote.lan]` table
    /// deserializes to [`LanRemoteConfig`] defaults (the LAN transport stays
    /// off).
    #[serde(default)]
    pub lan: LanRemoteConfig,
    /// Feature 015: how a shared window admits and authorizes remote
    /// participants (FR-004). Default [`SharingMode::SingleController`] keeps
    /// feature 013's exclusive single-writer behavior, so an existing config
    /// file loads with legacy behavior (FR-014). Applied live over the
    /// config-reload path; no server restart.
    #[serde(default)]
    pub sharing_mode: SharingMode,
    /// Feature 015: how input control is acquired in
    /// [`SharingMode::SharedSingleTypist`] mode (FR-005). Default
    /// [`ControlAcquisition::FreeClaim`]; only meaningful in single-typist mode.
    #[serde(default)]
    pub control_acquisition: ControlAcquisition,
    /// Feature 015: maximum number of REMOTE participants a shared window
    /// admits; the local owner is always exempt (FR-007, FR-018). `None`
    /// (default) means unlimited; an over-limit join is refused with the
    /// existing busy-style refusal.
    #[serde(default)]
    pub participant_limit: Option<u32>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_remote_port(),
            lan: LanRemoteConfig::default(),
            sharing_mode: SharingMode::default(),
            control_acquisition: ControlAcquisition::default(),
            participant_limit: None,
        }
    }
}

fn default_remote_port() -> u16 {
    46061
}

/// Feature 015: how a shared window admits and authorizes remote participants
/// (spec "Sharing Mode", FR-004). The owning machine's [`RemoteConfig`] holds
/// this; a live share snapshots it at mutation time. Default `SingleController`
/// preserves feature 013's exclusive single-writer behavior for existing config
/// files (FR-014).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Legacy 013 exclusive ownership: at most one controller; a takeover
    /// displaces the previous holder. No additive sharing.
    #[default]
    SingleController,
    /// Shared view, single typist: many participants view live; at most one
    /// holds input control, passed via claim or request-and-grant.
    SharedSingleTypist,
    /// Collaborative free-for-all: every attached participant may type,
    /// interleaved in arrival order.
    FreeForAll,
}

/// Feature 015: how input control is acquired in
/// [`SharingMode::SharedSingleTypist`] mode (FR-005). Default `FreeClaim`. Only
/// meaningful under single-typist sharing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAcquisition {
    /// A viewer takes control instantly by claiming it; the previous holder is
    /// demoted to a still-live viewer.
    #[default]
    FreeClaim,
    /// A viewer must request control and the current holder (or owner) grants
    /// it before the handoff.
    RequestAndGrant,
}

/// The `[remote.lan]` TOML sub-table (feature 014 — LAN remote window control
/// without Tailscale). Nested under [`RemoteConfig`]; a missing table
/// deserializes to these defaults — the LAN transport stays fully off —
/// because the field carries `#[serde(default)]` on [`RemoteConfig`]. This is a
/// separate opt-in from the tailnet `[remote]` listener (FR-012).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanRemoteConfig {
    /// Whether LAN remote window control is enabled. Default `false`: a
    /// separate opt-in from the tailnet `[remote]` listener (FR-012). Even when
    /// `true`, the LAN transport is dormant unless the machine is on a trusted
    /// network (FR-018).
    #[serde(default)]
    pub enabled: bool,
    /// TCP port for the LAN listener, bound on the physical LAN address only
    /// (distinct from the tailnet `46061`). Rebound live on change via the
    /// config-reload path; no server restart.
    #[serde(default = "default_lan_port")]
    pub port: u16,
}

impl Default for LanRemoteConfig {
    fn default() -> Self {
        Self { enabled: false, port: default_lan_port() }
    }
}

fn default_lan_port() -> u16 {
    46062
}

// ---------------------------------------------------------------------------
// Load / Save
// ---------------------------------------------------------------------------

/// Load the Scribe config from `~/.config/scribe/config.toml`.
///
/// Returns `ScribeConfig::default()` if the file does not exist.
pub fn load_config() -> Result<ScribeConfig, ScribeError> {
    let Some(config_dir) = current_config_dir() else {
        tracing::info!("no config directory found, using defaults");
        return Ok(ScribeConfig::default());
    };

    let config_path = config_dir.join("config.toml");

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

    #[cfg(target_os = "macos")]
    if migrate_legacy_macos_keybindings(&mut config, &content) {
        tracing::info!(?config_path, "migrated legacy non-mac keybindings to macOS defaults");
    }

    Ok(config)
}

/// Serialize the config to TOML and write it to `~/.config/scribe/config.toml`.
///
/// Creates parent directories if they do not exist.
pub fn save_config(config: &ScribeConfig) -> Result<(), ScribeError> {
    let Some(config_dir) = current_config_dir() else {
        return Err(ScribeError::ConfigError {
            reason: String::from("could not determine config directory"),
        });
    };

    std::fs::create_dir_all(&config_dir).map_err(|e| ScribeError::ConfigError {
        reason: format!("failed to create config directory {}: {e}", config_dir.display()),
    })?;

    let config_path = config_dir.join("config.toml");
    let content = toml::to_string_pretty(config)
        .map_err(|e| ScribeError::ConfigError { reason: format!("TOML serialize error: {e}") })?;

    std::fs::write(&config_path, content).map_err(|e| ScribeError::ConfigError {
        reason: format!("failed to write {}: {e}", config_path.display()),
    })?;

    tracing::info!(?config_path, "config saved");
    invalidate_config_snapshot();
    Ok(())
}

// ---------------------------------------------------------------------------
// Cached snapshot
// ---------------------------------------------------------------------------

/// A parsed config paired with the theme resolved from it.
///
/// Handed out behind an `Arc` by [`config_snapshot`] so callers on a hot path
/// pay neither the disk read nor the parse.
#[derive(Debug)]
pub struct ConfigSnapshot {
    pub config: ScribeConfig,
    pub theme: Theme,
}

static CONFIG_SNAPSHOT: RwLock<Option<Arc<ConfigSnapshot>>> = RwLock::new(None);

/// Return the cached config + resolved theme, loading them from disk on the
/// first call after startup or after an invalidation.
///
/// Dynamic color queries (OSC 4/10/11/12) resolve one palette entry per
/// sequence, so an uncached [`load_config`] + [`resolve_theme`] there turns a
/// 256-index probe into 256 config reads — plus another 256 theme-file reads
/// when the active theme lives outside the built-in presets. A load failure is
/// not cached: the next call retries the disk so a transiently unreadable
/// config does not pin an error for the life of the process.
pub fn config_snapshot() -> Result<Arc<ConfigSnapshot>, ScribeError> {
    if let Some(snapshot) = CONFIG_SNAPSHOT.read().unwrap_or_else(PoisonError::into_inner).clone() {
        return Ok(snapshot);
    }

    let config = load_config()?;
    let theme = resolve_theme(&config);
    let snapshot = Arc::new(ConfigSnapshot { config, theme });

    let mut cache = CONFIG_SNAPSHOT.write().unwrap_or_else(PoisonError::into_inner);
    // A concurrent caller may have populated the cache while this one was
    // reading from disk; keep whichever landed first so every holder of a
    // snapshot Arc observes the same generation.
    Ok(Arc::clone(cache.get_or_insert(snapshot)))
}

/// Drop the cached snapshot so the next [`config_snapshot`] re-reads disk.
///
/// Called from every path that knows the config file changed: the server's
/// `ConfigReloaded` handler and [`save_config`].
pub fn invalidate_config_snapshot() {
    CONFIG_SNAPSHOT.write().unwrap_or_else(PoisonError::into_inner).take();
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
    let config_dir = current_config_dir().ok_or_else(|| ScribeError::ConfigError {
        reason: String::from("could not determine config directory"),
    })?;

    let theme_path = config_dir.join("themes").join(format!("{name}.toml"));

    let content = std::fs::read_to_string(&theme_path).map_err(|e| ScribeError::ConfigError {
        reason: format!("failed to read {}: {e}", theme_path.display()),
    })?;

    let tc: ThemeConfig = toml::from_str(&content)
        .map_err(|e| ScribeError::ConfigError { reason: format!("theme parse error: {e}") })?;

    try_build_theme_from_config(&tc)
}

#[cfg(test)]
mod tests {
    use super::{AiContextThresholds, ContextBand};

    #[test]
    fn ai_context_thresholds_defaults() {
        let t = AiContextThresholds::default();
        assert_eq!(t.warn, 70);
        assert_eq!(t.danger, 90);
        assert_eq!(t.band(0), ContextBand::Ok);
        assert_eq!(t.band(50), ContextBand::Ok);
        assert_eq!(t.band(70), ContextBand::Warn);
        assert_eq!(t.band(89), ContextBand::Warn);
        assert_eq!(t.band(90), ContextBand::Danger);
        assert_eq!(t.band(100), ContextBand::Danger);
        assert_eq!(t.color_for(50), "#5fa05f");
        assert_eq!(t.color_for(75), "#d4a017");
        assert_eq!(t.color_for(95), "#c83030");
    }

    #[test]
    fn ai_context_thresholds_deserialize_partial() {
        // Allow users to override only some keys; missing keys take defaults.
        let toml = r##"
warn = 60
danger_color = "#ff0000"
"##;
        let t: AiContextThresholds = toml::from_str(toml).expect("partial config should parse");
        assert_eq!(t.warn, 60);
        assert_eq!(t.danger, 90); // default
        assert_eq!(t.danger_color, "#ff0000");
        assert_eq!(t.ok_color, "#5fa05f"); // default
    }

    #[test]
    fn ai_context_thresholds_inverted_config_collapses_warn() {
        // Inverted (warn > danger): Warn band collapses to empty due to the
        // order-of-checks in band(). Values >= danger resolve to Danger; values
        // < danger are < warn by definition, so they resolve to Ok. Conservative
        // failure mode keeps usage reporting as Danger rather than missing the
        // Warn band entirely due to inverted config.
        let t = AiContextThresholds {
            warn: 95,
            danger: 70,
            ok_color: "#000".into(),
            warn_color: "#111".into(),
            danger_color: "#222".into(),
        };
        assert_eq!(t.band(50), ContextBand::Ok);
        assert_eq!(t.band(70), ContextBand::Danger);
        assert_eq!(t.band(99), ContextBand::Danger);
    }

    #[test]
    fn ai_context_thresholds_pct_above_100_resolves_to_danger() {
        let t = AiContextThresholds::default();
        assert_eq!(t.band(150), ContextBand::Danger);
        assert_eq!(t.band(255), ContextBand::Danger);
    }

    // @lat: [[common#Configuration#Terminal#Prompt bar font size follows the terminal]]
    #[test]
    fn prompt_bar_font_size_defaults_to_following_the_terminal() {
        let default = super::ScribeConfig::default();
        assert_eq!(default.terminal.prompt_bar.font_size, None);

        // `prompt_bar` is a flattened table, so an unset override has to survive
        // the same `toml::to_string_pretty` round trip `save_config` performs
        // rather than erroring or writing a placeholder.
        let written = toml::to_string_pretty(&default).expect("default config serializes");
        assert!(
            !written.contains("prompt_bar_font_size"),
            "an unset override writes no key: {written}"
        );

        let parsed: super::ScribeConfig =
            toml::from_str("[terminal]\nprompt_bar_font_size = 22.0\n")
                .expect("explicit override parses");
        assert_eq!(parsed.terminal.prompt_bar.font_size, Some(22.0));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::{KeyComboList, ScribeConfig, migrate_legacy_macos_keybindings};

    #[test]
    fn migrates_legacy_non_mac_defaults_when_config_looks_stale() {
        let mut config = ScribeConfig::default();
        config.keybindings.split_vertical = KeyComboList::single("ctrl+shift+\\");
        config.keybindings.close_pane = KeyComboList::single("ctrl+shift+w");
        config.keybindings.new_tab = KeyComboList::single("ctrl+shift+t");
        config.keybindings.copy = KeyComboList::single("ctrl+shift+c");
        config.keybindings.paste = KeyComboList::single("ctrl+shift+v");
        config.keybindings.find = KeyComboList::single("ctrl+shift+f");
        config.keybindings.new_window = KeyComboList::single("ctrl+shift+n");
        config.keybindings.settings = KeyComboList::single("ctrl+,");

        let raw = "[keybindings]\ndriver = [\"ctrl+shift+d\"]\n";
        assert!(migrate_legacy_macos_keybindings(&mut config, raw));

        assert_eq!(config.keybindings.split_vertical, KeyComboList::single("cmd+d"));
        assert_eq!(config.keybindings.close_pane, KeyComboList::single("super+ctrl+w"));
        assert_eq!(config.keybindings.new_tab, KeyComboList::single("cmd+t"));
        assert_eq!(config.keybindings.copy, KeyComboList::single("cmd+c"));
        assert_eq!(config.keybindings.paste, KeyComboList::single("cmd+v"));
        assert_eq!(config.keybindings.find, KeyComboList::single("cmd+f"));
        assert_eq!(config.keybindings.new_window, KeyComboList::single("cmd+n"));
        assert_eq!(config.keybindings.settings, KeyComboList::single("cmd+,"));
    }

    #[test]
    fn leaves_one_off_non_mac_customizations_alone() {
        let mut config = ScribeConfig::default();
        config.keybindings.close_pane = KeyComboList::single("ctrl+shift+w");

        assert!(!migrate_legacy_macos_keybindings(&mut config, ""));
        assert_eq!(config.keybindings.close_pane, KeyComboList::single("ctrl+shift+w"));
    }
}
