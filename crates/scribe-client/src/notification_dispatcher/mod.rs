//! Cross-platform desktop notification dispatcher.
//!
//! Exposes a single API — `spawn_dispatcher` and `NotifReq` — to the
//! rest of the client. Platform divergence is hidden inside this
//! module: Linux uses raw `zbus` so every notification shares one
//! D-Bus connection and `replaces_id` keeps one toast per session;
//! macOS uses `notify-rust` for fire-and-forget `NSUserNotification`
//! calls (the OS does not support programmatic dismiss or click
//! callbacks, so click-to-focus is handled by the focus-on-activate
//! fallback in `App::handle_focused`).
//!
//! `App` always holds an `Option<UnboundedSender<NotifReq>>` and
//! always sends through it — there are no `#[cfg(target_os = …)]`
//! gates at the call sites, mirroring the `winit::platform_impl` /
//! `wgpu::hal` style of OS-protocol abstraction.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use scribe_common::config::NotifyTimeoutMode;
use scribe_common::ids::SessionId;
use tokio::sync::mpsc;
use winit::event_loop::EventLoopProxy;

use crate::ipc_client::UiEvent;

/// Request to the dispatcher thread. Sent by the main thread on
/// notification fire, session exit, and shutdown.
pub enum NotifReq {
    /// Show or replace the notification associated with `session_id`.
    Show(ShowReq),
    /// Close the notification for `session_id`, if any. Sent on
    /// session exit / `AiStateCleared` so stale toasts do not linger.
    /// Best-effort on macOS — `notify-rust` cannot programmatically
    /// dismiss `NSUserNotification`, so the toast remains until the
    /// system retires it.
    Close { session_id: SessionId },
    /// Close every live notification and exit the dispatcher loop.
    /// Like `Close`, the close-all step is a no-op on macOS.
    Shutdown,
}

/// Payload for [`NotifReq::Show`]. Bundled into a struct so the
/// dispatcher's show path stays under clippy's argument limit and so
/// new fields (e.g. icon overrides) can land without churning every
/// call site.
pub struct ShowReq {
    pub session_id: SessionId,
    pub summary: String,
    pub body: String,
    /// Linux-only — the freedesktop spec exposes `expire_timeout`.
    /// macOS ignores this field because `notify-rust` cannot set
    /// `NSUserNotification` lifetime.
    pub timeout_mode: NotifyTimeoutMode,
    /// Linux-only — paired with [`NotifyTimeoutMode::Custom`].
    pub timeout_secs: u32,
}

/// Spawn the platform-appropriate dispatcher on a dedicated thread
/// and return an unbounded sender. Dropping the sender shuts the
/// dispatcher down naturally; sending [`NotifReq::Shutdown`] also
/// closes every live notification first.
///
/// Falls back to a sink that drops every request on platforms with
/// neither a Linux nor a macOS implementation, so the rest of the
/// client compiles unchanged.
pub fn spawn_dispatcher(proxy: EventLoopProxy<UiEvent>) -> mpsc::UnboundedSender<NotifReq> {
    #[cfg(target_os = "linux")]
    {
        linux::spawn(proxy)
    }
    #[cfg(target_os = "macos")]
    {
        macos::spawn(proxy)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = proxy;
        let (tx, _rx) = mpsc::unbounded_channel();
        tx
    }
}
