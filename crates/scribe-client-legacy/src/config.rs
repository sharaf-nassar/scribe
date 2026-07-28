//! Config file watcher for live-reloading the active Scribe config directory.

use std::path::Path;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use scribe_common::app::current_config_dir;
use winit::event_loop::EventLoopProxy;

use crate::ipc_client::UiEvent;

/// Return whether a notify path should trigger a config reload.
///
/// We normally care only about `config.toml` or files inside `themes/`.
/// On macOS, `notify` uses `FSEvents`, which can report only the watched
/// directory and expects clients to rescan it, so the root config dir itself
/// must also count as relevant there.
fn is_relevant_config_event_path(config_dir: &Path, path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "config.toml")
        || path.components().any(|component| component.as_os_str() == "themes")
        || (cfg!(target_os = "macos") && path == config_dir)
}

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
    let watched_config_dir = config_path.clone();

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
        let Some(event) = res.ok() else { return };
        if !event.kind.is_modify() && !event.kind.is_create() {
            return;
        }
        let relevant =
            event.paths.iter().any(|path| is_relevant_config_event_path(&watched_config_dir, path));
        if relevant && proxy.send_event(UiEvent::ConfigChanged).is_err() {
            tracing::debug!("event loop closed; config watcher event dropped");
        }
    })
    .ok()?;

    watcher.watch(&config_path, RecursiveMode::NonRecursive).ok()?;

    tracing::info!(?config_path, "config file watcher started");
    Some(watcher)
}
