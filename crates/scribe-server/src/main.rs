use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use scribe_common::config::UpdateConfig;
use scribe_common::error::ScribeError;
use scribe_common::socket::server_socket_path;

mod attach_flow;
mod child_identity;
mod clipboard_state;
mod config;
mod env_store;
mod handoff;
mod hook_ingress;
mod ipc_server;
mod macos_proc;
mod pty_guard;
mod releases;
mod search_cache;
mod session_manager;
mod shell_integration;
mod stop_classifier;
// Feature 013: the remote accept path (compiled into this binary via `mod
// ipc_server`) reaches the Tailscale LocalAPI client through `crate::tailnet`.
// Re-export the LIBRARY crate's module instead of declaring a second, private
// in-binary `mod tailnet`: that private copy compiles the module a second time,
// where its `pub` peer-picker fields (unused by the binary's own call sites) trip
// dead-code analysis. The library holds one fully-public copy, so this re-export
// eliminates the double compile AND the dead-code warning — no lint suppression.
use scribe_server::tailnet;
// Feature 014: likewise, the per-transport LAN state in `mod ipc_server` reaches
// the device-trust `DeviceId` type through `crate::lan`. Re-export the LIBRARY
// crate's `lan` module for the identical reason — a second in-binary `mod lan`
// would recompile its many `pub` discovery/identity/TLS/trust items (unused by
// the binary) into dead-code warnings.
use scribe_server::lan;
// Spec 017 US1-3: the PTY reader and the close paths in `mod ipc_server` reach
// the per-session exit funnel through `crate::session_exit`. Re-exported from
// the LIBRARY crate for the same reason as `tailnet`/`lan` — a second in-binary
// copy would recompile its `pub` items into dead-code warnings.
use scribe_server::session_exit;
// The PTY reader owns the library's single terminal-image seam implementation;
// re-export it here so the binary's `ipc_server` module uses that production
// type instead of compiling a second copy.
use scribe_server::terminal_image_state;
// The reader's reply write-back and capable-sink fan-out live beside that seam;
// re-export the library's copy for the same single-compile reason.
use scribe_server::terminal_image_sharing;
// Spec 017 US1-2: `mod session_manager` opens each child's pidfd and
// `mod ipc_server` arms the watcher over it, both through
// `crate::child_watch`. Re-exported for the same reason — the non-Linux build
// never reaches the watcher itself, so an in-binary copy would report its
// `pub` items dead there.
use scribe_server::child_watch;
mod updater;
mod workspace_manager;

#[cfg(test)]
mod handoff_tests;

/// How long process exit waits on outstanding blocking work before it
/// abandons those threads and lets the process go.
///
/// Above [`pty_guard::TEARDOWN_KILL_GRACE`] so a session torn down moments
/// before the signal still finishes its escalated reap inside the bound; this
/// is the backstop for every other blocking call — keystore, `netdev`, the
/// env-store writes — that could otherwise park exit indefinitely.
const RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Entry point. Calls `setup_env()` before spawning the tokio runtime so that
/// `env::set_var("TERM", …)` runs while the process is still single-threaded.
/// `env::set_var` is unsound in multi-threaded contexts (Rust 1.81+).
fn main() -> Result<(), ScribeError> {
    // Set TERM/COLORTERM before any threads are spawned.
    alacritty_terminal::tty::setup_env();

    let filter = EnvFilter::try_from_default_env().map_or(EnvFilter::new("info"), |filter| filter);

    fmt().with_env_filter(filter).init();

    let upgrade_mode = std::env::args().nth(1).is_some_and(|a| a == "--upgrade");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ScribeError::Io { source: e })?;

    let result = runtime.block_on(async {
        if upgrade_mode {
            Box::pin(run_upgrade_receiver()).await
        } else {
            Box::pin(run_normal_server()).await
        }
    });

    // `Runtime`'s own `Drop` waits on the blocking pool with no bound, so a
    // single blocking call that never returns holds the process open long
    // after `main` is done. Shut down explicitly instead: threads still busy
    // at the deadline are left to the exit.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);

    result
}

/// Normal server mode: start IPC server + handoff listener, run until shutdown.
async fn run_normal_server() -> Result<(), ScribeError> {
    info!("scribe-server starting (normal mode)");

    let cfg = config::load_config()?;

    // Feature 013: surface the configured remote-control state at startup. The
    // listener itself is started, stopped, and rebound live off this config by
    // the config-reload path (a later task); nothing is bound here.
    info!(
        remote_enabled = cfg.remote.enabled,
        remote_port = cfg.remote.port,
        "remote window control configuration"
    );

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
    // Read back from the manager rather than from the payload: the restore
    // admits at most `MAX_SESSIONS` sessions, and a workspace tree that still
    // named a refused one would advertise a session nothing can attach to.
    let live_session_ids: HashSet<_> = session_manager
        .pending_session_ids()
        .await
        .into_iter()
        .map(|(session_id, _workspace_id)| session_id)
        .collect();
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
    let window_shares = ipc_server::new_window_shares();

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
    ipc_server::activate_pending_sessions(
        &session_manager,
        &workspace_manager,
        &live_sessions,
        &window_shares,
    )
    .await;

    // Spawn the background updater. The handle is passed into the IPC server
    // so that TriggerUpdate / DismissUpdate messages can reach it.
    let updater_handle =
        Arc::new(updater::spawn_updater(Arc::clone(&window_shares), update_config));

    // Build the release catalog + GitHub fetcher used by the Releases settings
    // panel. The catalog is empty until the first `ListReleases` request; the
    // fetcher reuses the shared HTTP client from `updater::http_client()`.
    let release_catalog = Arc::new(tokio::sync::Mutex::new(releases::ReleaseCatalog::default()));
    let release_fetcher: Arc<dyn releases::ReleaseFetcher> =
        Arc::new(releases::GithubReleaseFetcher::new());
    // The env-store registry holds per-session env-capture state for the
    // life of the server. `Arc` so the per-session persist tasks spawned
    // by `schedule_persist` can hold a back-pointer alongside hook ingress.
    let env_store = Arc::new(env_store::EnvStoreState::default());

    // T035: seed the cached `terminal.env_persistence.enabled` value so
    // the very first `ConfigReloaded` can compare against the real
    // startup value (not the `false` default). Load failure fails safe
    // to `false` — the feature is disabled by default (FR-009).
    env_store.seed_last_enabled(load_env_persistence_seed());

    // Sweep env envelopes no window snapshot still names. Spawned rather than
    // awaited: it walks the state tree and talks to the OS keystore, and a slow
    // or wedged secret service must never delay the accept loop. Startup is the
    // only sound moment for it — mid-run, a window that has not yet flushed its
    // snapshot looks exactly like an orphan.
    tokio::spawn(env_store::gc::sweep_orphaned_envelopes(env_store::gc::ORPHAN_RETENTION));

    // T036: spawn the env-status forwarder before the IPC accept loop so
    // any pre-attach transitions (e.g. from sessions restored during
    // `activate_pending_sessions` above) are observed by the broadcast
    // receiver from the first tick. The forwarder owns its own
    // `Arc<EnvStoreState>` clone and exits when the channel closes.
    ipc_server::spawn_env_status_forwarder(&env_store, Arc::clone(&live_sessions));

    // Feature 013: shared remote-control listener handle, threaded into the IPC
    // server state so the `ConfigReloaded` path can start/stop/rebind it live.
    let remote_control = ipc_server::RemoteControl::new();
    let server_state = ipc_server::IpcServerState {
        session_manager: Arc::clone(&session_manager),
        workspace_manager: Arc::clone(&workspace_manager),
        live_sessions: Arc::clone(&live_sessions),
        window_shares: Arc::clone(&window_shares),
        updater_handle: Arc::clone(&updater_handle),
        release_catalog: Arc::clone(&release_catalog),
        release_fetcher: Arc::clone(&release_fetcher),
        env_store: Arc::clone(&env_store),
        remote_control: Arc::clone(&remote_control),
    };

    // Start the remote-control supervisor: it applies the current `[remote]`
    // config (a no-op when disabled — the default) and then rebinds/stops the
    // listener live on every `ConfigReloaded`. Spawned, not awaited, so a wedged
    // tailscaled cannot delay local serving; the server is never restarted.
    tokio::spawn(ipc_server::remote_supervisor(Arc::clone(&remote_control), server_state.clone()));

    let handoff_triggered = tokio::select! {
        result = ipc_server::start_ipc_server(listener, server_state) => {
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
        // The readers are deliberately left running: the new server owns these
        // children now, so nothing here may cancel a reader into the exit
        // funnel and report their sessions dead.
        ipc_server::defuse_for_handoff(&live_sessions).await;
    } else {
        // Stop the readers before the runtime unwinds, under the same bounded
        // join the close paths use, so shutdown is not the one exit path that
        // abandons a task parked on a PTY read (spec 017 US1-3).
        ipc_server::shutdown_pty_readers(&live_sessions).await;
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
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(?path, "failed to remove socket on shutdown: {e}");
    }
}

/// Load the startup value of `terminal.env_persistence.enabled` for the
/// env-store seed, falling back to `false` (disabled) on load failure so the
/// feature stays off by default per FR-009.
fn load_env_persistence_seed() -> bool {
    match scribe_common::config::load_config() {
        Ok(cfg) => cfg.terminal.env_persistence.enabled,
        Err(e) => {
            warn!(error = %e, "failed to load config for env_persistence seed; defaulting to false");
            false
        }
    }
}
