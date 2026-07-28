//! Feature 013 (T017) displaced-client "lost control" state, ported into the
//! GPUI rebuild.
//!
//! When another controller claims a window this client was driving, the server
//! sends [`WindowTakenOver`](scribe_common::protocol::ServerMessage::WindowTakenOver).
//! The client freezes its last frame (it expects no further `PtyOutput`),
//! suppresses ALL input for that window, and — in the GPUI chrome — renders a
//! dimmed backdrop under a centered banner naming the new controller, offering
//! one-action reclaim (Enter or click) that reconnects with `Hello { takeover:
//! true }`.
//!
//! The same state drives BOTH a local client displaced by a remote peer and a
//! remote client displaced by a reclaim — the displaced-client obligations are
//! transport-agnostic, so the state lives here once and the app layer wires the
//! reclaim to the transport it already speaks. This port keeps the identity /
//! headline / reclaim-intent logic; the winit `CellInstance` painting is dropped
//! in favour of the GPUI banner element [`lost_control_overlay`], which the
//! shell hangs over the whole window while [`crate::remote_chrome::RemoteChrome`]
//! holds a displaced state.

use gpui::{AnyElement, Rgba, div, prelude::*, px};
use scribe_common::theme::ChromeColors;

use crate::tab_bar::srgba;

/// What a displaced client renders after `WindowTakenOver`. Holds only the new
/// controller's identity strings; the frozen grid content is the panes' own
/// last-rendered state, which the app stops advancing while this is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostControlState {
    /// New controller's device name (or "this machine" for a local reclaim).
    device_name: String,
    /// New controller's tailnet account display name.
    login_name: String,
}

impl LostControlState {
    #[must_use]
    pub fn new(device_name: String, login_name: String) -> Self {
        Self { device_name, login_name }
    }

    /// Banner headline per FR-009b / settings-and-config.md:
    /// `Controlled by <device> (<account>)`.
    #[must_use]
    pub fn headline(&self) -> String {
        format!("Controlled by {} ({})", self.device_name, self.login_name)
    }

    /// The one-action reclaim hint drawn under the headline.
    #[must_use]
    pub fn hint() -> &'static str {
        "Press Enter or click to take back control"
    }

    /// Whether `key` triggers the one-action reclaim (Enter). The chrome also maps
    /// a click on the banner to the same reclaim; every other key is suppressed
    /// while the window is frozen.
    #[must_use]
    pub fn reclaim_requested(key: ReclaimKey) -> bool {
        matches!(key, ReclaimKey::Enter)
    }
}

/// The framework-neutral key the displaced banner reacts to. The GPUI view lowers
/// a `KeyDownEvent` into this shape; only [`ReclaimKey::Enter`] reclaims, every
/// other key is [`ReclaimKey::Other`] and stays suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimKey {
    Enter,
    Other,
}

impl ReclaimKey {
    /// Lower a GPUI keystroke name onto the framework-neutral reclaim key.
    ///
    /// Only `enter` reclaims. Everything else is [`ReclaimKey::Other`] and stays
    /// suppressed, which is the whole point of the displaced state: the window
    /// is frozen and the banner is its one affordance.
    #[must_use]
    pub fn from_keystroke(key: &str) -> Self {
        if key.eq_ignore_ascii_case("enter") { Self::Enter } else { Self::Other }
    }
}

/// Colours the displaced banner paints with, derived from the active theme so
/// the frozen window stays recognisably the same window.
#[derive(Clone, Copy)]
pub struct LostControlColors {
    /// Full-window dimming backdrop over the frozen grid.
    pub backdrop: Rgba,
    /// Banner background.
    pub panel_bg: Rgba,
    /// 1px banner border.
    pub border: Rgba,
    /// Headline text naming the new controller.
    pub title_fg: Rgba,
    /// The reclaim hint under the headline.
    pub body_fg: Rgba,
}

impl From<&ChromeColors> for LostControlColors {
    fn from(chrome: &ChromeColors) -> Self {
        Self {
            // Heavier than the share modal's 0.55: this backdrop is not a
            // transient prompt over a live window, it is the visual statement
            // that the grid underneath has stopped advancing.
            backdrop: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.72 },
            panel_bg: Rgba { a: 0.98, ..srgba(chrome.tab_bar_active_bg) },
            border: Rgba { a: 0.30, ..srgba(chrome.accent) },
            title_fg: srgba(chrome.tab_text_active),
            body_fg: srgba(chrome.tab_text),
        }
    }
}

/// Lower a displaced [`LostControlState`] onto the window's topmost overlay
/// layer: a dimming backdrop over the frozen grid under a centred banner naming
/// the new controller and offering the one-action reclaim.
///
/// The element is inert on its own — the shell attaches the click listener and
/// the key path routes Enter — so this stays a pure function of the state and
/// the theme.
#[must_use]
pub fn lost_control_overlay(state: &LostControlState, colors: &LostControlColors) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(colors.backdrop)
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .items_center()
                .px_6()
                .py_4()
                .min_w(px(320.0))
                .bg(colors.panel_bg)
                .border_1()
                .border_color(colors.border)
                .rounded(px(4.0))
                .child(div().text_sm().text_color(colors.title_fg).child(state.headline()))
                .child(div().text_xs().text_color(colors.body_fg).child(LostControlState::hint())),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests;
