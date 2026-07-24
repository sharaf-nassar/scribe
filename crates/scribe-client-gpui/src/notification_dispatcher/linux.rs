//! Linux notification dispatcher backed by raw `zbus`.
//!
//! One dedicated thread owns one session-bus connection, one
//! `org.freedesktop.Notifications` proxy, and the two signal streams
//! (`ActionInvoked`, `NotificationClosed`) for every notification this client
//! ever fires. Live notification state lives in [`NotifState`], keyed both by
//! `SessionId` and the daemon-assigned notification id.
//!
//! Repeated state changes for the same session reuse `replaces_id` so the
//! daemon atomically swaps the existing toast in place rather than stacking a
//! new one — the freedesktop spec guarantees the returned id equals
//! `replaces_id` when the prior notification still exists. Click → focus emits
//! [`NotifOutput::FocusSession`] on the caller-supplied channel.
//!
//! Replaces the old "spawn one `std::thread` per notification, each blocking
//! forever on `wait_for_action`" path that leaked threads and D-Bus
//! connections under `condition = "always"` + `timeout_mode = "never"`.

use std::collections::HashMap;

use futures_util::stream::StreamExt;
use scribe_common::ids::SessionId;
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::zvariant::Value;

use super::{NotifOutput, NotifReq, NotifState, ShowReq, expire_timeout_millis};

pub(super) fn spawn(out: mpsc::UnboundedSender<NotifOutput>) -> mpsc::UnboundedSender<NotifReq> {
    let (tx, rx) = mpsc::unbounded_channel();
    let spawn_result = std::thread::Builder::new()
        .name("scribe-notif-dispatcher".to_string())
        .spawn(move || run_thread(rx, out));
    if let Err(e) = spawn_result {
        tracing::warn!(error = %e, "failed to spawn notification dispatcher thread");
    }
    tx
}

fn run_thread(rx: mpsc::UnboundedReceiver<NotifReq>, out: mpsc::UnboundedSender<NotifOutput>) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(error = %e, "could not build notification runtime");
            return;
        }
    };
    rt.block_on(run(rx, out));
}

/// Connection-bound state. Two signal streams plus the output channel come out
/// of `setup_dbus`; the rest is owned by the dispatcher loop.
struct Dispatcher<'a> {
    dbus: NotificationsProxy<'a>,
    out: mpsc::UnboundedSender<NotifOutput>,
    app_name: &'static str,
    icon: &'static str,
    state: NotifState,
}

async fn run(mut rx: mpsc::UnboundedReceiver<NotifReq>, out: mpsc::UnboundedSender<NotifOutput>) {
    let Some((conn, mut invoked, mut closed)) = setup_dbus(&mut rx).await else {
        return;
    };
    let dbus = match NotificationsProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "could not rebind NotificationsProxy after setup");
            drain(&mut rx).await;
            return;
        }
    };

    let identity = scribe_common::app::current_identity();
    let mut dispatcher = Dispatcher {
        dbus,
        out,
        app_name: identity.window_title_name(),
        icon: identity.slug(),
        state: NotifState::new(),
    };

    loop {
        tokio::select! {
            biased;
            req = rx.recv() => {
                let Some(req) = req else { break };
                if !dispatcher.handle_request(req).await { break; }
            }
            sig = invoked.next() => {
                let Some(sig) = sig else { break };
                dispatcher.on_action_invoked(&sig);
            }
            sig = closed.next() => {
                let Some(sig) = sig else { break };
                dispatcher.on_notification_closed(&sig);
            }
        }
    }
}

/// Open the session bus and subscribe to the two notification signals. On any
/// failure, drain the request channel so senders don't block on a closed
/// receiver, and return `None`.
async fn setup_dbus(
    rx: &mut mpsc::UnboundedReceiver<NotifReq>,
) -> Option<(Connection, ActionInvokedStream, NotificationClosedStream)> {
    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "no D-Bus session bus; notifications disabled");
            drain(rx).await;
            return None;
        }
    };
    let dbus = match NotificationsProxy::new(&conn).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "no org.freedesktop.Notifications service");
            drain(rx).await;
            return None;
        }
    };
    let invoked = match dbus.receive_action_invoked().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not subscribe ActionInvoked");
            drain(rx).await;
            return None;
        }
    };
    let closed = match dbus.receive_notification_closed().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not subscribe NotificationClosed");
            drain(rx).await;
            return None;
        }
    };
    Some((conn, invoked, closed))
}

impl Dispatcher<'_> {
    async fn handle_request(&mut self, req: NotifReq) -> bool {
        match req {
            NotifReq::Show(req) => {
                self.show(req).await;
                true
            }
            NotifReq::Close { session_id } => {
                self.close_for_session(session_id).await;
                true
            }
            NotifReq::Shutdown => {
                self.shutdown().await;
                false
            }
        }
    }

    async fn show(&mut self, req: ShowReq) {
        let replaces = self.state.replaces_for(req.session_id);
        let expire_timeout = expire_timeout_millis(req.timeout_mode, req.timeout_secs);
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", self.icon.into());
        let actions = ["default", "Focus"];
        let result = self
            .dbus
            .notify(
                self.app_name,
                replaces,
                self.icon,
                &req.summary,
                &req.body,
                &actions,
                hints,
                expire_timeout,
            )
            .await;
        match result {
            Ok(id) if id != 0 => self.state.record_shown(req.session_id, replaces, id),
            Ok(_) => tracing::debug!("notify returned id 0"),
            Err(e) => tracing::debug!(error = %e, "notify call failed"),
        }
    }

    async fn close_for_session(&mut self, session_id: SessionId) {
        let Some(id) = self.state.take_session(session_id) else { return };
        self.close_id_logging_errors(id).await;
    }

    async fn shutdown(&mut self) {
        for id in self.state.live_ids() {
            self.close_id_logging_errors(id).await;
        }
        self.state.clear();
    }

    async fn close_id_logging_errors(&self, id: u32) {
        if let Err(e) = self.dbus.close_notification(id).await {
            tracing::debug!(error = %e, "close_notification failed");
        }
    }

    fn on_action_invoked(&self, sig: &ActionInvoked) {
        let Ok(args) = sig.args() else { return };
        let Some(session_id) = self.state.session_for_id(args.id) else { return };
        if self.out.send(NotifOutput::FocusSession { session_id }).is_err() {
            tracing::debug!("output channel closed; dropping FocusSession");
        }
    }

    fn on_notification_closed(&mut self, sig: &NotificationClosed) {
        let Ok(args) = sig.args() else { return };
        self.state.on_closed(args.id);
    }
}

async fn drain(rx: &mut mpsc::UnboundedReceiver<NotifReq>) {
    while rx.recv().await.is_some() {}
}

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications",
    gen_blocking = false
)]
trait Notifications {
    #[allow(clippy::too_many_arguments, reason = "matches the freedesktop spec signature")]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}
