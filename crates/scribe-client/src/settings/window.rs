//! The GPUI settings window view.
//!
//! Renders the eleven-page [`crate::settings::model`] onto a GPUI view: a sidebar
//! nav plus a scrollable content pane whose controls read their current value
//! from the loaded [`ScribeConfig`] via [`crate::settings::values`] and write
//! edits back through the ported [`crate::settings::apply::apply_settings_change`]
//! path. Interactive controls commit immediately except closed theme-preset
//! cycling, which updates its label at once and coalesces repeated arrow steps;
//! the file watcher in the running client picks writes up as `ConfigReloaded`.
//!
//! Colors open one anchored preset/custom palette; its exact-value field and
//! general free-text controls share one inline editor. Enter commits through
//! the same apply path every other control uses and Escape restores the value
//! the edit opened with. Keybinding rows list every action's combos as key caps
//! and record a replacement from the keyboard: activating one puts it in
//! listening state, and the next keystroke is written through the same path.
//!
//! The content pane carries the terminal's own overlay scrollbar
//! ([`crate::scrollbar`]) as its page-length affordance: the same thumb
//! geometry and the same fade-in-on-scroll, fade-out-after-idle state, driven
//! from [`SettingsWindow::tick_content_scrollbar`] on the render pass, and the
//! same pointer gestures — hover widens the thumb and pins the overlay, a press
//! on the thumb drags it, and a press elsewhere on the track jumps the page.
//!
//! Two pages additionally talk to the local server through
//! [`crate::settings::server_action`]: `Environment` gates its env-persistence
//! toggle on an `EnvPreflight` probe, and `Remote` renders the feature-014 LAN
//! trust surface (`GetLanEnv`, `ListTrustedNetworks`, `ListTrustedDevices`) with
//! `AddCurrentNetworkTrusted` / `RemoveTrustedNetwork` / `RevokeTrustedDevice`
//! mutations, plus the feature-013 tailnet summary (`GetRemoteEnv`). Every one
//! of those calls is reached from [`SettingsWindow::run_action`].
//!
//! THESIS: Settings reads like impeccably typeset technical documentation —
//! the man-page tradition of the audience's own world — refusing the boxed
//! line-soup of category-default settings chrome.
//! OWN-WORLD: Typeset Ink. One unified deep-ink ground; hierarchy carried by
//! type scale and spacing, not containers; hairline seams only at structural
//! boundaries; monospace reserved for real data (paths, hex, combos, numbers);
//! amber reserved for live state (selection, focus, on, status).
//! STORY: Grouped contents establish scope, search shortens retrieval, and
//! each page reads as one typeset column of term–value entries whose controls
//! commit instantly.
//! FIRST VIEWPORT: Titlebar, contents column with search, page title with its
//! config-source line, and the first two sections legible at 1040×720.
//! FORM: Typeset documentation grammar — candidate 7 of the grounded list,
//! seed 8145ae64. Dealt challengers (desk instrument, tape deck, dive,
//! origami) lost on Operate product clarity; the desk instrument survives as
//! control-precision detail only.
//! FINISH: unreviewed and undocumented is unfinished; this build ends with
//! the finish review, the verdict, and DESIGN.md.

use std::{cell::Cell, ops::Range, rc::Rc, time::Duration};

use gpui::{
    AccessibleAction, App, Bounds, Context, CursorStyle, Decorations, ElementInputHandler,
    EntityInputHandler, FocusHandle, FontWeight, HitboxBehavior, KeyDownEvent, Keystroke,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathPromptOptions,
    Pixels, Point, ResizeEdge, Rgba, Role, ScrollHandle, Size, Text, Tiling, TitlebarOptions,
    Toggled, UTF16Selection, Window, WindowBackgroundAppearance, WindowBounds, WindowControlArea,
    WindowDecorations, WindowHandle, WindowOptions, canvas, div, fill, point, prelude::*, px, rgb,
    rgba, size,
};
use scribe_common::config::{ScribeConfig, load_config};
use scribe_common::protocol::{
    PreflightError, Release, ReleaseListResultState, TrustedDeviceInfo, TrustedNetworkInfo,
    UpdateCheckResultState,
};
use scribe_common::theme::{Theme, all_preset_names, resolve_preset};
use serde_json::{Value, json};

use crate::app_shortcuts::CloseWindow;
use crate::keybindings::Keybinding;
use crate::layout::Rect;
use crate::scrollbar::{
    CommandMarkColors, SCROLLBAR_WIDTH, ScrollMetrics, ScrollbarDrag, ScrollbarLayout,
    ScrollbarQuad, ScrollbarState, ScrollbarStyle, build_scrollbar_render, hit_test_scrollbar,
    hit_test_thumb, offset_from_drag, offset_from_track_click, round_scroll_units,
};
use crate::settings::apply::{canonical_color_value, workspace_root_from_value};
use crate::settings::model::{
    ADD_CURRENT_NETWORK_ACTION, Control, ControlKind, ENV_PERSISTENCE_KEY, ENV_PREFLIGHT_ACTION,
    REFRESH_TRUST_ACTION, REMOVE_TRUSTED_NETWORK_PREFIX, REVOKE_TRUSTED_DEVICE_PREFIX,
    SettingsPage, keybinding_actions, keybinding_label, page_controls,
    workspace_badge_color_controls,
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

/// Idle window that folds repeated closed theme-preset steps into one write.
const THEME_PRESET_DEBOUNCE: Duration = Duration::from_millis(250);

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

/// Ceiling on the whole-pixel scroll units the content pane's scrollbar
/// geometry counts in. A settings page is nowhere near 65 535 pixels tall, and
/// the cap is what keeps the pixel-to-unit conversion cast-free.
const UNIT_CAP: usize = 65_535;

/// Smallest composition that keeps the fixed navigation and control columns
/// visible without clipping their labels.
const SETTINGS_MIN_WIDTH: f32 = 1040.0;
const SETTINGS_MIN_HEIGHT: f32 = 720.0;

/// Native macOS traffic lights occupy a 28px titlebar band; 38px clears them
/// and gives the seamless ground-colored band a calm cross-platform height.
const SETTINGS_TITLEBAR_HEIGHT: f32 = 38.0;

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

/// Fixed GPUI colors for the settings chrome — the Typeset Ink palette.
///
/// One unified ground carries the titlebar, sidebar, and content; hairline
/// alpha strokes mark structural boundaries; amber is spent only on live
/// state. Hover and selection washes are white-alpha tints so they read
/// identically over every surface.
#[derive(Clone, Copy)]
struct SettingsColors {
    /// The single interior ground every region shares.
    page_bg: Rgba,
    /// Fill for the client-decoration resize gutter — a matte one step below
    /// the ground so the frame reads as window chrome.
    frame_bg: Rgba,
    /// Selected contents-row wash.
    nav_active_bg: Rgba,
    /// Hovered contents-row wash.
    nav_hover_bg: Rgba,
    /// Hovered settings-row wash.
    row_hover_bg: Rgba,
    /// Raised surface for click targets (choices, steppers, buttons).
    control_bg: Rgba,
    control_hover_bg: Rgba,
    control_pressed_bg: Rgba,
    /// Engraved inset for text entry (search, paths, hex fields).
    input_bg: Rgba,
    /// Elevated surface for the anchored choice menu.
    menu_bg: Rgba,
    /// Ground for the pinned status bar.
    status_bg: Rgba,
    /// Hairline stroke for structural seams and control outlines.
    border: Rgba,
    /// One step firmer hairline for the window edge and emphasized outlines.
    strong_border: Rgba,
    accent: Rgba,
    /// Ink for validation failures — a separate channel from amber, so a
    /// rejected edit never scans like live state.
    error: Rgba,
    text: Rgba,
    dim_text: Rgba,
    quiet_text: Rgba,
    /// The content pane's overlay scrollbar thumb — a quiet white wash, not
    /// amber: page length is structure, not live state.
    scrollbar: Rgba,
}

impl SettingsColors {
    fn resolve(_config: &ScribeConfig) -> Self {
        // Settings must remain a stable instrument while the user edits the
        // terminal theme, so none of these roles derive from the active preset.
        let page_bg = rgb(0x0014_1518);
        let accent = rgb(0x00f5_b83a);
        let text = rgb(0x00e9_e8e4);
        Self {
            page_bg,
            frame_bg: rgb(0x000b_0c0e),
            nav_active_bg: rgba(0xffff_ff0f),
            nav_hover_bg: rgba(0xffff_ff08),
            row_hover_bg: rgba(0xffff_ff06),
            control_bg: rgb(0x001d_1f24),
            control_hover_bg: rgb(0x0026_2931),
            control_pressed_bg: rgb(0x0019_1b1f),
            input_bg: rgb(0x0010_1114),
            menu_bg: rgb(0x0022_242a),
            status_bg: rgb(0x001a_1b1f),
            border: rgba(0xffff_ff14),
            strong_border: rgba(0xffff_ff1f),
            accent,
            error: rgb(0x00e0_584c),
            text,
            dim_text: rgb(0x00a6_a5a0),
            quiet_text: rgb(0x0083_827b),
            scrollbar: rgba(0xffff_ff3d),
        }
    }
}

/// The settings window view: a page selector plus the live-editing content pane.
pub struct SettingsWindow {
    config: ScribeConfig,
    theme: Theme,
    theme_presets: Vec<PresetEntry>,
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
    choice_filter_handle: FocusHandle,
    choice_filter: String,
    choice_filter_marked_range: Option<Range<usize>>,
    workspace_root_handle: FocusHandle,
    workspace_root_input: String,
    workspace_root_marked_range: Option<Range<usize>>,
    workspace_root_error: Option<String>,
    /// The one inline text editor, shared by every color and free-text control:
    /// at most one row is in edit at a time, so the field, its opening value
    /// (the Escape target), and its rejection ink are single-slot state.
    edit_handle: FocusHandle,
    edit_input: String,
    edit_original: String,
    /// The control the open edit belongs to. The whole control rather than its
    /// key, because the commit path needs its kind: a color canonicalizes
    /// before it is written, general free text does not.
    edit_control: Option<Control>,
    edit_marked_range: Option<Range<usize>>,
    edit_error: Option<String>,
    /// The keybinding action whose row is listening for a keystroke, and the
    /// reason the last capture was refused. Single-slot for the same reason the
    /// inline editor is: one row records at a time.
    capture_action: Option<String>,
    capture_error: Option<String>,
    active_input: Option<NativeInputTarget>,
    input_selection: NativeInputSelection,
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
    /// Token of the open menu row moved by Up/Down. This stays separate from
    /// the applied value so keyboard browsing never previews or commits early.
    choice_highlight: Option<String>,
    /// Closed `theme.preset` cycling paints this value before it reaches disk.
    pending_theme_preset: Option<String>,
    /// Bumped by every step, flush, or discard so a stale timer cannot apply.
    theme_preset_generation: u64,
    /// Dropping a superseded task cancels its timer, matching the find overlay.
    theme_preset_task: Option<gpui::Task<()>>,
    /// Scroll position of that dropdown. Shared because only one is ever open,
    /// and rewound on open so the live value is on screen even in the ~190-entry
    /// theme preset list.
    choice_scroll: ScrollHandle,
    /// Config key of the color selector whose anchored palette is open.
    open_color: Option<String>,
    /// Hue shown by the custom saturation/brightness palette.
    color_hue: f32,
    /// The content pane's page-length affordance: the terminal's own overlay
    /// scrollbar state, so the thumb fades in on scroll and out after idle
    /// exactly as it does in a pane.
    content_scrollbar: ScrollbarState,
    /// Scroll distance from the top, in whole pixels, as of the last render.
    /// A change is what pulses the thumb — the scroller applies the wheel
    /// itself, so there is no scroll event to hang the pulse off.
    content_scrolled: usize,
}

struct PresetEntry {
    token: &'static str,
    label: String,
    theme: Theme,
}

fn build_theme_preset_cache() -> Vec<PresetEntry> {
    let mut presets = all_preset_names()
        .into_iter()
        .filter_map(|token| {
            resolve_preset(token).map(|theme| PresetEntry {
                token,
                label: choice_label(token, token),
                theme,
            })
        })
        .collect::<Vec<_>>();
    presets.sort_unstable_by(|left, right| left.theme.name.as_ref().cmp(right.theme.name.as_ref()));
    presets
}

fn preset_strip_colors(theme: &Theme) -> [[f32; 4]; 10] {
    [
        theme.background,
        theme.foreground,
        theme.ansi_colors[0],
        theme.ansi_colors[1],
        theme.ansi_colors[2],
        theme.ansi_colors[3],
        theme.ansi_colors[4],
        theme.ansi_colors[5],
        theme.ansi_colors[6],
        theme.ansi_colors[7],
    ]
}

fn theme_preset_preview(
    config: &ScribeConfig,
    key: &str,
    current: &str,
    option: &str,
    presets: &[PresetEntry],
) -> Option<[[f32; 4]; 10]> {
    if key != "theme.preset" {
        return None;
    }
    if option == "custom" {
        return (current == "custom")
            .then(|| preset_strip_colors(&scribe_common::config::resolve_theme(config)));
    }
    presets
        .iter()
        .find(|preset| preset.token == option)
        .map(|preset| preset_strip_colors(&preset.theme))
}

/// One focus stop in the settings window's stable keyboard traversal order.
/// The sidebar comes first, followed by actionable controls on the selected
/// page and (on Remote) every live trust mutation row.
// @lat: [[settings#GPUI Settings Window#Page model]]
#[derive(Clone)]
enum SettingsFocusTarget {
    Page(SettingsPage),
    Control(Control),
    PromptBarColorReset(String),
    Action(String),
    WorkspaceRootInput,
    WorkspaceRootBrowse,
    WorkspaceRootAdd,
    WorkspaceRootRemove { index: usize, root: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NativeInputTarget {
    WorkspaceRoot,
    /// The shared inline editor behind every color and free-text control row.
    Inline,
    ChoiceFilter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DismissedTransient {
    ChoiceFilter,
    ChoiceMenu,
    PageSearch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChoiceMenuKey {
    Previous,
    Next,
    Apply,
    Dismiss,
    Swallow,
}

fn replace_pending_theme_preset(
    pending: &mut Option<String>,
    generation: &mut u64,
    value: String,
) -> u64 {
    *pending = Some(value);
    *generation = generation.wrapping_add(1);
    *generation
}

fn take_pending_theme_preset(
    pending: &mut Option<String>,
    generation: &mut u64,
    expected_generation: Option<u64>,
) -> Option<String> {
    if expected_generation.is_some_and(|expected| expected != *generation) {
        return None;
    }
    *generation = generation.wrapping_add(1);
    pending.take()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeInputSelection {
    Caret,
    All,
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
        (
            SettingsFocusTarget::PromptBarColorReset(a),
            SettingsFocusTarget::PromptBarColorReset(b),
        )
        | (SettingsFocusTarget::Action(a), SettingsFocusTarget::Action(b)) => a == b,
        (SettingsFocusTarget::WorkspaceRootInput, SettingsFocusTarget::WorkspaceRootInput)
        | (SettingsFocusTarget::WorkspaceRootBrowse, SettingsFocusTarget::WorkspaceRootBrowse)
        | (SettingsFocusTarget::WorkspaceRootAdd, SettingsFocusTarget::WorkspaceRootAdd) => true,
        (
            SettingsFocusTarget::WorkspaceRootRemove { index: a, root: a_root },
            SettingsFocusTarget::WorkspaceRootRemove { index: b, root: b_root },
        ) => a == b && a_root == b_root,
        _ => false,
    }
}

fn is_prompt_bar_color_override(key: &str) -> bool {
    matches!(
        key,
        "appearance.prompt_bar_first_row_bg"
            | "appearance.prompt_bar_second_row_bg"
            | "appearance.prompt_bar_text"
            | "appearance.prompt_bar_icon_first"
            | "appearance.prompt_bar_icon_latest"
    )
}

fn push_control_focus_targets(targets: &mut Vec<SettingsFocusTarget>, control: Control) {
    let reset_key = is_prompt_bar_color_override(&control.key).then(|| control.key.clone());
    targets.push(SettingsFocusTarget::Control(control));
    if let Some(key) = reset_key {
        targets.push(SettingsFocusTarget::PromptBarColorReset(key));
    }
}

fn prompt_bar_reset_change(key: &str) -> (&str, &'static str) {
    (key, "")
}

fn workspace_root_focus_index(
    targets: &[SettingsFocusTarget],
    intended: &SettingsFocusTarget,
) -> Option<usize> {
    targets
        .iter()
        .position(|target| focus_targets_match(target, intended))
        .or_else(|| {
            let SettingsFocusTarget::WorkspaceRootRemove { index, .. } = intended else {
                return None;
            };
            targets.iter().position(|target| {
                matches!(
                    target,
                    SettingsFocusTarget::WorkspaceRootRemove { index: candidate, .. }
                        if candidate == index
                )
            })
        })
        .or_else(|| {
            matches!(intended, SettingsFocusTarget::WorkspaceRootRemove { .. }).then(|| {
                targets
                    .iter()
                    .position(|target| matches!(target, SettingsFocusTarget::WorkspaceRootInput))
            })?
        })
}

fn release_inline_input(
    active_input: &mut Option<NativeInputTarget>,
    input_selection: &mut NativeInputSelection,
) -> bool {
    if *active_input != Some(NativeInputTarget::Inline) {
        return false;
    }
    *active_input = None;
    *input_selection = NativeInputSelection::Caret;
    true
}

/// The value an inline edit commits: a color canonicalizes through the apply
/// path's own hex/ansi validator (so a rejected color never reaches the config
/// writer), and a general free-text value commits verbatim — the apply path is
/// the single authority on what each key accepts, so there is no second
/// validator here.
fn inline_commit_value(is_color: bool, key: &str, input: &str) -> Result<String, String> {
    if is_color {
        canonical_color_value(key, &Value::String(input.to_owned()))
    } else {
        Ok(input.to_owned())
    }
}

/// A keystroke that names only a modifier: a recording row waits through it
/// rather than reading Ctrl-on-its-own as the shortcut.
fn is_modifier_key(key: &str) -> bool {
    matches!(
        key,
        "ctrl"
            | "control"
            | "shift"
            | "alt"
            | "cmd"
            | "super"
            | "platform"
            | "win"
            | "function"
            | "fn"
            | "capslock"
            | "numlock"
    )
}

/// Whether none of the four real modifiers were held.
const fn is_unmodified(modifiers: Modifiers) -> bool {
    !(modifiers.control || modifiers.alt || modifiers.shift || modifiers.platform)
}

/// Spell a captured keystroke in the combo grammar
/// [`crate::keybindings::Keybinding::parse`] reads back: modifiers in a fixed
/// order, then the layout base key GPUI reports.
fn combo_from_keystroke(keystroke: &Keystroke) -> String {
    let mut combo = String::new();
    for (held, name) in [
        (keystroke.modifiers.control, "ctrl"),
        (keystroke.modifiers.alt, "alt"),
        (keystroke.modifiers.shift, "shift"),
        (keystroke.modifiers.platform, "cmd"),
    ] {
        if held {
            combo.push_str(name);
            combo.push('+');
        }
    }
    combo.push_str(&keystroke.key.to_lowercase());
    combo
}

/// The canonical spelling of a combo, or `None` when the client's own parser
/// cannot bind it.
///
/// Both sides of a conflict check go through here, so `super+ctrl+w` written by
/// hand in `config.toml` and `ctrl+cmd+w` captured from the keyboard compare
/// equal. The parse call is what keeps the settings window from inventing a
/// second opinion about which keys bind: an F-key or an unknown name is
/// rejected here because the runtime dispatcher would drop it anyway.
fn canonical_combo(combo: &str) -> Option<String> {
    Keybinding::parse(combo)?;
    let (mut ctrl, mut alt, mut shift, mut cmd) = (false, false, false, false);
    let mut key = None;
    for part in combo.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "cmd" | "super" => cmd = true,
            other => key = Some(other.to_owned()),
        }
    }
    let mut canonical = String::new();
    for (held, name) in [(ctrl, "ctrl"), (alt, "alt"), (shift, "shift"), (cmd, "cmd")] {
        if held {
            canonical.push_str(name);
            canonical.push('+');
        }
    }
    canonical.push_str(&key?);
    Some(canonical)
}

/// The combo a captured keystroke binds to, or the reason it cannot.
///
/// A shortcut without Ctrl, Alt, or Super is refused because the terminal would
/// never see that key again — plain `t` bound to a layout action makes the
/// letter untypeable in every pane, which is not a mistake the settings window
/// should be able to commit.
fn combo_for_capture(keystroke: &Keystroke) -> Result<String, String> {
    let modifiers = keystroke.modifiers;
    if !(modifiers.control || modifiers.alt || modifiers.platform) {
        return Err("Shortcuts need Ctrl, Alt, or Super — anything less stays with the terminal."
            .to_owned());
    }
    let combo = combo_from_keystroke(keystroke);
    canonical_combo(&combo)
        .ok_or_else(|| format!("{} cannot be used in a shortcut.", key_cap_label(&keystroke.key)))
}

/// The keybinding action `combo` is already bound to, ignoring `recording`
/// itself so re-pressing a row's current shortcut is not a conflict.
fn conflicting_action(config: &ScribeConfig, combo: &str, recording: &str) -> Option<&'static str> {
    let canonical = canonical_combo(combo)?;
    keybinding_actions().into_iter().find(|action| {
        *action != recording
            && keybinding_combos(config, action)
                .iter()
                .any(|existing| canonical_combo(existing).as_deref() == Some(canonical.as_str()))
    })
}

/// The reader-facing name of one combo token: `ctrl` → `Ctrl`, `pageup` →
/// `Page Up`, `t` → `T`.
fn key_cap_label(token: &str) -> String {
    let named = match token.to_lowercase().as_str() {
        "ctrl" => "Ctrl",
        "shift" => "Shift",
        "alt" => "Alt",
        "cmd" | "super" => {
            if cfg!(target_os = "macos") {
                "Cmd"
            } else {
                "Super"
            }
        }
        "escape" | "esc" => "Esc",
        "enter" | "return" => "Enter",
        "space" => "Space",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "tab" => "Tab",
        "pageup" => "Page Up",
        "pagedown" => "Page Down",
        "home" => "Home",
        "end" => "End",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        _ => return token.to_uppercase(),
    };
    named.to_owned()
}

/// A whole combo in reader-facing words, for status lines and screen readers.
fn key_combo_text(combo: &str) -> String {
    combo.split('+').map(key_cap_label).collect::<Vec<_>>().join(" ")
}

/// Cancel an inline edit: the field returns to the value it opened with, and
/// the rejection ink clears with it, so an abandoned edit leaves nothing of
/// itself on screen.
fn revert_inline_input(
    input: &mut String,
    original: &str,
    marked_range: &mut Option<Range<usize>>,
    error: &mut Option<String>,
) {
    original.clone_into(input);
    *marked_range = None;
    *error = None;
}

impl SettingsWindow {
    /// Build the view, loading the current config (or defaults on failure).
    pub fn new(cx: &mut Context<Self>) -> Self {
        let config = load_config().unwrap_or_default();
        let theme = scribe_common::config::resolve_theme(&config);
        let theme_presets = build_theme_preset_cache();
        let colors = SettingsColors::resolve(&config);
        let mut settings = Self {
            config,
            theme,
            theme_presets,
            colors,
            page: SettingsPage::Appearance,
            status: None,
            trust: TrustState::default(),
            focus_handle: cx.focus_handle(),
            search_handle: cx.focus_handle().tab_index(0),
            search_query: String::new(),
            choice_filter_handle: cx.focus_handle(),
            choice_filter: String::new(),
            choice_filter_marked_range: None,
            workspace_root_handle: cx.focus_handle().tab_index(0),
            workspace_root_input: String::new(),
            workspace_root_marked_range: None,
            workspace_root_error: None,
            edit_handle: cx.focus_handle().tab_index(0),
            edit_input: String::new(),
            edit_original: String::new(),
            edit_control: None,
            edit_marked_range: None,
            edit_error: None,
            capture_action: None,
            capture_error: None,
            active_input: None,
            input_selection: NativeInputSelection::Caret,
            focus_index: 0,
            keyboard_navigation: false,
            resize_edge: None,
            should_move: false,
            scroll_handle: ScrollHandle::new(),
            open_choice: None,
            choice_highlight: None,
            pending_theme_preset: None,
            theme_preset_generation: 0,
            theme_preset_task: None,
            choice_scroll: ScrollHandle::new(),
            open_color: None,
            color_hue: 0.0,
            content_scrollbar: ScrollbarState::new(),
            content_scrolled: 0,
        };
        // Show the thumb once on open, the way a page switch does: the whole
        // point of the affordance is telling the reader the page runs past the
        // fold before they have scrolled to find out.
        settings.pulse_content_scrollbar();
        settings
    }

    /// Reveal the content pane's scrollbar and re-arm its idle timer.
    fn pulse_content_scrollbar(&mut self) {
        self.content_scrollbar.on_scroll_action();
    }

    /// The content pane's scrollbar placement, in coordinates relative to the
    /// scroller. Paint and every pointer hit test resolve through this, so they
    /// can never disagree about where the track is.
    ///
    /// The pure geometry counts scroll units from the live bottom; a pixel
    /// scroller is that same shape with one pixel as the unit.
    fn content_scrollbar_layout(&self) -> ScrollbarLayout {
        let viewport = self.scroll_handle.bounds().size;
        let overflow = round_scroll_units(f32::from(self.scroll_handle.max_offset().y), UNIT_CAP);
        let scrolled = round_scroll_units(f32::from(-self.scroll_handle.offset().y), overflow);
        ScrollbarLayout {
            pane_rect: Rect {
                x: 0.0,
                y: 0.0,
                width: f32::from(viewport.width),
                height: f32::from(viewport.height),
            },
            metrics: ScrollMetrics {
                history_size: overflow,
                screen_lines: round_scroll_units(f32::from(viewport.height), UNIT_CAP),
                display_offset: overflow.saturating_sub(scrolled),
            },
            tab_bar_height: 0.0,
        }
    }

    /// Advance the content pane's overlay-scrollbar animations and return the
    /// thumb quad, in coordinates relative to the scroller, or `None` while it
    /// is invisible or the page fits.
    ///
    /// Routed through [`build_scrollbar_render`] rather than `compute_thumb`
    /// because that is where the shared module drives the hover width target;
    /// the settings pane has no command marks, so the tick list comes back
    /// empty and only the thumb is painted. The fade is wall-clock, so a
    /// visible thumb asks for the next animation frame — the scroller itself
    /// only repaints when the offset changes.
    fn tick_content_scrollbar(&mut self, window: &Window, cx: &App) -> Option<ScrollbarQuad> {
        let layout = self.content_scrollbar_layout();
        let scrolled = layout.metrics.history_size.saturating_sub(layout.metrics.display_offset);
        if scrolled != self.content_scrolled {
            self.content_scrolled = scrolled;
            self.pulse_content_scrollbar();
        }
        if cx.reduce_motion() {
            // Reduced motion keeps the affordance rather than the animation:
            // the thumb simply stays on an overflowing page, so nothing moves
            // and no animation frames are requested.
            self.content_scrollbar.opacity = 1.0;
        } else if self.content_scrollbar.tick_fade(layout.metrics.display_offset) {
            window.request_animation_frame();
        }
        let style = ScrollbarStyle {
            width: SCROLLBAR_WIDTH,
            color: [
                self.colors.scrollbar.r,
                self.colors.scrollbar.g,
                self.colors.scrollbar.b,
                self.colors.scrollbar.a,
            ],
            // Never read: the settings pane passes no command marks.
            command_mark_colors: CommandMarkColors { success: [0.0; 4], failure: [0.0; 4] },
        };
        build_scrollbar_render(&layout, &[], &mut self.content_scrollbar, &style)
            .map(|render| render.thumb)
    }

    /// A window pointer position in the content scroller's own coordinates —
    /// the space the scrollbar geometry is computed in.
    fn content_scrollbar_local(&self, position: Point<Pixels>) -> (f32, f32) {
        let origin = self.scroll_handle.bounds().origin;
        (f32::from(position.x - origin.x), f32::from(position.y - origin.y))
    }

    /// Track the pointer over the content pane's scrollbar hit zone.
    ///
    /// Hover pins the overlay open and widens the thumb, which is what makes it
    /// grabbable: the resting 6 px thumb is a hint, and the 3x hit zone plus
    /// the widen are what turn it into a control.
    fn hover_content_scrollbar(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let layout = self.content_scrollbar_layout();
        let (x, y) = self.content_scrollbar_local(position);
        let width = self.content_scrollbar.current_width(SCROLLBAR_WIDTH);
        let inside = hit_test_scrollbar(&layout, x, y, width.max(SCROLLBAR_WIDTH));
        if inside == self.content_scrollbar.hover {
            return;
        }
        if inside {
            self.content_scrollbar.on_hover_enter();
        } else {
            self.content_scrollbar.on_hover_leave();
        }
        cx.notify();
    }

    /// Claim a left press that landed on the content pane's scrollbar.
    ///
    /// A press on the thumb starts a drag; a press anywhere else in the hit
    /// zone jumps the page to that point on the track. Returns `true` when the
    /// press was consumed, so the caller stops it reaching the control it was
    /// painted over. A page that fits has no track to press: `hit_test_scrollbar`
    /// declines on an empty history, and the press stays the control's.
    fn press_content_scrollbar(&mut self, position: Point<Pixels>) -> bool {
        let layout = self.content_scrollbar_layout();
        let (x, y) = self.content_scrollbar_local(position);
        let width = self.content_scrollbar.current_width(SCROLLBAR_WIDTH);
        if !hit_test_scrollbar(&layout, x, y, width) {
            return false;
        }
        if hit_test_thumb(&layout, x, y, width) {
            self.content_scrollbar.drag = Some(ScrollbarDrag {
                start_mouse_y: y,
                start_display_offset: layout.metrics.display_offset,
            });
            // A drag holds the overlay open by itself; clearing the timer keeps
            // it from fading out from under the pointer mid-drag.
            self.content_scrollbar.opacity = 1.0;
            self.content_scrollbar.fade_start = None;
            return true;
        }
        self.scroll_content_to_offset(&layout, offset_from_track_click(&layout, y, width));
        true
    }

    /// Continue an in-flight thumb drag. Returns `true` while a drag owns the
    /// pointer, so the move never doubles as a hover update.
    fn drag_content_scrollbar(&mut self, position: Point<Pixels>) -> bool {
        let Some(drag) = self.content_scrollbar.drag else { return false };
        let layout = self.content_scrollbar_layout();
        let (_, y) = self.content_scrollbar_local(position);
        let width = self.content_scrollbar.current_width(SCROLLBAR_WIDTH);
        self.scroll_content_to_offset(&layout, offset_from_drag(&layout, &drag, y, width));
        true
    }

    /// Finish a thumb drag, re-arming the fade unless the pointer is still
    /// hovering. Returns `true` when a drag was actually in flight.
    fn release_content_scrollbar(&mut self, cx: &mut Context<Self>) -> bool {
        if self.content_scrollbar.drag.is_none() {
            return false;
        }
        self.content_scrollbar.on_drag_end();
        cx.notify();
        true
    }

    /// Move the scroller to an absolute `display_offset` a scrollbar gesture
    /// resolved to.
    fn scroll_content_to_offset(&mut self, layout: &ScrollbarLayout, target: usize) {
        let offset = content_scroll_offset(layout.metrics.history_size, target);
        self.scroll_handle.set_offset(point(self.scroll_handle.offset().x, offset));
    }

    /// Reload the config from disk after an edit so the UI reflects the saved
    /// state (including any clamping the apply path performed).
    fn reload(&mut self) {
        if let Ok(config) = load_config() {
            self.colors = SettingsColors::resolve(&config);
            self.theme = scribe_common::config::resolve_theme(&config);
            self.config = config;
            self.theme_presets = build_theme_preset_cache();
        }
    }

    fn controls_for_page(&self, page: SettingsPage) -> Vec<Control> {
        let mut controls = page_controls(page);
        if page == SettingsPage::Workspaces {
            let mut badge_colors =
                workspace_badge_color_controls(self.config.workspaces.badge_colors.len());
            badge_colors.append(&mut controls);
            badge_colors
        } else {
            controls
        }
    }

    /// The product-language name of a committed key for the status line: the
    /// control's own label when the selected page declares one, a plain name
    /// for the structural workspace mutations that have no control row, and a
    /// humanized last key segment as the fallback.
    fn commit_label(&self, key: &str) -> String {
        match key {
            "workspaces.add_root" | "workspaces.remove_root" => "Workspace root".to_owned(),
            "workspaces.reset_badge_colors" => "Badge colors".to_owned(),
            // A keybinding commits under `keybindings.<action>` while its row is
            // keyed on the bare action, so the prefix comes off before the
            // lookup or every shortcut would fall back to a generated name.
            _ => {
                let control_key = key.strip_prefix("keybindings.").unwrap_or(key);
                self.controls_for_page(self.page)
                    .into_iter()
                    .find(|control| control.key == control_key)
                    .map_or_else(
                        || humanize_choice_token(key.rsplit('.').next().unwrap_or(key)),
                        |control| control.label,
                    )
            }
        }
    }

    /// The success line for a committed key, in the product's own words
    /// rather than the dotted config key the apply path consumes.
    fn commit_status(&self, key: &str) -> String {
        match key {
            "workspaces.add_root" => "Workspace root added.".to_owned(),
            "workspaces.remove_root" => "Workspace root removed.".to_owned(),
            "workspaces.reset_badge_colors" => "Badge colors reset to defaults.".to_owned(),
            _ => format!("Saved {}.", self.commit_label(key)),
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
                self.status = Some(self.commit_status(key));
                true
            }
            Err(e) => {
                self.status = Some(format!("{} was not saved — {e}", self.commit_label(key)));
                false
            }
        };
        cx.notify();
        applied
    }

    fn select_page(&mut self, page: SettingsPage, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_theme_preset(cx);
        if page != self.page {
            let edit_was_focused = self.edit_handle.is_focused(window);
            let edit_was_active = self.clear_inline_edit();
            if (edit_was_focused || edit_was_active) && !self.search_handle.is_focused(window) {
                window.focus(&self.focus_handle, cx);
            }
        }
        self.page = page;
        self.status = None;
        // A dropdown belongs to one control on one page; leaving the page must
        // not leave its menu floating over the next one, and a row that was
        // listening for a shortcut is not on screen to say so any more.
        self.clear_choice_menu_state();
        self.open_color = None;
        self.capture_action = None;
        self.capture_error = None;
        // The scroller is one retained element across every page, so without an
        // explicit rewind a short page inherits the previous page's offset and
        // opens blank below its own content.
        self.scroll_handle.set_offset(Point::default());
        self.content_scrolled = 0;
        self.pulse_content_scrollbar();
        if self.keyboard_navigation {
            self.focus_index =
                settings_nav_pages().iter().position(|candidate| *candidate == page).unwrap_or(0);
        }
        // Opening the Remote page pulls the LAN trust surface in the same way the
        // old webview's `inject_lan_state` did on load, so the lists are populated
        // before the user reaches for Remove/Revoke. Only the first visit auto-
        // refreshes; afterwards the explicit "Refresh trust state" action drives it.
        if page == SettingsPage::Remote && !self.trust.loaded {
            self.run_action(REFRESH_TRUST_ACTION, window, cx);
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
            if self.current_network_can_be_trusted() {
                targets.push(SettingsFocusTarget::Action(ADD_CURRENT_NETWORK_ACTION.to_owned()));
            }
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
        if self.page == SettingsPage::Workspaces && self.workspace_roots_match_search() {
            targets.extend(
                self.config
                    .workspaces
                    .roots
                    .iter()
                    .enumerate()
                    .filter(|(_, root)| self.workspace_root_matches_search(root))
                    .map(|(index, root)| SettingsFocusTarget::WorkspaceRootRemove {
                        index,
                        root: root.clone(),
                    }),
            );
            if self.workspace_root_controls_match_search() {
                targets.push(SettingsFocusTarget::WorkspaceRootInput);
                targets.push(SettingsFocusTarget::WorkspaceRootBrowse);
                targets.push(SettingsFocusTarget::WorkspaceRootAdd);
            }
        }
        for control in self.controls_for_page(self.page) {
            // A control its parent toggle has gated renders inert, so keyboard
            // traversal must skip it too rather than stop on a dead stop.
            if !self.control_matches_search(&control) || !self.control_is_enabled(&control.key) {
                continue;
            }
            push_control_focus_targets(&mut targets, control);
        }
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
        self.flush_theme_preset(cx);
        let had_visible_focus = self.keyboard_navigation;
        self.keyboard_navigation = false;
        self.focus_index = 0;
        // A press anywhere ends a recording: the row that was listening is not
        // where the user is looking any more. The keybinding row's own click
        // handler runs on release, so it still starts its own recording.
        let was_recording = self.cancel_capture(cx);
        if had_visible_focus && !was_recording {
            cx.notify();
        }
    }

    fn move_focus(&mut self, direction: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_theme_preset(cx);
        let count = self.focus_targets().len();
        if count > 0 {
            self.focus_index = if direction.is_negative() {
                (self.focus_index + count - 1) % count
            } else {
                (self.focus_index + 1) % count
            };
            self.keyboard_navigation = true;
            self.sync_focus_handle(window, cx);
            cx.notify();
        }
    }

    fn sync_focus_handle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.focused_target() {
            Some(SettingsFocusTarget::WorkspaceRootInput) => {
                self.active_input = Some(NativeInputTarget::WorkspaceRoot);
                self.input_selection = NativeInputSelection::Caret;
                window.focus(&self.workspace_root_handle, cx);
            }
            Some(SettingsFocusTarget::Control(control))
                if matches!(control.kind, ControlKind::Text) =>
            {
                self.begin_inline_edit(&control);
                window.focus(&self.edit_handle, cx);
            }
            _ => {
                self.active_input = None;
                self.input_selection = NativeInputSelection::Caret;
                window.focus(&self.focus_handle, cx);
            }
        }
    }

    /// The config key of the control the open inline edit belongs to.
    fn edit_key(&self) -> Option<&str> {
        self.edit_control.as_ref().map(|control| control.key.as_str())
    }

    /// Open the shared inline editor on a color or free-text control, seeding it
    /// with the saved value that Escape later restores.
    fn begin_inline_edit(&mut self, control: &Control) {
        let target = SettingsFocusTarget::Control(control.clone());
        if let Some(index) = self
            .focus_targets()
            .iter()
            .position(|candidate| focus_targets_match(candidate, &target))
        {
            self.focus_index = index;
        }
        if self.edit_key() != Some(control.key.as_str()) {
            let value = current_value(&self.config, &control.key).as_str().unwrap_or("").to_owned();
            self.edit_input.clone_from(&value);
            self.edit_original = value;
            self.edit_control = Some(control.clone());
            self.edit_marked_range = None;
            self.edit_error = None;
        }
        self.active_input = Some(NativeInputTarget::Inline);
        self.input_selection = NativeInputSelection::Caret;
    }

    /// Start listening for the keystroke that replaces a keybinding.
    ///
    /// Focus goes to the root so the recording row reads every key through
    /// [`SettingsWindow::on_key_down`] rather than through a text input: a
    /// shortcut is pressed, not typed.
    fn begin_capture(&mut self, action: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_inline_edit();
        self.capture_action = Some(action.to_owned());
        self.capture_error = None;
        self.status = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Stop listening, leaving the binding as it was. Returns whether a
    /// recording was actually running.
    fn cancel_capture(&mut self, cx: &mut Context<Self>) -> bool {
        let was_recording = self.capture_action.take().is_some();
        self.capture_error = None;
        if was_recording {
            cx.notify();
        }
        was_recording
    }

    /// Read one keystroke into the recording row.
    ///
    /// Modifier presses keep it listening, Escape abandons the recording, and a
    /// bare Backspace unbinds the action. Anything else is written as the
    /// action's combo once it parses, keeps a modifier the terminal can live
    /// without, and is not already spoken for — a refusal keeps the row
    /// listening with the reason on screen, so the next press is the fix.
    fn capture_keystroke(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(action) = self.capture_action.clone() else {
            return;
        };
        let key = event.keystroke.key.as_str();
        if is_modifier_key(key) {
            return;
        }
        let bare = is_unmodified(event.keystroke.modifiers);
        if bare && key == "escape" {
            self.cancel_capture(cx);
            return;
        }
        if bare && key == "backspace" {
            self.capture_action = None;
            self.capture_error = None;
            self.commit(&format!("keybindings.{action}"), Value::Array(Vec::new()), cx);
            return;
        }
        let combo = match combo_for_capture(&event.keystroke) {
            Ok(combo) => combo,
            Err(error) => {
                self.capture_error = Some(error);
                cx.notify();
                return;
            }
        };
        if let Some(other) = conflicting_action(&self.config, &combo, &action) {
            self.capture_error =
                Some(format!("Already bound to {}.", keybinding_label(other).to_lowercase()));
            cx.notify();
            return;
        }
        self.capture_action = None;
        self.capture_error = None;
        self.commit(&format!("keybindings.{action}"), Value::Array(vec![Value::String(combo)]), cx);
    }

    fn clear_inline_edit(&mut self) -> bool {
        let was_active = release_inline_input(&mut self.active_input, &mut self.input_selection);
        self.edit_control = None;
        self.edit_input.clear();
        self.edit_original.clear();
        self.edit_marked_range = None;
        self.edit_error = None;
        was_active
    }

    fn restore_workspace_root_focus(
        &mut self,
        intended: &SettingsFocusTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.focus_targets();
        self.focus_index = workspace_root_focus_index(&targets, intended)
            .unwrap_or_else(|| self.focus_index.min(targets.len().saturating_sub(1)));
        self.sync_focus_handle(window, cx);
    }

    fn activate_target(
        &mut self,
        target: SettingsFocusTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            SettingsFocusTarget::Page(page) => self.select_page(page, window, cx),
            SettingsFocusTarget::PromptBarColorReset(key) => {
                let (key, value) = prompt_bar_reset_change(&key);
                self.select_color(key, value, cx);
            }
            SettingsFocusTarget::Control(control) => match control.kind {
                ControlKind::Toggle => self.toggle(&control.key, cx),
                // Keyboard activation opens the same dropdown a click opens, so
                // the option set is discoverable without pointer use; the arrow
                // keys then step the live value through it.
                ControlKind::Choice(options) => {
                    self.toggle_choice_menu(&control.key, &options, window, cx);
                }
                ControlKind::Stepper { min, max, step, .. } => {
                    self.step(&control.key, (min, max), step, cx);
                }
                ControlKind::Action => self.run_action(&control.key, window, cx),
                ControlKind::Color => self.toggle_color_picker(&control, cx),
                ControlKind::Text => {
                    self.begin_inline_edit(&control);
                    window.focus(&self.edit_handle, cx);
                    cx.notify();
                }
                ControlKind::Keybinding => self.begin_capture(&control.key, window, cx),
            },
            SettingsFocusTarget::Action(key) => self.run_action(&key, window, cx),
            SettingsFocusTarget::WorkspaceRootInput => {
                self.active_input = Some(NativeInputTarget::WorkspaceRoot);
                self.input_selection = NativeInputSelection::Caret;
                window.focus(&self.workspace_root_handle, cx);
                cx.notify();
            }
            SettingsFocusTarget::WorkspaceRootBrowse => {
                Self::browse_workspace_root(window, cx);
            }
            SettingsFocusTarget::WorkspaceRootAdd => {
                self.add_workspace_root(&SettingsFocusTarget::WorkspaceRootAdd, window, cx);
            }
            target @ SettingsFocusTarget::WorkspaceRootRemove { .. } => {
                self.remove_workspace_root(&target, window, cx);
            }
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
            ControlKind::Color => {
                let value = current_value(&self.config, &control.key);
                let current = value.as_str().unwrap_or("");
                let preset = adjacent_color_preset(current, direction);
                self.select_color(&control.key, preset, cx);
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
            || (page == SettingsPage::Workspaces
                && workspace_roots_match_query(&query, &self.config.workspaces.roots))
            || self.controls_for_page(page).iter().any(|control| {
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
            // A shortcut is looked up by the keys it uses at least as often as
            // by the action it runs, so "ctrl+shift" finds its own rows.
            || (matches!(control.kind, ControlKind::Keybinding)
                && keybinding_combos(&self.config, &control.key)
                    .iter()
                    .any(|combo| combo.to_lowercase().contains(&query)))
    }

    fn workspace_roots_match_search(&self) -> bool {
        workspace_roots_match_query(
            &self.search_query.trim().to_lowercase(),
            &self.config.workspaces.roots,
        )
    }

    fn workspace_root_controls_match_search(&self) -> bool {
        workspace_root_controls_match_query(&self.search_query.trim().to_lowercase())
    }

    fn workspace_root_matches_search(&self, root: &str) -> bool {
        workspace_root_matches_query(&self.search_query.trim().to_lowercase(), root)
    }

    fn align_page_to_search(&mut self) {
        if !self.page_matches_search(self.page)
            && let Some(page) =
                settings_nav_pages().into_iter().find(|page| self.page_matches_search(*page))
        {
            self.clear_inline_edit();
            self.capture_action = None;
            self.capture_error = None;
            self.open_color = None;
            self.clear_choice_menu_state();
            self.page = page;
            self.status = None;
            // Jumping to a different page for a match must rewind the shared
            // scroller too, or the match lands above the retained offset.
            self.scroll_handle.set_offset(Point::default());
            self.content_scrolled = 0;
            self.pulse_content_scrollbar();
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

    fn active_input_text(&self) -> &str {
        match self.active_input {
            Some(NativeInputTarget::WorkspaceRoot) => &self.workspace_root_input,
            Some(NativeInputTarget::Inline) => &self.edit_input,
            Some(NativeInputTarget::ChoiceFilter) => &self.choice_filter,
            None => "",
        }
    }

    fn active_input_marked_range(&self) -> Option<&Range<usize>> {
        match self.active_input {
            Some(NativeInputTarget::WorkspaceRoot) => self.workspace_root_marked_range.as_ref(),
            Some(NativeInputTarget::Inline) => self.edit_marked_range.as_ref(),
            Some(NativeInputTarget::ChoiceFilter) => self.choice_filter_marked_range.as_ref(),
            None => None,
        }
    }

    fn active_input_state_mut(&mut self) -> Option<(&mut String, &mut Option<Range<usize>>)> {
        match self.active_input {
            Some(NativeInputTarget::WorkspaceRoot) => {
                Some((&mut self.workspace_root_input, &mut self.workspace_root_marked_range))
            }
            Some(NativeInputTarget::Inline) => {
                Some((&mut self.edit_input, &mut self.edit_marked_range))
            }
            Some(NativeInputTarget::ChoiceFilter) => {
                Some((&mut self.choice_filter, &mut self.choice_filter_marked_range))
            }
            None => None,
        }
    }

    fn clear_active_input_error(&mut self) {
        match self.active_input {
            Some(NativeInputTarget::WorkspaceRoot) => self.workspace_root_error = None,
            Some(NativeInputTarget::Inline) => self.edit_error = None,
            Some(NativeInputTarget::ChoiceFilter) | None => {}
        }
    }

    fn append_active_input(&mut self, text: &str) {
        let filters_choice = self.active_input == Some(NativeInputTarget::ChoiceFilter);
        let select_all = self.input_selection == NativeInputSelection::All;
        if let Some((input, marked_range)) = self.active_input_state_mut() {
            if select_all {
                input.clear();
            }
            input.push_str(text);
            *marked_range = None;
        }
        self.input_selection = NativeInputSelection::Caret;
        self.clear_active_input_error();
        if filters_choice {
            self.align_choice_highlight();
        }
    }

    fn backspace_active_input(&mut self) {
        let filters_choice = self.active_input == Some(NativeInputTarget::ChoiceFilter);
        let select_all = self.input_selection == NativeInputSelection::All;
        if let Some((input, marked_range)) = self.active_input_state_mut() {
            if select_all {
                input.clear();
            } else {
                input.pop();
            }
            *marked_range = None;
        }
        self.input_selection = NativeInputSelection::Caret;
        self.clear_active_input_error();
        if filters_choice {
            self.align_choice_highlight();
        }
    }

    fn cancel_native_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_input == Some(NativeInputTarget::ChoiceFilter) {
            let closed = self.choice_filter.is_empty();
            self.dismiss_transient_state(cx);
            if closed {
                window.focus(&self.focus_handle, cx);
                self.keyboard_navigation = true;
            }
            return;
        }
        if self.active_input != Some(NativeInputTarget::Inline) {
            self.dismiss_transient_state(cx);
            window.focus(&self.focus_handle, cx);
            self.keyboard_navigation = true;
            cx.notify();
            return;
        }

        let closes_color_picker = self
            .edit_control
            .as_ref()
            .is_some_and(|control| matches!(control.kind, ControlKind::Color));
        revert_inline_input(
            &mut self.edit_input,
            &self.edit_original,
            &mut self.edit_marked_range,
            &mut self.edit_error,
        );
        self.input_selection = NativeInputSelection::Caret;
        if closes_color_picker {
            self.open_color = None;
            self.clear_inline_edit();
            window.focus(&self.focus_handle, cx);
            self.keyboard_navigation = true;
        }
        cx.notify();
    }

    fn handle_native_input_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if (event.keystroke.modifiers.control || event.keystroke.modifiers.platform)
            && event.keystroke.key == "a"
        {
            self.input_selection = NativeInputSelection::All;
            cx.notify();
            return true;
        }
        if (event.keystroke.modifiers.control || event.keystroke.modifiers.platform)
            && event.keystroke.key == "v"
        {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.append_active_input(&text);
                cx.notify();
            }
            return true;
        }
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return false;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.cancel_native_input(window, cx);
                true
            }
            "backspace" => {
                self.backspace_active_input();
                cx.notify();
                true
            }
            // Input is append-only, matching settings search: the caret is at
            // the end, so forward Delete has nothing to remove.
            "delete" => true,
            "enter" => {
                match self.active_input {
                    Some(NativeInputTarget::WorkspaceRoot) => self.add_workspace_root(
                        &SettingsFocusTarget::WorkspaceRootInput,
                        window,
                        cx,
                    ),
                    Some(NativeInputTarget::Inline) => {
                        self.save_inline_edit(cx);
                    }
                    Some(NativeInputTarget::ChoiceFilter) | None => {}
                }
                true
            }
            "tab" => {
                if self
                    .edit_control
                    .as_ref()
                    .is_some_and(|control| matches!(control.kind, ControlKind::Color))
                {
                    self.open_color = None;
                    self.clear_inline_edit();
                }
                self.move_focus(if event.keystroke.modifiers.shift { -1 } else { 1 }, window, cx);
                true
            }
            "down" => {
                self.move_focus(1, window, cx);
                true
            }
            "up" => {
                self.move_focus(-1, window, cx);
                true
            }
            // Printable text, including Option/AltGr and IME commits, belongs
            // to the registered `EntityInputHandler`; it must keep propagating
            // or GPUI cannot deliver the platform text event.
            _ => false,
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // A recording row claims every key ahead of the window's own shortcuts,
        // or Ctrl+K and Tab could never be bound to anything.
        if self.capture_action.is_some() {
            self.capture_keystroke(event, cx);
            cx.stop_propagation();
            return;
        }
        if (event.keystroke.modifiers.control || event.keystroke.modifiers.platform)
            && event.keystroke.key == "k"
        {
            self.flush_theme_preset(cx);
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
        // An open choice owns navigation before either its filter input or the
        // page traversal can see it. Printable filter text still propagates to
        // GPUI's native input path.
        if self.open_choice.is_some() {
            let handled = self.handle_open_choice_key(event, window, cx)
                || (self.choice_filter_handle.is_focused(window)
                    && self.handle_native_input_key(event, window, cx));
            if handled {
                cx.stop_propagation();
            }
            return;
        }
        if (self.workspace_root_handle.is_focused(window)
            || self.edit_handle.is_focused(window)
            || self.choice_filter_handle.is_focused(window))
            && self.handle_native_input_key(event, window, cx)
        {
            cx.stop_propagation();
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
            "tab" if self.open_color.is_some() => {
                let control = match self.focused_target() {
                    Some(SettingsFocusTarget::Control(control))
                        if matches!(control.kind, ControlKind::Color)
                            && self.open_color.as_deref() == Some(control.key.as_str()) =>
                    {
                        Some(control)
                    }
                    _ => None,
                };
                if let Some(control) = control {
                    self.begin_inline_edit(&control);
                    window.focus(&self.edit_handle, cx);
                    cx.notify();
                    true
                } else {
                    self.move_focus(1, window, cx);
                    true
                }
            }
            "tab" | "down" => {
                self.move_focus(1, window, cx);
                true
            }
            "up" => {
                self.move_focus(-1, window, cx);
                true
            }
            "left" => self.adjust_target(-1.0, cx),
            "right" => self.adjust_target(1.0, cx),
            "enter" | "space" => self.focused_target().is_some_and(|target| {
                self.activate_target(target, window, cx);
                true
            }),
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    /// Back out of whatever transient state Escape should unwind, innermost
    /// first: an open palette, choice filter, choice menu, then page search.
    fn dismiss_transient_state(&mut self, cx: &mut Context<Self>) -> bool {
        if self.discard_theme_preset(cx) {
            return true;
        }
        if self.close_color_picker(cx) {
            return true;
        }
        match dismiss_choice_or_search(
            &mut self.choice_filter,
            &mut self.open_choice,
            &mut self.search_query,
        ) {
            Some(DismissedTransient::ChoiceFilter) => {
                self.choice_filter_marked_range = None;
                self.align_choice_highlight();
            }
            Some(DismissedTransient::ChoiceMenu) => {
                self.choice_filter_marked_range = None;
                if self.active_input == Some(NativeInputTarget::ChoiceFilter) {
                    self.active_input = None;
                    self.input_selection = NativeInputSelection::Caret;
                }
            }
            Some(DismissedTransient::PageSearch) => self.align_page_to_search(),
            None => return false,
        }
        cx.notify();
        true
    }

    fn handle_open_choice_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(action) = choice_menu_key_action(&event.keystroke.key, event.keystroke.modifiers)
        else {
            return false;
        };
        match action {
            ChoiceMenuKey::Previous => self.move_choice_menu_highlight(-1, cx),
            ChoiceMenuKey::Next => self.move_choice_menu_highlight(1, cx),
            ChoiceMenuKey::Apply => self.apply_choice_highlight(window, cx),
            ChoiceMenuKey::Dismiss => self.cancel_native_input(window, cx),
            ChoiceMenuKey::Swallow => {}
        }
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

    /// Commit the open inline edit through the one shared apply path, then
    /// re-seed the field from what was actually stored so a later Escape
    /// restores the saved value rather than the pre-edit one.
    fn save_inline_edit(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(control) = self.edit_control.clone() else {
            return false;
        };
        let key = control.key;
        let is_color = matches!(control.kind, ControlKind::Color);
        let stored = match inline_commit_value(is_color, &key, &self.edit_input) {
            Ok(stored) => stored,
            Err(error) => {
                self.edit_error = Some(error);
                cx.notify();
                return false;
            }
        };
        if self.commit(&key, Value::String(stored.clone()), cx) {
            self.edit_input.clone_from(&stored);
            self.edit_original = stored;
            self.edit_marked_range = None;
            self.edit_error = None;
            self.input_selection = NativeInputSelection::Caret;
            true
        } else {
            self.edit_error = Some(format!(
                "{} was not saved — check the status below.",
                self.commit_label(&key)
            ));
            cx.notify();
            false
        }
    }

    fn add_workspace_root(
        &mut self,
        intended_focus: &SettingsFocusTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = Value::String(self.workspace_root_input.clone());
        let root = match workspace_root_from_value(&value) {
            Ok(root) => root,
            Err(error) => {
                self.workspace_root_error = Some(error);
                cx.notify();
                return;
            }
        };
        if self.commit("workspaces.add_root", Value::String(root), cx) {
            self.workspace_root_input.clear();
            self.workspace_root_marked_range = None;
            self.workspace_root_error = None;
            self.restore_workspace_root_focus(intended_focus, window, cx);
        } else {
            self.workspace_root_error = Some(
                "Workspace root was not saved — check the status below and try again.".to_owned(),
            );
        }
        cx.notify();
    }

    fn browse_workspace_root(window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(workspace_root_prompt_options());
        cx.spawn_in(window, async move |settings, async_cx| {
            let selection = match paths.await {
                Ok(Ok(None)) => return,
                Ok(Ok(Some(paths))) => match paths.as_slice() {
                    [path] => path.clone().into_os_string().into_string().map_err(|_| {
                        "Selected directory path is not valid UTF-8. Choose another directory or \
                         enter a UTF-8 path manually."
                            .to_owned()
                    }),
                    [] => {
                        Err("Directory chooser returned no directory. Try again or enter the path \
                         manually."
                            .to_owned())
                    }
                    _ => Err(
                        "Directory chooser returned multiple directories. Choose one directory or \
                         enter the path manually."
                            .to_owned(),
                    ),
                },
                Ok(Err(error)) => Err(format!(
                    "Could not open the directory chooser: {error}. Try again or enter the path \
                     manually."
                )),
                Err(error) => Err(format!(
                    "Directory chooser closed unexpectedly: {error}. Try again or enter the path \
                     manually."
                )),
            };

            settings
                .update_in(async_cx, |settings, prompt_window, prompt_cx| match selection {
                    Ok(path) => {
                        settings.workspace_root_input = path;
                        settings.workspace_root_marked_range = None;
                        settings.workspace_root_error = None;
                        settings.add_workspace_root(
                            &SettingsFocusTarget::WorkspaceRootBrowse,
                            prompt_window,
                            prompt_cx,
                        );
                    }
                    Err(error) => {
                        settings.workspace_root_error = Some(error);
                        prompt_cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    fn remove_workspace_root(
        &mut self,
        target: &SettingsFocusTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SettingsFocusTarget::WorkspaceRootRemove { root, .. } = target else {
            return;
        };
        if self.commit("workspaces.remove_root", Value::String(root.clone()), cx) {
            self.restore_workspace_root_focus(target, window, cx);
        }
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
        let token = self.choice_token(key);
        choice_options_from_cache(key, &token, options, &self.theme_presets)
    }

    fn choice_token(&self, key: &str) -> String {
        let value = current_value(&self.config, key);
        let saved = value.as_str().unwrap_or("");
        if key == "theme.preset" {
            self.pending_theme_preset.as_deref().unwrap_or(saved).to_owned()
        } else {
            saved.to_owned()
        }
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
        let token = self.choice_token(key);
        let index = options.iter().position(|(candidate, _)| candidate == &token).unwrap_or(0);
        let next =
            if direction.is_negative() { (index + count - 1) % count } else { (index + 1) % count };
        let Some((chosen, _)) = options.get(next) else {
            return;
        };
        if key == "theme.preset" {
            self.defer_theme_preset(chosen.clone(), cx);
        } else {
            self.commit(key, Value::String(chosen.clone()), cx);
        }
    }

    fn defer_theme_preset(&mut self, value: String, cx: &mut Context<Self>) {
        let generation = replace_pending_theme_preset(
            &mut self.pending_theme_preset,
            &mut self.theme_preset_generation,
            value,
        );
        self.theme_preset_task = Some(cx.spawn(async move |settings, app| {
            app.background_executor().timer(THEME_PRESET_DEBOUNCE).await;
            settings
                .update(app, |settings, ctx| settings.settle_theme_preset(generation, ctx))
                .ok();
        }));
        cx.notify();
    }

    fn settle_theme_preset(&mut self, generation: u64, cx: &mut Context<Self>) {
        let Some(value) = take_pending_theme_preset(
            &mut self.pending_theme_preset,
            &mut self.theme_preset_generation,
            Some(generation),
        ) else {
            return;
        };
        self.theme_preset_task = None;
        self.commit("theme.preset", Value::String(value), cx);
    }

    fn flush_theme_preset(&mut self, cx: &mut Context<Self>) -> bool {
        self.theme_preset_task = None;
        let Some(value) = take_pending_theme_preset(
            &mut self.pending_theme_preset,
            &mut self.theme_preset_generation,
            None,
        ) else {
            return false;
        };
        self.commit("theme.preset", Value::String(value), cx);
        true
    }

    fn discard_theme_preset(&mut self, cx: &mut Context<Self>) -> bool {
        self.theme_preset_task = None;
        let discarded = take_pending_theme_preset(
            &mut self.pending_theme_preset,
            &mut self.theme_preset_generation,
            None,
        )
        .is_some();
        if discarded {
            cx.notify();
        }
        discarded
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
    /// fail-closed default. The two replies that govern the current network's
    /// trust action remain fallible here so a refresh cannot turn a transport
    /// failure into a false "not trusted" result.
    fn refresh_trust(&mut self) -> Result<(), ()> {
        let lan = server_action::try_request_lan_env(SERVER_ACTION_TIMEOUT);
        let networks = server_action::try_request_trusted_networks(SERVER_ACTION_TIMEOUT);
        let devices = server_action::request_trusted_devices(SERVER_ACTION_TIMEOUT);
        let remote = server_action::request_remote_env(SERVER_ACTION_TIMEOUT);
        let (lan, networks) = match (lan, networks) {
            (Ok(lan), Ok(networks)) => (lan, networks),
            (lan, networks) => {
                if let Err(reason) = lan {
                    tracing::warn!("LAN trust refresh failed: {reason}");
                }
                if let Err(reason) = networks {
                    tracing::warn!("trusted-network refresh failed: {reason}");
                }
                // Stale addability must never leave an active Trust control.
                self.trust.loaded = false;
                return Err(());
            }
        };
        self.trust = TrustState {
            loaded: true,
            lan,
            remote,
            networks: networks.networks,
            current_trusted: networks.current_trusted,
            devices,
        };
        Ok(())
    }

    /// Whether fresh server state permits adding the current network.
    fn current_network_can_be_trusted(&self) -> bool {
        self.trust.loaded && self.trust.lan.current_network_addable && !self.trust.current_trusted
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

    fn refresh_trust_status(&mut self) -> String {
        if self.refresh_trust().is_ok() {
            return self.trust_summary();
        }
        "Could not refresh trust state. Check that Scribe is running, then try Refresh again."
            .to_owned()
    }

    fn remove_trusted_network_status(&mut self, id: &str) -> String {
        if let Err(reason) =
            server_action::request_remove_trusted_network(id.to_owned(), SERVER_ACTION_TIMEOUT)
        {
            tracing::warn!("remove trusted network failed: {reason}");
            return "Could not remove the trusted network. Check that Scribe is running, then try \
                    again."
                .to_owned();
        }
        if self.refresh_trust().is_err() {
            return "Remove request sent, but Scribe could not refresh trust state. Use Refresh to \
                    confirm the change."
                .to_owned();
        }
        format!("Removed trusted network {id}. {}", self.trust_summary())
    }

    fn revoke_trusted_device_status(&mut self, device_id: &str) -> String {
        if let Err(reason) = server_action::request_revoke_trusted_device(
            device_id.to_owned(),
            SERVER_ACTION_TIMEOUT,
        ) {
            tracing::warn!("revoke trusted device failed: {reason}");
            return "Could not revoke the approved device. Check that Scribe is running, then try \
                    again."
                .to_owned();
        }
        if self.refresh_trust().is_err() {
            return "Revoke request sent, but Scribe could not refresh trust state. Use Refresh to \
                    confirm the change."
                .to_owned();
        }
        format!("Revoked device {device_id}. {}", self.trust_summary())
    }

    fn trust_current_network_status(&mut self) -> String {
        if !self.current_network_can_be_trusted() {
            return "This network cannot be trusted from the current state. Use Refresh, then try \
                    again when Trust it is available."
                .to_owned();
        }
        if let Err(reason) = server_action::request_add_current_network(SERVER_ACTION_TIMEOUT) {
            tracing::warn!("trust current network failed: {reason}");
            return "Could not trust the current network. Check that Scribe is running, then try \
                    again."
                .to_owned();
        }
        if self.refresh_trust().is_err() {
            return "Trust request sent, but Scribe could not confirm it. Use Refresh before trying \
                    again."
                .to_owned();
        }
        if self.trust.current_trusted {
            return format!("Trusted the current network. {}", self.trust_summary());
        }
        if self.trust.lan.current_network_addable {
            return "Scribe did not confirm this network as trusted. Use Refresh, then try again."
                .to_owned();
        }
        format!(
            "Scribe could not trust this network — {}. Change networks or use Refresh to try \
             again.",
            self.trust
                .lan
                .current_network_reason
                .as_deref()
                .unwrap_or("it cannot be fingerprinted")
        )
    }

    /// Dispatch an action button.
    ///
    /// Anything that talks to the local server paints a pending line first and
    /// runs on the next frame; purely local actions run inline. Resetting badge
    /// colors stays here because it also repairs the native input focus.
    fn run_action(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if key == "workspaces.reset_badge_colors" {
            self.reset_badge_colors(key, window, cx);
            return;
        }
        let Some(pending) = action_pending_message(key) else {
            self.perform_action(key, cx);
            return;
        };
        self.status = Some(pending);
        cx.notify();
        let key = key.to_owned();
        Self::after_paint(cx, move |this, ctx| this.perform_action(&key, ctx));
    }

    fn reset_badge_colors(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let edit_was_focused = self.edit_handle.is_focused(window);
        if !self.commit(key, Value::Bool(true), cx) {
            return;
        }
        let edit_was_active = self.clear_inline_edit();
        self.focus_index = self
            .focus_targets()
            .iter()
            .position(|target| {
                matches!(target, SettingsFocusTarget::Control(control) if control.key == key)
            })
            .unwrap_or(self.focus_index);
        if edit_was_focused || edit_was_active {
            window.focus(&self.focus_handle, cx);
        }
    }

    fn perform_action(&mut self, key: &str, cx: &mut Context<Self>) {
        // Per-row trust mutations carry their record key in the action id, so they
        // are matched by prefix before the fixed action table.
        if let Some(id) = key.strip_prefix(REMOVE_TRUSTED_NETWORK_PREFIX) {
            self.status = Some(self.remove_trusted_network_status(id));
            cx.notify();
            return;
        }
        if let Some(device_id) = key.strip_prefix(REVOKE_TRUSTED_DEVICE_PREFIX) {
            self.status = Some(self.revoke_trusted_device_status(device_id));
            cx.notify();
            return;
        }
        match key {
            REFRESH_TRUST_ACTION => {
                self.status = Some(self.refresh_trust_status());
            }
            ADD_CURRENT_NETWORK_ACTION => {
                self.status = Some(self.trust_current_network_status());
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
            "terminal.smart_selection.reset" => {
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
        let nav = self.render_nav(window, cx);
        let thumb = self.tick_content_scrollbar(window, cx);
        let content = self.render_content(thumb, window, cx);
        let body = div()
            .id("settings-root")
            .role(Role::Application)
            .aria_label("Scribe settings")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &CloseWindow, action_window, ctx| {
                this.flush_theme_preset(ctx);
                action_window.remove_window();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, key_window, ctx| {
                this.on_key_down(event, key_window, ctx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _key_window, ctx| {
                    this.clear_keyboard_navigation(ctx);
                }),
            )
            // The scrollbar gestures ride the root rather than the scroll track
            // they are painted over: the pointer has to keep driving a drag —
            // and has to be able to leave the hover zone — after it has moved
            // off that element, exactly as the terminal's own handlers do on
            // the pane container. The press is claimed in the capture phase so
            // a hit on the overlay never also arms the control underneath; the
            // scrollbar is chrome painted over the page, and a click on it was
            // never meant for what it covers.
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _key_window, ctx| {
                if event.button == MouseButton::Left
                    && this.press_content_scrollbar(event.position)
                {
                    ctx.notify();
                    ctx.stop_propagation();
                }
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _key_window, ctx| {
                if this.drag_content_scrollbar(event.position) {
                    ctx.notify();
                    return;
                }
                this.hover_content_scrollbar(event.position, ctx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _key_window, ctx| {
                    this.release_content_scrollbar(ctx);
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

impl EntityInputHandler for SettingsWindow {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let input = self.active_input_text();
        let range = utf16_range_to_utf8(input, range);
        actual_range.replace(utf8_range_to_utf16(input, &range));
        Some(input[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.active_input_text().encode_utf16().count();
        let range =
            if self.input_selection == NativeInputSelection::All { 0..end } else { end..end };
        Some(UTF16Selection { range, reversed: false })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.active_input_marked_range()
            .map(|range| utf8_range_to_utf16(self.active_input_text(), range))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self
            .active_input_state_mut()
            .is_some_and(|(_, marked_range)| marked_range.take().is_some())
        {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let filters_choice = self.active_input == Some(NativeInputTarget::ChoiceFilter);
        let select_all = self.input_selection == NativeInputSelection::All;
        if let Some((input, marked_range)) = self.active_input_state_mut() {
            let range = if select_all {
                0..input.len()
            } else {
                range
                    .map(|range| utf16_range_to_utf8(input, range))
                    .or_else(|| marked_range.take())
                    .unwrap_or(input.len()..input.len())
            };
            input.replace_range(range, text);
            *marked_range = None;
        }
        self.input_selection = NativeInputSelection::Caret;
        self.clear_active_input_error();
        if filters_choice {
            self.align_choice_highlight();
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let filters_choice = self.active_input == Some(NativeInputTarget::ChoiceFilter);
        let select_all = self.input_selection == NativeInputSelection::All;
        if let Some((input, marked_range)) = self.active_input_state_mut() {
            let range = if select_all {
                0..input.len()
            } else {
                range
                    .map(|range| utf16_range_to_utf8(input, range))
                    .or_else(|| marked_range.take())
                    .unwrap_or(input.len()..input.len())
            };
            let start = range.start;
            input.replace_range(range, new_text);
            *marked_range = (!new_text.is_empty()).then_some(start..start + new_text.len());
        }
        self.input_selection = NativeInputSelection::Caret;
        self.clear_active_input_error();
        if filters_choice {
            self.align_choice_highlight();
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.active_input_text().encode_utf16().count())
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.active_input_text().encode_utf16().count())
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
    if !tiling.top && !tiling.left && in_corner(point(px(0.0), px(0.0))) {
        return Some(ResizeEdge::TopLeft);
    }
    if !tiling.top && !tiling.right && in_corner(point(right_edge, px(0.0))) {
        return Some(ResizeEdge::TopRight);
    }
    if !tiling.bottom && !tiling.left && in_corner(point(px(0.0), bottom_edge)) {
        return Some(ResizeEdge::BottomLeft);
    }
    if !tiling.bottom && !tiling.right && in_corner(point(right_edge, bottom_edge)) {
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
            .h(px(SETTINGS_TITLEBAR_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            // The titlebar shares the interior ground; only the hairline seam
            // below it marks the band.
            .bg(colors.page_bg)
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
            // Balance the three 40px controls on the right. On macOS this also
            // reserves the traffic-light region, keeping the centered title
            // clear of the native buttons without platform-specific offsets.
            .child(
                div()
                    .w(px(120.0))
                    .h_full()
                    .flex_none()
                    .window_control_area(WindowControlArea::Drag)
            )
            .child(
                div()
                    .h_full()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .window_control_area(WindowControlArea::Drag)
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(colors.text)
                    .child("Scribe Settings"),
            )
            .child(settings_window_control(SettingsWindowControl::Minimize, window, &colors, cx))
            .child(settings_window_control(SettingsWindowControl::Maximize, window, &colors, cx))
            .child(settings_window_control(SettingsWindowControl::Close, window, &colors, cx))
            .into_any_element()
    }

    fn render_nav(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        // Groups that a search filters away entirely are dropped *before* the
        // index is assigned, so a filtered contents list never opens with a
        // stray group gap above its first entry.
        let items = settings_nav_groups()
            .into_iter()
            .filter(|(_, pages)| pages.iter().any(|page| self.page_matches_search(*page)))
            .enumerate()
            .flat_map(|(index, (group, pages))| self.render_nav_group(index, group, pages, cx))
            .collect::<Vec<_>>();
        div()
            .id("settings-sidebar")
            .w(px(232.0))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(colors.border)
            // Search leads the sidebar — the platform settings idiom — and
            // stays pinned above the scrolling contents list.
            .child(self.render_search(window, cx))
            .child(
                div()
                    .id("settings-navigation")
                    .role(Role::TabList)
                    .aria_label("Settings sections")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .pt(px(14.0))
                    .children(items),
            )
            .child(
                div()
                    .id("settings-sidebar-footer")
                    .role(Role::Note)
                    .aria_label(concat!("Scribe v", env!("CARGO_PKG_VERSION")))
                    .w_full()
                    .h(px(36.0))
                    .flex_none()
                    .px(px(20.0))
                    .flex()
                    .items_center()
                    .font_family("monospace")
                    .text_xs()
                    .text_color(colors.quiet_text)
                    .child(concat!("Scribe v", env!("CARGO_PKG_VERSION"))),
            )
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
        // Groups separate by air and their quiet label alone — no rules.
        let mut group_items = Vec::with_capacity(items.len() + 1);
        group_items.push(
            div()
                .id(("settings-nav-group", group_index))
                .role(Role::Heading)
                .aria_level(2)
                .aria_label(group)
                .w_full()
                .h(px(28.0))
                .flex_none()
                .when(group_index > 0, |el| el.mt(px(14.0)))
                .px(px(20.0))
                .flex()
                .items_end()
                .pb(px(6.0))
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
        let weight = if selected { FontWeight::MEDIUM } else { FontWeight::NORMAL };
        let foreground = if selected { colors.text } else { colors.dim_text };
        // Selection is a neutral wash plus the page glyph turning amber —
        // the only lit state in the column. Keyboard focus is an amber
        // hairline outline, so the two never look alike.
        let icon_color = if selected { colors.accent } else { colors.quiet_text };
        div()
            .id(("settings-nav", page as usize))
            .focusable()
            .tab_stop(true)
            .role(Role::Tab)
            .aria_label(page.nav_label())
            .aria_selected(selected)
            .aria_position_in_set(position + 1)
            .aria_size_of_set(settings_nav_pages().len())
            .h(px(32.0))
            .flex_none()
            .mx(px(8.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap_3()
            .rounded(px(5.0))
            .text_sm()
            .font_weight(weight)
            .text_color(foreground)
            .when(selected, |el| el.bg(colors.nav_active_bg))
            .when(focused, |el| el.border_1().border_color(colors.accent))
            .hover(move |style| style.bg(colors.nav_hover_bg).text_color(colors.text))
            .active(move |style| style.bg(colors.nav_active_bg))
            .on_click(cx.listener(move |this, _, page_window, ctx| {
                this.begin_pointer_interaction(&SettingsFocusTarget::Page(page));
                this.select_page(page, page_window, ctx);
            }))
            .child(
                div()
                    .size(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family("Symbols Nerd Font Mono")
                    .text_base()
                    .font_weight(FontWeight::NORMAL)
                    .text_color(icon_color)
                    .child(page_icon(page)),
            )
            .child(page.nav_label())
            .into_any_element()
    }

    fn render_content(
        &self,
        thumb: Option<ScrollbarQuad>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let mut children = vec![self.render_page_heading()];
        // The Remote page leads with the runtime "Local network" trust surface so
        // its lists and their Remove/Revoke buttons sit above the fold; the static
        // config controls follow underneath.
        if self.page == SettingsPage::Remote {
            children.extend(self.render_trust_sections(cx));
        }
        if self.page == SettingsPage::Workspaces && self.workspace_roots_match_search() {
            children.extend(self.render_workspace_root_sections(window, cx));
        }
        children.extend(self.render_control_rows(window, cx));

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
            // The overlay thumb is positioned against this wrapper rather than
            // appended to the scroller, so it neither scrolls with the content
            // nor reserves a column from it.
            .child(
                div()
                    .id("settings-scroll-track")
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("settings-scroll")
                            // `flex_1` claims the pane, and the zero `min_h` is what
                            // actually bounds it: a flex item's automatic minimum size
                            // is its content height, which on a long page would size
                            // the viewport to the content and leave nothing to scroll.
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
                            .px(px(44.0))
                            .pb(px(48.0))
                            .flex()
                            .flex_col()
                            // A typeset column keeps a readable measure: every row caps
                            // at 840px and stays left-aligned when the window grows.
                            // Capping per row (not via one wrapping column) preserves
                            // the scroller's content-extent measurement noted above.
                            .children(children.into_iter().map(|child| {
                                div().w_full().max_w(px(840.0)).flex_none().child(child)
                            })),
                    )
                    // Painted after the scroller so the overlay reads as an
                    // overlay; it is out of flow, so it reserves no column.
                    .children(thumb.map(|thumb| {
                        // The shared renderer already folded the fade opacity
                        // into the alpha and sized the corner radius to the
                        // hover-animated width.
                        let [red, green, blue, alpha] = thumb.color;
                        div()
                            .absolute()
                            .left(px(thumb.rect.x))
                            .top(px(thumb.rect.y))
                            .w(px(thumb.rect.width))
                            .h(px(thumb.rect.height))
                            .rounded(px(thumb.corner_radius))
                            .bg(Rgba { r: red, g: green, b: blue, a: alpha })
                    })),
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
        let shown = search_display_text(&query, focused);
        let field = div()
            .id("settings-search-input")
            .track_focus(&focus)
            .role(Role::SearchInput)
            .aria_label("Search settings")
            .aria_placeholder("Search settings")
            .aria_value(query.clone())
            .w_full()
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap_2()
            .rounded(px(5.0))
            .border_1()
            .border_color(if focused { colors.accent } else { colors.border })
            // Focus outranks hover here. The hover rule used to repaint a focused
            // field's border in the dim hover colour, so a focused-and-hovered
            // field looked exactly like an unfocused one.
            .bg(colors.input_bg)
            .text_sm()
            .text_color(colors.text)
            .cursor_text()
            .when(!focused, |el| el.hover(move |style| style.border_color(colors.strong_border)))
            .on_click(cx.listener(move |_, _, focused_window, ctx| {
                focused_window.focus(&focus, ctx);
            }))
            .child(settings_search_icon(colors.quiet_text))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .text_color(if shown == "Search settings" {
                        colors.quiet_text
                    } else {
                        colors.text
                    })
                    .child(shown)
                    // The insertion point is always at the end of the query, so a
                    // static bar there is the honest caret: it says the field
                    // takes typing without implying a movable cursor it has not
                    // got.
                    .when(focused, |el| {
                        el.child(
                            div()
                                .ml(px(2.0))
                                .w(px(2.0))
                                .h(px(14.0))
                                .flex_none()
                                .bg(colors.accent),
                        )
                    }),
            )
            // The shortcut hint has done its job once the field is focused.
            .when(!focused, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family("monospace")
                        .text_xs()
                        .text_color(colors.quiet_text)
                        .child("Ctrl+K"),
                )
            });
        div()
            .id("settings-search-region")
            .role(Role::Search)
            .aria_label("Settings search")
            .w_full()
            .flex_none()
            .pt(px(12.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .child(field)
            .into_any_element()
    }

    fn render_page_heading(&self) -> gpui::AnyElement {
        let colors = self.colors;
        let summary = page_summary(self.page);
        // The Releases page renders server data, not config keys, so the
        // config-source line would be untrue there.
        let source = (self.page != SettingsPage::Releases).then_some("config.toml · live apply");
        div()
            .id("settings-page-heading")
            .role(Role::Heading)
            .aria_level(1)
            .aria_label(format!("{} — {summary}", self.page.nav_label()))
            .w_full()
            .flex_none()
            .pt(px(28.0))
            .flex()
            .flex_col()
            .text_color(colors.text)
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_end()
                    .justify_between()
                    .gap_4()
                    .child(
                        div().text_xl().font_weight(FontWeight::BOLD).child(self.page.nav_label()),
                    )
                    // The quiet man-page corner note: where these values
                    // persist, and that edits commit as they are made.
                    .children(source.map(|source| {
                        div()
                            .flex_none()
                            .pb(px(2.0))
                            .font_family("monospace")
                            .text_xs()
                            .text_color(colors.quiet_text)
                            .child(source)
                    })),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .text_sm()
                    .text_color(colors.dim_text)
                    .child(Text::new_inaccessible(elide(summary, PAGE_SUMMARY_MAX_CHARS).into())),
            )
            .into_any_element()
    }

    fn render_control_rows(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let mut rows = Vec::new();
        let mut previous_section = None;
        for control in self
            .controls_for_page(self.page)
            .into_iter()
            .filter(|control| self.control_matches_search(control))
        {
            let section = control_section(self.page, &control.key);
            if previous_section != Some(section) {
                rows.push(self.control_section_heading(section));
                previous_section = Some(section);
            }
            rows.push(self.render_control(&control, window, cx));
        }
        if rows.is_empty()
            && !(self.page == SettingsPage::Workspaces && self.workspace_roots_match_search())
        {
            rows.push(self.note_row("No settings match this search."));
        }
        rows
    }

    fn render_workspace_root_sections(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let roots = &self.config.workspaces.roots;
        let matching_roots = roots
            .iter()
            .enumerate()
            .filter(|(_, root)| self.workspace_root_matches_search(root))
            .collect::<Vec<_>>();
        let rows = if roots.is_empty() {
            vec![
                div()
                    .id("workspace-roots-empty")
                    .role(Role::ListItem)
                    .aria_label("No workspace roots configured")
                    .w_full()
                    .h(px(40.0))
                    .flex()
                    .items_center()
                    .text_sm()
                    .text_color(self.colors.dim_text)
                    .child("No workspace roots configured.")
                    .into_any_element(),
            ]
        } else {
            let count = matching_roots.len();
            matching_roots
                .into_iter()
                .enumerate()
                .map(|(position, (index, root))| {
                    self.render_workspace_root(index, root, (position, count), cx)
                })
                .collect()
        };
        let mut sections = vec![
            self.control_section_heading("Workspace roots"),
            div()
                .id("workspace-roots-list")
                .role(Role::List)
                .aria_label("Configured workspace roots")
                .w_full()
                .flex()
                .flex_col()
                .children(rows)
                .into_any_element(),
        ];
        if self.workspace_root_controls_match_search() {
            sections.push(self.render_workspace_root_input(window, cx));
        }
        sections
    }

    fn render_workspace_root(
        &self,
        index: usize,
        root: &str,
        set_position: (usize, usize),
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let target = SettingsFocusTarget::WorkspaceRootRemove { index, root: root.to_owned() };
        let focused = self.target_is_focused(&target);
        let label = format!("Remove workspace root {root}");
        let button = action_button("Remove", &colors)
            .id(("workspace-root-remove", index))
            .when(focused, |el| el.border_color(colors.accent))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(label)
            .on_click(cx.listener(move |this, _, root_window, ctx| {
                this.begin_pointer_interaction(&target);
                this.remove_workspace_root(&target, root_window, ctx);
            }));
        div()
            .id(("workspace-root", index))
            .role(Role::ListItem)
            .aria_label(root.to_owned())
            .aria_position_in_set(set_position.0 + 1)
            .aria_size_of_set(set_position.1)
            .h(px(46.0))
            .flex_none()
            .flex()
            .items_center()
            .gap_6()
            .mx(px(-12.0))
            .px(px(12.0))
            .rounded(px(6.0))
            .hover(move |style| style.bg(colors.row_hover_bg))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .font_family("monospace")
                    .text_sm()
                    .text_color(colors.text)
                    .child(Text::new_inaccessible(elide(root, NOTE_MAX_CHARS).into())),
            )
            .child(
                div()
                    .w(px(VALUE_COLUMN_WIDTH))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .child(button),
            )
            .into_any_element()
    }

    fn render_workspace_root_input(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let field = self.render_workspace_root_field(window, cx);
        let (browse, add) = self.render_workspace_root_actions(cx);
        let error = self.workspace_root_error.clone().map(|error| {
            div()
                .id("workspace-root-error")
                .role(Role::Alert)
                .aria_label(format!("Workspace root error: {error}"))
                .aria_description(
                    "Enter an absolute path or a path starting with ~/, then try again.",
                )
                .w_full()
                .text_xs()
                .text_color(colors.error)
                .child(Text::new_inaccessible(error.into()))
        });
        // The field spans the row — a path wants the whole measure — with its
        // actions trailing; the field's own placeholder and ARIA name carry
        // the label.
        div()
            .id("workspace-root-add-row")
            .role(Role::Group)
            .aria_label("Add workspace root")
            .w_full()
            .h(px(if self.workspace_root_error.is_some() { 72.0 } else { 46.0 }))
            .flex_none()
            .flex()
            .flex_col()
            .justify_center()
            .gap_1()
            .child(
                div().w_full().flex().items_center().gap_2().child(field).child(browse).child(add),
            )
            .children(error)
            .into_any_element()
    }

    fn render_workspace_root_field(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let focus = self.workspace_root_handle.clone();
        let input_focus = focus.clone();
        let settings_entity = cx.entity();
        let focused = self.workspace_root_handle.is_focused(window);
        let value = self.workspace_root_input.clone();
        let description = self
            .workspace_root_error
            .as_deref()
            .unwrap_or("Enter an absolute path or a home-relative path starting with ~/.");
        div()
            .id("workspace-root-input")
            .track_focus(&focus)
            .role(Role::TextInput)
            .aria_label("Workspace root path")
            .aria_description(description.to_owned())
            .aria_placeholder("Absolute path or ~/path")
            .aria_value(value.clone())
            .on_a11y_action(
                AccessibleAction::SetValue,
                a11y_workspace_root_handler(cx.entity().downgrade()),
            )
            .flex_1()
            .min_w(px(0.0))
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded(px(5.0))
            .border_1()
            .border_color(if focused { colors.accent } else { colors.border })
            .bg(colors.input_bg)
            .font_family("monospace")
            .text_sm()
            .text_color(if value.is_empty() { colors.quiet_text } else { colors.text })
            .cursor_text()
            .relative()
            .when(!focused, |el| el.hover(move |style| style.border_color(colors.strong_border)))
            .on_click(cx.listener(move |this, _, input_window, ctx| {
                this.begin_pointer_interaction(&SettingsFocusTarget::WorkspaceRootInput);
                this.active_input = Some(NativeInputTarget::WorkspaceRoot);
                this.input_selection = NativeInputSelection::Caret;
                input_window.focus(&focus, ctx);
            }))
            // The placeholder yields to the caret on focus — the same
            // focused-empty grammar as the search and color fields, so no
            // field ever looks pre-filled with text the user must delete.
            .child(Text::new_inaccessible(
                if value.is_empty() && !focused {
                    "Absolute path or ~/path".to_owned()
                } else {
                    value
                }
                .into(),
            ))
            .when(focused, |el| {
                el.child(div().ml(px(2.0)).w(px(2.0)).h(px(14.0)).flex_none().bg(colors.accent))
            })
            .child(
                canvas(
                    |_bounds, _window, _cx| {},
                    move |bounds, (), input_window, app| {
                        input_window.handle_input(
                            &input_focus,
                            ElementInputHandler::new(bounds, settings_entity),
                            app,
                        );
                    },
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }

    fn render_workspace_root_actions(
        &self,
        cx: &mut Context<Self>,
    ) -> (gpui::AnyElement, gpui::AnyElement) {
        let colors = self.colors;
        let add_target = SettingsFocusTarget::WorkspaceRootAdd;
        let add_focused = self.target_is_focused(&add_target);
        let add = action_button("Add", &colors)
            .id("workspace-root-add")
            .when(add_focused, |el| el.border_color(colors.accent))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label("Add workspace root")
            .on_click(cx.listener(move |this, _, add_window, ctx| {
                this.begin_pointer_interaction(&add_target);
                this.add_workspace_root(&add_target, add_window, ctx);
            }))
            .into_any_element();
        let browse_target = SettingsFocusTarget::WorkspaceRootBrowse;
        let browse_focused = self.target_is_focused(&browse_target);
        let browse = action_button("Browse…", &colors)
            .id("workspace-root-browse")
            .when(browse_focused, |el| el.border_color(colors.accent))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label("Browse for workspace root directory")
            .on_click(cx.listener(move |this, _, browse_window, ctx| {
                this.begin_pointer_interaction(&browse_target);
                Self::browse_workspace_root(browse_window, ctx);
            }))
            .into_any_element();
        (browse, add)
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
            .w_full()
            .px(px(44.0))
            .py(px(10.0))
            .flex()
            .items_center()
            .gap_3()
            .border_t_1()
            .border_color(colors.border)
            .bg(colors.status_bg)
            .text_sm()
            .text_color(colors.text)
            // The amber point is the live-state mark: this line is the result
            // of the most recent action, not passive chrome.
            .child(div().size(px(6.0)).flex_none().rounded_full().bg(colors.accent))
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
                    .rounded(px(4.0))
                    .font_family("Symbols Nerd Font Mono")
                    .text_xs()
                    .text_color(colors.dim_text)
                    .cursor_pointer()
                    .hover(move |style| style.bg(colors.control_hover_bg).text_color(colors.text))
                    .on_click(cx.listener(|this, _, _win, ctx| {
                        this.status = None;
                        ctx.notify();
                    }))
                    .child("\u{f00d}"),
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
        ];

        if self.current_network_can_be_trusted() {
            out.push(self.trust_row(
                TrustRow {
                    label: "This network".to_owned(),
                    button: "Trust it",
                    id: ("trust-action", 1),
                    action_key: ADD_CURRENT_NETWORK_ACTION.to_owned(),
                },
                cx,
            ));
        }

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

    /// A man-page section head between the trust lists: small tracked caps
    /// set off by air above, no rule.
    fn section_heading(&self, text: &str) -> gpui::AnyElement {
        heading_label("settings-section-heading", text, &self.colors)
    }

    /// Typography-led grouping label for config controls — the same section
    /// head, so runtime and config sections read as one system.
    fn control_section_heading(&self, text: &str) -> gpui::AnyElement {
        heading_label("settings-control-section-heading", text, &self.colors)
    }

    /// A read-only informational line inside a trust section.
    fn note_row(&self, text: &str) -> gpui::AnyElement {
        div()
            .id(("settings-note", key_hash(text)))
            .role(Role::Note)
            .aria_label(text.to_owned())
            .w_full()
            .h(px(40.0))
            .flex_none()
            .flex()
            .items_center()
            .text_sm()
            .text_color(self.colors.dim_text)
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
            .when(focused, |el| el.border_color(colors.accent))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(format!("{button} {label}"))
            .on_click(cx.listener(move |this, _, action_window, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                this.run_action(&action_key, action_window, ctx);
            }));
        div()
            .id(("settings-trust-row", key_hash(&label)))
            .role(Role::Group)
            .aria_label(label.clone())
            .h(px(46.0))
            .flex_none()
            .flex()
            .items_center()
            .gap_6()
            .mx(px(-12.0))
            .px(px(12.0))
            .rounded(px(6.0))
            .hover(move |style| style.bg(colors.row_hover_bg))
            .child(row_label(&label, colors.text))
            .child(
                div()
                    .w(px(VALUE_COLUMN_WIDTH))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .child(control),
            )
            .into_any_element()
    }

    fn render_control(
        &self,
        control: &Control,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let enabled = self.control_is_enabled(&control.key);
        let label = row_label(&control.label, colors.text);
        let value_widget = if enabled {
            self.render_value_widget(control, window, cx)
        } else {
            self.render_gated_value(control)
        };
        div()
            .id(("settings-control", key_hash(&control.key)))
            .role(Role::Group)
            .aria_label(control.label.clone())
            .when(!enabled, |el| el.aria_description("Unavailable while its parent setting is off"))
            // A row grows for the second line an open edit's rejection or a
            // recording row's hint occupies, so neither overlaps its neighbour.
            .h(px(
                if (self.edit_key() == Some(control.key.as_str()) && self.edit_error.is_some())
                    || self.capture_action.as_deref() == Some(control.key.as_str())
                {
                    78.0
                } else {
                    46.0
                },
            ))
            .flex_none()
            .flex()
            .items_center()
            .gap_6()
            // Rows separate by rhythm alone; the hover wash bleeds past the
            // text margin so the pill reads as a surface, not a stripe.
            .mx(px(-12.0))
            .px(px(12.0))
            .rounded(px(6.0))
            .when(!enabled, |el| el.opacity(GATED_OPACITY))
            .when(enabled, |el| el.hover(move |style| style.bg(colors.row_hover_bg)))
            .child(label)
            .child(
                div()
                    .w(px(VALUE_COLUMN_WIDTH))
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

    fn render_value_widget(
        &self,
        control: &Control,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match &control.kind {
            ControlKind::Toggle => self.render_toggle(control, cx),
            ControlKind::Choice(options) => self.render_choice(control, options, window, cx),
            ControlKind::Stepper { min, max, step, decimals } => {
                self.render_stepper(control, (*min, *max, *step), *decimals, cx)
            }
            ControlKind::Color if is_prompt_bar_color_override(&control.key) => {
                self.render_prompt_bar_color_control(control, window, cx)
            }
            ControlKind::Color => self.render_color_selector(control, window, cx),
            ControlKind::Text => self.render_inline_edit(control, window, cx),
            ControlKind::Keybinding => self.render_keybinding_value(control, cx),
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
        let hover_bg = if on { rgb(0x00ff_c85c) } else { colors.control_hover_bg };
        let pressed_bg = if on { rgb(0x00df_a42f) } else { colors.control_pressed_bg };
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        // The knob is warm white over amber and neutral white over ink, so the
        // on state reads at a glance even in a long column of switches.
        let knob = div().size(px(16.0)).rounded_full().bg(colors.text);
        div()
            .id(("toggle", key_hash(&control.key)))
            .focusable()
            .tab_stop(true)
            .role(Role::Switch)
            .aria_label(control.label.clone())
            .aria_toggled(if on { Toggled::True } else { Toggled::False })
            .w(px(38.0))
            .h(px(22.0))
            .p(px(2.0))
            .flex()
            .items_center()
            .when(on, gpui::Styled::justify_end)
            .rounded_full()
            .border_1()
            // A white ring, not amber: the focused ON track is already amber,
            // so an amber ring would make keyboard focus invisible there.
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
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let focused = self.target_is_focused(&SettingsFocusTarget::Control(control.clone()));
        let effective = self.choice_options(&control.key, options);
        let token = self.choice_token(&control.key);
        let display = effective
            .iter()
            .find(|(choice, _)| *choice == token)
            .map_or_else(|| token.clone(), |(_, label)| label.clone());
        let key = control.key.clone();
        let declared = options.to_vec();
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        let close_target = pointer_target.clone();
        let open = self.open_choice.as_deref() == Some(control.key.as_str());
        let button = div()
            .id(("choice", key_hash(&control.key)))
            .focusable()
            .tab_stop(true)
            .role(Role::ComboBox)
            .aria_label(control.label.clone())
            .aria_value(display.clone())
            .aria_expanded(open)
            .w(px(CHOICE_WIDTH))
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .rounded(px(5.0))
            .border_1()
            .border_color(if focused || open { colors.accent } else { colors.border })
            .bg(colors.control_bg)
            .text_sm()
            .text_color(colors.text)
            .cursor_pointer()
            .hover(move |style| {
                style.bg(colors.control_hover_bg).border_color(colors.strong_border)
            })
            .active(move |style| style.bg(colors.control_pressed_bg))
            .when(open, |button| {
                button.capture_any_mouse_down(cx.listener(
                    move |this, event: &MouseDownEvent, choice_window, ctx| {
                        this.press_open_choice_trigger(event, &close_target, choice_window, ctx);
                    },
                ))
            })
            .on_click(cx.listener(move |this, _, choice_window, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                this.toggle_choice_menu(&key, &declared, choice_window, ctx);
            }))
            .child(Text::new_inaccessible(display.into()))
            .child(
                div()
                    .font_family("Symbols Nerd Font Mono")
                    .text_sm()
                    .text_color(colors.quiet_text)
                    .child(if open { "\u{f106}" } else { "\u{f107}" }),
            );
        div()
            .id(("choice-shell", key_hash(&control.key)))
            .relative()
            .w(px(CHOICE_WIDTH))
            // The height is load-bearing, not decoration: the anchored menu is
            // absolutely positioned, so with an automatic height this shell
            // measures as zero-height and the hit test never reaches the button
            // inside it — the control stops responding to clicks entirely.
            .h(px(30.0))
            .flex_none()
            .when(open, |el| el.child(self.render_choice_menu(control, &effective, window, cx)))
            .child(button)
            .into_any_element()
    }

    fn render_choice_menu_rows(
        &self,
        control: &Control,
        options: &[(String, String)],
        cx: &mut Context<Self>,
    ) -> (Vec<gpui::AnyElement>, bool) {
        let colors = self.colors;
        let token = current_value(&self.config, &control.key).as_str().unwrap_or("").to_owned();
        let theme_menu = control.key == "theme.preset";
        let filtered =
            filter_choice_options(options, if theme_menu { &self.choice_filter } else { "" });
        let filtered_count = filtered.len();
        let rows = filtered
            .into_iter()
            .enumerate()
            .map(|(index, (option, label))| {
                let selected = *option == token;
                let highlighted = self.choice_highlight.as_deref() == Some(option.as_str());
                let (commit_key, commit_value) = (control.key.clone(), option.clone());
                let preview = theme_preset_preview(
                    &self.config,
                    &control.key,
                    &token,
                    option,
                    &self.theme_presets,
                )
                .map(theme_preview_strip);
                div()
                    .id(("choice-option", key_hash(&format!("{}:{option}", control.key))))
                    .focusable()
                    .tab_stop(true)
                    .role(Role::MenuItem)
                    .aria_label(label.clone())
                    .aria_selected(selected)
                    .aria_position_in_set(index + 1)
                    .aria_size_of_set(filtered_count)
                    .w_full()
                    .h(px(CHOICE_OPTION_HEIGHT))
                    .flex_none()
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .text_sm()
                    .text_color(if selected { colors.accent } else { colors.text })
                    .cursor_pointer()
                    .when(highlighted, |row| {
                        row.bg(colors.control_hover_bg).aria_active_descendant()
                    })
                    .hover(move |style| style.bg(colors.control_hover_bg))
                    .active(move |style| style.bg(colors.control_pressed_bg))
                    .on_click(cx.listener(move |this, _, choice_window, ctx| {
                        this.clear_choice_menu_state();
                        choice_window.focus(&this.focus_handle, ctx);
                        this.commit(&commit_key, Value::String(commit_value.clone()), ctx);
                    }))
                    .child(choice_option_label(label))
                    .children(preview)
                    .child(
                        div()
                            .font_family("Symbols Nerd Font Mono")
                            .text_xs()
                            .text_color(colors.accent)
                            .child(if selected { "\u{f00c}" } else { "" }),
                    )
                    .into_any_element()
            })
            .collect();
        (rows, theme_menu && filtered_count == 0)
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
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let theme_menu = control.key == "theme.preset";
        let (rows, empty) = self.render_choice_menu_rows(control, options, cx);
        let no_matches = empty.then(|| {
            div()
                .id("theme-preset-no-matches")
                .role(Role::Label)
                .aria_label("No matching themes")
                .h(px(CHOICE_OPTION_HEIGHT))
                .flex_none()
                .px(px(10.0))
                .flex()
                .items_center()
                .text_sm()
                .text_color(colors.dim_text)
                .child("No matching themes")
        });
        let filter = theme_menu.then(|| self.render_choice_filter(window, cx));
        let menu = div()
            .id(("choice-menu", key_hash(&control.key)))
            .role(Role::Menu)
            .aria_label(control.label.clone())
            .occlude()
            .on_mouse_down_out(cx.listener(move |this, _, choice_window, ctx| {
                this.close_choice_menu(ctx);
                choice_window.focus(&this.focus_handle, ctx);
            }))
            .when(theme_menu, |menu| menu.track_focus(&self.choice_filter_handle))
            .w(px(CHOICE_WIDTH))
            .mt(px(34.0))
            .max_h(px(CHOICE_MENU_MAX_HEIGHT))
            .flex()
            .flex_col()
            .rounded(px(6.0))
            .border_1()
            .border_color(colors.strong_border)
            .bg(colors.menu_bg)
            .shadow_lg();
        let menu = if theme_menu {
            menu.children(filter).child(
                div()
                    .id(("choice-options", key_hash(&control.key)))
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.choice_scroll)
                    .py(px(4.0))
                    .flex()
                    .flex_col()
                    .children(rows)
                    .children(no_matches),
            )
        } else {
            // Preserve the direct-child scroller used by compact choice menus.
            menu.overflow_y_scroll().track_scroll(&self.choice_scroll).py(px(4.0)).children(rows)
        };
        gpui::deferred(gpui::anchored().snap_to_window_with_margin(px(12.0)).child(menu))
            .with_priority(1)
            .into_any_element()
    }

    fn render_choice_filter(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let focus = self.choice_filter_handle.clone();
        let input_focus = focus.clone();
        let settings_entity = cx.entity();
        let focused = focus.is_focused(window);
        let value = self.choice_filter.clone();
        div()
            .px(px(4.0))
            .pt(px(4.0))
            .pb(px(2.0))
            .flex_none()
            .child(
                div()
                    .id("theme-preset-filter")
                    .role(Role::SearchInput)
                    .aria_label("Filter themes")
                    .aria_placeholder("Filter themes")
                    .aria_value(value.clone())
                    .w_full()
                    .h(px(30.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap_2()
                    .overflow_hidden()
                    .relative()
                    .rounded(px(5.0))
                    .border_1()
                    .border_color(if focused { colors.accent } else { colors.border })
                    .bg(colors.input_bg)
                    .text_sm()
                    .text_color(if value.is_empty() { colors.quiet_text } else { colors.text })
                    .cursor_text()
                    .on_click(cx.listener(move |this, _, input_window, ctx| {
                        this.active_input = Some(NativeInputTarget::ChoiceFilter);
                        this.input_selection = NativeInputSelection::Caret;
                        this.keyboard_navigation = false;
                        input_window.focus(&focus, ctx);
                        ctx.notify();
                    }))
                    .child(settings_search_icon(colors.quiet_text))
                    .child(Text::new_inaccessible(
                        if value.is_empty() && !focused {
                            "Filter themes".to_owned()
                        } else {
                            value
                        }
                        .into(),
                    ))
                    .when(focused, |el| {
                        el.child(
                            div().ml(px(2.0)).w(px(2.0)).h(px(14.0)).flex_none().bg(colors.accent),
                        )
                    })
                    .child(
                        canvas(
                            |_bounds, _window, _cx| {},
                            move |bounds, (), input_window, app| {
                                input_window.handle_input(
                                    &input_focus,
                                    ElementInputHandler::new(bounds, settings_entity),
                                    app,
                                );
                            },
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            .into_any_element()
    }

    /// Open this choice's dropdown, or close it when it is already the open one.
    fn toggle_choice_menu(
        &mut self,
        key: &str,
        options: &[(&'static str, &'static str)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.flush_theme_preset(cx);
        if self.open_choice.as_deref() == Some(key) {
            self.clear_choice_menu_state();
            window.focus(&self.focus_handle, cx);
        } else {
            self.open_color = None;
            self.clear_choice_menu_state();
            self.open_choice = Some(key.to_owned());
            self.align_choice_scroll(key, options);
            if key == "theme.preset" {
                self.active_input = Some(NativeInputTarget::ChoiceFilter);
                self.input_selection = NativeInputSelection::Caret;
                window.focus(&self.choice_filter_handle, cx);
            }
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
        self.choice_highlight = options.get(index).map(|(candidate, _)| candidate.clone());
        self.choice_scroll.set_offset(point(px(0.0), choice_scroll_offset(index)));
    }

    fn open_choice_control(&self) -> Option<Control> {
        let open_key = self.open_choice.as_deref()?;
        let SettingsFocusTarget::Control(control) = self.focused_target()? else {
            return None;
        };
        if !matches!(control.kind, ControlKind::Choice(_)) || control.key != open_key {
            return None;
        }
        Some(control)
    }

    fn align_choice_highlight(&mut self) {
        let Some(control) = self.open_choice_control() else {
            self.choice_highlight = None;
            return;
        };
        let ControlKind::Choice(declared) = &control.kind else {
            return;
        };
        let selected = current_value(&self.config, &control.key).as_str().unwrap_or("").to_owned();
        let options = self.choice_options(&control.key, declared);
        let query = if control.key == "theme.preset" { &self.choice_filter } else { "" };
        let filtered = filter_choice_options(&options, query);
        let highlighted = self.choice_highlight.as_deref();
        let index = highlighted
            .and_then(|token| filtered.iter().position(|(candidate, _)| candidate == token))
            .or_else(|| filtered.iter().position(|(candidate, _)| candidate == &selected))
            .or((!filtered.is_empty()).then_some(0));
        self.choice_highlight =
            index.and_then(|index| filtered.get(index)).map(|row| row.0.clone());
        self.choice_scroll.set_offset(point(px(0.0), index.map_or(px(0.0), choice_scroll_offset)));
    }

    fn move_choice_menu_highlight(&mut self, direction: isize, cx: &mut Context<Self>) {
        let Some(control) = self.open_choice_control() else {
            return;
        };
        let ControlKind::Choice(declared) = &control.kind else {
            return;
        };
        let selected = current_value(&self.config, &control.key).as_str().unwrap_or("").to_owned();
        let options = self.choice_options(&control.key, declared);
        let query = if control.key == "theme.preset" { &self.choice_filter } else { "" };
        let filtered = filter_choice_options(&options, query);
        let current = self
            .choice_highlight
            .as_deref()
            .and_then(|token| filtered.iter().position(|(candidate, _)| candidate == token))
            .or_else(|| filtered.iter().position(|(candidate, _)| candidate == &selected));
        let Some(next) = move_choice_highlight(current, filtered.len(), direction) else {
            self.choice_highlight = None;
            return;
        };
        let Some((token, _)) = filtered.get(next) else {
            return;
        };
        self.choice_highlight = Some(token.clone());
        self.choice_scroll.set_offset(point(px(0.0), choice_scroll_offset(next)));
        cx.notify();
    }

    fn apply_choice_highlight(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(control) = self.open_choice_control() else {
            return;
        };
        let ControlKind::Choice(declared) = &control.kind else {
            return;
        };
        let options = self.choice_options(&control.key, declared);
        let query = if control.key == "theme.preset" { &self.choice_filter } else { "" };
        let filtered = filter_choice_options(&options, query);
        let Some(token) = self
            .choice_highlight
            .as_deref()
            .filter(|token| filtered.iter().any(|(candidate, _)| candidate == token))
            .map(str::to_owned)
        else {
            return;
        };
        self.clear_choice_menu_state();
        window.focus(&self.focus_handle, cx);
        self.commit(&control.key, Value::String(token), cx);
    }

    fn clear_choice_menu_state(&mut self) -> bool {
        let was_open = self.open_choice.take().is_some();
        self.choice_highlight = None;
        self.choice_filter.clear();
        self.choice_filter_marked_range = None;
        if self.active_input == Some(NativeInputTarget::ChoiceFilter) {
            self.active_input = None;
            self.input_selection = NativeInputSelection::Caret;
        }
        was_open
    }

    /// Dismiss any open choice dropdown, notifying only when one was showing.
    fn close_choice_menu(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = self.clear_choice_menu_state();
        if was_open {
            cx.notify();
        }
        was_open
    }

    fn press_open_choice_trigger(
        &mut self,
        event: &MouseDownEvent,
        target: &SettingsFocusTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        self.begin_pointer_interaction(target);
        self.close_choice_menu(cx);
        window.focus(&self.focus_handle, cx);
        cx.stop_propagation();
    }

    /// Open one shared color palette, initialized from the live value's hue.
    fn toggle_color_picker(&mut self, control: &Control, cx: &mut Context<Self>) {
        if self.open_color.as_deref() == Some(control.key.as_str()) {
            self.open_color = None;
        } else {
            let value = current_value(&self.config, &control.key);
            self.color_hue = value
                .as_str()
                .and_then(|value| color_swatch(&self.theme, &control.key, value))
                .map(srgba)
                .map(gpui::Hsla::from)
                .map_or(self.color_hue, |color| color.h);
            self.clear_choice_menu_state();
            self.open_color = Some(control.key.clone());
        }
        cx.notify();
    }

    /// Commit a generated palette color through the same validator and writer
    /// as typed colors, retaining the canonical value for the exact-value field.
    fn select_color(&mut self, key: &str, value: &str, cx: &mut Context<Self>) -> bool {
        let stored = match inline_commit_value(true, key, value) {
            Ok(stored) => stored,
            Err(error) => {
                self.edit_error = Some(error);
                cx.notify();
                return false;
            }
        };
        if !self.commit(key, Value::String(stored.clone()), cx) {
            return false;
        }
        if self.edit_key() == Some(key) {
            self.edit_input.clone_from(&stored);
            self.edit_original = stored;
            self.edit_marked_range = None;
            self.edit_error = None;
        }
        true
    }

    fn close_color_picker(&mut self, cx: &mut Context<Self>) -> bool {
        let was_open = self.open_color.take().is_some();
        if was_open {
            cx.notify();
        }
        was_open
    }

    fn close_color_picker_for(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.open_color.as_deref() == Some(key) {
            self.close_color_picker(cx);
        }
    }

    fn press_open_color_trigger(
        &mut self,
        event: &MouseDownEvent,
        target: &SettingsFocusTarget,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        self.begin_pointer_interaction(target);
        self.close_color_picker(cx);
        cx.stop_propagation();
    }

    /// A keybinding row's value: its combos as key caps, or the listening state
    /// while the row records a replacement.
    ///
    /// The whole cell is the click target — there is no separate edit button,
    /// because on this page the value *is* the control.
    fn render_keybinding_value(
        &self,
        control: &Control,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let recording = self.capture_action.as_deref() == Some(control.key.as_str());
        let focused = self.target_is_focused(&SettingsFocusTarget::Control(control.clone()));
        let combos = keybinding_combos(&self.config, &control.key);
        let spoken = if combos.is_empty() {
            format!("{}: not bound", control.label)
        } else {
            format!(
                "{}: {}",
                control.label,
                combos.iter().map(|combo| key_combo_text(combo)).collect::<Vec<_>>().join(", ")
            )
        };
        let note = recording.then(|| {
            self.capture_error.clone().map_or_else(
                || ("Esc cancels · Backspace unbinds".to_owned(), colors.quiet_text),
                |error| (error, colors.error),
            )
        });
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        let capture_key = control.key.clone();
        let cell = div()
            .id(("settings-keybinding", key_hash(&control.key)))
            .role(Role::Button)
            .aria_label(spoken)
            .aria_description(if recording {
                "Listening for a shortcut"
            } else {
                "Activate to record a new shortcut"
            })
            .focusable()
            .tab_stop(true)
            .h(px(30.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .justify_end()
            .gap(px(5.0))
            .overflow_hidden()
            .rounded(px(5.0))
            .border_1()
            .border_color(if recording || focused { colors.accent } else { TRANSPARENT })
            .when(recording, |el| el.bg(colors.input_bg))
            .cursor_pointer()
            .when(!recording, |el| el.hover(move |style| style.border_color(colors.border)))
            .on_click(cx.listener(move |this, _, capture_window, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                this.begin_capture(&capture_key, capture_window, ctx);
            }))
            .children(keybinding_cell_content(recording, &combos, &colors));
        div()
            .w(px(VALUE_COLUMN_WIDTH))
            .flex()
            .flex_col()
            .items_end()
            .gap_1()
            .children(note.map(|(text, color)| {
                div()
                    .id(("settings-keybinding-note", key_hash(&control.key)))
                    .role(if self.capture_error.is_some() { Role::Alert } else { Role::Note })
                    .aria_label(text.clone())
                    .text_xs()
                    .text_color(color)
                    .child(Text::new_inaccessible(text.into()))
            }))
            .child(cell)
            .into_any_element()
    }

    fn render_action_control(&self, control: &Control, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let focused = self.target_is_focused(&SettingsFocusTarget::Control(control.clone()));
        let key = control.key.clone();
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        action_button(&control.label, &colors)
            .id(("action", key_hash(&control.key)))
            .when(focused, |el| el.border_color(colors.accent))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(control.label.clone())
            .on_click(cx.listener(move |this, _, action_window, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                this.run_action(&key, action_window, ctx);
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
            .w(px(152.0))
            .h(px(30.0))
            .rounded(px(5.0))
            .border_1()
            .border_color(if focused { colors.accent } else { colors.border })
            .bg(colors.control_bg)
            .overflow_hidden()
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

    /// Render one color as a compact swatch/value trigger with an anchored
    /// preset and custom palette. The exact-value editor stays inside the
    /// palette for hex entry and the AI color family's `ansi:N` values.
    fn render_color_selector(
        &self,
        control: &Control,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let open = self.open_color.as_deref() == Some(control.key.as_str());
        div()
            .id(("color-selector-shell", key_hash(&control.key)))
            .relative()
            .w(px(CHOICE_WIDTH))
            .h(px(30.0))
            .flex_none()
            .when(open, |el| el.child(self.render_color_menu(control, window, cx)))
            .child(self.render_color_trigger(control, open, cx))
            .into_any_element()
    }

    fn render_prompt_bar_color_control(
        &self,
        control: &Control,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let target = SettingsFocusTarget::PromptBarColorReset(control.key.clone());
        let focused = self.target_is_focused(&target);
        let key = control.key.clone();
        let reset = action_button("Reset", &colors)
            .id(("prompt-bar-color-reset", key_hash(&control.key)))
            .w(px(52.0))
            .px(px(0.0))
            .justify_center()
            .when(focused, |el| el.border_color(colors.accent))
            .focusable()
            .tab_stop(true)
            .role(Role::Button)
            .aria_label("Reset to theme default")
            .on_click(cx.listener(move |this, _, _window, ctx| {
                this.begin_pointer_interaction(&target);
                this.select_color(&key, "", ctx);
            }));
        div()
            .w(px(VALUE_COLUMN_WIDTH))
            .flex()
            .items_center()
            .justify_end()
            .gap_2()
            .child(self.render_color_selector(control, window, cx))
            .child(reset)
            .into_any_element()
    }

    fn render_color_trigger(
        &self,
        control: &Control,
        open: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let focused = self.target_is_focused(&SettingsFocusTarget::Control(control.clone()));
        let value = current_value(&self.config, &control.key).as_str().unwrap_or("").to_owned();
        let display = if value.is_empty() {
            inline_placeholder(&control.key, true).to_owned()
        } else {
            value.clone()
        };
        let swatch = color_swatch(&self.theme, &control.key, &value).map_or(colors.input_bg, srgba);
        let picker_control = control.clone();
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        let close_target = pointer_target.clone();
        div()
            .id(("color-selector", key_hash(&control.key)))
            .role(Role::ComboBox)
            .aria_label(format!("{} color", control.label))
            .aria_description("Press Enter or Space for presets and a custom palette; Left and Right choose presets; Tab edits the exact value")
            .aria_value(value)
            .aria_expanded(open)
            .focusable()
            .tab_stop(true)
            .w(px(CHOICE_WIDTH))
            .h(px(30.0))
            .px(px(7.0))
            .flex()
            .items_center()
            .gap_2()
            .rounded(px(5.0))
            .border_1()
            .border_color(if focused || open { colors.accent } else { colors.border })
            .bg(colors.control_bg)
            .cursor_pointer()
            .hover(move |style| {
                style.bg(colors.control_hover_bg).border_color(colors.strong_border)
            })
            .active(move |style| style.bg(colors.control_pressed_bg))
            .when(open, |trigger| {
                trigger.capture_any_mouse_down(cx.listener(
                    move |this, event: &MouseDownEvent, _window, ctx| {
                        this.press_open_color_trigger(event, &close_target, ctx);
                    },
                ))
            })
            .on_click(cx.listener(move |this, _, _window, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                this.toggle_color_picker(&picker_control, ctx);
            }))
            .child(
                div()
                    .size(px(18.0))
                    .flex_none()
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(colors.strong_border)
                    .bg(swatch),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .font_family("monospace")
                    .text_sm()
                    .text_color(if display.starts_with('#') || display.starts_with("ansi:") {
                        colors.text
                    } else {
                        colors.quiet_text
                    })
                    .child(Text::new_inaccessible(display.into())),
            )
            .child(
                div()
                    .font_family("Symbols Nerd Font Mono")
                    .text_sm()
                    .text_color(colors.quiet_text)
                    .child(if open { "\u{f106}" } else { "\u{f107}" }),
            )
            .into_any_element()
    }

    fn render_color_menu(
        &self,
        control: &Control,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let current = current_value(&self.config, &control.key).as_str().unwrap_or("").to_owned();
        let close_key = control.key.clone();
        let presets = self.render_color_presets(control, &current, cx);
        let hue_steps = self.render_color_hue_steps(control, cx);
        let palette = self.render_custom_color_palette(control, &current, cx);

        let error = (self.edit_key() == Some(control.key.as_str()))
            .then(|| self.edit_error.clone())
            .flatten()
            .map(|error| {
                div()
                    .id(("color-menu-error", key_hash(&control.key)))
                    .role(Role::Alert)
                    .aria_label(error.clone())
                    .text_xs()
                    .text_color(colors.error)
                    .child(Text::new_inaccessible(error.into()))
            });
        let exact = self.render_inline_field(control, window, cx);

        gpui::deferred(
            gpui::anchored().snap_to_window_with_margin(px(12.0)).child(
                div()
                    .id(("color-menu", key_hash(&control.key)))
                    .role(Role::Group)
                    .aria_label(format!("{} colors", control.label))
                    .occlude()
                    .on_mouse_down_out(cx.listener(move |this, _, _window, ctx| {
                        this.close_color_picker_for(&close_key, ctx);
                    }))
                    .w(px(COLOR_PICKER_WIDTH))
                    .ml(px(color_menu_left_offset()))
                    .mt(px(34.0))
                    .p(px(12.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(colors.strong_border)
                    .bg(colors.menu_bg)
                    .shadow_lg()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.quiet_text)
                            .child("PRESETS"),
                    )
                    .child(div().flex().flex_wrap().gap_2().children(presets))
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.quiet_text)
                            .child("CUSTOM"),
                    )
                    .child(div().flex().items_center().justify_between().children(hue_steps))
                    .child(palette)
                    .child(
                        div()
                            .font_family("monospace")
                            .text_xs()
                            .text_color(colors.quiet_text)
                            .child("EXACT VALUE"),
                    )
                    .children(error)
                    .child(exact),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn render_color_presets(
        &self,
        control: &Control,
        current: &str,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let colors = self.colors;
        COLOR_PRESETS
            .into_iter()
            .map(|(name, value)| {
                let selected = current == value;
                let key = control.key.clone();
                let pointer_target = SettingsFocusTarget::Control(control.clone());
                div()
                    .id(("color-preset", key_hash(&format!("{}:{value}", control.key))))
                    .role(Role::Image)
                    .aria_label(format!("{name} preset swatch, {value}"))
                    .size(px(32.0))
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(if selected { colors.accent } else { colors.strong_border })
                    .bg(rgb(u32::from_str_radix(&value[1..], 16).unwrap_or_default()))
                    .cursor_pointer()
                    .hover(move |style| style.border_color(colors.text))
                    .on_click(cx.listener(move |this, _, _window, ctx| {
                        this.begin_pointer_interaction(&pointer_target);
                        this.select_color(&key, value, ctx);
                    }))
                    .when(selected, |el| {
                        el.child(
                            div()
                                .font_family("Symbols Nerd Font Mono")
                                .text_xs()
                                .text_color(colors.accent)
                                .child("\u{f00c}"),
                        )
                    })
                    .into_any_element()
            })
            .collect()
    }

    fn render_color_hue_steps(
        &self,
        control: &Control,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let colors = self.colors;
        (0..COLOR_HUE_STEPS)
            .map(|index| {
                let hue = f32::from(index) / f32::from(COLOR_HUE_STEPS);
                let selected = (self.color_hue - hue).abs() < 0.04;
                let pointer_target = SettingsFocusTarget::Control(control.clone());
                div()
                    .id(("color-hue", usize::from(index)))
                    .role(Role::Image)
                    .aria_label(format!("Hue {} degree swatch", index * 30))
                    .w(px(17.0))
                    .h(px(18.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(if selected { colors.accent } else { colors.strong_border })
                    .bg(hsv_color(hue, 0.85, 0.9))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _window, ctx| {
                        this.begin_pointer_interaction(&pointer_target);
                        this.color_hue = hue;
                        ctx.notify();
                    }))
                    .into_any_element()
            })
            .collect()
    }

    fn render_custom_color_palette(
        &self,
        control: &Control,
        current: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let palette_bounds = Rc::new(Cell::new(None::<Bounds<Pixels>>));
        let paint_bounds = Rc::clone(&palette_bounds);
        let click_bounds = palette_bounds;
        let hue = self.color_hue;
        let key = control.key.clone();
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        div()
            .id(("custom-color-palette", key_hash(&control.key)))
            .role(Role::Image)
            .aria_label(format!("{} custom color palette", control.label))
            .aria_description("Pointer position chooses saturation and brightness; use the exact value field for keyboard entry")
            .aria_value(current.to_owned())
            .w_full()
            .h(px(COLOR_PALETTE_HEIGHT))
            .overflow_hidden()
            .rounded(px(4.0))
            .border_1()
            .border_color(colors.strong_border)
            .cursor_crosshair()
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _window, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                if let Some(bounds) = click_bounds.get()
                    && let Some(value) = palette_color_at(event.position(), bounds, hue)
                {
                    this.select_color(&key, &value, ctx);
                }
            }))
            .child(
                canvas(
                    move |bounds, _window, _cx| paint_bounds.set(Some(bounds)),
                    move |bounds, (), window, _cx| paint_color_palette(bounds, window, hue),
                )
                .size_full(),
            )
            .into_any_element()
    }

    /// Render the shared inline editor for general free-text fields. Color
    /// exact-value editing reuses this field inside its anchored palette.
    fn render_inline_edit(
        &self,
        control: &Control,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let is_color = matches!(control.kind, ControlKind::Color);
        let editing =
            self.edit_key() == Some(control.key.as_str()) && self.edit_handle.is_focused(window);
        let saved = current_value(&self.config, &control.key).as_str().unwrap_or("").to_owned();
        let value = if editing { self.edit_input.clone() } else { saved.clone() };
        let swatch_color = color_swatch(&self.theme, &control.key, &value)
            .or_else(|| color_swatch(&self.theme, &control.key, &saved))
            .map_or(colors.input_bg, srgba);
        let field = self.render_inline_field(control, window, cx);
        let error = (self.edit_key() == Some(control.key.as_str()))
            .then(|| self.edit_error.clone())
            .flatten()
            .map(|error| {
                div()
                    .id(("settings-inline-error", key_hash(&control.key)))
                    .role(Role::Alert)
                    .aria_label(error.clone())
                    .text_xs()
                    .text_color(colors.error)
                    .child(Text::new_inaccessible(error.into()))
            });
        div()
            .id(("settings-inline-value", key_hash(&control.key)))
            .role(Role::Group)
            .aria_label(if is_color {
                format!("{} color editor", control.label)
            } else {
                format!("{} editor", control.label)
            })
            .w(px(VALUE_COLUMN_WIDTH))
            .flex()
            .flex_col()
            .gap_1()
            .children(error)
            .child(
                div()
                    .w_full()
                    .h(px(30.0))
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(is_color, |el| {
                        el.child(
                            div()
                                .size(px(16.0))
                                .flex_none()
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(colors.strong_border)
                                .bg(swatch_color),
                        )
                    })
                    .child(field),
            )
            .into_any_element()
    }

    fn render_inline_field(
        &self,
        control: &Control,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let editing =
            self.edit_key() == Some(control.key.as_str()) && self.edit_handle.is_focused(window);
        let saved = current_value(&self.config, &control.key).as_str().unwrap_or("").to_owned();
        let value = if editing { self.edit_input.clone() } else { saved };
        let is_color = matches!(control.kind, ControlKind::Color);
        let placeholder = inline_placeholder(&control.key, is_color);
        let shown =
            if value.is_empty() && !editing { placeholder.to_owned() } else { value.clone() };
        let focus = self.edit_handle.clone();
        let input_focus = focus.clone();
        let settings_entity = cx.entity();
        let pointer_target = SettingsFocusTarget::Control(control.clone());
        let edit_control = control.clone();
        div()
            .id(("settings-inline-input", key_hash(&control.key)))
            .role(Role::TextInput)
            .aria_label(control.label.clone())
            .aria_placeholder(placeholder)
            .aria_value(value.clone())
            .on_a11y_action(
                AccessibleAction::SetValue,
                a11y_inline_edit_handler(cx.entity().downgrade(), control.clone()),
            )
            .focusable()
            .tab_stop(true)
            .w_full()
            .h(px(30.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .relative()
            .rounded(px(5.0))
            .border_1()
            .border_color(if editing { colors.accent } else { colors.border })
            .bg(colors.input_bg)
            .font_family("monospace")
            .text_sm()
            .text_color(if value.is_empty() { colors.quiet_text } else { colors.text })
            .cursor_text()
            .when(!editing, |el| el.hover(move |style| style.border_color(colors.strong_border)))
            .on_click(cx.listener(move |this, _, input_window, ctx| {
                this.begin_pointer_interaction(&pointer_target);
                this.begin_inline_edit(&edit_control);
                input_window.focus(&focus, ctx);
                ctx.notify();
            }))
            .child(Text::new_inaccessible(shown.into()))
            .when(editing, |el| {
                el.track_focus(&input_focus)
                    .child(div().ml(px(2.0)).w(px(2.0)).h(px(14.0)).flex_none().bg(colors.accent))
                    .child(
                        canvas(
                            |_bounds, _window, _cx| {},
                            move |bounds, (), input_window, app| {
                                input_window.handle_input(
                                    &input_focus,
                                    ElementInputHandler::new(bounds, settings_entity),
                                    app,
                                );
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
            })
            .into_any_element()
    }
}

/// Fixed width of the right-hand value column, the vertical rule that aligns
/// every control on a page.
const VALUE_COLUMN_WIDTH: f32 = 300.0;

/// Width of a choice button and its anchored menu inside the value column.
const CHOICE_WIDTH: f32 = 240.0;

/// Height of one dropdown option row, also the unit the open-scroll uses to put
/// the live value on screen.
const CHOICE_OPTION_HEIGHT: f32 = 30.0;

/// How much of a dropdown is kept visible above the live value when it opens.
const CHOICE_MENU_LEAD: f32 = 90.0;

/// Tallest a dropdown grows before it scrolls internally — the theme preset list
/// has close to 190 entries.
const CHOICE_MENU_MAX_HEIGHT: f32 = 360.0;

/// Fixed, useful starting colors. The custom palette below covers arbitrary
/// choices; these are the one-click path for common terminal roles.
const COLOR_PRESETS: [(&str, &str); 12] = [
    ("Black", "#000000"),
    ("White", "#ffffff"),
    ("Red", "#ef4444"),
    ("Orange", "#f97316"),
    ("Yellow", "#eab308"),
    ("Green", "#22c55e"),
    ("Teal", "#14b8a6"),
    ("Cyan", "#06b6d4"),
    ("Blue", "#3b82f6"),
    ("Indigo", "#6366f1"),
    ("Violet", "#a855f7"),
    ("Pink", "#ec4899"),
];

const COLOR_PICKER_WIDTH: f32 = 280.0;
const COLOR_PALETTE_HEIGHT: f32 = 120.0;
const COLOR_PALETTE_COLUMNS: u16 = 32;
const COLOR_PALETTE_ROWS: u16 = 16;
const COLOR_HUE_STEPS: u16 = 12;

fn color_menu_left_offset() -> f32 {
    CHOICE_WIDTH - COLOR_PICKER_WIDTH
}

/// Shared pending line for both keystore probes (the manual action and the
/// toggle's gated ON transition), so the two surfaces say the same thing.
const KEYSTORE_PENDING: &str = "Probing the OS keystore…";

/// How many releases the status line names before it summarizes the rest.
const RELEASE_SUMMARY_MAX: usize = 4;

/// Dimming applied to a control whose parent toggle is off.
const GATED_OPACITY: f32 = 0.42;

fn choice_scroll_offset(index: usize) -> Pixels {
    let rows = f32::from(u16::try_from(index).unwrap_or(u16::MAX));
    px(-(rows * CHOICE_OPTION_HEIGHT - CHOICE_MENU_LEAD).max(0.0))
}

fn choice_option_label(label: &str) -> gpui::AnyElement {
    div()
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .child(Text::new_inaccessible(label.to_owned().into()))
        .into_any_element()
}

fn theme_preview_strip(colors: [[f32; 4]; 10]) -> gpui::AnyElement {
    div()
        .flex_none()
        .h(px(10.0))
        .flex()
        .items_center()
        .gap(px(1.0))
        .children(
            colors.into_iter().map(|color| div().w(px(7.0)).h_full().flex_none().bg(srgba(color))),
        )
        .into_any_element()
}

/// An outline that reserves its space without drawing: a keybinding cell keeps
/// the same geometry whether or not it is the focused or recording row.
const TRANSPARENT: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

/// What a keybinding cell holds: the listening prompt, the "not bound" stand-in,
/// or one key cap per token with a quiet `or` between alternate combos.
fn keybinding_cell_content(
    recording: bool,
    combos: &[String],
    colors: &SettingsColors,
) -> Vec<gpui::AnyElement> {
    if recording {
        return vec![
            div()
                .text_sm()
                .text_color(colors.accent)
                .child(Text::new_inaccessible("Press a shortcut…".into()))
                .into_any_element(),
        ];
    }
    if combos.is_empty() {
        return vec![
            div()
                .text_sm()
                .text_color(colors.quiet_text)
                .child(Text::new_inaccessible("Not bound".into()))
                .into_any_element(),
        ];
    }
    let mut caps = Vec::new();
    for (index, combo) in combos.iter().enumerate() {
        if index > 0 {
            caps.push(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(colors.quiet_text)
                    .child(Text::new_inaccessible("or".into()))
                    .into_any_element(),
            );
        }
        caps.extend(combo.split('+').map(|token| key_cap(token, colors)));
    }
    caps
}

/// One key cap — the typeset grammar's mark for a literal key to press, and the
/// reason a shortcut is legible at a glance where `ctrl+shift+t` was a word to
/// be decoded.
fn key_cap(token: &str, colors: &SettingsColors) -> gpui::AnyElement {
    div()
        .flex_none()
        .h(px(20.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .rounded(px(4.0))
        .border_1()
        .border_color(colors.border)
        .bg(colors.control_bg)
        .font_family("monospace")
        .text_xs()
        .text_color(colors.text)
        .child(Text::new_inaccessible(key_cap_label(token).into()))
        .into_any_element()
}

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

fn workspace_roots_match_query(query: &str, roots: &[String]) -> bool {
    workspace_root_controls_match_query(query)
        || roots.iter().any(|root| workspace_root_matches_query(query, root))
}

fn workspace_root_prompt_options() -> PathPromptOptions {
    PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Choose".into()),
    }
}

fn search_display_text(query: &str, focused: bool) -> String {
    if query.is_empty() && !focused { "Search settings".to_owned() } else { query.to_owned() }
}

fn inline_placeholder(key: &str, is_color: bool) -> &'static str {
    if !is_color {
        return "Not set";
    }
    if key.starts_with("appearance.") {
        "Theme default"
    } else if key.starts_with("ai_states.") || key.starts_with("claude_states.") {
        "#rrggbb or ansi:0–15"
    } else {
        "#rrggbb"
    }
}

fn color_swatch(theme: &Theme, key: &str, value: &str) -> Option<[f32; 4]> {
    if key.starts_with("ai_states.") || key.starts_with("claude_states.") {
        let color: scribe_common::config::AiColor =
            serde_json::from_value(Value::String(value.to_owned())).ok()?;
        return Some(color.resolve(&theme.ansi_colors));
    }
    scribe_common::theme::hex_to_rgba(value)
        .ok()
        .or_else(|| value.is_empty().then(|| prompt_bar_theme_swatch(theme, key)).flatten())
}

fn prompt_bar_theme_swatch(theme: &Theme, key: &str) -> Option<[f32; 4]> {
    if !is_prompt_bar_color_override(key) {
        return None;
    }
    let chrome = theme.chrome;
    Some(match key {
        "appearance.prompt_bar_first_row_bg" => chrome.prompt_bar_first_row_bg,
        "appearance.prompt_bar_second_row_bg" => chrome.prompt_bar_second_row_bg,
        "appearance.prompt_bar_text" => chrome.prompt_bar_text,
        "appearance.prompt_bar_icon_first" => chrome.prompt_bar_icon_first,
        "appearance.prompt_bar_icon_latest" => chrome.prompt_bar_icon_latest,
        _ => return None,
    })
}

fn adjacent_color_preset(current: &str, direction: f64) -> &'static str {
    let index = COLOR_PRESETS.iter().position(|(_, value)| *value == current);
    let next = if direction.is_sign_negative() {
        index.map_or(COLOR_PRESETS.len() - 1, |index| {
            (index + COLOR_PRESETS.len() - 1) % COLOR_PRESETS.len()
        })
    } else {
        index.map_or(0, |index| (index + 1) % COLOR_PRESETS.len())
    };
    COLOR_PRESETS.get(next).or_else(|| COLOR_PRESETS.first()).map_or("", |preset| preset.1)
}

fn hsv_color(hue: f32, saturation: f32, value: f32) -> Rgba {
    let hue = hue.rem_euclid(1.0);
    let saturation = saturation.clamp(0.0, 1.0);
    let value = value.clamp(0.0, 1.0);
    let chroma = value * saturation;
    let sector = hue * 6.0;
    let secondary = chroma * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
    let (red, green, blue) = match sector.floor() {
        sector if sector < 1.0 => (chroma, secondary, 0.0),
        sector if sector < 2.0 => (secondary, chroma, 0.0),
        sector if sector < 3.0 => (0.0, chroma, secondary),
        sector if sector < 4.0 => (0.0, secondary, chroma),
        sector if sector < 5.0 => (secondary, 0.0, chroma),
        _ => (chroma, 0.0, secondary),
    };
    let offset = value - chroma;
    Rgba { r: red + offset, g: green + offset, b: blue + offset, a: 1.0 }
}

fn color_hex(color: Rgba) -> String {
    scribe_common::theme::rgba_to_hex([color.r, color.g, color.b, color.a])
}

fn paint_color_palette(bounds: Bounds<Pixels>, window: &mut Window, hue: f32) {
    let columns = f32::from(COLOR_PALETTE_COLUMNS);
    let rows = f32::from(COLOR_PALETTE_ROWS);
    let cell_width = f32::from(bounds.size.width) / columns;
    let cell_height = f32::from(bounds.size.height) / rows;
    for row in 0..COLOR_PALETTE_ROWS {
        for column in 0..COLOR_PALETTE_COLUMNS {
            let saturation = (f32::from(column) + 0.5) / columns;
            let value = 1.0 - (f32::from(row) + 0.5) / rows;
            let cell = Bounds {
                origin: point(
                    bounds.origin.x + px(f32::from(column) * cell_width),
                    bounds.origin.y + px(f32::from(row) * cell_height),
                ),
                size: size(px(cell_width + 0.5), px(cell_height + 0.5)),
            };
            window.paint_quad(fill(cell, hsv_color(hue, saturation, value)));
        }
    }
}

fn palette_color_at(position: Point<Pixels>, bounds: Bounds<Pixels>, hue: f32) -> Option<String> {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let saturation = (f32::from(position.x - bounds.origin.x) / width).clamp(0.0, 1.0);
    let value = (1.0 - f32::from(position.y - bounds.origin.y) / height).clamp(0.0, 1.0);
    Some(color_hex(hsv_color(hue, saturation, value)))
}

fn workspace_root_controls_match_query(query: &str) -> bool {
    query.is_empty()
        || query == "workspaces"
        || [
            "workspace roots",
            "configured workspace roots",
            "workspace root path",
            "add workspace root",
            "browse for workspace root directory",
            "workspaces.add_root",
        ]
        .iter()
        .any(|label| label.contains(query))
}

fn workspace_root_matches_query(query: &str, root: &str) -> bool {
    query.is_empty()
        || query == "workspaces"
        || [
            "workspace roots",
            "configured workspace roots",
            "remove workspace root",
            "workspaces.remove_root",
        ]
        .iter()
        .any(|label| label.contains(query))
        || root.to_lowercase().contains(query)
}

fn utf16_range_to_utf8(text: &str, range: Range<usize>) -> Range<usize> {
    utf16_offset_to_utf8(text, range.start)..utf16_offset_to_utf8(text, range.end)
}

fn utf16_offset_to_utf8(text: &str, offset: usize) -> usize {
    let mut utf16_offset = 0;
    for (utf8_offset, ch) in text.char_indices() {
        if utf16_offset >= offset {
            return utf8_offset;
        }
        utf16_offset += ch.len_utf16();
    }
    text.len()
}

fn utf8_range_to_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    let to_utf16 = |offset: usize| text[..offset.min(text.len())].encode_utf16().count();
    to_utf16(range.start)..to_utf16(range.end)
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

fn choice_options_from_cache(
    key: &str,
    current: &str,
    declared: &[(&'static str, &'static str)],
    theme_presets: &[PresetEntry],
) -> Vec<(String, String)> {
    let mut options = Vec::with_capacity(
        declared.len() + if key == "theme.preset" { theme_presets.len() } else { 0 },
    );
    if key == "theme.preset" {
        options.extend(
            theme_presets.iter().map(|preset| (preset.token.to_owned(), preset.label.clone())),
        );
    }
    options.extend(
        declared.iter().map(|(value, label)| ((*value).to_owned(), choice_label(value, label))),
    );
    if !current.is_empty() && !options.iter().any(|(candidate, _)| candidate == current) {
        options.insert(0, (current.to_owned(), humanize_choice_token(current)));
    }
    options
}

fn filter_choice_options<'a>(
    options: &'a [(String, String)],
    query: &str,
) -> Vec<&'a (String, String)> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return options.iter().collect();
    }
    options.iter().filter(|(_, label)| label.to_lowercase().contains(&query)).collect()
}

fn choice_menu_key_action(key: &str, modifiers: Modifiers) -> Option<ChoiceMenuKey> {
    if modifiers.control || modifiers.alt || modifiers.platform {
        return None;
    }
    match key {
        "up" => Some(ChoiceMenuKey::Previous),
        "down" => Some(ChoiceMenuKey::Next),
        "enter" => Some(ChoiceMenuKey::Apply),
        "escape" => Some(ChoiceMenuKey::Dismiss),
        "left" | "right" | "tab" => Some(ChoiceMenuKey::Swallow),
        _ => None,
    }
}

fn move_choice_highlight(current: Option<usize>, count: usize, direction: isize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    match current.filter(|index| *index < count) {
        Some(index) if direction.is_negative() => Some((index + count - 1) % count),
        Some(index) => Some((index + 1) % count),
        None if direction.is_negative() => Some(count - 1),
        None => Some(0),
    }
}

fn dismiss_choice_or_search(
    choice_filter: &mut String,
    open_choice: &mut Option<String>,
    search_query: &mut String,
) -> Option<DismissedTransient> {
    if !choice_filter.is_empty() {
        choice_filter.clear();
        return Some(DismissedTransient::ChoiceFilter);
    }
    if open_choice.take().is_some() {
        return Some(DismissedTransient::ChoiceMenu);
    }
    if !search_query.is_empty() {
        search_query.clear();
        return Some(DismissedTransient::PageSearch);
    }
    None
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

/// One section head: uppercase tracked caps in the quiet ink, more air above
/// than below — the typeset grammar's only grouping device.
fn heading_label(id: &'static str, text: &str, colors: &SettingsColors) -> gpui::AnyElement {
    div()
        .id((id, key_hash(text)))
        .role(Role::Heading)
        .aria_level(2)
        .aria_label(text.to_owned())
        .w_full()
        .flex_none()
        .pt(px(34.0))
        .pb(px(10.0))
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(colors.quiet_text)
        .child(Text::new_inaccessible(text.to_uppercase().into()))
        .into_any_element()
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
///
/// Visually it is plain right-aligned monospace in the dim ink — the typeset
/// grammar's mark for data that is stated, not edited. The absence of a
/// control outline is what says "not interactive"; the ARIA description says
/// it explicitly.
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
        .w(px(VALUE_COLUMN_WIDTH))
        .h(px(30.0))
        .flex()
        .items_center()
        .justify_end()
        .overflow_hidden()
        .font_family("monospace")
        .text_sm()
        .text_color(colors.dim_text)
        .child(Text::new_inaccessible(elide(value, READ_ONLY_MAX_CHARS).into()))
        .into_any_element()
}

/// Route AccessKit text-input actions back into the settings entity.
fn a11y_workspace_root_handler(
    weak_settings: gpui::WeakEntity<SettingsWindow>,
) -> impl FnMut(Option<&gpui::accesskit::ActionData>, &mut Window, &mut App) {
    move |data, _, app| {
        let Some(gpui::accesskit::ActionData::Value(action_value)) = data else {
            return;
        };
        let input = action_value.to_string();
        weak_settings
            .update(app, move |this, ctx| {
                this.workspace_root_input = input;
                this.workspace_root_marked_range = None;
                this.workspace_root_error = None;
                this.active_input = Some(NativeInputTarget::WorkspaceRoot);
                this.input_selection = NativeInputSelection::Caret;
                ctx.notify();
            })
            .ok();
    }
}

fn a11y_inline_edit_handler(
    weak_settings: gpui::WeakEntity<SettingsWindow>,
    set_control: Control,
) -> impl FnMut(Option<&gpui::accesskit::ActionData>, &mut Window, &mut App) {
    move |data, _, app| {
        let Some(gpui::accesskit::ActionData::Value(action_value)) = data else {
            return;
        };
        let input = action_value.to_string();
        let control = set_control.clone();
        weak_settings
            .update(app, move |this, ctx| {
                this.edit_input = input;
                current_value(&this.config, &control.key)
                    .as_str()
                    .unwrap_or("")
                    .clone_into(&mut this.edit_original);
                this.edit_control = Some(control);
                this.edit_marked_range = None;
                this.edit_error = None;
                this.active_input = Some(NativeInputTarget::Inline);
                this.input_selection = NativeInputSelection::Caret;
                ctx.notify();
            })
            .ok();
    }
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

/// Quiet neutral action button: amber stays reserved for live state, so an
/// idle action reads at the same volume as the rest of the page.
fn action_button(text: &str, colors: &SettingsColors) -> gpui::Stateful<gpui::Div> {
    let hover_bg = colors.control_hover_bg;
    let hover_border = colors.strong_border;
    let pressed_bg = colors.control_pressed_bg;
    div()
        .id("settings-action-button")
        .h(px(30.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .rounded(px(5.0))
        .border_1()
        .border_color(colors.border)
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .bg(colors.control_bg)
        .text_color(colors.text)
        .cursor_pointer()
        .hover(move |style| style.bg(hover_bg).border_color(hover_border))
        .active(move |style| style.bg(pressed_bg))
        .child(text.to_owned())
}

fn settings_search_icon(color: Rgba) -> gpui::AnyElement {
    // The magnifier from the embedded icon font — the same vocabulary as the
    // sidebar page glyphs — rather than a hand-assembled circle and slash.
    div()
        .flex_none()
        .font_family("Symbols Nerd Font Mono")
        .text_xs()
        .text_color(color)
        .child("\u{f002}")
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
            "\u{f2d1}",
            "Minimize window",
            WindowControlArea::Min,
            colors.control_hover_bg,
        ),
        SettingsWindowControl::Maximize => (
            "settings-window-maximize",
            "\u{f2d0}",
            "Maximize window",
            WindowControlArea::Max,
            colors.control_hover_bg,
        ),
        SettingsWindowControl::Close => (
            "settings-window-close",
            "\u{f00d}",
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
        .w(px(40.0))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .window_control_area(area)
        .font_family("Symbols Nerd Font Mono")
        .text_xs()
        .font_weight(FontWeight::NORMAL)
        .text_color(colors.quiet_text)
        .hover(move |style| style.bg(hover).text_color(colors.text))
        // Keep the press off the titlebar's drag arming so a click with a pixel
        // of jitter can never turn into a window move that eats the click.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, ctx| {
                this.flush_theme_preset(ctx);
                ctx.stop_propagation();
            }),
        )
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
        .w(px(36.0))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(if disabled { colors.quiet_text } else { colors.dim_text })
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
        SettingsPage::Keybindings => "Click a shortcut, then press the keys you want to use",
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
        SettingsPage::Workspaces if key.starts_with("workspaces.badge_colors.") => "Badge colors",
        SettingsPage::Workspaces => "Workspace configuration",
        SettingsPage::Updates => "Automatic updates",
        SettingsPage::Releases => "Release service",
        SettingsPage::Notifications => notification_section(key),
        SettingsPage::Remote => remote_section(key),
    }
}

fn appearance_section(key: &str) -> &'static str {
    if key.starts_with("appearance.focus_border") {
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
    } else if matches!(key, "terminal.claude_code_integration" | "terminal.codex_code_integration")
    {
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
            | "equalize"
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

const READ_ONLY_MAX_CHARS: usize = 34;

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
    let visible = cx.primary_display().map(|display| display.visible_bounds());
    let available =
        visible.map_or(size(px(SETTINGS_MIN_WIDTH), px(SETTINGS_MIN_HEIGHT)), |bounds| bounds.size);
    let minimum = size(
        px(SETTINGS_MIN_WIDTH.min(f32::from(available.width))),
        px(SETTINGS_MIN_HEIGHT.min(f32::from(available.height))),
    );
    let saved = crate::settings::state::load()
        .geometry
        .filter(|geometry| saved_settings_geometry_fits(*geometry, available));
    let bounds = saved.map_or_else(
        || Bounds::centered(None, minimum, cx),
        |geometry| {
            let saved_width = logical_i32(geometry.width);
            let saved_height = logical_i32(geometry.height);
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

/// Whether persisted geometry is already usable in GPUI's logical coordinate
/// space.
///
/// The retired settings app wrote physical Retina dimensions such as
/// 3520×2424. Clamping those numbers to a logical work area makes the window
/// effectively full-screen, so an out-of-area size is migrated to the compact
/// default instead of being interpreted as a user resize.
fn saved_settings_geometry_fits(
    geometry: crate::settings::state::SettingsWindowGeometry,
    available: Size<Pixels>,
) -> bool {
    let width = logical_i32(geometry.width);
    let height = logical_i32(geometry.height);
    width >= SETTINGS_MIN_WIDTH
        && height >= SETTINGS_MIN_HEIGHT
        && width <= f32::from(available.width)
        && height <= f32::from(available.height)
}

/// The scroller offset that puts the content pane at an absolute
/// `display_offset` — the inverse of the count-from-the-bottom mapping
/// [`SettingsWindow::content_scrollbar_layout`] builds, so a gesture the pure
/// geometry resolved lands on the pixel scroller it was measured from.
///
/// The `u16` conversion is the [`UNIT_CAP`] ceiling in code: a scroll unit is
/// one pixel, so the cap keeps the whole round trip cast-free.
fn content_scroll_offset(history_size: usize, target: usize) -> Pixels {
    let scrolled = u16::try_from(history_size.saturating_sub(target)).unwrap_or(u16::MAX);
    px(-f32::from(scrolled))
}

fn logical_i32(value: i32) -> f32 {
    f32::from(i16::try_from(value.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::{
        ChoiceMenuKey, NativeInputSelection, NativeInputTarget, Rect, SCROLLBAR_WIDTH,
        ScrollMetrics, ScrollbarDrag, ScrollbarLayout, SettingsFocusTarget, adjacent_color_preset,
        build_theme_preset_cache, canonical_combo, choice_menu_key_action,
        choice_options_from_cache, choice_scroll_offset, color_menu_left_offset, combo_for_capture,
        conflicting_action, content_scroll_offset, dismiss_choice_or_search, filter_choice_options,
        focus_targets_match, inline_commit_value, inline_placeholder, is_modifier_key,
        key_combo_text, move_choice_highlight, offset_from_drag, offset_from_track_click,
        palette_color_at, prompt_bar_reset_change, prompt_bar_theme_swatch,
        push_control_focus_targets, px, release_inline_input, replace_pending_theme_preset,
        revert_inline_input, search_display_text, take_pending_theme_preset, theme_preset_preview,
        utf16_range_to_utf8, workspace_badge_color_controls, workspace_root_controls_match_query,
        workspace_root_focus_index, workspace_root_matches_query, workspace_root_prompt_options,
        workspace_roots_match_query,
    };
    use gpui::{Bounds, Keystroke, Modifiers, point, size};
    use scribe_common::config::{KeyComboList, ScribeConfig, ThemeConfig};

    use crate::settings::model::{SettingsPage, page_controls};

    fn keystroke(modifiers: Modifiers, key: &str) -> Keystroke {
        Keystroke { modifiers, key: key.to_owned(), key_char: None }
    }

    // @lat: [[test#GPUI Settings Window#Scrollbar gestures map back onto the scroller]]
    #[test]
    fn scrollbar_gestures_map_back_onto_the_scroller() {
        // A 400px viewport over a 1200px page: 800px of overflow, opened at the
        // top, which the pure geometry reads as a full display offset.
        let layout = ScrollbarLayout {
            pane_rect: Rect { x: 0.0, y: 0.0, width: 600.0, height: 400.0 },
            metrics: ScrollMetrics { history_size: 800, screen_lines: 400, display_offset: 800 },
            tab_bar_height: 0.0,
        };

        let top = offset_from_track_click(&layout, 0.0, SCROLLBAR_WIDTH);
        let bottom = offset_from_track_click(&layout, 400.0, SCROLLBAR_WIDTH);
        assert_eq!(top, 800);
        assert_eq!(bottom, 0);
        // The offset counts up from the live bottom and the scroller counts
        // down from the top of the page: pressing the track top rewinds to
        // zero, and the bottom scrolls the whole overflow.
        assert_eq!(content_scroll_offset(800, top), px(0.0));
        assert_eq!(content_scroll_offset(800, bottom), px(-800.0));

        // Dragging the thumb down scrolls the page down, not up.
        let drag = ScrollbarDrag { start_mouse_y: 0.0, start_display_offset: top };
        let dragged = offset_from_drag(&layout, &drag, 100.0, SCROLLBAR_WIDTH);
        assert!(dragged < top);
        assert!(content_scroll_offset(800, dragged) < px(0.0));
    }

    // @lat: [[test#GPUI Settings Window#Shortcut capture]]
    #[test]
    fn capture_writes_a_combo_the_dispatcher_can_parse() {
        let pressed =
            keystroke(Modifiers { control: true, shift: true, ..Modifiers::default() }, "T");

        assert_eq!(combo_for_capture(&pressed).unwrap(), "ctrl+shift+t");
        // A modifier press is not a shortcut; the row waits through it.
        assert!(is_modifier_key("ctrl") && is_modifier_key("shift"));
        assert!(!is_modifier_key("t"));
    }

    // @lat: [[test#GPUI Settings Window#Shortcut capture#Unbindable keystrokes are refused]]
    #[test]
    fn capture_refuses_keystrokes_that_would_break_the_terminal() {
        let bare = keystroke(Modifiers::default(), "t");
        let shift_only = keystroke(Modifiers { shift: true, ..Modifiers::default() }, "t");
        let f_key = keystroke(Modifiers { control: true, ..Modifiers::default() }, "f5");

        assert!(combo_for_capture(&bare).is_err());
        assert!(combo_for_capture(&shift_only).is_err());
        // The client's own parser has no F-key vocabulary, so binding one would
        // write a shortcut that never fires.
        assert!(combo_for_capture(&f_key).is_err());
    }

    // @lat: [[test#GPUI Settings Window#Shortcut capture#Conflicts are named before they are written]]
    #[test]
    fn conflicts_compare_combos_by_canonical_spelling() {
        let mut config = ScribeConfig::default();
        config.keybindings.new_tab = KeyComboList::single("ctrl+shift+t");

        // Hand-written order and alias fold together before the comparison.
        assert_eq!(canonical_combo("shift+ctrl+t").as_deref(), Some("ctrl+shift+t"));
        assert_eq!(canonical_combo("super+ctrl+w").as_deref(), Some("ctrl+cmd+w"));
        assert!(canonical_combo("ctrl+f5").is_none());
        assert_eq!(conflicting_action(&config, "shift+ctrl+t", "close_tab"), Some("new_tab"));
        // Re-pressing a row's own shortcut is not a conflict with itself.
        assert_eq!(conflicting_action(&config, "ctrl+shift+t", "new_tab"), None);
    }

    #[test]
    fn key_caps_read_as_words() {
        assert_eq!(key_combo_text("ctrl+pagedown"), "Ctrl Page Down");
        assert_eq!(key_combo_text("alt+1"), "Alt 1");
    }

    #[test]
    fn search_placeholder_hides_only_for_focused_empty_input() {
        assert_eq!(search_display_text("", false), "Search settings");
        assert_eq!(search_display_text("", true), "");
        assert_eq!(search_display_text("colors", true), "colors");
    }

    #[test]
    fn workspace_badge_controls_follow_configured_palette_length() {
        let controls = workspace_badge_color_controls(2);

        assert_eq!(controls.len(), 2);
        assert_eq!(controls[0].key, "workspaces.badge_colors.0");
        assert_eq!(controls[1].label, "Badge color 2");
    }

    // @lat: [[test#GPUI Settings Window#Theme preset cache]]
    #[test]
    fn cached_theme_presets_feed_the_choice_list() {
        let presets = build_theme_preset_cache();
        let options =
            choice_options_from_cache("theme.preset", "dracula", &[("custom", "Custom")], &presets);

        assert_eq!(presets.len(), 192);
        assert_eq!(options.len(), 193);
        assert_eq!(options.first(), Some(&(String::from("3024-day"), String::from("3024 Day"))));
        assert_eq!(options.last(), Some(&(String::from("custom"), String::from("Custom"))));
        let dracula = presets
            .iter()
            .find(|preset| preset.token == "dracula")
            .expect("Dracula must be cached");
        assert_eq!(dracula.theme.name, "dracula");
    }

    // @lat: [[test#GPUI Settings Window#Theme preset filtering#Display-label matching]]
    #[test]
    fn theme_preset_filter_matches_display_labels_case_insensitively() {
        let presets = build_theme_preset_cache();
        let options =
            choice_options_from_cache("theme.preset", "dracula", &[("custom", "Custom")], &presets);

        let filtered = filter_choice_options(&options, "DRAC");

        assert_eq!(filtered, vec![&(String::from("dracula"), String::from("Dracula"))]);
        assert_eq!(filter_choice_options(&options, "").len(), 193);
        assert!(filter_choice_options(&options, "no such theme").is_empty());
    }

    // @lat: [[test#GPUI Settings Window#Theme preset filtering#Escape order]]
    #[test]
    fn escape_clears_choice_filter_then_menu_then_page_search() {
        let mut filter = String::from("drac");
        let mut menu = Some(String::from("theme.preset"));
        let mut search = String::from("colors");

        assert_eq!(
            dismiss_choice_or_search(&mut filter, &mut menu, &mut search),
            Some(super::DismissedTransient::ChoiceFilter)
        );
        assert!(filter.is_empty());
        assert!(menu.is_some());
        assert_eq!(search, "colors");

        assert_eq!(
            dismiss_choice_or_search(&mut filter, &mut menu, &mut search),
            Some(super::DismissedTransient::ChoiceMenu)
        );
        assert!(menu.is_none());
        assert_eq!(search, "colors");

        assert_eq!(
            dismiss_choice_or_search(&mut filter, &mut menu, &mut search),
            Some(super::DismissedTransient::PageSearch)
        );
        assert!(search.is_empty());
    }

    // @lat: [[test#GPUI Settings Window#Theme preset filtering#Keyboard routing]]
    #[test]
    fn open_choice_keys_stay_inside_the_menu() {
        let modifiers = Modifiers::default();
        assert_eq!(choice_menu_key_action("up", modifiers), Some(ChoiceMenuKey::Previous));
        assert_eq!(choice_menu_key_action("down", modifiers), Some(ChoiceMenuKey::Next));
        assert_eq!(choice_menu_key_action("enter", modifiers), Some(ChoiceMenuKey::Apply));
        assert_eq!(choice_menu_key_action("escape", modifiers), Some(ChoiceMenuKey::Dismiss));
        assert_eq!(choice_menu_key_action("left", modifiers), Some(ChoiceMenuKey::Swallow));
        assert_eq!(choice_menu_key_action("right", modifiers), Some(ChoiceMenuKey::Swallow));
        assert_eq!(choice_menu_key_action("tab", modifiers), Some(ChoiceMenuKey::Swallow));
        assert_eq!(choice_menu_key_action("space", modifiers), None);
    }

    // @lat: [[test#GPUI Settings Window#Theme preset filtering#Modified Tab routing]]
    #[test]
    fn open_choice_swallows_plain_and_shift_tab() {
        assert_eq!(
            choice_menu_key_action("tab", Modifiers::default()),
            Some(ChoiceMenuKey::Swallow)
        );
        assert_eq!(
            choice_menu_key_action("tab", Modifiers { shift: true, ..Modifiers::default() }),
            Some(ChoiceMenuKey::Swallow)
        );
    }

    // @lat: [[test#Test Harness#GPUI Settings Window#Closed theme preset debounce]]
    #[test]
    fn theme_preset_steps_settle_once_and_escape_discards() {
        let mut pending = None;
        let mut generation = 0;
        let stale =
            replace_pending_theme_preset(&mut pending, &mut generation, String::from("dracula"));
        replace_pending_theme_preset(&mut pending, &mut generation, String::from("gruvbox-dark"));
        let current = replace_pending_theme_preset(
            &mut pending,
            &mut generation,
            String::from("solarized-dark"),
        );
        let mut applied = Vec::new();

        if let Some(value) = take_pending_theme_preset(&mut pending, &mut generation, Some(stale)) {
            applied.push(value);
        }
        if let Some(value) = take_pending_theme_preset(&mut pending, &mut generation, Some(current))
        {
            applied.push(value);
        }

        assert_eq!(applied.len(), 1);
        assert_eq!(applied, ["solarized-dark"]);
        assert_eq!(pending.as_deref().unwrap_or("minimal-dark"), "minimal-dark");

        let mut cancelled = None;
        let mut cancelled_generation = 0;
        let cancelled_timer = replace_pending_theme_preset(
            &mut cancelled,
            &mut cancelled_generation,
            String::from("dracula"),
        );
        assert_eq!(cancelled.as_deref().unwrap_or("minimal-dark"), "dracula");
        let discarded = take_pending_theme_preset(&mut cancelled, &mut cancelled_generation, None);
        assert_eq!(discarded.as_deref(), Some("dracula"));
        assert_eq!(cancelled.as_deref().unwrap_or("minimal-dark"), "minimal-dark");
        let escaped_applies = usize::from(
            take_pending_theme_preset(
                &mut cancelled,
                &mut cancelled_generation,
                Some(cancelled_timer),
            )
            .is_some(),
        );
        assert_eq!(escaped_applies, 0);
    }

    // @lat: [[test#GPUI Settings Window#Theme preset filtering#Filtered highlight]]
    #[test]
    fn choice_highlight_moves_only_through_filtered_rows() {
        let options = vec![
            (String::from("day"), String::from("Day")),
            (String::from("dark"), String::from("Dark")),
            (String::from("darker"), String::from("Darker")),
        ];
        let filtered = filter_choice_options(&options, "dark");

        assert_eq!(move_choice_highlight(Some(0), filtered.len(), 1), Some(1));
        assert_eq!(move_choice_highlight(Some(1), filtered.len(), 1), Some(0));
        assert_eq!(move_choice_highlight(Some(0), filtered.len(), -1), Some(1));
        assert_eq!(move_choice_highlight(None, 0, 1), None);
    }

    // @lat: [[test#GPUI Settings Window#Theme preset cache#Preset preview order]]
    #[test]
    fn preset_strip_uses_background_foreground_then_normal_ansi() {
        let presets = build_theme_preset_cache();
        let colors = theme_preset_preview(
            &ScribeConfig::default(),
            "theme.preset",
            "minimal-dark",
            "dracula",
            &presets,
        )
        .expect("resolved presets carry a preview")
        .map(scribe_common::theme::rgba_to_hex);

        assert_eq!(
            colors,
            [
                "#282a36", "#f8f8f2", "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9",
                "#ff79c6", "#8be9fd", "#f8f8f2",
            ]
        );
    }

    // @lat: [[test#GPUI Settings Window#Theme preset cache#Active Custom preview]]
    #[test]
    fn custom_strip_uses_inline_theme_only_while_custom_is_active() {
        let presets = build_theme_preset_cache();
        let mut config = ScribeConfig::default();

        assert!(
            theme_preset_preview(&config, "theme.preset", "minimal-dark", "custom", &presets)
                .is_none()
        );

        config.appearance.theme = String::from("custom");
        config.theme = Some(ThemeConfig {
            name: String::from("custom"),
            foreground: String::from("#112233"),
            background: String::from("#445566"),
            cursor: String::from("#112233"),
            cursor_accent: String::from("#445566"),
            selection: String::from("#778899"),
            selection_foreground: String::from("#112233"),
            colors: vec![String::from("#000000"); 16],
        });
        let colors = theme_preset_preview(&config, "theme.preset", "custom", "custom", &presets)
            .expect("active custom theme carries its resolved preview")
            .map(scribe_common::theme::rgba_to_hex);

        assert_eq!(colors[0], "#445566");
        assert_eq!(colors[1], "#112233");
        assert_eq!(colors[2..], ["#000000"; 8]);
    }

    // @lat: [[test#GPUI Settings Window#Prompt bar color overrides#Theme defaults follow the live palette]]
    #[test]
    fn prompt_bar_theme_swatches_use_the_resolved_cache_and_keep_text_alpha() {
        let mut config = ScribeConfig::default();
        config.appearance.theme = String::from("custom");
        config.theme = Some(ThemeConfig {
            name: String::from("custom"),
            foreground: String::from("#e9e8e4"),
            background: String::from("#1d1f24"),
            cursor: String::from("#e9e8e4"),
            cursor_accent: String::from("#141518"),
            selection: String::from("#262931"),
            selection_foreground: String::from("#a6a5a0"),
            colors: ["#141518"; 16].map(str::to_owned).to_vec(),
        });
        let theme = config.theme.as_mut().expect("custom theme");
        theme.colors[3] = String::from("#f5b83a");
        theme.colors[4] = String::from("#e0584c");
        let resolved = scribe_common::config::resolve_theme(&config);

        let cases = [
            ("appearance.prompt_bar_first_row_bg", "#181a1f"),
            ("appearance.prompt_bar_second_row_bg", "#25272c"),
            ("appearance.prompt_bar_text", "#e9e8e4"),
            ("appearance.prompt_bar_icon_first", "#f5b83a"),
            ("appearance.prompt_bar_icon_latest", "#e0584c"),
        ];
        for (key, expected) in cases {
            let swatch = prompt_bar_theme_swatch(&resolved, key).expect("prompt-bar theme slot");
            assert_eq!(scribe_common::theme::rgba_to_hex(swatch), expected, "{key}");
        }
        assert_eq!(
            prompt_bar_theme_swatch(&resolved, "appearance.prompt_bar_text")
                .expect("prompt-bar text")[3]
                .to_bits(),
            0.5_f32.to_bits()
        );

        config.theme.as_mut().expect("custom theme").colors[4] = String::from("#ffc85c");
        assert_eq!(
            scribe_common::theme::rgba_to_hex(
                prompt_bar_theme_swatch(&resolved, "appearance.prompt_bar_icon_latest")
                    .expect("cached latest icon"),
            ),
            "#e0584c"
        );
        let reloaded = scribe_common::config::resolve_theme(&config);
        assert_eq!(
            scribe_common::theme::rgba_to_hex(
                prompt_bar_theme_swatch(&reloaded, "appearance.prompt_bar_icon_latest")
                    .expect("edited latest icon"),
            ),
            "#ffc85c"
        );
    }

    // @lat: [[test#GPUI Settings Window#Prompt bar color overrides#Reset is a keyboard focus stop]]
    #[test]
    fn prompt_bar_reset_follows_its_color_control_and_activates_with_empty_value() {
        let control = page_controls(SettingsPage::Colors)
            .into_iter()
            .find(|control| control.key == "appearance.prompt_bar_text")
            .expect("prompt-bar text control");
        let mut targets = Vec::new();

        push_control_focus_targets(&mut targets, control);

        assert!(matches!(
            targets.as_slice(),
            [
                SettingsFocusTarget::Control(target_control),
                SettingsFocusTarget::PromptBarColorReset(key),
            ] if target_control.key == "appearance.prompt_bar_text" && key == "appearance.prompt_bar_text"
        ));
        let SettingsFocusTarget::PromptBarColorReset(key) = &targets[1] else {
            panic!("second focus stop must reset the prompt-bar color");
        };
        assert_eq!(prompt_bar_reset_change(key), ("appearance.prompt_bar_text", ""));
    }

    // @lat: [[test#GPUI Settings Window#Theme preset cache#Selected row visibility]]
    #[test]
    fn choice_scroll_keeps_the_last_preset_row_visible() {
        let index = 192;
        let offset = choice_scroll_offset(index);
        let viewport_top = -f32::from(offset);
        let row_top = f32::from(u16::try_from(index).expect("preset index fits"))
            * super::CHOICE_OPTION_HEIGHT;

        assert_eq!(offset, px(-5670.0));
        assert!(row_top >= viewport_top);
        assert!(
            row_top + super::CHOICE_OPTION_HEIGHT <= viewport_top + super::CHOICE_MENU_MAX_HEIGHT
        );
    }

    #[test]
    fn workspace_root_search_matches_controls_and_paths() {
        let roots = vec![String::from("/srv/Projects")];

        assert!(workspace_roots_match_query("add workspace", &roots));
        assert!(workspace_roots_match_query("browse for workspace", &roots));
        assert!(workspace_roots_match_query("projects", &roots));
        assert!(workspace_roots_match_query("workspaces", &roots));
        assert!(workspace_root_controls_match_query("workspaces"));
        assert!(!workspace_root_controls_match_query("projects"));
        assert!(workspace_root_matches_query("projects", &roots[0]));
        assert!(!workspace_root_matches_query("projects", "/srv/other"));
        assert!(!workspace_roots_match_query("badge colors", &roots));
    }

    #[test]
    fn workspace_root_focus_targets_match_stable_rows() {
        let first =
            SettingsFocusTarget::WorkspaceRootRemove { index: 0, root: String::from("/srv/work") };
        let same = first.clone();
        let next =
            SettingsFocusTarget::WorkspaceRootRemove { index: 1, root: String::from("/srv/work") };

        assert!(focus_targets_match(&first, &same));
        assert!(!focus_targets_match(&first, &next));
        assert!(focus_targets_match(
            &SettingsFocusTarget::WorkspaceRootInput,
            &SettingsFocusTarget::WorkspaceRootInput,
        ));
        assert!(focus_targets_match(
            &SettingsFocusTarget::WorkspaceRootBrowse,
            &SettingsFocusTarget::WorkspaceRootBrowse,
        ));
        assert!(!focus_targets_match(
            &SettingsFocusTarget::WorkspaceRootInput,
            &SettingsFocusTarget::WorkspaceRootBrowse,
        ));
    }

    #[test]
    fn workspace_root_focus_re_resolves_after_rows_change() {
        let targets = vec![
            SettingsFocusTarget::WorkspaceRootRemove { index: 0, root: String::from("/srv/next") },
            SettingsFocusTarget::WorkspaceRootInput,
            SettingsFocusTarget::WorkspaceRootBrowse,
            SettingsFocusTarget::WorkspaceRootAdd,
        ];

        assert_eq!(
            workspace_root_focus_index(&targets, &SettingsFocusTarget::WorkspaceRootInput),
            Some(1)
        );
        assert_eq!(
            workspace_root_focus_index(&targets, &SettingsFocusTarget::WorkspaceRootBrowse),
            Some(2)
        );
        assert_eq!(
            workspace_root_focus_index(
                &targets,
                &SettingsFocusTarget::WorkspaceRootRemove {
                    index: 0,
                    root: String::from("/srv/removed"),
                },
            ),
            Some(0)
        );
        assert_eq!(
            workspace_root_focus_index(
                &targets[1..],
                &SettingsFocusTarget::WorkspaceRootRemove {
                    index: 0,
                    root: String::from("/srv/removed"),
                },
            ),
            Some(0)
        );
    }

    #[test]
    fn workspace_root_input_ranges_follow_utf16_offsets() {
        assert_eq!(utf16_range_to_utf8("a😀b", 1..3), 1..5);
    }

    #[test]
    fn workspace_root_chooser_selects_one_directory() {
        let options = workspace_root_prompt_options();

        assert!(!options.files);
        assert!(options.directories);
        assert!(!options.multiple);
        assert_eq!(options.prompt.as_deref(), Some("Choose"));
    }

    #[test]
    fn releasing_inline_input_preserves_workspace_root_input() {
        let mut active = Some(NativeInputTarget::WorkspaceRoot);
        let mut selection = NativeInputSelection::All;

        assert!(!release_inline_input(&mut active, &mut selection));
        assert!(matches!(active, Some(NativeInputTarget::WorkspaceRoot)));
        assert_eq!(selection, NativeInputSelection::All);

        active = Some(NativeInputTarget::Inline);
        assert!(release_inline_input(&mut active, &mut selection));
        assert!(active.is_none());
        assert_eq!(selection, NativeInputSelection::Caret);
    }

    // @lat: [[settings#GPUI Settings Window#Inline editing#Commit routes by control kind]]
    #[test]
    fn inline_commit_routes_color_and_free_text_differently() {
        assert_eq!(
            inline_commit_value(true, "theme.background", "#ABCDEF"),
            Ok(String::from("#abcdef"))
        );
        assert!(inline_commit_value(true, "theme.background", "not a color").is_err());
        // Free text commits verbatim — no colour validator stands between a
        // font name and the apply path.
        assert_eq!(
            inline_commit_value(false, "appearance.font_family", "not a color"),
            Ok(String::from("not a color"))
        );
        assert_eq!(inline_placeholder("appearance.font_family", false), "Not set");
        assert_eq!(inline_placeholder("theme.background", true), "#rrggbb");
    }

    // @lat: [[test#GPUI Settings Window#Color selector menu geometry]]
    #[test]
    fn color_menu_right_aligns_with_its_trigger() {
        let right_edge = color_menu_left_offset() + super::COLOR_PICKER_WIDTH;

        assert!((right_edge - super::CHOICE_WIDTH).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Settings Window#Color selector palette]]
    #[test]
    fn custom_palette_maps_clickable_interior_points_to_canonical_colors() {
        let bounds = Bounds { origin: point(px(10.0), px(20.0)), size: size(px(200.0), px(100.0)) };

        assert_eq!(
            palette_color_at(point(px(110.0), px(70.0)), bounds, 0.0).as_deref(),
            Some("#804040")
        );
        assert_eq!(
            palette_color_at(point(px(209.0), px(21.0)), bounds, 0.0).as_deref(),
            Some("#fc0101")
        );
        assert_eq!(
            palette_color_at(point(px(209.0), px(21.0)), bounds, 1.0 / 3.0).as_deref(),
            Some("#01fc01")
        );
        assert_eq!(adjacent_color_preset("#000000", 1.0), "#ffffff");
        assert_eq!(adjacent_color_preset("#000000", -1.0), "#ec4899");
    }

    // @lat: [[settings#GPUI Settings Window#Inline editing#Escape cancels the edit]]
    #[test]
    fn cancelling_an_inline_edit_restores_the_opening_value() {
        let mut input = String::from("Half-typed Fon");
        let mut marked = Some(3..7);
        let mut error = Some(String::from("rejected"));

        revert_inline_input(&mut input, "JetBrains Mono", &mut marked, &mut error);

        assert_eq!(input, "JetBrains Mono");
        assert!(marked.is_none());
        assert!(error.is_none());
    }
}
