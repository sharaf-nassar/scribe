use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use scribe_common::config::UpdateConfig;
use scribe_common::error::ScribeError;
use scribe_common::socket::server_socket_path;

mod attach_flow;
mod config;
mod handoff;
mod ipc_server;
mod macos_proc;
mod session_manager;
mod shell_integration;
mod updater;
mod workspace_manager;

#[cfg(test)]
mod handoff_tests;

/// Entry point. Calls `setup_env()` before spawning the tokio runtime so that
/// `env::set_var("TERM", …)` runs while the process is still single-threaded.
/// `env::set_var` is unsound in multi-threaded contexts (Rust 1.81+).
fn main() -> Result<(), ScribeError> {
    // Set TERM/COLORTERM before any threads are spawned.
    alacritty_terminal::tty::setup_env();

    let filter = EnvFilter::try_from_default_env().map_or(EnvFilter::new("info"), |filter| filter);

    fmt().with_env_filter(filter).init();

    let upgrade_mode = std::env::args().nth(1).is_some_and(|a| a == "--upgrade");

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ScribeError::Io { source: e })?
        .block_on(async {
            if upgrade_mode { run_upgrade_receiver().await } else { run_normal_server().await }
        })
}

/// Normal server mode: start IPC server + handoff listener, run until shutdown.
async fn run_normal_server() -> Result<(), ScribeError> {
    info!("scribe-server starting (normal mode)");

    let cfg = config::load_config()?;

    let session_manager = {
        let sm = session_manager::SessionManager::with_scrollback(
            usize::try_from(cfg.scrollback_lines).unwrap_or(usize::MAX),
        );
        sm.set_shell_integration_enabled(cfg.shell_integration_enabled);
        Arc::new(sm)
    };
    let workspace_manager =
        Arc::new(RwLock::new(workspace_manager::WorkspaceManager::new(cfg.workspace_roots)));
    run_server_loop(session_manager, workspace_manager, false, cfg.update).await
}

/// Upgrade receiver mode: connect to old server, receive handoff, then serve.
///
/// The `--upgrade` process takes over from the old server: it receives the
/// PTY fds and session state, then starts serving on the IPC socket. The
/// old server exits after handoff. The `postinst` script runs this in the
/// background so it doesn't block the package install.
async fn run_upgrade_receiver() -> Result<(), ScribeError> {
    info!("scribe-server starting (upgrade mode)");

    let cfg = config::load_config()?;

    // Receive handoff from the old server (blocking until complete).
    let (state, fds) = handoff::receive_handoff()?;

    info!(
        sessions = state.sessions.len(),
        workspaces = state.workspaces.len(),
        fds = fds.len(),
        "handoff received — reconstructing sessions"
    );

    // Reconstruct managers from handoff state.
    let scrollback = usize::try_from(cfg.scrollback_lines).unwrap_or(usize::MAX);

    let session_manager =
        Arc::new(session_manager::SessionManager::restore_from_handoff(&state, fds, scrollback)?);
    let live_session_ids: HashSet<_> =
        state.sessions.iter().map(|handoff_session| handoff_session.session_id).collect();
    let workspace_manager =
        Arc::new(RwLock::new(workspace_manager::WorkspaceManager::restore_from_handoff(
            cfg.workspace_roots,
            &state.workspaces,
            state.workspace_tree,
            &state.windows,
            &live_session_ids,
        )));

    info!("session restoration complete — starting IPC server");

    run_server_loop(session_manager, workspace_manager, true, cfg.update).await
}

/// Run the IPC server, handoff listener, and signal handler concurrently.
///
/// Shared between normal and upgrade startup paths. Cleans up the IPC socket
/// on exit. `upgrade_mode` is forwarded to the socket acquisition logic so
/// that upgrade receivers skip the singleton lock (the old server holds it).
async fn run_server_loop(
    session_manager: Arc<session_manager::SessionManager>,
    workspace_manager: Arc<RwLock<workspace_manager::WorkspaceManager>>,
    upgrade_mode: bool,
    update_config: UpdateConfig,
) -> Result<(), ScribeError> {
    let path = server_socket_path();
    let live_sessions = ipc_server::new_live_session_registry();
    let connected_clients = ipc_server::new_connected_clients();

    // Acquire the server socket with singleton enforcement. The lock guard
    // must live until the server shuts down to hold the advisory flock.
    let (_lock_guard, listener) = ipc_server::acquire_server_socket(&path, upgrade_mode)?;

    // Emit the bind-ready signal as soon as the socket is acquired. The
    // Debian postinst watchdog greps the upgrade log for this exact string
    // and counts it as "new server is reachable". Logging it here, before
    // session activation, guarantees the watchdog never times out on the
    // per-session restore work that follows. Queued client connections sit
    // in the kernel backlog until `start_ipc_server` begins accepting.
    info!("IPC server listening");

    // Activate sessions restored from a hot-reload handoff. Moves them from
    // SessionManager into the live registry and starts their PTY reader tasks
    // in detached mode. No-op for normal (non-upgrade) startup.
    ipc_server::activate_pending_sessions(&session_manager, &workspace_manager, &live_sessions)
        .await;

    // Spawn the background updater. The handle is passed into the IPC server
    // so that TriggerUpdate / DismissUpdate messages can reach it.
    let updater_handle =
        Arc::new(updater::spawn_updater(Arc::clone(&connected_clients), update_config));

    let handoff_triggered = tokio::select! {
        result = ipc_server::start_ipc_server(
            listener,
            ipc_server::IpcServerState {
                session_manager: Arc::clone(&session_manager),
                workspace_manager: Arc::clone(&workspace_manager),
                live_sessions: Arc::clone(&live_sessions),
                connected_clients: Arc::clone(&connected_clients),
                updater_handle: Arc::clone(&updater_handle),
            },
        ) => {
            result?;
            false
        }
        result = handoff::run_handoff_listener(
            Arc::clone(&workspace_manager),
            Arc::clone(&live_sessions),
        ) => {
            match result {
                Ok(()) => {
                    info!("handoff complete — shutting down old server");
                }
                Err(e) => {
                    warn!("handoff listener error: {e}");
                }
            }
            true
        }
        result = tokio::signal::ctrl_c() => {
            result.map_err(|e| ScribeError::Io { source: e })?;
            info!("received shutdown signal");
            false
        }
    };

    if handoff_triggered {
        // Defuse Pty objects so the old server's exit doesn't send SIGHUP to
        // child processes. alacritty_terminal::Pty::drop() explicitly calls
        // kill(child_pid, SIGHUP) — the new server already has the master fds.
        ipc_server::defuse_for_handoff(&live_sessions).await;
    } else {
        // Only clean up the IPC socket if we're NOT handing off. During a
        // handoff the new server has already bound to the same socket path —
        // removing it would make the new server unreachable.
        cleanup_socket(&path);
    }

    info!("scribe-server stopped");
    Ok(())
}

/// Remove the IPC socket file, ignoring "not found" errors.
fn cleanup_socket(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(?path, "failed to remove socket on shutdown: {e}");
        }
    }
}
