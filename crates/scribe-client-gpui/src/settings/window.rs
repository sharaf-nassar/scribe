//! The GPUI settings window view.
//!
//! Renders the ten-page [`crate::settings::model`] onto a GPUI view: a sidebar
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

use std::time::Duration;

use gpui::{
    App, Bounds, Context, FocusHandle, Rgba, TitlebarOptions, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, size,
};
use scribe_common::config::{ScribeConfig, load_config, resolve_theme};
use serde_json::{Value, json};

use crate::settings::model::{Control, ControlKind, SettingsPage, page_controls};
use crate::settings::server_action;
use crate::settings::values::{current_value, keybinding_combos};
use crate::tab_bar::srgba;

/// One-shot server-action timeout for the releases page (update check / release
/// list). Short so a click never hangs the UI thread if the server is down.
const SERVER_ACTION_TIMEOUT: Duration = Duration::from_secs(3);

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
    focus_handle: FocusHandle,
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
            focus_handle: cx.focus_handle(),
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
        cx.notify();
    }

    fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        let current = current_value(&self.config, key).as_bool().unwrap_or(false);
        self.commit(key, Value::Bool(!current), cx);
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

    fn run_action(&mut self, key: &str, cx: &mut Context<Self>) {
        match key {
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
            .track_focus(&self.focus_handle)
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
            let bg = if selected { colors.nav_active_bg } else { colors.nav_bg };
            let fg = if selected { colors.text } else { colors.dim_text };
            div()
                .id(("settings-nav", page as usize))
                .w_full()
                .px_3()
                .py_2()
                .text_sm()
                .bg(bg)
                .text_color(fg)
                .hover(move |s| s.bg(colors.nav_active_bg))
                .on_click(cx.listener(move |this, _, _win, ctx| this.select_page(page, ctx)))
                .child(page.nav_label())
                .into_any_element()
        });
        div()
            .w(px(200.0))
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
                .px_4()
                .py_3()
                .text_lg()
                .text_color(colors.text)
                .child(self.page.nav_label())
                .into_any_element(),
        );
        children.extend(rows);
        if let Some(status) = &self.status {
            children.push(
                div()
                    .px_4()
                    .py_2()
                    .mt_2()
                    .text_sm()
                    .text_color(colors.accent)
                    .child(status.clone())
                    .into_any_element(),
            );
        }

        div()
            .id("settings-content")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .bg(colors.page_bg)
            .children(children)
            .into_any_element()
    }

    fn render_control(&self, control: &Control, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let label = div().flex_1().text_sm().text_color(colors.text).child(control.label.clone());
        let value_widget = self.render_value_widget(control, cx);
        div()
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
        match &control.kind {
            ControlKind::Toggle => {
                let on = current_value(&self.config, &control.key).as_bool().unwrap_or(false);
                let key = control.key.clone();
                let bg = if on { colors.accent } else { colors.control_bg };
                let text = if on { "On" } else { "Off" };
                pill(text, bg, colors.text)
                    .id(("toggle", key_hash(&control.key)))
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
                div().text_sm().text_color(colors.dim_text).child(shown).into_any_element()
            }
            ControlKind::Keybinding => {
                let combos = keybinding_combos(&self.config, &control.key);
                let shown = if combos.is_empty() { "—".to_owned() } else { combos.join(", ") };
                div().text_sm().text_color(colors.dim_text).child(shown).into_any_element()
            }
            ControlKind::Action => {
                let key = control.key.clone();
                pill(&control.label, colors.accent, colors.text)
                    .id(("action", key_hash(&control.key)))
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
        let (min, max, step) = bounds;
        let current = current_value(&self.config, &control.key).as_f64().unwrap_or(min);
        let display = format!("{current:.*}", decimals as usize);
        let key_dec = control.key.clone();
        let key_inc = control.key.clone();
        let minus = pill("−", colors.control_bg, colors.text)
            .id(("dec", key_hash(&control.key)))
            .on_click(cx.listener(move |this, _, _win, ctx| {
                this.step(&key_dec, (min, max), -step, ctx);
            }));
        let plus = pill("+", colors.control_bg, colors.text)
            .id(("inc", key_hash(&control.key)))
            .on_click(cx.listener(move |this, _, _win, ctx| {
                this.step(&key_inc, (min, max), step, ctx);
            }));
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(minus)
            .child(div().min_w(px(56.0)).text_sm().text_color(colors.text).child(display))
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
        row.child(div().text_sm().text_color(colors.dim_text).child(shown)).into_any_element()
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
pub fn open_settings_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(820.0), px(620.0)), cx);
    if let Err(error) = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Scribe Settings".into()),
                ..Default::default()
            }),
            app_id: Some("scribe-settings".to_owned()),
            ..Default::default()
        },
        |_, cx| cx.new(SettingsWindow::new),
    ) {
        tracing::error!(%error, "failed to open GPUI settings window");
    }
}
