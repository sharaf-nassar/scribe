//! macOS notification dispatcher backed by `notify-rust`.
//!
//! `NSUserNotification` (the backend `notify-rust` uses) does not
//! support click callbacks or programmatic dismiss, so this dispatcher
//! is mostly a serializer in front of `Notification::show`. The work
//! still runs on a dedicated thread instead of a `std::thread::spawn`
//! per fire so the public API matches the Linux dispatcher and the
//! consumer in `App` is cfg-free.
//!
//! Click-to-focus is handled by `App::handle_focused` via
//! `NotificationTracker::take_pending_focus` — a focus-on-activate
//! fallback that records the most recently fired session and consumes
//! it the next time macOS activates the window.

use scribe_common::app::current_identity;
use tokio::sync::mpsc;
use winit::event_loop::EventLoopProxy;

use super::{NotifReq, ShowReq};
use crate::ipc_client::UiEvent;

pub(super) fn spawn(_proxy: EventLoopProxy<UiEvent>) -> mpsc::UnboundedSender<NotifReq> {
    let (tx, rx) = mpsc::unbounded_channel();
    let spawn_result = std::thread::Builder::new()
        .name("scribe-notif-dispatcher".to_string())
        .spawn(move || run_thread(rx));
    if let Err(e) = spawn_result {
        tracing::warn!(error = %e, "failed to spawn notification dispatcher thread");
    }
    tx
}

fn run_thread(rx: mpsc::UnboundedReceiver<NotifReq>) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(error = %e, "could not build notification runtime");
            return;
        }
    };
    rt.block_on(run(rx));
}

async fn run(mut rx: mpsc::UnboundedReceiver<NotifReq>) {
    while let Some(req) = rx.recv().await {
        match req {
            NotifReq::Show(req) => fire(&req),
            // No-op: NSUserNotification has no client-driven dismiss
            // through `notify-rust`. The system retires toasts on its
            // own timeline.
            NotifReq::Close { .. } => {}
            NotifReq::Shutdown => break,
        }
    }
}

fn fire(req: &ShowReq) {
    let identity = current_identity();
    let mut notif = notify_rust::Notification::new();
    notif.summary(&req.summary).body(&req.body).appname(identity.window_title_name());
    if let Err(e) = notif.show() {
        tracing::debug!(error = %e, "macOS notification failed");
    }
}
