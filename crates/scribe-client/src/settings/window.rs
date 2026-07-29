//! The GPUI settings window view.
//!
//! Renders the eleven-page [`crate::settings::model`] onto a GPUI view: a sidebar
//! nav plus a scrollable content pane whose controls read their current value
//! from the loaded [`ScribeConfig`] via [`crate::settings::values`] and write
//! edits back through the ported [`crate::settings::apply::apply_settings_change`]
//! path. Interactive controls (toggle, choice-cycle, numeric stepper) commit
//! immediately, mirroring the old webview's live-apply behaviour; the file
//! watcher in the running client picks the change up as a `ConfigReloaded`.
//!
//! Color and free-text controls render their current value read-only — inline
//! hex/text entry is a tracked follow-on — while keybinding rows list every
//! action's current combos so the full shortcut inventory stays visible.
//!
//! Two pages additionally talk to the local server through
//! [`crate::settings::server_action`]: `Environment` gates its env-persistence
//! toggle on an `EnvPreflight` probe, and `Remote` renders the feature-014 LAN
//! trust surface (`GetLanEnv`, `ListTrustedNetworks`, `ListTrustedDevices`) with
//! `AddCurrentNetworkTrusted` / `RemoveTrustedNetwork` / `RevokeTrustedDevice`
//! mutations, plus the feature-013 tailnet summary (`GetRemoteEnv`). Every one
//! of those calls is reached from [`SettingsWindow::run_action`].

use std::time::Duration;

use gpui::{
    AccessibleAction, App, Bounds, Context, FocusHandle, KeyDownEvent, Rgba, Role, Text,
    TitlebarOptions, Toggled, Window, WindowBounds, WindowHandle, WindowOptions, div, prelude::*,
    px, size,
};
use scribe_common::config::{ScribeConfig, load_config, resolve_theme};
use scribe_common::protocol::{PreflightError, TrustedDeviceInfo, TrustedNetworkInfo};
use serde_json::{Value, json};

use crate::settings::model::{
    ADD_CURRENT_NETWORK_ACTION, Control, ControlKind, ENV_PERSISTENCE_KEY, ENV_PREFLIGHT_ACTION,
    REFRESH_TRUST_ACTION, REMOVE_TRUSTED_NETWORK_PREFIX, REVOKE_TRUSTED_DEVICE_PREFIX,
    SettingsPage, page_controls,
};
use crate::settings::server_action::{self, EnvPreflightOutcome, LanEnvOutcome, RemoteEnvOutcome};
use crate::settings::values::{current_value, keybinding_combos};
use crate::tab_bar::srgba;

/// One-shot server-action timeout for the releases page (update check / release
/// list). Short so a click never hangs the UI thread if the server is down.
const SERVER_ACTION_TIMEOUT: Duration = Duration::from_secs(3);

/// The feature-014 LAN trust state the Remote page renders, refreshed from the
/// local server by [`SettingsWindow::refresh_trust`].
///
/// `loaded` distinguishes "never queried" from "queried and genuinely empty" so
/// the page can say which; every field otherwise carries the fail-closed default
/// the transport helpers produce when the server is unreachable.
#[derive(Default)]
struct TrustState {
    /// Whether a refresh has completed at least once in this window.
    loaded: bool,
    /// This machine's own LAN identity plus whether the current network is
    /// addable, from `GetLanEnv`.
    lan: LanEnvOutcome,
    /// This machine's signed-in tailnet account and whether Tailscale was
    /// detected at all, from `GetRemoteEnv` (feature 013, UX-003 / FR-015).
    remote: RemoteEnvOutcome,
    /// Trusted networks from `ListTrustedNetworks`.
    networks: Vec<TrustedNetworkInfo>,
    /// Whether the network this machine is on right now is trusted (UX-004).
    current_trusted: bool,
    /// Approved LAN devices from `ListTrustedDevices`.
    devices: Vec<TrustedDeviceInfo>,
}

/// One mutable trusted-network / approved-device row, bundled so the renderer
/// keeps a small signature.
struct TrustRow {
    /// The row's descriptive text (label plus the identifying detail).
    label: String,
    /// The mutation button's caption ("Remove" / "Revoke").
    button: &'static str,
    /// Stable per-row GPUI element id seed.
    id: (&'static str, usize),
    /// The [`SettingsWindow::run_action`] key, with the record key appended after
    /// its prefix.
    action_key: String,
}

/// Plain-language rendering of a [`PreflightError`], reused by the toggle gate
/// and the manual probe action so both surfaces say the same thing.
fn preflight_reason(error: &PreflightError) -> String {
    match error {
        PreflightError::KeychainLocked => "the login keychain is locked".to_owned(),
        PreflightError::SecretServiceUnavailable => {
            "the Secret Service / D-Bus session bus is unavailable".to_owned()
        }
        PreflightError::KeystoreAccessDenied => "keystore access was denied".to_owned(),
        PreflightError::Unknown { reason } => reason.clone(),
    }
}

/// Resolved GPUI colours for the settings chrome, derived from the active theme.
#[derive(Clone, Copy)]
struct SettingsColors {
    page_bg: Rgba,
    nav_bg: Rgba,
    nav_active_bg: Rgba,
    control_bg: Rgba,
    border: Rgba,
    accent: Rgba,
    text: Rgba,
    dim_text: Rgba,
}

impl SettingsColors {
    fn resolve(config: &ScribeConfig) -> Self {
        let theme = resolve_theme(config);
        let chrome = theme.chrome;
        let text = srgba(chrome.tab_text_active);
        Self {
            page_bg: srgba(theme.background),
            nav_bg: srgba(chrome.tab_bar_bg),
            nav_active_bg: srgba(chrome.tab_bar_active_bg),
            control_bg: srgba(chrome.tab_bar_active_bg),
            border: srgba(chrome.divider),
            accent: srgba(chrome.accent),
            text,
            dim_text: Rgba { a: text.a * 0.65, ..text },
        }
    }
}

/// The settings window view: a page selector plus the live-editing content pane.
pub struct SettingsWindow {
    config: ScribeConfig,
    colors: SettingsColors,
    page: SettingsPage,
    /// Last action/error line shown under the content (server-action results,
    /// apply failures, and follow-on notices).
    status: Option<String>,
    /// LAN trust state rendered by the Remote page.
    trust: TrustState,
    focus_handle: FocusHandle,
    /// Keyboard traversal is deliberately window-local: Settings claims only
    /// keys while its own window is focused, never terminal-window shortcuts.
    focus_index: usize,
    keyboard_navigation: bool,
}

/// One focus stop in the settings window's stable keyboard traversal order.
/// The sidebar comes first, followed by actionable controls on the selected
/// page and (on Remote) every live trust mutation row.
// @lat: [[settings#GPUI Settings Window#Page model]]
#[derive(Clone)]
enum SettingsFocusTarget {
    Page(SettingsPage),
    Control(Control),
    Action(String),
}

impl SettingsWindow {
    /// Build the view, loading the current config (or defaults on failure).
    pub fn new(cx: &mut Context<Self>) -> Self {
        let config = load_config().unwrap_or_default();
        let colors = SettingsColors::resolve(&config);
        Self {
            config,
            colors,
            page: SettingsPage::Appearance,
            status: None,
            trust: TrustState::default(),
            focus_handle: cx.focus_handle(),
            focus_index: 0,
            keyboard_navigation: false,
        }
    }

    /// Reload the config from disk after an edit so the UI reflects the saved
    /// state (including any clamping the apply path performed).
    fn reload(&mut self) {
        if let Ok(config) = load_config() {
            self.colors = SettingsColors::resolve(&config);
            self.config = config;
        }
    }

    /// Route a `{key, value}` edit through the ported apply path, then reload.
    fn commit(&mut self, key: &str, value: Value, cx: &mut Context<Self>) {
        let mut obj = serde_json::Map::new();
        obj.insert("key".to_owned(), Value::String(key.to_owned()));
        obj.insert("value".to_owned(), value);
        let payload = Value::Object(obj).to_string();
        match crate::settings::apply::apply_settings_change(&payload) {
            Ok(()) => {
                self.reload();
                self.status = None;
            }
            Err(e) => self.status = Some(format!("{key}: {e}")),
        }
        cx.notify();
    }

    fn select_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        self.page = page;
        self.status = None;
        // Opening the Remote page pulls the LAN trust surface in the same way the
        // old webview's `inject_lan_state` did on load, so the lists are populated
        // before the user reaches for Remove/Revoke. Only the first visit auto-
        // refreshes; afterwards the explicit "Refresh trust state" action drives it.
        if page == SettingsPage::Remote && !self.trust.loaded {
            self.run_action(REFRESH_TRUST_ACTION, cx);
            return;
        }
        cx.notify();
    }

    fn focus_targets(&self) -> Vec<SettingsFocusTarget> {
        let mut targets =
            SettingsPage::all().into_iter().map(SettingsFocusTarget::Page).collect::<Vec<_>>();
        if self.page == SettingsPage::Remote {
            targets.push(SettingsFocusTarget::Action(REFRESH_TRUST_ACTION.to_owned()));
            targets.push(SettingsFocusTarget::Action(ADD_CURRENT_NETWORK_ACTION.to_owned()));
            targets.extend(self.trust.networks.iter().map(|network| {
                SettingsFocusTarget::Action(format!(
                    "{REMOVE_TRUSTED_NETWORK_PREFIX}{}",
                    network.id
                ))
            }));
            targets.extend(self.trust.devices.iter().map(|device| {
                SettingsFocusTarget::Action(format!(
                    "{REVOKE_TRUSTED_DEVICE_PREFIX}{}",
                    device.device_id_hex
                ))
            }));
        }
        targets.extend(page_controls(self.page).into_iter().filter_map(
            |control| match control.kind {
                ControlKind::Toggle
                | ControlKind::Choice(_)
                | ControlKind::Stepper { .. }
                | ControlKind::Action => Some(SettingsFocusTarget::Control(control)),
                ControlKind::Color | ControlKind::Text | ControlKind::Keybinding => None,
            },
        ));
        targets
    }

    fn focused_target(&self) -> Option<SettingsFocusTarget> {
        let targets = self.focus_targets();
        targets.get(self.focus_index % targets.len().max(1)).cloned()
    }

    fn target_is_focused(&self, target: &SettingsFocusTarget) -> bool {
        self.keyboard_navigation
            && self.focused_target().is_some_and(|current| match (current, target) {
                (SettingsFocusTarget::Page(a), SettingsFocusTarget::Page(b)) => a == *b,
                (SettingsFocusTarget::Control(a), SettingsFocusTarget::Control(b)) => {
                    a.key == b.key
                }
                (SettingsFocusTarget::Action(a), SettingsFocusTarget::Action(b)) => a == *b,
                _ => false,
            })
    }

    fn move_focus(&mut self, direction: isize, cx: &mut Context<Self>) {
        let count = self.focus_targets().len();
        if count > 0 {
            self.focus_index = if direction.is_negative() {
                (self.focus_index + count - 1) % count
            } else {
                (self.focus_index + 1) % count
            };
            self.keyboard_navigation = true;
            cx.notify();
        }
    }

    fn activate_target(&mut self, target: SettingsFocusTarget, cx: &mut Context<Self>) {
        match target {
            SettingsFocusTarget::Page(page) => self.select_page(page, cx),
            SettingsFocusTarget::Control(control) => match control.kind {
                ControlKind::Toggle => self.toggle(&control.key, cx),
                ControlKind::Choice(options) => self.cycle(&control.key, &options, cx),
                ControlKind::Stepper { min, max, step, .. } => {
                    self.step(&control.key, (min, max), step, cx);
                }
                ControlKind::Action => self.run_action(&control.key, cx),
                ControlKind::Color | ControlKind::Text | ControlKind::Keybinding => {}
            },
            SettingsFocusTarget::Action(key) => self.run_action(&key, cx),
        }
    }

    fn adjust_target(&mut self, direction: f64, cx: &mut Context<Self>) -> bool {
        let Some(SettingsFocusTarget::Control(control)) = self.focused_target() else {
            return false;
        };
        match control.kind {
            ControlKind::Choice(options) => {
                if direction > 0.0 {
                    self.cycle(&control.key, &options, cx);
                } else {
                    self.cycle_previous(&control.key, &options, cx);
                }
                true
            }
            ControlKind::Stepper { min, max, step, .. } => {
                self.step(&control.key, (min, max), step * direction, cx);
                true
            }
            ControlKind::Toggle if direction != 0.0 => {
                self.toggle(&control.key, cx);
                true
            }
            _ => false,
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        let handled = match event.keystroke.key.as_str() {
            "tab" | "down" => {
                self.move_focus(1, cx);
                true
            }
            "up" => {
                self.move_focus(-1, cx);
                true
            }
            "left" => self.adjust_target(-1.0, cx),
            "right" => self.adjust_target(1.0, cx),
            "enter" | "space" => self.focused_target().is_some_and(|target| {
                self.activate_target(target, cx);
                true
            }),
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        let current = current_value(&self.config, key).as_bool().unwrap_or(false);
        let next = !current;
        // Turning env persistence ON is gated on the server's OS-keystore probe:
        // committing a setting the keystore cannot back would silently degrade at
        // runtime, so a failing probe refuses the edit and surfaces the reason.
        if next && key == ENV_PERSISTENCE_KEY {
            self.enable_env_persistence(key, cx);
            return;
        }
        self.commit(key, Value::Bool(next), cx);
    }

    /// The gated ON transition of [`ENV_PERSISTENCE_KEY`]: probe the server's OS
    /// keystore first and commit only if it answers `ok`. A failing probe leaves
    /// the config untouched and reports the actionable reason.
    fn enable_env_persistence(&mut self, key: &str, cx: &mut Context<Self>) {
        match server_action::request_env_preflight(SERVER_ACTION_TIMEOUT) {
            EnvPreflightOutcome::Ok => {
                self.commit(key, Value::Bool(true), cx);
                let ok = self.status.is_none();
                if ok {
                    self.status =
                        Some("Keystore preflight passed; environment persistence is on.".into());
                }
            }
            EnvPreflightOutcome::Err(error) => {
                self.status = Some(format!(
                    "Environment persistence stays off — {}",
                    preflight_reason(&error)
                ));
            }
        }
        cx.notify();
    }

    fn cycle(
        &mut self,
        key: &str,
        options: &[(&'static str, &'static str)],
        cx: &mut Context<Self>,
    ) {
        if options.is_empty() {
            return;
        }
        let current = current_value(&self.config, key);
        let current_token = current.as_str().unwrap_or("");
        let idx = options.iter().position(|(v, _)| *v == current_token).unwrap_or(0);
        let Some((next, _)) = options.get((idx + 1) % options.len()) else {
            return;
        };
        self.commit(key, Value::String((*next).to_owned()), cx);
    }

    fn cycle_previous(
        &mut self,
        key: &str,
        options: &[(&'static str, &'static str)],
        cx: &mut Context<Self>,
    ) {
        if options.is_empty() {
            return;
        }
        let current = current_value(&self.config, key);
        let current_token = current.as_str().unwrap_or("");
        let index = options.iter().position(|(value, _)| *value == current_token).unwrap_or(0);
        let previous = (index + options.len() - 1) % options.len();
        if let Some((value, _)) = options.get(previous) {
            self.commit(key, Value::String((*value).to_owned()), cx);
        }
    }

    fn step(&mut self, key: &str, bounds: (f64, f64), delta: f64, cx: &mut Context<Self>) {
        let (min, max) = bounds;
        let current = current_value(&self.config, key).as_f64().unwrap_or(min);
        let next = (current + delta).clamp(min, max);
        // Preserve integer shape for whole-number steppers so serde deserializes
        // into the u16/u32/u64 fields the apply path expects. The integer branch
        // formats to a string and reparses to sidestep a lossy `f64 as i64` cast.
        let value = if next.fract() == 0.0 {
            json!(format!("{next:.0}").parse::<i64>().unwrap_or_default())
        } else {
            json!((next * 100.0).round() / 100.0)
        };
        self.commit(key, value, cx);
    }

    /// Re-read the whole Remote page's runtime surface from the local server:
    /// the feature-014 LAN identity/addability (`GetLanEnv`), the trusted
    /// networks plus current-network trust flag (`ListTrustedNetworks`), the
    /// approved devices (`ListTrustedDevices`), and the feature-013 tailnet
    /// environment (`GetRemoteEnv`). Every helper folds its own failures into a
    /// fail-closed default, so this always leaves one renderable shape behind.
    fn refresh_trust(&mut self) {
        let lan = server_action::request_lan_env(SERVER_ACTION_TIMEOUT);
        let networks = server_action::request_trusted_networks(SERVER_ACTION_TIMEOUT);
        let devices = server_action::request_trusted_devices(SERVER_ACTION_TIMEOUT);
        let remote = server_action::request_remote_env(SERVER_ACTION_TIMEOUT);
        self.trust = TrustState {
            loaded: true,
            lan,
            remote,
            networks: networks.networks,
            current_trusted: networks.current_trusted,
            devices,
        };
    }

    /// One-line summary of the trust state for the status footer.
    fn trust_summary(&self) -> String {
        format!(
            "Trust state: {} trusted network(s), {} approved device(s); this network is {}.",
            self.trust.networks.len(),
            self.trust.devices.len(),
            if self.trust.current_trusted { "trusted" } else { "not trusted" }
        )
    }

    fn run_action(&mut self, key: &str, cx: &mut Context<Self>) {
        // Per-row trust mutations carry their record key in the action id, so they
        // are matched by prefix before the fixed action table.
        if let Some(id) = key.strip_prefix(REMOVE_TRUSTED_NETWORK_PREFIX) {
            self.status = match server_action::request_remove_trusted_network(
                id.to_owned(),
                SERVER_ACTION_TIMEOUT,
            ) {
                Ok(()) => {
                    self.refresh_trust();
                    Some(format!("Removed trusted network {id}. {}", self.trust_summary()))
                }
                Err(e) => Some(format!("Remove trusted network failed: {e}")),
            };
            cx.notify();
            return;
        }
        if let Some(device_id) = key.strip_prefix(REVOKE_TRUSTED_DEVICE_PREFIX) {
            self.status = match server_action::request_revoke_trusted_device(
                device_id.to_owned(),
                SERVER_ACTION_TIMEOUT,
            ) {
                Ok(()) => {
                    self.refresh_trust();
                    Some(format!("Revoked device {device_id}. {}", self.trust_summary()))
                }
                Err(e) => Some(format!("Revoke trusted device failed: {e}")),
            };
            cx.notify();
            return;
        }
        match key {
            REFRESH_TRUST_ACTION => {
                self.refresh_trust();
                self.status = Some(self.trust_summary());
            }
            ADD_CURRENT_NETWORK_ACTION => {
                self.status =
                    match server_action::request_add_current_network(SERVER_ACTION_TIMEOUT) {
                        Ok(()) => {
                            self.refresh_trust();
                            Some(format!("Trusted the current network. {}", self.trust_summary()))
                        }
                        Err(e) => Some(format!("Trust current network failed: {e}")),
                    };
            }
            ENV_PREFLIGHT_ACTION => {
                self.status =
                    Some(match server_action::request_env_preflight(SERVER_ACTION_TIMEOUT) {
                        EnvPreflightOutcome::Ok => {
                            "Keystore preflight passed; environment persistence can be enabled."
                                .to_owned()
                        }
                        EnvPreflightOutcome::Err(error) => {
                            format!("Keystore preflight failed — {}", preflight_reason(&error))
                        }
                    });
            }
            "action.check_for_updates" => {
                let state = server_action::request_update_check(SERVER_ACTION_TIMEOUT);
                self.status = Some(format!("Update check: {state:?}"));
            }
            "action.list_releases" => {
                let state = server_action::request_release_list(SERVER_ACTION_TIMEOUT);
                self.status = Some(format!("Releases: {state:?}"));
            }
            "workspaces.reset_badge_colors" | "terminal.smart_selection.reset" => {
                self.commit(key, Value::Bool(true), cx);
                return;
            }
            "workspaces.add_root" => {
                self.status = Some(
                    "Adding a workspace root needs inline path entry (tracked follow-on)."
                        .to_owned(),
                );
            }
            _ => self.status = Some(format!("no handler for {key}")),
        }
        cx.notify();
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        let nav = self.render_nav(cx);
        let content = self.render_content(cx);
        div()
            .id("settings-root")
            .role(Role::Application)
            .aria_label("Scribe settings")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _key_window, ctx| {
                this.on_key_down(event, ctx);
            }))
            .size_full()
            .flex()
            .bg(colors.page_bg)
            .text_color(colors.text)
            .child(nav)
            .child(content)
    }
}

impl SettingsWindow {
    fn render_nav(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let active = self.page;
        let items = SettingsPage::all().into_iter().map(|page| {
            let selected = page == active;
            let focused = self.target_is_focused(&SettingsFocusTarget::Page(page));
            let bg = if selected { colors.nav_active_bg } else { colors.nav_bg };
            let fg = if selected { colors.text } else { colors.dim_text };
            div()
                .id(("settings-nav", page as usize))
                .focusable()
                .tab_stop(true)
                .role(Role::Tab)
                .aria_label(page.nav_label())
                .aria_selected(selected)
                .aria_position_in_set(page as usize + 1)
                .aria_size_of_set(SettingsPage::all().len())
                .w_full()
                .px_3()
                .py_2()
                .text_sm()
                .bg(bg)
                .text_color(fg)
                .when(focused, |el| el.border_l_2().border_color(colors.accent))
                .hover(move |s| s.bg(colors.nav_active_bg))
                .on_click(cx.listener(move |this, _, _win, ctx| this.select_page(page, ctx)))
                .child(page.nav_label())
                .into_any_element()
        });
        div()
            .id("settings-navigation")
            .role(Role::TabList)
            .aria_label("Settings sections")
            .w(px(200.0))
            // Without an explicit `flex_none`, a long content row (the LAN trust
            // notes are the worst case) raises the content pane's automatic
            // minimum width and squeezes the sidebar instead.
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(colors.nav_bg)
            .border_r_1()
            .border_color(colors.border)
            .children(items)
            .into_any_element()
    }

    fn render_content(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let controls = page_controls(self.page);
        let rows = controls.into_iter().map(|c| self.render_control(&c, cx)).collect::<Vec<_>>();

        let mut children: Vec<gpui::AnyElement> = Vec::new();
        children.push(
            div()
                .id("settings-page-heading")
                .role(Role::Heading)
                .aria_level(1)
                .aria_label(self.page.nav_label())
                .px_4()
                .py_3()
                .text_lg()
                .text_color(colors.text)
                .child(self.page.nav_label())
                .into_any_element(),
        );
        // The Remote page leads with the runtime "Local network" trust surface so
        // its lists and their Remove/Revoke buttons sit above the fold; the static
        // config controls follow underneath.
        if self.page == SettingsPage::Remote {
            children.extend(self.render_trust_sections(cx));
        }
        children.extend(rows);
        if let Some(status) = &self.status {
            children.push(
                div()
                    .id("settings-status")
                    .role(Role::Status)
                    .aria_label(status.clone())
                    .px_4()
                    .py_2()
                    .mt_2()
                    .text_sm()
                    .text_color(colors.accent)
                    .child(Text::new_inaccessible(status.clone().into()))
                    .into_any_element(),
            );
        }

        div()
            .id("settings-content")
            .role(Role::TabPanel)
            .aria_label(self.page.nav_label())
            .flex_1()
            // A flex item's automatic minimum size is its min-content width, which
            // for a row of unwrapped text is the whole string. Pinning `min_w` to
            // zero lets the pane take exactly the space the sidebar leaves, so a
            // long trust note can never push the right-aligned controls off-window.
            .min_w(px(0.0))
            .h_full()
            .overflow_x_hidden()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .bg(colors.page_bg)
            .children(children)
            .into_any_element()
    }

    /// The feature-014 "Local network" trust surface appended under the Remote
    /// page's config controls: this machine's own fingerprint, the current
    /// network's trust status, the trusted-network list with per-row Remove, and
    /// the approved-device list with per-row Revoke.
    ///
    /// These rows are runtime data (server replies), not config keys, so they are
    /// rendered here rather than described in
    /// [`crate::settings::model::page_controls`].
    fn render_trust_sections(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        // The row order is fixed — heading, two action rows, three status notes,
        // then each list — so the scripted E2E can address every control by a
        // stable window-relative offset.
        let mut out = vec![
            self.section_heading("Local network"),
            self.trust_row(
                TrustRow {
                    label: "Trust state".to_owned(),
                    button: "Refresh",
                    id: ("trust-action", 0),
                    action_key: REFRESH_TRUST_ACTION.to_owned(),
                },
                cx,
            ),
            self.trust_row(
                TrustRow {
                    label: "This network".to_owned(),
                    button: "Trust it",
                    id: ("trust-action", 1),
                    action_key: ADD_CURRENT_NETWORK_ACTION.to_owned(),
                },
                cx,
            ),
        ];

        if !self.trust.loaded {
            out.push(self.note_row("Trust state not loaded — use Refresh."));
            return out;
        }

        out.extend(self.trust_status_notes());
        out.push(self.section_heading("Trusted networks"));
        out.extend(self.trusted_network_rows(cx));
        out.push(self.section_heading("Approved devices"));
        out.extend(self.trusted_device_rows(cx));
        // The feature-013 tailnet summary is APPENDED, never interleaved: the
        // rows above are addressed by stable window-relative offsets in
        // `tests/e2e/visual/settings-trust.sh`, so anything new has to land past
        // the last of them.
        out.push(self.section_heading("Tailscale"));
        out.push(self.note_row(&self.tailnet_note()));
        out
    }

    /// The read-only tailnet line under the Remote page (UX-003, FR-015).
    ///
    /// `GetRemoteEnv` fails closed to `{ account: None, tailscale_detected:
    /// false }` on any transport error, which is exactly the shape that drives
    /// the passive "not detected" copy — so an unreachable server and an absent
    /// `tailscaled` say the same true thing rather than showing a spinner.
    fn tailnet_note(&self) -> String {
        let remote = &self.trust.remote;
        if !remote.tailscale_detected {
            return "Tailscale not detected — remote control over the tailnet is unavailable."
                .to_owned();
        }
        remote.account.as_ref().map_or_else(
            || "Tailscale detected; the signed-in account is unknown.".to_owned(),
            |account| format!("Signed in to Tailscale as {account}."),
        )
    }

    /// The three read-only status lines under the trust actions: whether the
    /// current network is trusted (UX-004), this machine's own fingerprint (the
    /// out-of-band MITM check, FR-006), and whether the current network can be
    /// fingerprinted at all.
    fn trust_status_notes(&self) -> Vec<gpui::AnyElement> {
        let lan = &self.trust.lan;
        vec![
            self.note_row(&format!(
                "This network is {}",
                if self.trust.current_trusted { "trusted" } else { "not trusted" }
            )),
            self.note_row(&format!(
                "This device: {}",
                lan.fingerprint_words
                    .clone()
                    .or_else(|| lan.device_id_hex.clone())
                    .unwrap_or_else(|| "no LAN identity yet".to_owned())
            )),
            self.note_row(&if lan.current_network_addable {
                "This network can be fingerprinted and trusted.".to_owned()
            } else {
                format!(
                    "This network cannot be trusted — {}",
                    lan.current_network_reason
                        .clone()
                        .unwrap_or_else(|| "the server could not fingerprint it".to_owned())
                )
            }),
        ]
    }

    /// One row per `TrustedNetworkList` entry, each with a `RemoveTrustedNetwork`
    /// button keyed by the record id.
    fn trusted_network_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if self.trust.networks.is_empty() {
            return vec![self.note_row("No trusted networks yet.")];
        }
        self.trust
            .networks
            .iter()
            .enumerate()
            .map(|(index, network)| {
                self.trust_row(
                    TrustRow {
                        label: format!(
                            "{} — {} ({})",
                            network.label,
                            network.gateway_mac,
                            network.ssid.clone().unwrap_or_else(|| network.subnet_cidr.clone())
                        ),
                        button: "Remove",
                        id: ("remove-network", index),
                        action_key: format!("{REMOVE_TRUSTED_NETWORK_PREFIX}{}", network.id),
                    },
                    cx,
                )
            })
            .collect()
    }

    /// One row per `TrustedDeviceList` entry, each with a `RevokeTrustedDevice`
    /// button keyed by the device's hex id.
    fn trusted_device_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if self.trust.devices.is_empty() {
            return vec![self.note_row("No approved devices.")];
        }
        self.trust
            .devices
            .iter()
            .enumerate()
            .map(|(index, device)| {
                self.trust_row(
                    TrustRow {
                        label: format!("{} — {}", device.label, device.fingerprint_words),
                        button: "Revoke",
                        id: ("revoke-device", index),
                        action_key: format!(
                            "{REVOKE_TRUSTED_DEVICE_PREFIX}{}",
                            device.device_id_hex
                        ),
                    },
                    cx,
                )
            })
            .collect()
    }

    /// A bold-ish sub-heading between the trust lists.
    fn section_heading(&self, text: &str) -> gpui::AnyElement {
        div()
            .id(("settings-section-heading", key_hash(text)))
            .role(Role::Heading)
            .aria_level(2)
            .aria_label(text.to_owned())
            .w_full()
            .px_4()
            .pt_4()
            .pb_1()
            .text_sm()
            .text_color(self.colors.accent)
            .child(Text::new_inaccessible(text.to_owned().into()))
            .into_any_element()
    }

    /// A read-only informational line inside a trust section.
    /// The text is elided in Rust rather than with `text_ellipsis`, which
    /// collapses the row's line box and would make the section's row heights —
    /// and therefore the scripted E2E's click offsets — depend on the string.
    fn note_row(&self, text: &str) -> gpui::AnyElement {
        div()
            .id(("settings-note", key_hash(text)))
            .role(Role::Note)
            .aria_label(text.to_owned())
            .w_full()
            .px_4()
            .py_2()
            .text_sm()
            .text_color(self.colors.dim_text)
            .border_b_1()
            .border_color(self.colors.border)
            .child(Text::new_inaccessible(elide(text, NOTE_MAX_CHARS).into()))
            .into_any_element()
    }

    /// One trusted-network / approved-device row: its description plus a mutation
    /// pill that routes back through [`SettingsWindow::run_action`] with the
    /// record key embedded in the action id.
    fn trust_row(&self, row: TrustRow, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let TrustRow { label, button, id, action_key } = row;
        let focused = self.target_is_focused(&SettingsFocusTarget::Action(action_key.clone()));
        let control = pill(button, colors.accent, colors.text)
            .id(id)
            .when(focused, |el| el.border_1().border_color(colors.text))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(format!("{button} {label}"))
            .on_click(cx.listener(move |this, _, _win, ctx| this.run_action(&action_key, ctx)));
        div()
            .id(("settings-trust-row", key_hash(&label)))
            .role(Role::Group)
            .aria_label(label.clone())
            .w_full()
            .px_4()
            .py_2()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(colors.border)
            .child(row_label(&label, colors.text))
            .child(control)
            .into_any_element()
    }

    fn render_control(&self, control: &Control, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let label = row_label(&control.label, colors.text);
        let value_widget = self.render_value_widget(control, cx);
        div()
            .id(("settings-control", key_hash(&control.key)))
            .role(Role::Group)
            .aria_label(control.label.clone())
            .w_full()
            .px_4()
            .py_2()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(colors.border)
            .child(label)
            .child(value_widget)
            .into_any_element()
    }

    fn render_value_widget(&self, control: &Control, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let focused = self.target_is_focused(&SettingsFocusTarget::Control(control.clone()));
        match &control.kind {
            ControlKind::Toggle => {
                let on = current_value(&self.config, &control.key).as_bool().unwrap_or(false);
                let key = control.key.clone();
                let bg = if on { colors.accent } else { colors.control_bg };
                let text = if on { "On" } else { "Off" };
                pill(text, bg, colors.text)
                    .id(("toggle", key_hash(&control.key)))
                    .when(focused, |el| el.border_1().border_color(colors.text))
                    .focusable()
                    .tab_stop(true)
                    .role(Role::Switch)
                    .aria_label(control.label.clone())
                    .aria_toggled(if on { Toggled::True } else { Toggled::False })
                    .on_click(cx.listener(move |this, _, _win, ctx| this.toggle(&key, ctx)))
                    .into_any_element()
            }
            ControlKind::Choice(options) => {
                let value = current_value(&self.config, &control.key);
                let token = value.as_str().unwrap_or("");
                let display = options
                    .iter()
                    .find(|(v, _)| *v == token)
                    .map_or(token, |(_, label)| *label)
                    .to_owned();
                let key = control.key.clone();
                let options = options.clone();
                pill(&display, colors.control_bg, colors.text)
                    .id(("choice", key_hash(&control.key)))
                    .when(focused, |el| el.border_1().border_color(colors.accent))
                    .focusable()
                    .tab_stop(true)
                    .role(Role::Button)
                    .aria_label(control.label.clone())
                    .aria_value(display.clone())
                    .on_click(
                        cx.listener(move |this, _, _win, ctx| this.cycle(&key, &options, ctx)),
                    )
                    .into_any_element()
            }
            ControlKind::Stepper { min, max, step, decimals } => {
                self.render_stepper(control, (*min, *max, *step), *decimals, cx)
            }
            ControlKind::Color => self.render_color(control),
            ControlKind::Text => {
                let value = current_value(&self.config, &control.key);
                let shown = value.as_str().unwrap_or("").to_owned();
                read_only_value(&control.key, &control.label, shown, colors.dim_text)
            }
            ControlKind::Keybinding => {
                let combos = keybinding_combos(&self.config, &control.key);
                let shown = if combos.is_empty() { "—".to_owned() } else { combos.join(", ") };
                read_only_value(&control.key, &control.label, shown, colors.dim_text)
            }
            ControlKind::Action => {
                let key = control.key.clone();
                pill(&control.label, colors.accent, colors.text)
                    .id(("action", key_hash(&control.key)))
                    .when(focused, |el| el.border_1().border_color(colors.text))
                    .focusable()
                    .tab_stop(true)
                    .role(Role::Button)
                    .aria_label(control.label.clone())
                    .on_click(cx.listener(move |this, _, _win, ctx| this.run_action(&key, ctx)))
                    .into_any_element()
            }
        }
    }

    /// Render a numeric stepper: a `−`/`+` pair around the current value, each
    /// committing the clamped step through [`SettingsWindow::step`].
    fn render_stepper(
        &self,
        control: &Control,
        bounds: (f64, f64, f64),
        decimals: u8,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let focused = self.target_is_focused(&SettingsFocusTarget::Control(control.clone()));
        let (min, max, step) = bounds;
        let current = current_value(&self.config, &control.key).as_f64().unwrap_or(min);
        let display = format!("{current:.*}", decimals as usize);
        let key_dec = control.key.clone();
        let key_inc = control.key.clone();
        let key_a11y_dec = control.key.clone();
        let key_a11y_inc = control.key.clone();
        let minus = pill("−", colors.control_bg, colors.text)
            .id(("dec", key_hash(&control.key)))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(format!("Decrease {}", control.label))
            .on_click(cx.listener(move |this, _, _win, ctx| {
                this.step(&key_dec, (min, max), -step, ctx);
            }));
        let plus = pill("+", colors.control_bg, colors.text)
            .id(("inc", key_hash(&control.key)))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(format!("Increase {}", control.label))
            .on_click(cx.listener(move |this, _, _win, ctx| {
                this.step(&key_inc, (min, max), step, ctx);
            }));
        div()
            .id(("stepper", key_hash(&control.key)))
            .focusable()
            .tab_stop(true)
            .role(Role::SpinButton)
            .aria_label(control.label.clone())
            .aria_numeric_value(current)
            .aria_min_numeric_value(min)
            .aria_max_numeric_value(max)
            .aria_numeric_value_step(step)
            .on_a11y_action(
                AccessibleAction::Decrement,
                a11y_step_handler(cx.entity().downgrade(), key_a11y_dec, (min, max), -step),
            )
            .on_a11y_action(
                AccessibleAction::Increment,
                a11y_step_handler(cx.entity().downgrade(), key_a11y_inc, (min, max), step),
            )
            .flex()
            .items_center()
            .gap_2()
            .when(focused, |el| el.border_1().border_color(colors.accent))
            .child(minus)
            .child(
                div()
                    .id(("stepper-value", key_hash(&control.key)))
                    .role(Role::Label)
                    .aria_label(display.clone())
                    .min_w(px(56.0))
                    .text_sm()
                    .text_color(colors.text)
                    .child(Text::new_inaccessible(display.into())),
            )
            .child(plus)
            .into_any_element()
    }

    /// Render a color control: an optional swatch of the current hex plus the
    /// hex text (read-only; inline hex entry is a tracked follow-on).
    fn render_color(&self, control: &Control) -> gpui::AnyElement {
        let colors = self.colors;
        let value = current_value(&self.config, &control.key);
        let hex = value.as_str().unwrap_or("").to_owned();
        let shown = if hex.is_empty() { "(theme default)".to_owned() } else { hex.clone() };
        let mut row = div().flex().items_center().gap_2();
        if let Ok(rgba) = scribe_common::theme::hex_to_rgba(&hex) {
            row = row.child(
                div()
                    .w(px(16.0))
                    .h(px(16.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border)
                    .bg(srgba(rgba)),
            );
        }
        row.child(read_only_value(&control.key, &control.label, shown, colors.dim_text))
            .into_any_element()
    }
}

/// Character budget for a single unwrapped row of text in the content pane at
/// its 620px width. Longer strings are elided rather than clipped.
const NOTE_MAX_CHARS: usize = 76;

/// Shorten `text` to `max` characters, appending an ellipsis when it was cut.
fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The left-hand label of a settings row: it takes the leftover width but never
/// forces the row wider than the pane, so the right-aligned control stays on
/// screen no matter how long the text is.
fn row_label(text: &str, color: Rgba) -> gpui::Stateful<gpui::Div> {
    div()
        .id(("settings-label", key_hash(text)))
        .role(Role::Label)
        .aria_label(text.to_owned())
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .text_sm()
        .text_color(color)
        .child(Text::new_inaccessible(elide(text, NOTE_MAX_CHARS).into()))
}

/// A read-only current value still needs a semantic node: visual text alone is
/// otherwise absent from AccessKit's tree.
fn read_only_value(key: &str, label: &str, value: String, color: Rgba) -> gpui::AnyElement {
    div()
        .id(("settings-read-only-value", key_hash(key)))
        .role(Role::Label)
        .aria_label(format!("{label}: {value}"))
        .text_sm()
        .text_color(color)
        .child(Text::new_inaccessible(value.into()))
        .into_any_element()
}

/// Route an AccessKit spin-button action back into the settings entity.
fn a11y_step_handler(
    settings: gpui::WeakEntity<SettingsWindow>,
    key: String,
    bounds: (f64, f64),
    delta: f64,
) -> impl FnMut(Option<&gpui::accesskit::ActionData>, &mut Window, &mut App) {
    move |_, _, app| {
        settings.update(app, |settings, cx| settings.step(&key, bounds, delta, cx)).ok();
    }
}

/// A small rounded, clickable chip used for toggles, choices, steppers, and
/// action buttons.
fn pill(text: &str, bg: Rgba, fg: Rgba) -> gpui::Stateful<gpui::Div> {
    div()
        .id("settings-pill")
        .px_2()
        .py_1()
        .rounded_sm()
        .text_sm()
        .bg(bg)
        .text_color(fg)
        .cursor_pointer()
        .child(text.to_owned())
}

/// Stable per-key element id seed so GPUI can track click targets across
/// re-renders without colliding between controls on the same page.
fn key_hash(key: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

/// Open the settings window in the running GPUI [`App`].
///
/// The caller is responsible for the singleton handshake (see
/// [`crate::settings::singleton`]) before invoking this — a second launch should
/// hand focus to the existing window rather than open a duplicate.
///
/// The handle comes back so an in-process caller can keep it and raise the very
/// same window on the next request instead of stacking duplicates: the terminal
/// shell's settings entry point ([`crate::settings`] is a window in the client
/// process, not a separate binary) holds it for exactly that. `None` means the
/// platform refused the window, which is already logged here.
pub fn open_settings_window(cx: &mut App) -> Option<WindowHandle<SettingsWindow>> {
    let bounds = Bounds::centered(None, size(px(820.0), px(620.0)), cx);
    match cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Scribe Settings".into()),
                ..Default::default()
            }),
            app_id: Some("scribe-client".to_owned()),
            ..Default::default()
        },
        |_, cx| cx.new(SettingsWindow::new),
    ) {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::error!(%error, "failed to open GPUI settings window");
            None
        }
    }
}
