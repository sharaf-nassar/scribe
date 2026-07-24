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
//! in favour of the GPUI banner element.

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

#[cfg(test)]
mod tests;
