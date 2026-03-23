use std::path::Path;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use scribe_common::error::ScribeError;
use scribe_common::socket::server_socket_path;

mod config;
mod handoff;
mod ipc_server;
mod session_manager;
mod workspace_manager;

/// Entry point. Calls `setup_env()` before spawning the tokio runtime so that
/// `env::set_var("TERM", …)` runs while the process is still single-threaded.
/// `env::set_var` is unsound in multi-threaded contexts (Rust 1.81+).
fn main() -> Result<(), ScribeError> {
    // Set TERM/COLORTERM before any threads are spawned.
    alacritty_terminal::tty::setup_env();

    #[allow(clippy::unwrap_used, reason = "EnvFilter::new with static string cannot fail")]
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

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

    #[allow(
        clippy::cast_possible_truncation,
        reason = "scrollback_lines is clamped to 100_000 in config which fits usize"
    )]
    let session_manager =
        Arc::new(session_manager::SessionManager::with_scrollback(cfg.scrollback_lines as usize));
    let workspace_manager =
        Arc::new(RwLock::new(workspace_manager::WorkspaceManager::new(cfg.workspace_roots)));

    run_server_loop(session_manager, workspace_manager).await
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
    #[allow(
        clippy::cast_possible_truncation,
        reason = "scrollback_lines is clamped to 100_000 in config which fits usize"
    )]
    let scrollback = cfg.scrollback_lines as usize;

    let session_manager =
        Arc::new(session_manager::SessionManager::restore_from_handoff(&state, fds, scrollback)?);
    let workspace_manager =
        Arc::new(RwLock::new(workspace_manager::WorkspaceManager::restore_from_handoff(
            cfg.workspace_roots,
            &state.workspaces,
        )));

    info!("session restoration complete — starting IPC server");

    run_server_loop(session_manager, workspace_manager).await
}

/// Run the IPC server, handoff listener, and signal handler concurrently.
///
/// Shared between normal and upgrade startup paths. Cleans up the IPC socket
/// on exit.
async fn run_server_loop(
    session_manager: Arc<session_manager::SessionManager>,
    workspace_manager: Arc<RwLock<workspace_manager::WorkspaceManager>>,
) -> Result<(), ScribeError> {
    let path = server_socket_path();

    let handoff_triggered = tokio::select! {
        result = ipc_server::start_ipc_server(&path, Arc::clone(&session_manager), Arc::clone(&workspace_manager)) => {
            result?;
            false
        }
        result = handoff::run_handoff_listener(Arc::clone(&session_manager), Arc::clone(&workspace_manager)) => {
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

    // Only clean up the IPC socket if we're NOT handing off. During a handoff
    // the new server has already bound to the same socket path — removing it
    // would make the new server unreachable for new client connections.
    if !handoff_triggered {
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
