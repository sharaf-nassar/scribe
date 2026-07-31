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
//!
//! THESIS: Settings is a quiet professional instrument: dense, exact, and fast
//! to scan without looking like a terminal-themed novelty.
//! OWN-WORLD: Obsidian tonal layers, warm type, graphite seams, and one sparse
//! theme accent create Native Precision.
//! STORY: Grouped navigation establishes scope; search shortens retrieval; a
//! spacious section rhythm and aligned controls carry the user to live values.
//! FIRST VIEWPORT: The active destination, page purpose, and highest-frequency
//! controls are legible immediately in the preferred 1500×1050 composition.
//! FORM: Contemporary native settings workspace in fixed Obsidian Amber.
//! No dealt staging was adopted because it did not fit settings truth.

use std::time::Duration;

use gpui::{
    AccessibleAction, App, Bounds, Context, CursorStyle, Decorations, FocusHandle, FontWeight,
    HitboxBehavior, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point,
    ResizeEdge, Rgba, Role, ScrollHandle, Size, Text, Tiling, TitlebarOptions, Toggled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowDecorations, WindowHandle,
    WindowOptions, canvas, div, point, prelude::*, px, rgb, size,
};
use scribe_common::config::{ScribeConfig, load_config};
use scribe_common::protocol::{
    PreflightError, Release, ReleaseListResultState, TrustedDeviceInfo, TrustedNetworkInfo,
    UpdateCheckResultState,
};
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

/// How long [`SettingsWindow::after_paint`] yields before starting a blocking
/// server action — two frames at 60 Hz, enough for the pending status line to
/// be drawn and presented.
const PENDING_PAINT_DELAY: Duration = Duration::from_millis(34);

/// Width of the client-side-decoration resize gutter painted around the window.
///
/// The window opts into [`WindowDecorations::Client`], which removes the WM
/// frame on both Linux backends, so this band *is* the window's resize border:
/// [`resize_edge`] hit-tests it and [`Window::start_window_resize`] hands the
/// drag to the compositor. Kept narrow because the window background is
/// [`WindowBackgroundAppearance::Opaque`] — the gutter is a solid painted frame,
/// not Zed's translucent drop shadow, so it has to read as chrome rather than as
/// stray padding.
const RESIZE_GUTTER: Pixels = px(6.0);

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
        // `Unknown` also carries every transport failure, whose reason is a raw
        // socket/OS string. Framing it keeps the diagnostic without presenting
        // "Resource temporarily unavailable (os error 11)" as the answer.
        PreflightError::Unknown { reason } => {
            format!("the local Scribe server did not complete the probe ({reason})")
        }
    }
}

/// Fixed GPUI colors for the settings chrome.
#[derive(Clone, Copy)]
struct SettingsColors {
    page_bg: Rgba,
    /// Fill for the client-decoration resize gutter — a graphite matte one step
    /// below every interior surface so the frame reads as window chrome.
    frame_bg: Rgba,
    nav_bg: Rgba,
    nav_active_bg: Rgba,
    nav_hover_bg: Rgba,
    header_bg: Rgba,
    row_hover_bg: Rgba,
    control_bg: Rgba,
    control_hover_bg: Rgba,
    control_pressed_bg: Rgba,
    read_only_bg: Rgba,
    status_bg: Rgba,
    border: Rgba,
    strong_border: Rgba,
    accent: Rgba,
    accent_soft: Rgba,
    accent_text: Rgba,
    text: Rgba,
    dim_text: Rgba,
    quiet_text: Rgba,
}

impl SettingsColors {
    fn resolve(_config: &ScribeConfig) -> Self {
        // Settings must remain a stable instrument while the user edits the
        // terminal theme, so none of these roles derive from the active preset.
        let page_bg = rgb(0x0016_1719);
        let nav_bg = rgb(0x001e_1f20);
        let accent = rgb(0x00f5_b83a);
        let text = rgb(0x00ef_ede8);
        Self {
            page_bg,
            frame_bg: rgb(0x000c_0d0e),
            nav_bg,
            nav_active_bg: rgb(0x002b_2925),
            nav_hover_bg: rgb(0x0023_2426),
            header_bg: rgb(0x0019_1a1c),
            row_hover_bg: rgb(0x001e_1f21),
            control_bg: rgb(0x0027_2829),
            control_hover_bg: rgb(0x0030_3133),
            control_pressed_bg: rgb(0x0038_3327),
            read_only_bg: rgb(0x001e_1f20),
            status_bg: rgb(0x002b_261b),
            border: rgb(0x0038_393b),
            strong_border: rgb(0x004f_4f51),
            accent,
            accent_soft: rgb(0x0038_3020),
            accent_text: page_bg,
            text,
            dim_text: rgb(0x00b8_b6b1),
            quiet_text: rgb(0x0097_9692),
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
    search_handle: FocusHandle,
    search_query: String,
    /// Keyboard traversal is deliberately window-local: Settings claims only
    /// keys while its own window is focused, never terminal-window shortcuts.
    focus_index: usize,
    keyboard_navigation: bool,
    /// Resize edge the pointer currently sits over, tracked only so a change of
    /// edge forces the repaint that lets the cursor overlay swap glyphs.
    resize_edge: Option<ResizeEdge>,
    /// Armed by a left press on the titlebar drag region; the next left-button
    /// move hands the window to the compositor via
    /// [`Window::start_window_move`]. `WindowControlArea::Drag` is a no-op on
    /// both Linux backends, so this is the real path there.
    should_move: bool,
    /// Vertical scroll position of the settings page body, retained across
    /// frames so the scroller can report how far its content overflows and so a
    /// page switch can rewind it to the top.
    scroll_handle: ScrollHandle,
    /// Config key of the choice control whose dropdown is currently open, or
    /// `None` when no menu is showing. At most one menu is open at a time.
    open_choice: Option<String>,
    /// Scroll position of that dropdown. Shared because only one is ever open,
    /// and rewound on open so the live value is on screen even in the ~190-entry
    /// theme preset list.
    choice_scroll: ScrollHandle,
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

#[derive(Clone, Copy)]
enum StepDirection {
    Decrease,
    Increase,
}

#[derive(Clone, Copy)]
enum SettingsWindowControl {
    Minimize,
    Maximize,
    Close,
}

#[derive(Clone, Copy)]
struct StepperState {
    current: f64,
    min: f64,
    max: f64,
    step: f64,
}

fn focus_targets_match(a: &SettingsFocusTarget, b: &SettingsFocusTarget) -> bool {
    match (a, b) {
        (SettingsFocusTarget::Page(a), SettingsFocusTarget::Page(b)) => a == b,
        (SettingsFocusTarget::Control(a), SettingsFocusTarget::Control(b)) => a.key == b.key,
        (SettingsFocusTarget::Action(a), SettingsFocusTarget::Action(b)) => a == b,
        _ => false,
    }
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
            search_handle: cx.focus_handle().tab_index(0),
            search_query: String::new(),
            focus_index: 0,
            keyboard_navigation: false,
            resize_edge: None,
            should_move: false,
            scroll_handle: ScrollHandle::new(),
            open_choice: None,
            choice_scroll: ScrollHandle::new(),
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
    ///
    /// Returns whether the edit was accepted. A success used to clear the status
    /// line, which made a saved edit and a rejected one look identical; it now
    /// confirms in the same place a rejection reports.
    fn commit(&mut self, key: &str, value: Value, cx: &mut Context<Self>) -> bool {
        let mut obj = serde_json::Map::new();
        obj.insert("key".to_owned(), Value::String(key.to_owned()));
        obj.insert("value".to_owned(), value);
        let payload = Value::Object(obj).to_string();
        let applied = match crate::settings::apply::apply_settings_change(&payload) {
            Ok(()) => {
                self.reload();
                self.status = Some(format!("Saved {key}."));
                true
            }
            Err(e) => {
                self.status = Some(format!("{key} was not saved — {e}"));
                false
            }
        };
        cx.notify();
        applied
    }

    fn select_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        self.page = page;
        self.status = None;
        // A dropdown belongs to one control on one page; leaving the page must
        // not leave its menu floating over the next one.
        self.open_choice = None;
        // The scroller is one retained element across every page, so without an
        // explicit rewind a short page inherits the previous page's offset and
        // opens blank below its own content.
        self.scroll_handle.set_offset(Point::default());
        if self.keyboard_navigation {
            self.focus_index =
                settings_nav_pages().iter().position(|candidate| *candidate == page).unwrap_or(0);
        }
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
        let mut targets = settings_nav_pages()
            .into_iter()
            .filter(|page| self.page_matches_search(*page))
            .map(SettingsFocusTarget::Page)
            .collect::<Vec<_>>();
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
        targets.extend(page_controls(self.page).into_iter().filter_map(|control| {
            // A control its parent toggle has gated renders inert, so keyboard
            // traversal must skip it too rather than stop on a dead stop.
            if !self.control_matches_search(&control) || !self.control_is_enabled(&control.key) {
                return None;
            }
            match control.kind {
                ControlKind::Toggle
                | ControlKind::Choice(_)
                | ControlKind::Stepper { .. }
                | ControlKind::Action => Some(SettingsFocusTarget::Control(control)),
                ControlKind::Color | ControlKind::Text | ControlKind::Keybinding => None,
            }
        }));
        targets
    }

    fn focused_target(&self) -> Option<SettingsFocusTarget> {
        let targets = self.focus_targets();
        targets.get(self.focus_index % targets.len().max(1)).cloned()
    }

    fn target_is_focused(&self, target: &SettingsFocusTarget) -> bool {
        self.keyboard_navigation
            && self.focused_target().is_some_and(|current| focus_targets_match(&current, target))
    }

    /// Pointer use hides keyboard-only focus styling and records the exact
    /// clicked target so the next Tab/arrow move resumes from a truthful place.
    fn begin_pointer_interaction(&mut self, target: &SettingsFocusTarget) {
        self.focus_index = self
            .focus_targets()
            .iter()
            .position(|candidate| focus_targets_match(candidate, target))
            .unwrap_or(0);
        self.keyboard_navigation = false;
    }

    /// A pointer press outside a registered focus target must still clear any
    /// stale keyboard seam. Registered click handlers replace this index with
    /// their precise target before applying an action.
    fn clear_keyboard_navigation(&mut self, cx: &mut Context<Self>) {
        let had_visible_focus = self.keyboard_navigation;
        self.keyboard_navigation = false;
        self.focus_index = 0;
        if had_visible_focus {
            cx.notify();
        }
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
                // Keyboard activation opens the same dropdown a click opens, so
                // the option set is discoverable without pointer use; the arrow
                // keys then step the live value through it.
                ControlKind::Choice(options) => {
                    self.toggle_choice_menu(&control.key, &options, cx);
                }
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

    fn page_matches_search(&self, page: SettingsPage) -> bool {
        let query = self.search_query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        page.nav_label().to_lowercase().contains(&query)
            || page_summary(page).to_lowercase().contains(&query)
            || page_controls(page).iter().any(|control| {
                control.label.to_lowercase().contains(&query)
                    || control.key.to_lowercase().contains(&query)
                    || control_section(page, &control.key).to_lowercase().contains(&query)
            })
    }

    fn control_matches_search(&self, control: &Control) -> bool {
        let query = self.search_query.trim().to_lowercase();
        query.is_empty()
            || self.page.nav_label().to_lowercase() == query
            || control.label.to_lowercase().contains(&query)
            || control.key.to_lowercase().contains(&query)
            || control_section(self.page, &control.key).to_lowercase().contains(&query)
    }

    fn align_page_to_search(&mut self) {
        if !self.page_matches_search(self.page)
            && let Some(page) =
                settings_nav_pages().into_iter().find(|page| self.page_matches_search(*page))
        {
            self.page = page;
            self.status = None;
            // Jumping to a different page for a match must rewind the shared
            // scroller too, or the match lands above the retained offset.
            self.scroll_handle.set_offset(Point::default());
        }
    }

    fn handle_search_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let claimed_by_modifier = event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform;
        match event.keystroke.key.as_str() {
            "escape" => {
                self.search_query.clear();
                window.focus(&self.focus_handle, cx);
            }
            "backspace" => {
                self.search_query.pop();
                self.align_page_to_search();
            }
            // The caret is pinned to the end of the query, so there is nothing to
            // the right to forward-delete. Wiping the whole query here made Delete
            // and Backspace behave wildly differently for no reason; Escape is the
            // deliberate clear.
            "delete" => {}
            "enter" => self.align_page_to_search(),
            "tab" => window.focus_next(cx),
            _ if !claimed_by_modifier => {
                if let Some(text) =
                    event.keystroke.key_char.as_ref().filter(|text| !text.is_empty())
                {
                    self.search_query.push_str(text);
                    self.align_page_to_search();
                }
            }
            _ => {}
        }
        self.keyboard_navigation = false;
        cx.notify();
        cx.stop_propagation();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if (event.keystroke.modifiers.control || event.keystroke.modifiers.platform)
            && event.keystroke.key == "k"
        {
            window.focus(&self.search_handle, cx);
            self.keyboard_navigation = false;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if self.search_handle.is_focused(window) {
            self.handle_search_key(event, window, cx);
            return;
        }
        if event.keystroke.modifiers.modified() {
            return;
        }
        let handled = match event.keystroke.key.as_str() {
            // Escape is handled on the root, not only inside the search field, so
            // a user who filters and then clicks a control is never stranded in a
            // filtered UI with no visible way out.
            "escape" => self.dismiss_transient_state(cx),
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

    /// Back out of whatever transient state Escape should unwind, innermost
    /// first: an open dropdown, then an active search filter. Returns whether
    /// anything was actually dismissed so the caller only claims the keystroke
    /// when it did something.
    fn dismiss_transient_state(&mut self, cx: &mut Context<Self>) -> bool {
        if self.close_choice_menu(cx) {
            return true;
        }
        if self.search_query.is_empty() {
            return false;
        }
        self.search_query.clear();
        self.align_page_to_search();
        cx.notify();
        true
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

    /// Run `work` after the pending status line the caller just set has been
    /// drawn and presented.
    ///
    /// Every server action here is a blocking round trip on the UI thread, so
    /// running it inline paints the pending state and the result in the same
    /// frame: the user sees a freeze and then a result, never a working-on-it
    /// state. `Window::on_next_frame` is not enough — its callbacks run at the
    /// top of the frame, ahead of the draw — so this yields to the event loop
    /// for long enough that the pending frame reaches the screen first.
    fn after_paint(
        cx: &mut Context<Self>,
        work: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        cx.spawn(async move |settings, app| {
            app.background_executor().timer(PENDING_PAINT_DELAY).await;
            settings.update(app, work).ok();
        })
        .detach();
    }

    /// The gated ON transition of [`ENV_PERSISTENCE_KEY`]: probe the server's OS
    /// keystore first and commit only if it answers `ok`. A failing probe leaves
    /// the config untouched and reports the actionable reason.
    fn enable_env_persistence(&mut self, key: &str, cx: &mut Context<Self>) {
        self.status = Some(KEYSTORE_PENDING.to_owned());
        cx.notify();
        let key = key.to_owned();
        Self::after_paint(cx, move |this, ctx| {
            let outcome = server_action::request_env_preflight(SERVER_ACTION_TIMEOUT);
            this.finish_env_preflight(&key, outcome, ctx);
        });
    }

    /// Apply the gated toggle's probe result: commit and confirm on success,
    /// leave the config untouched and say why on failure.
    fn finish_env_preflight(
        &mut self,
        key: &str,
        outcome: EnvPreflightOutcome,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            EnvPreflightOutcome::Ok if self.commit(key, Value::Bool(true), cx) => {
                self.status =
                    Some("Keystore preflight passed; environment persistence is on.".to_owned());
            }
            EnvPreflightOutcome::Ok => {}
            EnvPreflightOutcome::Err(error) => {
                self.status = Some(format!(
                    "Environment persistence stays off — {}",
                    preflight_reason(&error)
                ));
            }
        }
        cx.notify();
    }

    /// The option set a choice control actually offers, which is the declared
    /// list plus the live config value whenever that value is not one of the
    /// declared options.
    ///
    /// `theme.preset` is open-ended — any installed theme name is legal — so a
    /// cycle restricted to the declared options steps off the user's value on
    /// the first click and can never return to it. Splicing the live value in
    /// keeps every reachable state reachable again.
    fn choice_options(
        &self,
        key: &str,
        options: &[(&'static str, &'static str)],
    ) -> Vec<(String, String)> {
        let value = current_value(&self.config, key);
        let token = value.as_str().unwrap_or("");
        let mut out = options
            .iter()
            .map(|(option, label)| ((*option).to_owned(), choice_label(option, label)))
            .collect::<Vec<_>>();
        if !token.is_empty() && !out.iter().any(|(candidate, _)| candidate == token) {
            out.insert(0, (token.to_owned(), humanize_choice_token(token)));
        }
        out
    }

    /// Step a choice control by one position through [`Self::choice_options`],
    /// wrapping at both ends.
    fn cycle_by(
        &mut self,
        key: &str,
        options: &[(&'static str, &'static str)],
        direction: isize,
        cx: &mut Context<Self>,
    ) {
        let options = self.choice_options(key, options);
        let count = options.len();
        if count == 0 {
            return;
        }
        let value = current_value(&self.config, key);
        let token = value.as_str().unwrap_or("");
        let index = options.iter().position(|(candidate, _)| candidate == token).unwrap_or(0);
        let next =
            if direction.is_negative() { (index + count - 1) % count } else { (index + 1) % count };
        let Some((chosen, _)) = options.get(next) else {
            return;
        };
        self.commit(key, Value::String(chosen.clone()), cx);
    }

    fn cycle(
        &mut self,
        key: &str,
        options: &[(&'static str, &'static str)],
        cx: &mut Context<Self>,
    ) {
        self.cycle_by(key, options, 1, cx);
    }

    fn cycle_previous(
        &mut self,
        key: &str,
        options: &[(&'static str, &'static str)],
        cx: &mut Context<Self>,
    ) {
        self.cycle_by(key, options, -1, cx);
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

    /// Dispatch an action button.
    ///
    /// Anything that talks to the local server paints a pending line first and
    /// runs on the next frame; purely local actions run inline. Every rendered
    /// action key reaches a real arm of [`Self::perform_action`].
    fn run_action(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(pending) = action_pending_message(key) else {
            self.perform_action(key, cx);
            return;
        };
        self.status = Some(pending);
        cx.notify();
        let key = key.to_owned();
        Self::after_paint(cx, move |this, ctx| this.perform_action(&key, ctx));
    }

    fn perform_action(&mut self, key: &str, cx: &mut Context<Self>) {
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
                self.status = Some(update_check_summary(&state));
            }
            "action.list_releases" => {
                let state = server_action::request_release_list(SERVER_ACTION_TIMEOUT);
                self.status = Some(release_list_summary(&state));
            }
            "workspaces.reset_badge_colors" | "terminal.smart_selection.reset" => {
                self.commit(key, Value::Bool(true), cx);
                return;
            }
            // Unreachable from the UI: every action key the model renders has an
            // arm above. Kept loud rather than silent so a future control wired
            // to a missing key reports itself instead of looking inert.
            _ => self.status = Some(format!("Settings bug: no handler is wired to {key}.")),
        }
        cx.notify();
    }
}

/// The line shown while a server-backed action is in flight, or `None` for an
/// action that completes locally and needs no pending state.
fn action_pending_message(key: &str) -> Option<String> {
    if key.starts_with(REMOVE_TRUSTED_NETWORK_PREFIX) {
        return Some("Removing the trusted network…".to_owned());
    }
    if key.starts_with(REVOKE_TRUSTED_DEVICE_PREFIX) {
        return Some("Revoking the approved device…".to_owned());
    }
    match key {
        REFRESH_TRUST_ACTION => Some("Reading trust state from the server…".to_owned()),
        ADD_CURRENT_NETWORK_ACTION => Some("Trusting the current network…".to_owned()),
        ENV_PREFLIGHT_ACTION => Some(KEYSTORE_PENDING.to_owned()),
        "action.check_for_updates" => Some("Checking for updates…".to_owned()),
        "action.list_releases" => Some("Fetching the release list…".to_owned()),
        _ => None,
    }
}

/// Plain-language rendering of a manual update check, replacing the raw `{:?}`
/// of the state enum the panel used to print.
fn update_check_summary(state: &UpdateCheckResultState) -> String {
    match state {
        UpdateCheckResultState::NoUpdate => {
            "Up to date — no newer release on this channel.".to_owned()
        }
        UpdateCheckResultState::UpdateAvailable { version, release_url } => {
            format!("Update available: {version} — {release_url}")
        }
        UpdateCheckResultState::Failed { reason } => format!("Update check failed — {reason}"),
    }
}

/// Plain-language rendering of a release listing.
///
/// Only the version, date, and pre-release flag are shown: a `Release` also
/// carries `body_html`, and Debug-printing the whole state dumped that sanitized
/// markup — escaped entities and literal `\n` included — straight into the panel.
fn release_list_summary(state: &ReleaseListResultState) -> String {
    match state {
        ReleaseListResultState::Fresh { releases } => {
            format!("Releases — {}", release_versions(releases))
        }
        ReleaseListResultState::Stale { releases, reason } => {
            format!("Releases (cached; refresh failed — {reason}) — {}", release_versions(releases))
        }
        ReleaseListResultState::Failed { reason } => format!("Release list failed — {reason}"),
    }
}

/// The newest few releases as `version — date`, with a count of the remainder.
fn release_versions(releases: &[Release]) -> String {
    if releases.is_empty() {
        return "none published.".to_owned();
    }
    let shown = releases
        .iter()
        .take(RELEASE_SUMMARY_MAX)
        .map(|release| {
            let date = release.published_at.split('T').next().unwrap_or(&release.published_at);
            let tag = if release.prerelease { " (pre-release)" } else { "" };
            format!("{} — {date}{tag}", release.version)
        })
        .collect::<Vec<_>>()
        .join("; ");
    match releases.len().saturating_sub(RELEASE_SUMMARY_MAX) {
        0 => shown,
        rest => format!("{shown}; +{rest} older"),
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        // The window asked for client-side decorations, but the compositor gets
        // the last word: X11 without a running compositor falls back to server
        // decorations, in which case the WM still paints resize borders and the
        // app must not add a gutter of its own.
        let decorations = window.window_decorations();
        let tiling = client_tiling(decorations);
        match decorations {
            Decorations::Client { .. } => window.set_client_inset(RESIZE_GUTTER),
            Decorations::Server => window.set_client_inset(px(0.0)),
        }
        let client_side = matches!(decorations, Decorations::Client { .. });
        let nav = self.render_nav(cx);
        let content = self.render_content(window, cx);
        let body = div()
            .id("settings-root")
            .role(Role::Application)
            .aria_label("Scribe settings")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, key_window, ctx| {
                this.on_key_down(event, key_window, ctx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _key_window, ctx| {
                    this.clear_keyboard_navigation(ctx);
                }),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.page_bg)
            .text_color(colors.text)
            .when(client_side, |this| this.border_1().border_color(colors.strong_border))
            .child(self.render_titlebar(window, cx))
            .child(div().flex_1().min_h(px(0.0)).w_full().flex().child(nav).child(content));
        self.render_window_frame(decorations, tiling, body, cx)
    }
}

/// Edge tiling reported for the window, or "nothing tiled" under server
/// decorations where the app never draws a gutter in the first place.
fn client_tiling(decorations: Decorations) -> Tiling {
    match decorations {
        Decorations::Server => Tiling::default(),
        Decorations::Client { tiling } => tiling,
    }
}

/// Map a window-relative press position onto the resize edge it grabs, or
/// `None` when the position is inside the content area.
///
/// Corners take a square of `gutter * 1.5` so diagonal resizes stay reachable;
/// tiled sides are excluded because a tiled or maximized edge cannot be dragged.
/// Mirrors the geometry of Zed's `client_side_decorations` helper.
fn resize_edge(
    pos: Point<Pixels>,
    gutter: Pixels,
    window_size: Size<Pixels>,
    tiling: Tiling,
) -> Option<ResizeEdge> {
    if Bounds::new(Point::default(), window_size).inset(gutter * 1.5).contains(&pos) {
        return None;
    }
    let corner = size(gutter * 1.5, gutter * 1.5);
    let right_edge = window_size.width - corner.width;
    let bottom_edge = window_size.height - corner.height;
    let in_corner = |origin: Point<Pixels>| Bounds::new(origin, corner).contains(&pos);
    if !tiling.top && in_corner(point(px(0.0), px(0.0))) {
        return Some(ResizeEdge::TopLeft);
    }
    if !tiling.top && in_corner(point(right_edge, px(0.0))) {
        return Some(ResizeEdge::TopRight);
    }
    if !tiling.bottom && in_corner(point(px(0.0), bottom_edge)) {
        return Some(ResizeEdge::BottomLeft);
    }
    if !tiling.bottom && in_corner(point(right_edge, bottom_edge)) {
        return Some(ResizeEdge::BottomRight);
    }
    if !tiling.top && pos.y < gutter {
        Some(ResizeEdge::Top)
    } else if !tiling.bottom && pos.y > window_size.height - gutter {
        Some(ResizeEdge::Bottom)
    } else if !tiling.left && pos.x < gutter {
        Some(ResizeEdge::Left)
    } else if !tiling.right && pos.x > window_size.width - gutter {
        Some(ResizeEdge::Right)
    } else {
        None
    }
}

/// The directional pointer shown while hovering a resize edge or corner.
const fn resize_cursor(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}

/// A full-window overlay whose only job is to set the directional resize cursor.
///
/// The hitbox is deliberately window-sized and [`HitboxBehavior::Normal`] so it
/// never swallows a click; it exists purely so [`Window::set_cursor_style`] has
/// a hovered hitbox to attach to, and it paints last so its cursor wins over the
/// content beneath it.
fn resize_cursor_overlay(tiling: Tiling) -> impl IntoElement {
    canvas(
        |_bounds, window, _cx| {
            let bounds =
                Bounds::new(point(px(0.0), px(0.0)), window.window_bounds().get_bounds().size);
            window.insert_hitbox(bounds, HitboxBehavior::Normal)
        },
        move |_bounds, hitbox, window, _cx| {
            let window_size = window.window_bounds().get_bounds().size;
            let Some(edge) =
                resize_edge(window.mouse_position(), RESIZE_GUTTER, window_size, tiling)
            else {
                return;
            };
            window.set_cursor_style(resize_cursor(edge), &hitbox);
        },
    )
    .size_full()
    .absolute()
}

impl SettingsWindow {
    /// Wrap the settings body in the client-side-decoration frame: a resize
    /// gutter on every untiled side, plus the cursor overlay. Under server
    /// decorations the window manager still owns the frame, so the wrapper is a
    /// plain pass-through with no gutter and no hit-testing.
    fn render_window_frame(
        &self,
        decorations: Decorations,
        tiling: Tiling,
        body: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let frame = div().id("settings-window-frame").size_full().bg(self.colors.frame_bg);
        let Decorations::Client { .. } = decorations else {
            return frame.child(body).into_any_element();
        };
        frame
            .when(!tiling.top, |this| this.pt(RESIZE_GUTTER))
            .when(!tiling.bottom, |this| this.pb(RESIZE_GUTTER))
            .when(!tiling.left, |this| this.pl(RESIZE_GUTTER))
            .when(!tiling.right, |this| this.pr(RESIZE_GUTTER))
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, key_window, ctx| {
                let window_size = key_window.window_bounds().get_bounds().size;
                let edge = resize_edge(event.position, RESIZE_GUTTER, window_size, tiling);
                if edge != this.resize_edge {
                    this.resize_edge = edge;
                    ctx.notify();
                }
            }))
            // Sits above the body's own left-press handler, which only clears
            // keyboard navigation and never stops propagation.
            .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, key_window, _| {
                let window_size = key_window.window_bounds().get_bounds().size;
                if let Some(edge) = resize_edge(event.position, RESIZE_GUTTER, window_size, tiling)
                {
                    key_window.start_window_resize(edge);
                }
            })
            .child(body)
            .child(resize_cursor_overlay(tiling))
            .into_any_element()
    }

    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let tiling = client_tiling(window.window_decorations());
        div()
            .id("settings-titlebar")
            .role(Role::TitleBar)
            .aria_label("Scribe Settings title bar")
            .w_full()
            .h(px(54.0))
            .flex_none()
            .flex()
            .items_center()
            .bg(colors.header_bg)
            .border_b_1()
            .border_color(colors.border)
            // `WindowControlArea::Drag` is what makes dragging work on Windows;
            // both Linux backends implement `on_hit_test_window_control` as an
            // empty body, so the explicit press/move pair below is the real
            // path there.
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, key_window, _| {
                    // The top corners' resize squares reach a few pixels into
                    // the titlebar; the resize grab has to win there, so only
                    // arm the move outside them.
                    let window_size = key_window.window_bounds().get_bounds().size;
                    this.should_move =
                        resize_edge(event.position, RESIZE_GUTTER, window_size, tiling).is_none();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _key_window, _| {
                    this.should_move = false;
                }),
            )
            .on_mouse_down_out(cx.listener(|this, _, _key_window, _| {
                this.should_move = false;
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, key_window, _| {
                if this.should_move && event.pressed_button == Some(MouseButton::Left) {
                    this.should_move = false;
                    key_window.start_window_move();
                }
            }))
            .child(
                div()
                    .w(px(54.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .window_control_area(WindowControlArea::Drag)
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .text_color(colors.text)
                    .child("S"),
            )
            .child(
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .window_control_area(WindowControlArea::Drag)
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(colors.text)
                    .child("Scribe Settings"),
            )
            .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
            .child(settings_window_control(SettingsWindowControl::Minimize, window, &colors, cx))
            .child(settings_window_control(SettingsWindowControl::Maximize, window, &colors, cx))
            .child(settings_window_control(SettingsWindowControl::Close, window, &colors, cx))
            .into_any_element()
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        // Groups that a search filters away entirely are dropped *before* the
        // index is assigned. Enumerating the full list instead left the first
        // surviving group with a non-zero index, so a filtered nav opened with a
        // separator above its first entry and nothing above the separator.
        let items = settings_nav_groups()
            .into_iter()
            .filter(|(_, pages)| pages.iter().any(|page| self.page_matches_search(*page)))
            .enumerate()
            .flat_map(|(index, (group, pages))| self.render_nav_group(index, group, pages, cx))
            .collect::<Vec<_>>();
        div()
            .id("settings-navigation")
            .role(Role::TabList)
            .aria_label("Settings sections")
            .w(px(314.0))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .pt(px(18.0))
            .bg(colors.nav_bg)
            .border_r_1()
            .border_color(colors.border)
            .children(items)
            .into_any_element()
    }

    fn render_nav_group(
        &self,
        group_index: usize,
        group: &'static str,
        pages: &'static [SettingsPage],
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let visible = pages.iter().copied().filter(|page| self.page_matches_search(*page));
        let mut items = visible.map(|page| self.render_nav_page(page, cx)).collect::<Vec<_>>();
        if items.is_empty() {
            return items;
        }
        let mut group_items = Vec::with_capacity(items.len() + 2);
        if group_index > 0 {
            group_items.push(
                div()
                    .id(("settings-nav-separator", group_index))
                    .mx(px(18.0))
                    .mt(px(12.0))
                    .h(px(1.0))
                    .flex_none()
                    .bg(self.colors.border)
                    .into_any_element(),
            );
        }
        group_items.push(
            div()
                .id(("settings-nav-group", group_index))
                .role(Role::Heading)
                .aria_level(2)
                .aria_label(group)
                .w_full()
                .h(px(46.0))
                .flex_none()
                .px(px(32.0))
                .flex()
                .items_end()
                .pb_2()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(self.colors.quiet_text)
                .child(group)
                .into_any_element(),
        );
        group_items.append(&mut items);
        group_items
    }

    fn render_nav_page(&self, page: SettingsPage, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let selected = page == self.page;
        let focused = self.target_is_focused(&SettingsFocusTarget::Page(page));
        let position =
            settings_nav_pages().iter().position(|candidate| *candidate == page).unwrap_or(0);
        let weight = if selected { FontWeight::SEMIBOLD } else { FontWeight::NORMAL };
        let background = if selected { colors.nav_active_bg } else { colors.nav_bg };
        let foreground = if selected { colors.text } else { colors.dim_text };
        let icon_color = if selected { colors.accent } else { colors.dim_text };
        div()
            .id(("settings-nav", page as usize))
            .focusable()
            .tab_stop(true)
            .role(Role::Tab)
            .aria_label(page.nav_label())
            .aria_selected(selected)
            .aria_position_in_set(position + 1)
            .aria_size_of_set(settings_nav_pages().len())
            .w_full()
            .h(px(44.0))
            .flex_none()
            .px(px(32.0))
            .flex()
            .items_center()
            .gap_3()
            .text_sm()
            .font_weight(weight)
            .bg(background)
            .text_color(foreground)
            .relative()
            // Selection is the accent bar; keyboard focus is a full outline in a
            // different colour. Both used to draw the same 4px accent bar, so a
            // focused row and the selected row were indistinguishable — and two
            // identical bars appeared whenever they differed.
            .when(selected, |el| {
                el.child(
                    div().absolute().left_0().top_0().bottom_0().w(px(4.0)).bg(colors.accent),
                )
            })
            .when(focused, |el| el.border_1().border_color(colors.text))
            .hover(move |style| style.bg(colors.nav_hover_bg).text_color(colors.text))
            .active(move |style| style.bg(colors.control_pressed_bg))
            .on_click(cx.listener(move |this, _, _win, ctx| {
                this.begin_pointer_interaction(&SettingsFocusTarget::Page(page));
                this.select_page(page, ctx);
            }))
            .child(
                div()
                    .size(px(20.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family("Symbols Nerd Font Mono")
                    .text_lg()
                    .font_weight(FontWeight::NORMAL)
                    .text_color(icon_color)
                    .child(page_icon(page)),
            )
            .child(page.nav_label())
            .into_any_element()
    }

    fn render_content(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let mut children = vec![self.render_page_heading()];
        // The Remote page leads with the runtime "Local network" trust surface so
        // its lists and their Remove/Revoke buttons sit above the fold; the static
        // config controls follow underneath.
        if self.page == SettingsPage::Remote {
            children.extend(self.render_trust_sections(cx));
        }
        children.extend(self.render_control_rows(cx));

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
            .flex()
            .flex_col()
            .bg(colors.page_bg)
            // The search field stays pinned above the scroller so a filtered
            // query never scrolls out of reach of the results it is filtering.
            .child(self.render_search(window, cx))
            .child(
                div()
                    .id("settings-scroll")
                    // `flex_1` claims the space the search field leaves, and the
                    // zero `min_h` is what actually bounds it: a flex item's
                    // automatic minimum size is its content height, which on a
                    // long page would size the viewport to the content and leave
                    // nothing to scroll.
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_x_hidden()
                    .overflow_y_scroll()
                    // The rows are children of the scroller itself. Wrapping them
                    // in an intermediate div makes the scroller see one flex item
                    // that reports its own box rather than its children's extent,
                    // so the content never measures as overflowing and the wheel
                    // is clamped to a zero maximum.
                    .track_scroll(&self.scroll_handle)
                    .px(px(46.0))
                    .pb(px(48.0))
                    .flex()
                    .flex_col()
                    .children(children),
            )
            // Pinned below the scroller, not appended to it. As the last row of a
            // page taller than the viewport the status line landed off-screen, so
            // a rejected edit and a successful one looked identical.
            .children(self.status.clone().map(|status| self.render_status(&status, cx)))
            .into_any_element()
    }

    fn render_search(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let focus = self.search_handle.clone();
        let focused = self.search_handle.is_focused(window);
        let query = self.search_query.clone();
        let field = div()
            .id("settings-search-input")
            .track_focus(&focus)
            .role(Role::SearchInput)
            .aria_label("Search settings")
            .aria_value(query.clone())
            .w_full()
            .max_w(px(580.0))
            .h(px(42.0))
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .rounded_sm()
            .border_1()
            .border_color(if focused { colors.accent } else { colors.strong_border })
            // Focus outranks hover here. The hover rule used to repaint a focused
            // field's border in the dim hover colour, so a focused-and-hovered
            // field looked exactly like an unfocused one.
            .bg(if focused { colors.control_bg } else { colors.read_only_bg })
            .text_sm()
            .text_color(colors.text)
            .cursor_text()
            .when(!focused, |el| el.hover(move |style| style.border_color(colors.dim_text)))
            .on_click(cx.listener(move |_, _, focused_window, ctx| {
                focused_window.focus(&focus, ctx);
            }))
            .child(settings_search_icon(colors.dim_text))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .text_color(if query.is_empty() { colors.quiet_text } else { colors.text })
                    .child(if query.is_empty() { "Search settings".to_owned() } else { query })
                    // The insertion point is always at the end of the query, so a
                    // static bar there is the honest caret: it says the field
                    // takes typing without implying a movable cursor it has not
                    // got.
                    .when(focused, |el| {
                        el.child(
                            div()
                                .ml(px(2.0))
                                .w(px(2.0))
                                .h(px(18.0))
                                .flex_none()
                                .bg(colors.accent),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .font_family("monospace")
                    .text_sm()
                    .text_color(colors.quiet_text)
                    .child("Ctrl+K"),
            );
        div()
            .id("settings-search-region")
            .role(Role::Search)
            .aria_label("Settings search")
            .w_full()
            .h(px(70.0))
            .flex_none()
            .px(px(22.0))
            .flex()
            .items_center()
            .justify_end()
            .child(field)
            .into_any_element()
    }

    fn render_page_heading(&self) -> gpui::AnyElement {
        let colors = self.colors;
        let summary = page_summary(self.page);
        div()
            .id("settings-page-heading")
            .role(Role::Heading)
            .aria_level(1)
            .aria_label(format!("{} — {summary}", self.page.nav_label()))
            .w_full()
            .h(px(134.0))
            .flex_none()
            .mt(px(-8.0))
            .flex()
            .flex_col()
            .justify_start()
            .border_b_1()
            .border_color(colors.strong_border)
            .text_color(colors.text)
            .child(div().text_3xl().font_weight(FontWeight::SEMIBOLD).child(self.page.nav_label()))
            .child(
                div()
                    .mt_1()
                    .text_sm()
                    .text_color(colors.dim_text)
                    .child(Text::new_inaccessible(elide(summary, PAGE_SUMMARY_MAX_CHARS).into())),
            )
            .child(
                div()
                    .mt_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_sm()
                    .text_color(colors.accent)
                    .child(div().size(px(12.0)).rounded_sm().bg(colors.accent))
                    .child("Changes apply instantly"),
            )
            .into_any_element()
    }

    fn render_control_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut rows = Vec::new();
        let mut previous_section = None;
        for control in page_controls(self.page)
            .into_iter()
            .filter(|control| self.control_matches_search(control))
        {
            let section = control_section(self.page, &control.key);
            if previous_section != Some(section) {
                rows.push(self.control_section_heading(section));
                previous_section = Some(section);
            }
            rows.push(self.render_control(&control, cx));
        }
        if rows.is_empty() {
            rows.push(self.note_row("No settings match this search."));
        }
        rows
    }

    /// The pinned result line under the content pane, with an explicit dismiss
    /// so a long action result can be cleared instead of sitting there until the
    /// next edit replaces it.
    fn render_status(&self, status: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        div()
            .id("settings-status")
            .role(Role::Status)
            .aria_label(status.to_owned())
            .mx_4()
            .px_3()
            .py_2()
            .my_3()
            .flex()
            .items_center()
            .gap_3()
            .rounded_xs()
            .border_1()
            .border_color(colors.accent)
            .bg(colors.status_bg)
            .text_sm()
            .text_color(colors.text)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(Text::new_inaccessible(status.to_owned().into())),
            )
            .child(
                div()
                    .id("settings-status-dismiss")
                    .focusable()
                    .tab_stop(true)
                    .role(Role::Button)
                    .aria_label("Dismiss status message")
                    .flex_none()
                    .size(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_xs()
                    .text_color(colors.dim_text)
                    .cursor_pointer()
                    .hover(move |style| style.bg(colors.control_hover_bg).text_color(colors.text))
                    .on_click(cx.listener(|this, _, _win, ctx| {
                        this.status = None;
                        ctx.notify();
                    }))
                    .child("×"),
            )
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
        // Runtime trust actions lead, followed by passive state and mutable
        // network/device collections.
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
        // Keep the feature-013 tailnet summary after LAN trust collections so
        // the trust workflow reads from local state to broader connectivity.
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
            .h(px(66.0))
            .flex_none()
            .flex()
            .items_end()
            .pb_3()
            .text_lg()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(self.colors.text)
            .border_b_1()
            .border_color(self.colors.border)
            .child(Text::new_inaccessible(text.to_owned().into()))
            .into_any_element()
    }

    /// Typography-led grouping label for config controls.
    fn control_section_heading(&self, text: &str) -> gpui::AnyElement {
        div()
            .id(("settings-control-section-heading", key_hash(text)))
            .role(Role::Heading)
            .aria_level(2)
            .aria_label(text.to_owned())
            .w_full()
            .h(px(66.0))
            .flex_none()
            .flex()
            .items_end()
            .pb_3()
            .text_lg()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(self.colors.text)
            .border_b_1()
            .border_color(self.colors.border)
            .child(Text::new_inaccessible(text.to_owned().into()))
            .into_any_element()
    }

    /// A read-only informational line inside a trust section.
    fn note_row(&self, text: &str) -> gpui::AnyElement {
        div()
            .id(("settings-note", key_hash(text)))
            .role(Role::Note)
            .aria_label(text.to_owned())
            .w_full()
            .h(px(54.0))
            .flex_none()
            .flex()
            .items_center()
            .text_sm()
            .text_color(self.colors.dim_text)
            .border_b_1()
            .border_color(self.colors.border)
            .bg(self.colors.read_only_bg)
            .child(Text::new_inaccessible(elide(text, NOTE_MAX_CHARS).into()))
            .into_any_element()
    }

    /// One trusted-network / approved-device row: its description plus a mutation
    /// action button that routes back through [`SettingsWindow::run_action`] with the
    /// record key embedded in the action id.
    fn trust_row(&self, row: TrustRow, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let TrustRow { label, button, id, action_key } = row;
        let focused = self.target_is_focused(&SettingsFocusTarget::Action(action_key.clone()));
        let pointer_target = SettingsFocusTarget::Action(action_key.clone());
        let control = action_button(button, &colors)
            .id(id)
            .when(focused, |el| el.border_color(colors.text))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(format!("{button} {label}"))
            .on_click(cx.listener(move |this, _, _win, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                this.run_action(&action_key, ctx);
            }));
        div()
            .id(("settings-trust-row", key_hash(&label)))
            .role(Role::Group)
            .aria_label(label.clone())
            .w_full()
            .h(px(54.0))
            .flex_none()
            .flex()
            .items_center()
            .gap_6()
            .border_b_1()
            .border_color(colors.border)
            .hover(move |style| style.bg(colors.row_hover_bg))
            .child(row_label(&label, colors.text))
            .child(
                div().w(px(438.0)).flex_none().flex().items_center().justify_end().child(control),
            )
            .into_any_element()
    }

    fn render_control(&self, control: &Control, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let enabled = self.control_is_enabled(&control.key);
        let label = row_label(&control.label, colors.text);
        let value_widget = if enabled {
            self.render_value_widget(control, cx)
        } else {
            self.render_gated_value(control)
        };
        div()
            .id(("settings-control", key_hash(&control.key)))
            .role(Role::Group)
            .aria_label(control.label.clone())
            .when(!enabled, |el| el.aria_description("Unavailable while its parent setting is off"))
            .w_full()
            .h(px(54.0))
            .flex_none()
            .flex()
            .items_center()
            .gap_6()
            .border_b_1()
            .border_color(colors.border)
            .when(!enabled, |el| el.opacity(GATED_OPACITY))
            .when(enabled, |el| el.hover(move |style| style.bg(colors.row_hover_bg)))
            .child(label)
            .child(
                div()
                    .w(px(438.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .child(value_widget),
            )
            .into_any_element()
    }

    /// Whether a control is live, or dimmed and inert because the toggle it
    /// depends on is off.
    ///
    /// The notification delivery condition and both timeout controls do nothing
    /// while `notifications.enabled` is off, but they used to render at full
    /// brightness and accept edits, which read as three working controls with no
    /// effect.
    fn control_is_enabled(&self, key: &str) -> bool {
        let Some(parent) = gating_toggle(key) else {
            return true;
        };
        current_value(&self.config, parent).as_bool().unwrap_or(false)
    }

    /// The read-only stand-in shown for a control its parent toggle has gated:
    /// the live value, so the row still says what it is set to.
    fn render_gated_value(&self, control: &Control) -> gpui::AnyElement {
        let shown = match &control.kind {
            ControlKind::Toggle => {
                let on = current_value(&self.config, &control.key).as_bool().unwrap_or(false);
                if on { "On".to_owned() } else { "Off".to_owned() }
            }
            ControlKind::Choice(options) => {
                let value = current_value(&self.config, &control.key);
                let token = value.as_str().unwrap_or("").to_owned();
                self.choice_options(&control.key, options)
                    .into_iter()
                    .find(|(candidate, _)| *candidate == token)
                    .map_or(token, |(_, label)| label)
            }
            ControlKind::Stepper { min, decimals, .. } => {
                let current = current_value(&self.config, &control.key).as_f64().unwrap_or(*min);
                format!("{current:.*}", *decimals as usize)
            }
            ControlKind::Color
            | ControlKind::Text
            | ControlKind::Keybinding
            | ControlKind::Action => String::new(),
        };
        read_only_value(&control.key, &control.label, &shown, &self.colors)
    }

    fn render_value_widget(&self, control: &Control, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &control.kind {
            ControlKind::Toggle => self.render_toggle(control, cx),
            ControlKind::Choice(options) => self.render_choice(control, options, cx),
            ControlKind::Stepper { min, max, step, decimals } => {
                self.render_stepper(control, (*min, *max, *step), *decimals, cx)
            }
            ControlKind::Color => self.render_color(control),
            ControlKind::Text => self.render_text_value(control),
            ControlKind::Keybinding => self.render_keybinding_value(control),
            ControlKind::Action => self.render_action_control(control, cx),
        }
    }

    fn render_toggle(&self, control: &Control, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let focused = self.target_is_focused(&SettingsFocusTarget::Control(control.clone()));
        let on = current_value(&self.config, &control.key).as_bool().unwrap_or(false);
        let key = control.key.clone();
        let track_bg = if on { colors.accent } else { colors.control_bg };
        let track_border = if on { colors.accent } else { colors.strong_border };
        let hover_bg = if on { rgb(0x00ff_c24a) } else { colors.control_hover_bg };
        let pressed_bg = if on { rgb(0x00d7_aa47) } else { colors.control_pressed_bg };
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        let knob = div().size(px(22.0)).rounded_full().bg(colors.text);
        div()
            .id(("toggle", key_hash(&control.key)))
            .focusable()
            .tab_stop(true)
            .role(Role::Switch)
            .aria_label(control.label.clone())
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
            .w(px(52.0))
            .h(px(30.0))
            .p(px(3.0))
            .flex()
            .items_center()
            .when(on, gpui::Styled::justify_end)
            .rounded_full()
            .border_1()
            .border_color(if focused { colors.text } else { track_border })
            .bg(track_bg)
            .cursor_pointer()
            .hover(move |style| style.bg(hover_bg))
            .active(move |style| style.bg(pressed_bg))
            .on_click(cx.listener(move |this, _, _win, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                this.toggle(&key, ctx);
            }))
            .child(knob)
            .into_any_element()
    }

    fn render_choice(
        &self,
        control: &Control,
        options: &[(&'static str, &'static str)],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let focused = self.target_is_focused(&SettingsFocusTarget::Control(control.clone()));
        let effective = self.choice_options(&control.key, options);
        let value = current_value(&self.config, &control.key);
        let token = value.as_str().unwrap_or("").to_owned();
        let display = effective
            .iter()
            .find(|(choice, _)| *choice == token)
            .map_or_else(|| token.clone(), |(_, label)| label.clone());
        let key = control.key.clone();
        let declared = options.to_vec();
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        let open = self.open_choice.as_deref() == Some(control.key.as_str());
        let button = div()
            .id(("choice", key_hash(&control.key)))
            .focusable()
            .tab_stop(true)
            .role(Role::ComboBox)
            .aria_label(control.label.clone())
            .aria_value(display.clone())
            .aria_expanded(open)
            .w(px(438.0))
            .h(px(42.0))
            .px_4()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .rounded_xs()
            .border_1()
            .border_color(if focused || open { colors.accent } else { colors.strong_border })
            .bg(colors.control_bg)
            .text_sm()
            .text_color(colors.text)
            .cursor_pointer()
            .hover(move |style| style.bg(colors.control_hover_bg).border_color(colors.accent))
            .active(move |style| style.bg(colors.control_pressed_bg))
            .on_click(cx.listener(move |this, _, _win, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                this.toggle_choice_menu(&key, &declared, ctx);
            }))
            .child(Text::new_inaccessible(display.into()))
            .child(div().text_base().text_color(colors.dim_text).child(if open {
                "⌃"
            } else {
                "⌄"
            }));
        let close_key = control.key.clone();
        div()
            .id(("choice-shell", key_hash(&control.key)))
            .relative()
            .w(px(438.0))
            // The height is load-bearing, not decoration: the anchored menu is
            // absolutely positioned, so with an automatic height this shell
            // measures as zero-height and the hit test never reaches the button
            // inside it — the control stops responding to clicks entirely.
            .h(px(42.0))
            .flex_none()
            .when(open, |el| {
                el.on_mouse_down_out(cx.listener(move |this, _, _win, ctx| {
                    this.close_choice_menu(ctx);
                }))
                .child(self.render_choice_menu(control, &effective, &close_key, cx))
            })
            .child(button)
            .into_any_element()
    }

    /// The anchored dropdown for an open choice control: one activatable row per
    /// option, with the live value marked. Deferred so it paints above the rows
    /// beneath it and escapes the scroller's clip, and anchored so a menu opened
    /// near the bottom of the window flips above its button instead of running
    /// off-screen.
    fn render_choice_menu(
        &self,
        control: &Control,
        options: &[(String, String)],
        key: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let value = current_value(&self.config, &control.key);
        let token = value.as_str().unwrap_or("").to_owned();
        let rows = options
            .iter()
            .enumerate()
            .map(|(index, (option, label))| {
                let selected = *option == token;
                let commit_key = key.to_owned();
                let commit_value = option.clone();
                div()
                    .id(("choice-option", key_hash(&format!("{key}:{option}"))))
                    .focusable()
                    .tab_stop(true)
                    .role(Role::MenuItem)
                    .aria_label(label.clone())
                    .aria_selected(selected)
                    .aria_position_in_set(index + 1)
                    .aria_size_of_set(options.len())
                    .w_full()
                    .h(px(CHOICE_OPTION_HEIGHT))
                    .flex_none()
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .text_sm()
                    .text_color(if selected { colors.accent } else { colors.text })
                    .cursor_pointer()
                    .hover(move |style| style.bg(colors.control_hover_bg))
                    .active(move |style| style.bg(colors.control_pressed_bg))
                    .on_click(cx.listener(move |this, _, _win, ctx| {
                        this.open_choice = None;
                        this.commit(&commit_key, Value::String(commit_value.clone()), ctx);
                    }))
                    .child(Text::new_inaccessible(label.clone().into()))
                    .child(if selected { "✓" } else { "" })
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        gpui::deferred(
            gpui::anchored().snap_to_window_with_margin(px(12.0)).child(
                div()
                    .id(("choice-menu", key_hash(key)))
                    .role(Role::Menu)
                    .aria_label(control.label.clone())
                    .w(px(438.0))
                    .mt(px(46.0))
                    .max_h(px(CHOICE_MENU_MAX_HEIGHT))
                    // The option rows are direct children of the scroller: an
                    // intermediate wrapper would report its own box as the
                    // content extent and clamp the wheel to zero.
                    .overflow_y_scroll()
                    .track_scroll(&self.choice_scroll)
                    .flex()
                    .flex_col()
                    .rounded_xs()
                    .border_1()
                    .border_color(colors.accent)
                    .bg(colors.nav_active_bg)
                    .children(rows),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// Open this choice's dropdown, or close it when it is already the open one.
    fn toggle_choice_menu(
        &mut self,
        key: &str,
        options: &[(&'static str, &'static str)],
        cx: &mut Context<Self>,
    ) {
        if self.open_choice.as_deref() == Some(key) {
            self.open_choice = None;
        } else {
            self.open_choice = Some(key.to_owned());
            self.align_choice_scroll(key, options);
        }
        cx.notify();
    }

    /// Position the dropdown scroller so the live value is on screen when the
    /// menu opens. Without it a config sitting near the end of the generated
    /// theme preset list would open on an unrelated stretch of the alphabet.
    fn align_choice_scroll(&mut self, key: &str, options: &[(&'static str, &'static str)]) {
        let options = self.choice_options(key, options);
        let value = current_value(&self.config, key);
        let token = value.as_str().unwrap_or("");
        let index = options.iter().position(|(candidate, _)| candidate == token).unwrap_or(0);
        let rows = f32::from(u16::try_from(index).unwrap_or(u16::MAX));
        let lead = (rows * CHOICE_OPTION_HEIGHT - CHOICE_MENU_LEAD).max(0.0);
        self.choice_scroll.set_offset(point(px(0.0), px(-lead)));
    }

    /// Dismiss any open choice dropdown, notifying only when one was showing.
    fn close_choice_menu(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = self.open_choice.take().is_some();
        if was_open {
            cx.notify();
        }
        was_open
    }

    fn render_text_value(&self, control: &Control) -> gpui::AnyElement {
        let value = current_value(&self.config, &control.key);
        read_only_value(&control.key, &control.label, value.as_str().unwrap_or(""), &self.colors)
    }

    fn render_keybinding_value(&self, control: &Control) -> gpui::AnyElement {
        let combos = keybinding_combos(&self.config, &control.key);
        let shown = if combos.is_empty() { "—".to_owned() } else { combos.join(", ") };
        read_only_value(&control.key, &control.label, &shown, &self.colors)
    }

    fn render_action_control(&self, control: &Control, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let focused = self.target_is_focused(&SettingsFocusTarget::Control(control.clone()));
        let key = control.key.clone();
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        action_button(&control.label, &colors)
            .id(("action", key_hash(&control.key)))
            .when(focused, |el| el.border_color(colors.text))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(control.label.clone())
            .on_click(cx.listener(move |this, _, _win, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                this.run_action(&key, ctx);
            }))
            .into_any_element()
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
        let key_a11y_dec = control.key.clone();
        let key_a11y_inc = control.key.clone();
        let state = StepperState { current, min, max, step };
        let minus = self.render_step_adjustment(control, state, StepDirection::Decrease, cx);
        let plus = self.render_step_adjustment(control, state, StepDirection::Increase, cx);
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
            .w(px(207.0))
            .h(px(38.0))
            .rounded_xs()
            .border_1()
            .border_color(if focused { colors.accent } else { colors.strong_border })
            .bg(colors.control_bg)
            .child(minus)
            .child(
                div()
                    .id(("stepper-value", key_hash(&control.key)))
                    .role(Role::Label)
                    .aria_label(display.clone())
                    .h_full()
                    .flex_1()
                    .px_2()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_l_1()
                    .border_r_1()
                    .border_color(colors.border)
                    .font_family("monospace")
                    .text_sm()
                    .text_color(colors.text)
                    .child(Text::new_inaccessible(display.into())),
            )
            .child(plus)
            .into_any_element()
    }

    fn render_step_adjustment(
        &self,
        control: &Control,
        state: StepperState,
        direction: StepDirection,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (id, symbol, verb, bound_name, limit, disabled, delta) = match direction {
            StepDirection::Decrease => (
                "dec",
                "−",
                "Decrease",
                "minimum",
                "Minimum value reached",
                state.current <= state.min,
                -state.step,
            ),
            StepDirection::Increase => (
                "inc",
                "+",
                "Increase",
                "maximum",
                "Maximum value reached",
                state.current >= state.max,
                state.step,
            ),
        };
        let label = if disabled {
            format!("{verb} {} — unavailable at {bound_name}", control.label)
        } else {
            format!("{verb} {}", control.label)
        };
        let key = control.key.clone();
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        let bounds = (state.min, state.max);
        stepper_button(symbol, &self.colors, disabled)
            .id((id, key_hash(&control.key)))
            .role(Role::Button)
            .aria_label(label)
            .when(disabled, |el| el.aria_description(limit))
            .when(!disabled, |el| {
                el.focusable().tab_stop(true).on_click(cx.listener(move |this, _, _win, ctx| {
                    this.begin_pointer_interaction(&pointer_target);
                    this.step(&key, bounds, delta, ctx);
                }))
            })
            .into_any_element()
    }

    /// Render a color control: an optional swatch of the current hex plus the
    /// hex text (read-only; inline hex entry is a tracked follow-on).
    fn render_color(&self, control: &Control) -> gpui::AnyElement {
        let colors = self.colors;
        let value = current_value(&self.config, &control.key);
        let hex = value.as_str().unwrap_or("").to_owned();
        let shown = if hex.is_empty() { "(theme default)".to_owned() } else { hex.clone() };
        let swatch = scribe_common::theme::hex_to_rgba(&hex).ok().map(|rgba| {
            div()
                .size(px(20.0))
                .flex_none()
                .rounded_xs()
                .border_1()
                .border_color(colors.strong_border)
                .bg(srgba(rgba))
                .into_any_element()
        });
        div()
            .id(("settings-color-value", key_hash(&control.key)))
            .role(Role::Label)
            .aria_label(format!("{}: {shown}", control.label))
            .aria_description("Read-only value")
            .w(px(438.0))
            .h(px(42.0))
            .px_4()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.read_only_bg)
            .children(swatch)
            .child(read_only_marker(&colors))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .justify_end()
                    .font_family("monospace")
                    .text_sm()
                    .text_color(colors.dim_text)
                    .child(Text::new_inaccessible(elide(&shown, READ_ONLY_MAX_CHARS).into())),
            )
            .into_any_element()
    }
}

/// Height of one dropdown option row, also the unit the open-scroll uses to put
/// the live value on screen.
const CHOICE_OPTION_HEIGHT: f32 = 40.0;

/// How much of a dropdown is kept visible above the live value when it opens.
const CHOICE_MENU_LEAD: f32 = 120.0;

/// Tallest a dropdown grows before it scrolls internally — the theme preset list
/// has close to 190 entries.
const CHOICE_MENU_MAX_HEIGHT: f32 = 400.0;

/// Shared pending line for both keystore probes (the manual action and the
/// toggle's gated ON transition), so the two surfaces say the same thing.
const KEYSTORE_PENDING: &str = "Probing the OS keystore…";

/// How many releases the status line names before it summarizes the rest.
const RELEASE_SUMMARY_MAX: usize = 4;

/// Dimming applied to a control whose parent toggle is off.
const GATED_OPACITY: f32 = 0.42;

/// The toggle a control depends on, or `None` when it stands alone.
///
/// Kept as one table so [`SettingsWindow::control_is_enabled`] and the keyboard
/// traversal filter can never disagree about what is gated.
fn gating_toggle(key: &str) -> Option<&'static str> {
    match key {
        "notifications.condition" | "notifications.timeout_mode" | "notifications.timeout_secs" => {
            Some("notifications.enabled")
        }
        _ => None,
    }
}

const NOTE_MAX_CHARS: usize = 120;
const PAGE_SUMMARY_MAX_CHARS: usize = 100;

/// Display text for one choice option: the model's declared caption, or a
/// title-cased token when the model declared none.
///
/// A generated option set — the ~190-entry theme preset list — carries its raw
/// name in both slots because there is no hand-written caption to carry, so the
/// two being equal is exactly the "no caption" signal. Every hand-written option
/// differs from its value by at least capitalization.
fn choice_label(value: &str, label: &str) -> String {
    if value == label { humanize_choice_token(value) } else { label.to_owned() }
}

/// Title-case a raw config token (`gruvbox-dark` → `Gruvbox Dark`) so a value
/// that has no declared display label still reads like the rest of the list.
fn humanize_choice_token(token: &str) -> String {
    token
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                let mut capitalized = first.to_uppercase().collect::<String>();
                capitalized.push_str(chars.as_str());
                capitalized
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

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
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(Text::new_inaccessible(elide(text, NOTE_MAX_CHARS).into()))
}

/// A read-only current value still needs a semantic node: visual text alone is
/// otherwise absent from AccessKit's tree.
fn read_only_value(
    key: &str,
    label: &str,
    value: &str,
    colors: &SettingsColors,
) -> gpui::AnyElement {
    div()
        .id(("settings-read-only-value", key_hash(key)))
        .role(Role::Label)
        .aria_label(format!("{label}: {value}"))
        .aria_description("Read-only value")
        .w(px(438.0))
        .h(px(42.0))
        .px_4()
        .flex()
        .items_center()
        .gap_3()
        .overflow_hidden()
        .border_b_1()
        .border_color(colors.border)
        .bg(colors.read_only_bg)
        .child(read_only_marker(colors))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .flex()
                .justify_end()
                .font_family("monospace")
                .text_sm()
                .text_color(colors.dim_text)
                .child(Text::new_inaccessible(elide(value, READ_ONLY_MAX_CHARS).into())),
        )
        .into_any_element()
}

/// Compact status text that distinguishes inert values from bordered,
/// hoverable controls without changing the aligned value-column geometry.
fn read_only_marker(colors: &SettingsColors) -> gpui::Div {
    div()
        .flex_none()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(colors.quiet_text)
        .child(Text::new_inaccessible("READ ONLY".into()))
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

/// Quiet rectangular action silhouette with an accent seam.
fn action_button(text: &str, colors: &SettingsColors) -> gpui::Stateful<gpui::Div> {
    let hover_bg = colors.accent;
    let hover_text = colors.accent_text;
    let pressed_bg = colors.control_pressed_bg;
    let pressed_text = colors.text;
    div()
        .id("settings-pill")
        .h(px(40.0))
        .px_4()
        .flex()
        .items_center()
        .rounded_xs()
        .border_1()
        .border_color(colors.accent)
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .bg(colors.accent_soft)
        .text_color(colors.accent)
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg).text_color(hover_text))
        .active(move |style| style.bg(pressed_bg).text_color(pressed_text))
        .child(text.to_owned())
}

fn settings_search_icon(color: Rgba) -> gpui::AnyElement {
    div()
        .size(px(20.0))
        .flex_none()
        .relative()
        .child(
            div()
                .absolute()
                .left(px(1.0))
                .top(px(1.0))
                .size(px(13.0))
                .rounded_full()
                .border_1()
                .border_color(color),
        )
        .child(
            div()
                .absolute()
                .left(px(10.0))
                .top(px(8.0))
                .font_family("monospace")
                .text_sm()
                .font_weight(FontWeight::NORMAL)
                .text_color(color)
                .child("╲"),
        )
        .into_any_element()
}

fn settings_window_control(
    kind: SettingsWindowControl,
    _window: &Window,
    colors: &SettingsColors,
    cx: &mut Context<SettingsWindow>,
) -> gpui::AnyElement {
    let (id, glyph, label, area, hover) = match kind {
        SettingsWindowControl::Minimize => (
            "settings-window-minimize",
            "–",
            "Minimize window",
            WindowControlArea::Min,
            colors.control_hover_bg,
        ),
        SettingsWindowControl::Maximize => (
            "settings-window-maximize",
            "□",
            "Maximize window",
            WindowControlArea::Max,
            colors.control_hover_bg,
        ),
        SettingsWindowControl::Close => (
            "settings-window-close",
            "×",
            "Close window",
            WindowControlArea::Close,
            rgb(0x00c8_3030),
        ),
    };
    div()
        .id(id)
        .focusable()
        .tab_stop(true)
        .role(Role::Button)
        .aria_label(label)
        .w(px(48.0))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .window_control_area(area)
        .text_sm()
        .font_weight(FontWeight::NORMAL)
        .text_color(colors.quiet_text)
        .hover(move |style| style.bg(hover).text_color(colors.text))
        // Keep the press off the titlebar's drag arming so a click with a pixel
        // of jitter can never turn into a window move that eats the click.
        .on_mouse_down(MouseButton::Left, |_, _, ctx| ctx.stop_propagation())
        .on_click(cx.listener(move |_, _, window, _| match kind {
            SettingsWindowControl::Minimize => window.minimize_window(),
            SettingsWindowControl::Maximize => window.zoom_window(),
            SettingsWindowControl::Close => window.remove_window(),
        }))
        .on_key_down(cx.listener(move |_, event: &KeyDownEvent, window, ctx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                // Activation belongs to this button, but settings-wide keys
                // (Ctrl+K, Escape, and navigation) must continue along the
                // focused event path to `#settings-root`.
                ctx.stop_propagation();
                match kind {
                    SettingsWindowControl::Minimize => window.minimize_window(),
                    SettingsWindowControl::Maximize => window.zoom_window(),
                    SettingsWindowControl::Close => window.remove_window(),
                }
            }
        }))
        .child(glyph)
        .into_any_element()
}

/// One side of a connected numeric stepper.
fn stepper_button(
    text: &'static str,
    colors: &SettingsColors,
    disabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let hover_bg = colors.control_hover_bg;
    let pressed_bg = colors.control_pressed_bg;
    div()
        .id("settings-stepper-button")
        .w(px(48.0))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_lg()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(if disabled { colors.quiet_text } else { colors.text })
        .when(disabled, |el| el.cursor_not_allowed().opacity(0.55))
        .when(!disabled, |el| {
            el.cursor_pointer()
                .hover(move |style| style.bg(hover_bg))
                .active(move |style| style.bg(pressed_bg))
        })
        .child(text)
}

fn settings_nav_pages() -> [SettingsPage; 11] {
    [
        SettingsPage::Appearance,
        SettingsPage::Colors,
        SettingsPage::Terminal,
        SettingsPage::Keybindings,
        SettingsPage::Ai,
        SettingsPage::Environment,
        SettingsPage::Workspaces,
        SettingsPage::Updates,
        SettingsPage::Releases,
        SettingsPage::Notifications,
        SettingsPage::Remote,
    ]
}

fn settings_nav_groups() -> [(&'static str, &'static [SettingsPage]); 5] {
    const TERMINAL: &[SettingsPage] = &[
        SettingsPage::Appearance,
        SettingsPage::Colors,
        SettingsPage::Terminal,
        SettingsPage::Keybindings,
    ];
    const INTELLIGENCE: &[SettingsPage] = &[SettingsPage::Ai];
    const WORKFLOW: &[SettingsPage] = &[SettingsPage::Environment, SettingsPage::Workspaces];
    const SYSTEM: &[SettingsPage] =
        &[SettingsPage::Updates, SettingsPage::Releases, SettingsPage::Notifications];
    const CONNECTIVITY: &[SettingsPage] = &[SettingsPage::Remote];
    [
        ("TERMINAL", TERMINAL),
        ("INTELLIGENCE", INTELLIGENCE),
        ("WORKFLOW", WORKFLOW),
        ("SYSTEM", SYSTEM),
        ("CONNECTIVITY", CONNECTIVITY),
    ]
}

fn page_icon(page: SettingsPage) -> &'static str {
    match page {
        SettingsPage::Appearance => "\u{f108}",
        SettingsPage::Colors => "\u{f1fc}",
        SettingsPage::Terminal => "\u{f120}",
        SettingsPage::Keybindings => "\u{f11c}",
        SettingsPage::Ai => "\u{f0d0}",
        SettingsPage::Environment => "\u{f121}",
        SettingsPage::Workspaces => "\u{f07b}",
        SettingsPage::Updates => "\u{f019}",
        SettingsPage::Releases => "\u{f02b}",
        SettingsPage::Notifications => "\u{f0f3}",
        SettingsPage::Remote => "\u{f1eb}",
    }
}

fn page_summary(page: SettingsPage) -> &'static str {
    match page {
        SettingsPage::Appearance => "Type, cursor, spacing, and terminal chrome",
        SettingsPage::Colors => "Theme palette, ANSI colors, and prompt bar overrides",
        SettingsPage::Ai => "Assistant integrations, prompt bar, and state signals",
        SettingsPage::Terminal => "Session behavior, clipboard policy, and status metrics",
        SettingsPage::Environment => "Securely restore environment variables across sessions",
        SettingsPage::Keybindings => "Current shortcuts for tabs, panes, navigation, and commands",
        SettingsPage::Workspaces => "Workspace roots and badge appearance",
        SettingsPage::Updates => "Automatic update cadence and release channel",
        SettingsPage::Releases => "Query available versions from the Scribe server",
        SettingsPage::Notifications => "Desktop delivery conditions and timeout behavior",
        SettingsPage::Remote => "Tailnet, local-network trust, and sharing policy",
    }
}

/// Visual grouping only: control order and keys remain exactly those returned
/// by `page_controls`.
///
/// Every page names a section, Environment included — it used to return nothing
/// and open on bare rows, which read as a different kind of page.
fn control_section(page: SettingsPage, key: &str) -> &'static str {
    match page {
        SettingsPage::Appearance => appearance_section(key),
        SettingsPage::Colors => colors_section(key),
        SettingsPage::Ai => ai_section(key),
        SettingsPage::Terminal => terminal_section(key),
        SettingsPage::Environment => "Environment persistence",
        SettingsPage::Keybindings => keybinding_section(key),
        SettingsPage::Workspaces => "Workspace configuration",
        SettingsPage::Updates => "Automatic updates",
        SettingsPage::Releases => "Release service",
        SettingsPage::Notifications => notification_section(key),
        SettingsPage::Remote => remote_section(key),
    }
}

fn appearance_section(key: &str) -> &'static str {
    if key.starts_with("appearance.content_padding") || key.starts_with("appearance.focus_border") {
        "Content frame"
    } else if key.starts_with("appearance.cursor") {
        "Cursor"
    } else if matches!(
        key,
        "appearance.opacity"
            | "appearance.scrollbar_width"
            | "appearance.tab_bar_padding"
            | "appearance.tab_width"
            | "appearance.status_bar_height"
            | "appearance.tab_height"
    ) {
        "Window chrome"
    } else {
        "Typography"
    }
}

fn colors_section(key: &str) -> &'static str {
    if key.starts_with("theme.ansi_") {
        "ANSI palette"
    } else if key.starts_with("appearance.prompt_bar_") {
        "Prompt bar"
    } else {
        "Theme"
    }
}

fn ai_section(key: &str) -> &'static str {
    if key.starts_with("ai_states.") {
        "Assistant state signals"
    } else if matches!(
        key,
        "terminal.claude_code_integration"
            | "terminal.codex_code_integration"
            | "terminal.ai_tab_cwd"
    ) {
        "Integrations"
    } else {
        "Assistant surface"
    }
}

fn terminal_section(key: &str) -> &'static str {
    if key.starts_with("terminal.clipboard.") {
        "Clipboard (OSC 52)"
    } else if key.starts_with("terminal.status_bar_stats.") {
        "Status bar"
    } else if key == "terminal.smart_selection.reset" {
        "Smart selection"
    } else {
        "Session"
    }
}

fn keybinding_section(key: &str) -> &'static str {
    if key.starts_with("workspace_") {
        "Workspaces"
    } else if pane_keybinding(key) {
        "Panes"
    } else if tab_keybinding(key) {
        "Tabs"
    } else if matches!(key, "copy" | "paste") {
        "Clipboard"
    } else if terminal_editing_keybinding(key) {
        "Terminal editing"
    } else if navigation_keybinding(key) {
        "Navigation"
    } else {
        "Application"
    }
}

fn pane_keybinding(key: &str) -> bool {
    matches!(
        key,
        "split_vertical"
            | "split_horizontal"
            | "close_pane"
            | "cycle_pane"
            | "focus_left"
            | "focus_right"
            | "focus_up"
            | "focus_down"
    )
}

fn tab_keybinding(key: &str) -> bool {
    matches!(
        key,
        "new_tab"
            | "new_claude_tab"
            | "new_claude_resume_tab"
            | "new_codex_tab"
            | "new_codex_resume_tab"
            | "close_tab"
            | "next_tab"
            | "prev_tab"
            | "select_tab_1"
            | "select_tab_2"
            | "select_tab_3"
            | "select_tab_4"
            | "select_tab_5"
            | "select_tab_6"
            | "select_tab_7"
            | "select_tab_8"
            | "select_tab_9"
    )
}

fn terminal_editing_keybinding(key: &str) -> bool {
    matches!(
        key,
        "word_left"
            | "word_right"
            | "delete_word_backward"
            | "delete_word_backward_ctrl"
            | "delete_word_forward"
            | "line_start"
            | "line_end"
    )
}

fn navigation_keybinding(key: &str) -> bool {
    matches!(
        key,
        "scroll_up"
            | "scroll_down"
            | "scroll_top"
            | "scroll_bottom"
            | "find"
            | "jump_to_failure"
            | "prompt_jump_up"
            | "prompt_jump_down"
    )
}

fn notification_section(key: &str) -> &'static str {
    if key.starts_with("notifications.timeout") { "Timing" } else { "Delivery" }
}

fn remote_section(key: &str) -> &'static str {
    if key.starts_with("remote.lan.") {
        "LAN listener"
    } else if matches!(
        key,
        "remote.sharing_mode" | "remote.control_acquisition" | "remote.participant_limit"
    ) {
        "Window sharing"
    } else {
        "Tailnet listener"
    }
}

const READ_ONLY_MAX_CHARS: usize = 42;

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
    const PREFERRED_WIDTH: f32 = 1500.0;
    const PREFERRED_HEIGHT: f32 = 1050.0;
    const MIN_WIDTH: f32 = 1040.0;
    const MIN_HEIGHT: f32 = 720.0;

    let visible = cx.primary_display().map(|display| display.visible_bounds());
    let available =
        visible.map_or(size(px(PREFERRED_WIDTH), px(PREFERRED_HEIGHT)), |bounds| bounds.size);
    let width = PREFERRED_WIDTH.min(f32::from(available.width));
    let height = PREFERRED_HEIGHT.min(f32::from(available.height));
    let minimum = size(
        px(MIN_WIDTH.min(f32::from(available.width))),
        px(MIN_HEIGHT.min(f32::from(available.height))),
    );
    let preferred = size(px(width), px(height));
    let saved = crate::settings::state::load().geometry;
    let bounds =
        saved.filter(|geometry| geometry.width >= 1040 && geometry.height >= 720).map_or_else(
            || Bounds::centered(None, preferred, cx),
            |geometry| {
                let saved_width = logical_i32(geometry.width)
                    .min(f32::from(available.width))
                    .max(f32::from(minimum.width));
                let saved_height = logical_i32(geometry.height)
                    .min(f32::from(available.height))
                    .max(f32::from(minimum.height));
                let saved_size = size(px(saved_width), px(saved_height));
                let Some(display) = visible else {
                    return Bounds::centered(None, saved_size, cx);
                };
                let left = f32::from(display.origin.x);
                let top = f32::from(display.origin.y);
                let right = left + f32::from(display.size.width) - saved_width;
                let bottom = top + f32::from(display.size.height) - saved_height;
                let x = logical_i32(geometry.x).clamp(left, right.max(left));
                let y = logical_i32(geometry.y).clamp(top, bottom.max(top));
                Bounds::new(point(px(x), px(y)), saved_size)
            },
        );
    match cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Scribe Settings".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            app_id: Some("scribe-client".to_owned()),
            window_min_size: Some(minimum),
            window_decorations: Some(WindowDecorations::Client),
            window_background: WindowBackgroundAppearance::Opaque,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(SettingsWindow::new);
            // GPUI dispatches key events along the path built from the focused
            // node, so until something is focused the path excludes
            // `#settings-root` and its `on_key_down` never runs. Focusing the
            // root here is what makes Ctrl+K and keyboard traversal live from
            // the first frame instead of after an incidental click.
            let root = view.read(cx).focus_handle.clone();
            window.focus(&root, cx);
            view
        },
    ) {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::error!(%error, "failed to open GPUI settings window");
            None
        }
    }
}

fn logical_i32(value: i32) -> f32 {
    f32::from(i16::try_from(value.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).unwrap_or(0))
}
