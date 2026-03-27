//! Standalone driver window process.
//!
//! Enforces singleton behavior via a Unix domain socket. Manages the `SQLite`
//! task database and the webview UI for launching/monitoring driver tasks.

use std::sync::Arc;

fn main() {
    init_tracing();

    // Attempt to become the singleton driver process.
    #[allow(
        clippy::used_underscore_binding,
        reason = "lock_file is kept alive for its drop guard; underscore prefix signals intent"
    )]
    let (listener, socket_path, _lock_file) = match scribe_driver::singleton::acquire() {
        Ok(scribe_driver::singleton::SingletonResult::Primary {
            listener,
            socket_path,
            _lock_file,
        }) => (listener, socket_path, _lock_file),
        Ok(scribe_driver::singleton::SingletonResult::AlreadyRunning) => {
            tracing::info!("another driver instance is running, sent focus and exiting");
            return;
        }
        Err(e) => {
            tracing::error!("failed to acquire singleton: {e}");
            return;
        }
    };

    // Open SQLite database.
    let repo: Arc<dyn scribe_driver::repository::TaskRepository> =
        match scribe_driver::repository::sqlite::SqliteTaskRepository::open() {
            Ok(r) => Arc::new(r),
            Err(e) => {
                tracing::error!("failed to open driver database: {e}");
                return;
            }
        };

    // Start the server IPC client.  The handler will receive server-side
    // events (task state changes, output) via the callback.
    let repo_for_events = Arc::clone(&repo);
    let cmd_tx = scribe_driver::server_client::start_server_client(move |event| {
        handle_server_event(&repo_for_events, event);
    });

    // Build the IPC handler that wires webview → SQLite + server.
    let handler = Arc::new(scribe_driver::handler::DriverHandler::new(Arc::clone(&repo), cmd_tx));

    // Load saved state (geometry + open flag).
    let saved = scribe_driver::state::load();

    // Mark as open for restart restoration.
    scribe_driver::state::save(&scribe_driver::state::DriverState {
        open: true,
        geometry: saved.geometry,
    });

    // Build initial state JSON by querying the database.
    let initial_state_json = build_initial_state_json(&repo);

    let socket_path_cleanup = socket_path.clone();
    let saved_geometry = saved.geometry;

    let on_close = move |geometry: scribe_driver::DriverWindowGeometry| {
        scribe_driver::state::save(&scribe_driver::state::DriverState {
            open: false,
            geometry: Some(geometry),
        });
        scribe_driver::singleton::cleanup_socket(&socket_path_cleanup);
    };

    let on_ipc = move |body: String| -> String {
        let resp = handler.handle_raw(&body);
        if !resp.is_empty() {
            tracing::debug!("driver IPC response: {resp}");
        }
        resp
    };

    if let Err(e) = scribe_driver::run_driver_window(
        saved_geometry,
        &initial_state_json,
        on_ipc,
        on_close,
        listener,
        socket_path,
    ) {
        tracing::error!("driver window failed: {e}");
        scribe_driver::singleton::cleanup_socket(&scribe_common::socket::driver_socket_path());
        scribe_driver::state::save(&scribe_driver::state::DriverState {
            open: false,
            geometry: saved_geometry,
        });
    }
}

/// Handle a server event by updating the `SQLite` database.
fn handle_server_event(
    repo: &Arc<dyn scribe_driver::repository::TaskRepository>,
    event: scribe_driver::server_client::DriverServerEvent,
) {
    use scribe_driver::server_client::DriverServerEvent;

    match event {
        DriverServerEvent::TaskCreated { task_id, project_path: _ } => {
            let id = task_id.to_string();
            if let Err(e) = repo.update_task_state(&id, "Starting") {
                tracing::warn!(%task_id, "failed to update task state on TaskCreated: {e}");
            }
        }
        DriverServerEvent::TaskStateChanged { task_id, state, ai_state: _ } => {
            let id = task_id.to_string();
            let state_str = format!("{state:?}");
            if let Err(e) = repo.update_task_state(&id, &state_str) {
                tracing::warn!(%task_id, "failed to update task state: {e}");
            }
        }
        DriverServerEvent::TaskExited { task_id, exit_code } => {
            let id = task_id.to_string();
            if let Err(e) = repo.complete_task(&id, exit_code) {
                tracing::warn!(%task_id, "failed to complete task: {e}");
            }
        }
        DriverServerEvent::TaskOutput { task_id, data } => {
            let id = task_id.to_string();
            let chunk = String::from_utf8_lossy(&data);
            if let Err(e) = repo.append_output(&id, &chunk) {
                tracing::warn!(%task_id, "failed to append task output: {e}");
            }
        }
        DriverServerEvent::TaskList { .. } | DriverServerEvent::ConnectionLost => {}
    }
}

/// Serialize the current task list, stats, and projects from `SQLite` for webview injection.
fn build_initial_state_json(repo: &Arc<dyn scribe_driver::repository::TaskRepository>) -> String {
    let tasks = repo.list_tasks().unwrap_or_else(|e| {
        tracing::warn!("failed to list tasks for initial state: {e}");
        Vec::new()
    });
    let stats = repo.get_stats().unwrap_or_else(|e| {
        tracing::warn!("failed to get stats for initial state: {e}");
        scribe_driver::repository::DriverStats {
            running: 0,
            completed: 0,
            failed: 0,
            total_tokens: 0,
        }
    });
    let projects = repo.list_projects().unwrap_or_else(|e| {
        tracing::warn!("failed to list projects for initial state: {e}");
        Vec::new()
    });

    serde_json::json!({
        "tasks": tasks,
        "stats": {
            "running": stats.running,
            "completed": stats.completed,
            "failed": stats.failed,
            "total_tokens": stats.total_tokens,
        },
        "projects": projects,
    })
    .to_string()
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    #[allow(clippy::unwrap_used, reason = "EnvFilter::new with static string cannot fail")]
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
