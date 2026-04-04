//! Config file watcher for live-reloading `~/.config/scribe/config.toml`.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use scribe_common::app::current_config_dir;
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
    let config_path = current_config_dir()?;

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        let Some(event) = res.ok() else { return };
        if !event.kind.is_modify() && !event.kind.is_create() {
            return;
        }
        // Only fire for `config.toml` itself or files inside `themes/` —
        // avoids spurious reloads from editor swap/backup files.
        let relevant = event.paths.iter().any(|p| {
            p.file_name().is_some_and(|n| n == "config.toml")
                || p.components().any(|c| c.as_os_str() == "themes")
        });
        if relevant && proxy.send_event(UiEvent::ConfigChanged).is_err() {
            tracing::debug!("event loop closed; config watcher event dropped");
        }
    })
    .ok()?;

    watcher.watch(&config_path, RecursiveMode::NonRecursive).ok()?;

    tracing::info!(?config_path, "config file watcher started");
    Some(watcher)
}
