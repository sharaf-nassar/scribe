//! Standalone settings window process.
//!
//! Enforces singleton behavior via a Unix domain socket. Terminal clients
//! spawn this process or send it a focus command.

fn main() {
    init_tracing();

    // Attempt to become the singleton settings process.
    let (listener, socket_path, lock_file_guard) = match scribe_settings::singleton::acquire() {
        Ok(scribe_settings::singleton::SingletonResult::Primary {
            listener,
            socket_path,
            lock_file,
        }) => (listener, socket_path, lock_file),
        Ok(scribe_settings::singleton::SingletonResult::AlreadyRunning) => {
            tracing::info!("another settings instance is running, sent focus and exiting");
            return;
        }
        Err(e) => {
            tracing::error!("failed to acquire singleton: {e}");
            return;
        }
    };

    // Load saved state (geometry + open flag).
    let saved = scribe_settings::state::load();

    // Mark as open for restart restoration.
    scribe_settings::state::save(&scribe_settings::state::SettingsState {
        open: true,
        geometry: saved.geometry,
    });

    // Run the GTK settings window (blocks until closed).
    // Signal handlers are installed inside run_settings_window via
    // glib::unix_signal_add_local (signal-safe, runs in GTK main loop).
    let socket_path_cleanup = socket_path.clone();
    let saved_geometry = saved.geometry;

    let on_close = move |geometry: scribe_settings::SettingsWindowGeometry| {
        // Save geometry + mark closed.
        scribe_settings::state::save(&scribe_settings::state::SettingsState {
            open: false,
            geometry: Some(geometry),
        });
        // Clean up the socket file.
        scribe_settings::singleton::cleanup_socket(&socket_path_cleanup);
    };

    let on_change = |change_json: String| {
        tracing::debug!("settings change: {change_json}");
        if let Err(e) = scribe_settings::apply::apply_settings_change(&change_json) {
            tracing::warn!("failed to apply settings change: {e}");
        }
    };

    if let Err(e) = scribe_settings::run_settings_window(
        saved_geometry,
        on_change,
        on_close,
        listener,
        socket_path,
    ) {
        tracing::error!("settings window failed: {e}");
        // Still try to clean up.
        scribe_settings::singleton::cleanup_socket(&scribe_common::socket::settings_socket_path());
        scribe_settings::state::save(&scribe_settings::state::SettingsState {
            open: false,
            geometry: saved_geometry,
        });
    }

    drop(lock_file_guard);
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().map_or(EnvFilter::new("info"), |filter| filter);

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
