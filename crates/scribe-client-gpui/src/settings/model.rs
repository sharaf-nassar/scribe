//! Declarative page/control model for the GPUI settings window.
//!
//! The old `scribe-settings` webview described its ten pages in `settings.html`
//! and wrote every edit through a `{key, value}` message that
//! [`crate::settings::apply::apply_settings_change`] routed to the config file.
//! This module reproduces that page inventory as a data-driven model: each
//! [`SettingsPage`] owns an ordered list of [`Control`]s keyed by the exact
//! dotted config key the apply path understands. The GPUI window renders these
//! generically, and the parity checklist test asserts every apply-handled key
//! namespace is represented, so the port stays 1:1 with the deleted surface
//! without hand-transcribing 3000 lines of HTML.

/// The ten settings pages, in the nav order the old `settings.html` used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    Appearance,
    Colors,
    Ai,
    Terminal,
    Keybindings,
    Workspaces,
    Updates,
    Releases,
    Notifications,
    Remote,
}

impl SettingsPage {
    /// Every page, in nav order.
    #[must_use]
    pub fn all() -> [SettingsPage; 10] {
        [
            SettingsPage::Appearance,
            SettingsPage::Colors,
            SettingsPage::Ai,
            SettingsPage::Terminal,
            SettingsPage::Keybindings,
            SettingsPage::Workspaces,
            SettingsPage::Updates,
            SettingsPage::Releases,
            SettingsPage::Notifications,
            SettingsPage::Remote,
        ]
    }

    /// The sidebar nav label for this page.
    #[must_use]
    pub fn nav_label(self) -> &'static str {
        match self {
            SettingsPage::Appearance => "Appearance",
            SettingsPage::Colors => "Colors",
            SettingsPage::Ai => "AI",
            SettingsPage::Terminal => "Terminal",
            SettingsPage::Keybindings => "Keybindings",
            SettingsPage::Workspaces => "Workspaces",
            SettingsPage::Updates => "Updates",
            SettingsPage::Releases => "Releases",
            SettingsPage::Notifications => "Notifications",
            SettingsPage::Remote => "Remote",
        }
    }
}

/// How a [`Control`] is edited. Interactive kinds map to a concrete gesture the
/// GPUI window wires to [`crate::settings::apply::apply_settings_change`];
/// display kinds render the current value read-only (inline text/color entry is
/// a documented follow-on).
#[derive(Debug, Clone)]
pub enum ControlKind {
    /// Boolean toggle; click flips it.
    Toggle,
    /// One-of-N string/enum; click cycles to the next option. The stored tuple
    /// is `(config_value, display_label)` pairs in cycle order.
    Choice(Vec<(&'static str, &'static str)>),
    /// Numeric stepper with `-`/`+` buttons.
    Stepper { min: f64, max: f64, step: f64, decimals: u8 },
    /// A hex color value; rendered as a swatch plus its hex (read-only for now).
    Color,
    /// A free-text value (font family, path); rendered read-only.
    Text,
    /// A fire-and-forget action button (add root, reset, check for updates…).
    Action,
    /// A keybinding action row; renders the action's current combo list.
    Keybinding,
}

/// A single labelled control on a settings page, keyed by the dotted config key
/// the apply path routes on.
#[derive(Debug, Clone)]
pub struct Control {
    /// The dotted config key (e.g. `appearance.font_size`), or the action name
    /// for [`ControlKind::Keybinding`] rows (e.g. `split_vertical`).
    pub key: String,
    /// Human-readable label.
    pub label: String,
    /// How the control is edited/displayed.
    pub kind: ControlKind,
}

impl Control {
    fn new(key: &str, label: &str, kind: ControlKind) -> Self {
        Self { key: key.to_owned(), label: label.to_owned(), kind }
    }
}

fn toggle(key: &str, label: &str) -> Control {
    Control::new(key, label, ControlKind::Toggle)
}

fn stepper(key: &str, label: &str, bounds: (f64, f64, f64, u8)) -> Control {
    let (min, max, step, decimals) = bounds;
    Control::new(key, label, ControlKind::Stepper { min, max, step, decimals })
}

fn choice(key: &str, label: &str, options: Vec<(&'static str, &'static str)>) -> Control {
    Control::new(key, label, ControlKind::Choice(options))
}

fn color(key: &str, label: &str) -> Control {
    Control::new(key, label, ControlKind::Color)
}

fn text(key: &str, label: &str) -> Control {
    Control::new(key, label, ControlKind::Text)
}

fn action(key: &str, label: &str) -> Control {
    Control::new(key, label, ControlKind::Action)
}

/// The canonical keybinding action names, in display order — the full set the
/// old settings keybindings page exposed and that
/// [`crate::settings::apply::apply_config_key`] routes under `keybindings.*`.
#[must_use]
pub fn keybinding_actions() -> Vec<&'static str> {
    vec![
        "new_tab",
        "new_claude_tab",
        "new_claude_resume_tab",
        "new_codex_tab",
        "new_codex_resume_tab",
        "close_tab",
        "next_tab",
        "prev_tab",
        "select_tab_1",
        "select_tab_2",
        "select_tab_3",
        "select_tab_4",
        "select_tab_5",
        "select_tab_6",
        "select_tab_7",
        "select_tab_8",
        "select_tab_9",
        "split_vertical",
        "split_horizontal",
        "close_pane",
        "cycle_pane",
        "focus_left",
        "focus_right",
        "focus_up",
        "focus_down",
        "workspace_split_vertical",
        "workspace_split_horizontal",
        "workspace_focus_left",
        "workspace_focus_right",
        "workspace_focus_up",
        "workspace_focus_down",
        "copy",
        "paste",
        "scroll_up",
        "scroll_down",
        "scroll_top",
        "scroll_bottom",
        "find",
        "jump_to_failure",
        "prompt_jump_up",
        "prompt_jump_down",
        "zoom_in",
        "zoom_out",
        "zoom_reset",
        "command_palette",
        "settings",
        "new_window",
        "word_left",
        "word_right",
        "delete_word_backward",
        "delete_word_backward_ctrl",
        "delete_word_forward",
        "line_start",
        "line_end",
    ]
}

/// The AI assistant states surfaced on the AI page, keyed as the apply path
/// expects under `ai_states.<state>.<field>`.
#[must_use]
pub fn ai_states() -> Vec<(&'static str, &'static str)> {
    vec![
        ("processing", "Processing"),
        ("waiting_for_input", "Waiting for input"),
        ("permission_prompt", "Permission prompt"),
        ("error", "Error"),
    ]
}

/// Build the ordered control list for a page.
#[must_use]
pub fn page_controls(page: SettingsPage) -> Vec<Control> {
    match page {
        SettingsPage::Appearance => appearance_controls(),
        SettingsPage::Colors => colors_controls(),
        SettingsPage::Ai => ai_controls(),
        SettingsPage::Terminal => terminal_controls(),
        SettingsPage::Keybindings => keybinding_controls(),
        SettingsPage::Workspaces => workspace_controls(),
        SettingsPage::Updates => update_controls(),
        SettingsPage::Releases => release_controls(),
        SettingsPage::Notifications => notification_controls(),
        SettingsPage::Remote => remote_controls(),
    }
}

fn appearance_controls() -> Vec<Control> {
    vec![
        text("appearance.font_family", "Font family"),
        stepper("appearance.font_size", "Font size", (6.0, 48.0, 1.0, 0)),
        stepper("appearance.font_weight", "Font weight", (100.0, 900.0, 100.0, 0)),
        stepper("appearance.bold_weight", "Bold weight", (100.0, 900.0, 100.0, 0)),
        toggle("appearance.ligatures", "Ligatures"),
        stepper("appearance.line_padding", "Line padding", (0.0, 20.0, 1.0, 0)),
        choice(
            "appearance.cursor_shape",
            "Cursor shape",
            vec![("block", "Block"), ("beam", "Beam"), ("underline", "Underline")],
        ),
        toggle("appearance.cursor_blink", "Cursor blink"),
        stepper("appearance.opacity", "Opacity", (0.0, 1.0, 0.05, 2)),
        stepper("appearance.scrollbar_width", "Scrollbar width", (2.0, 20.0, 1.0, 0)),
        stepper("appearance.tab_bar_padding", "Tab bar padding", (0.0, 20.0, 1.0, 0)),
        stepper("appearance.tab_width", "Tab width", (8.0, 50.0, 1.0, 0)),
        stepper("appearance.status_bar_height", "Status bar height", (8.0, 48.0, 1.0, 0)),
        stepper("appearance.tab_height", "Tab height", (16.0, 60.0, 1.0, 0)),
        stepper("appearance.content_padding_top", "Content padding top", (0.0, 50.0, 1.0, 0)),
        stepper("appearance.content_padding_right", "Content padding right", (0.0, 50.0, 1.0, 0)),
        stepper("appearance.content_padding_bottom", "Content padding bottom", (0.0, 50.0, 1.0, 0)),
        stepper("appearance.content_padding_left", "Content padding left", (0.0, 50.0, 1.0, 0)),
        stepper("appearance.focus_border_width", "Focus border width", (1.0, 10.0, 1.0, 0)),
        color("appearance.focus_border_color", "Focus border color"),
    ]
}

fn colors_controls() -> Vec<Control> {
    let mut controls = vec![
        choice(
            "theme.preset",
            "Preset",
            vec![
                ("minimal-dark", "Minimal Dark"),
                ("minimal-light", "Minimal Light"),
                ("custom", "Custom"),
            ],
        ),
        color("theme.foreground", "Foreground"),
        color("theme.background", "Background"),
        color("theme.cursor", "Cursor"),
        color("theme.cursor_text", "Cursor text"),
        color("theme.selection", "Selection"),
        color("theme.selection_text", "Selection text"),
    ];
    for i in 0..8 {
        controls.push(color(&format!("theme.ansi_normal.{i}"), &format!("ANSI normal {i}")));
    }
    for i in 0..8 {
        controls.push(color(&format!("theme.ansi_bright.{i}"), &format!("ANSI bright {i}")));
    }
    // Prompt-bar color overrides (Colors page tail in the old surface).
    controls.push(color("appearance.prompt_bar_first_row_bg", "Prompt bar first row"));
    controls.push(color("appearance.prompt_bar_second_row_bg", "Prompt bar second row"));
    controls.push(color("appearance.prompt_bar_text", "Prompt bar text"));
    controls.push(color("appearance.prompt_bar_icon_first", "Prompt bar first icon"));
    controls.push(color("appearance.prompt_bar_icon_latest", "Prompt bar latest icon"));
    controls
}

fn ai_controls() -> Vec<Control> {
    let mut controls = vec![
        toggle("terminal.prompt_bar", "Prompt bar"),
        stepper("terminal.prompt_bar_font_size", "Prompt bar font size", (8.0, 32.0, 1.0, 0)),
        choice(
            "terminal.prompt_bar_position",
            "Prompt bar position",
            vec![("top", "Top"), ("bottom", "Bottom")],
        ),
        toggle("terminal.preserve_ai_scrollback", "Preserve AI scrollback"),
        stepper("terminal.indicator_height", "Indicator height", (1.0, 10.0, 1.0, 0)),
        toggle("terminal.claude_code_integration", "Claude Code integration"),
        toggle("terminal.codex_code_integration", "Codex integration"),
        choice(
            "terminal.ai_tab_cwd",
            "AI tab working dir",
            vec![("pane", "Active pane"), ("project_root", "Project root")],
        ),
    ];
    // The AI assistant states table: per-state indicator config.
    for (state, label) in ai_states() {
        controls.push(toggle(
            &format!("ai_states.{state}.tab_indicator"),
            &format!("{label}: tab indicator"),
        ));
        controls.push(toggle(
            &format!("ai_states.{state}.pane_border"),
            &format!("{label}: pane border"),
        ));
        controls.push(color(&format!("ai_states.{state}.color"), &format!("{label}: color")));
        controls.push(stepper(
            &format!("ai_states.{state}.pulse_ms"),
            &format!("{label}: pulse (ms)"),
            (0.0, 5000.0, 50.0, 0),
        ));
        controls.push(stepper(
            &format!("ai_states.{state}.timeout_secs"),
            &format!("{label}: timeout (s)"),
            (0.0, 120.0, 1.0, 1),
        ));
    }
    controls
}

fn terminal_controls() -> Vec<Control> {
    vec![
        stepper("terminal.scrollback_lines", "Scrollback lines", (0.0, 1_000_000.0, 1000.0, 0)),
        toggle("terminal.copy_on_select", "Copy on select"),
        toggle("terminal.claude_copy_cleanup", "AI copy cleanup"),
        toggle("terminal.natural_scroll", "Natural scroll"),
        toggle("terminal.keyboard_protocol_enhanced", "Enhanced keyboard protocol"),
        toggle("terminal.paste_confirmation", "Paste confirmation"),
        toggle("terminal.env_persistence.enabled", "Persist environment"),
        choice(
            "terminal.clipboard.read_mode",
            "Clipboard read (OSC 52)",
            vec![("deny", "Deny"), ("prompt", "Prompt"), ("allow", "Allow")],
        ),
        choice(
            "terminal.clipboard.write_mode",
            "Clipboard write (OSC 52)",
            vec![("deny", "Deny"), ("prompt", "Prompt"), ("allow", "Allow")],
        ),
        stepper(
            "terminal.clipboard.max_write_bytes",
            "Clipboard max write bytes",
            (0.0, 536_870_912.0, 1_048_576.0, 0),
        ),
        toggle("terminal.clipboard.focus_gate_writes", "Focus-gate clipboard writes"),
        toggle("terminal.status_bar_stats.cpu", "Status bar: CPU"),
        toggle("terminal.status_bar_stats.memory", "Status bar: memory"),
        toggle("terminal.status_bar_stats.gpu", "Status bar: GPU"),
        toggle("terminal.status_bar_stats.network", "Status bar: network"),
        action("terminal.smart_selection.reset", "Reset smart selection rules"),
    ]
}

fn keybinding_controls() -> Vec<Control> {
    keybinding_actions()
        .into_iter()
        .map(|a| Control::new(a, &a.replace('_', " "), ControlKind::Keybinding))
        .collect()
}

fn workspace_controls() -> Vec<Control> {
    vec![
        action("workspaces.add_root", "Add workspace root"),
        action("workspaces.reset_badge_colors", "Reset badge colors"),
    ]
}

fn update_controls() -> Vec<Control> {
    vec![
        toggle("update.enabled", "Automatic updates"),
        stepper("update.check_interval_hours", "Check interval (hours)", (1.0, 168.0, 1.0, 0)),
        choice("update.channel", "Channel", vec![("stable", "Stable"), ("beta", "Beta")]),
    ]
}

fn release_controls() -> Vec<Control> {
    vec![
        action("action.check_for_updates", "Check for updates"),
        action("action.list_releases", "List releases"),
    ]
}

fn notification_controls() -> Vec<Control> {
    vec![
        toggle("notifications.enabled", "Notifications"),
        choice(
            "notifications.condition",
            "Condition",
            vec![
                ("when_unfocused", "When unfocused"),
                ("when_unfocused_or_background_tab", "Unfocused or background tab"),
                ("always", "Always"),
            ],
        ),
        choice(
            "notifications.timeout_mode",
            "Timeout mode",
            vec![("system_default", "System default"), ("custom", "Custom"), ("never", "Never")],
        ),
        stepper("notifications.timeout_secs", "Timeout (seconds)", (0.0, 120.0, 1.0, 0)),
    ]
}

fn remote_controls() -> Vec<Control> {
    vec![
        toggle("remote.enabled", "Tailnet remote control"),
        stepper("remote.port", "Tailnet port", (1024.0, 65535.0, 1.0, 0)),
        toggle("remote.lan.enabled", "LAN remote control"),
        stepper("remote.lan.port", "LAN port", (1024.0, 65535.0, 1.0, 0)),
        choice(
            "remote.sharing_mode",
            "Sharing mode",
            vec![
                ("single_controller", "Single controller"),
                ("shared_single_typist", "Shared single typist"),
                ("free_for_all", "Free for all"),
            ],
        ),
        choice(
            "remote.control_acquisition",
            "Control acquisition",
            vec![("free_claim", "Free claim"), ("request_and_grant", "Request and grant")],
        ),
        stepper(
            "remote.participant_limit",
            "Participant limit (0 = unlimited)",
            (0.0, 64.0, 1.0, 0),
        ),
    ]
}
