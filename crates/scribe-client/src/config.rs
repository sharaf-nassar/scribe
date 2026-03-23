//! Config file watcher for live-reloading `~/.config/scribe/config.toml`.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use winit::event_loop::EventLoopProxy;

use crate::ipc_client::UiEvent;

/// Start a file watcher on the scribe config directory.
///
/// Watches `~/.config/scribe/` (not just the file) because editors often
/// delete + recreate files on save.  Sends [`UiEvent::ConfigChanged`] to the
/// event loop when a modify or create event is detected.
///
/// Returns the watcher handle.  **The caller must store this** -- dropping it
/// stops the watcher.
pub fn start_config_watcher(proxy: EventLoopProxy<UiEvent>) -> Option<RecommendedWatcher> {
    let config_dir = dirs::config_dir()?;
    let config_path = config_dir.join("scribe");

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        let dominated = res.ok().is_some_and(|e| e.kind.is_modify() || e.kind.is_create());
        if dominated && proxy.send_event(UiEvent::ConfigChanged).is_err() {
            tracing::debug!("event loop closed; config watcher event dropped");
        }
    })
    .ok()?;

    watcher.watch(&config_path, RecursiveMode::NonRecursive).ok()?;

    tracing::info!(?config_path, "config file watcher started");
    Some(watcher)
}
