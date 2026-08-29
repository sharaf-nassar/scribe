use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use scribe_common::config::{AgentApiConfig, UpdateConfig};
use scribe_common::error::ScribeError;
use scribe_common::macos_launchd::{self, LaunchdSlot};
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
mod state_dump;
mod stop_classifier;
// Feature 013: the remote accept path (compiled into this binary via `mod
// ipc_server`) reaches the Tailscale LocalAPI client through `crate::tailnet`.
// Re-export the LIBRARY crate's module instead of declaring a second, private
// in-binary `mod tailnet`: that private copy compiles the module a second time,
// where its `pub` peer-picker fields (unused by the binary's own call sites) trip
// dead-code analysis. The library holds one fully-public copy, so this re-export
// eliminates the double compile AND the dead-code warning — no lint suppression.
use scribe_server::agent_api;
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
// CI ref watching is implemented once in the library; binary startup and CWD
// wiring reach that copy through `crate::git_ref_watcher`.
pub use scribe_server::git_ref_watcher;
// The PTY reader owns the library's single terminal-image seam implementation;
// re-export it here so the binary's `ipc_server` module uses that production
// type instead of compiling a second copy.
use scribe_server::terminal_image_state;
// The reader's reply write-back and capable-sink fan-out live beside that seam;
// re-export the library's copy for the same single-compile reason.
use scribe_server::terminal_image_sharing;
// Combined replay planning lives beside them and is reached from the reader's
// recovery path; re-export the library's copy for the same reason.
use scribe_server::terminal_image_replay;
// The binary's `mod handoff` names the library's image handoff wire type on
// `HandoffSession`; re-export it so both crates agree on one type.
use scribe_server::terminal_image_handoff;
// Spec 017 US1-2: `mod session_manager` opens each child's pidfd and
// `mod ipc_server` arms the watcher over it, both through
// `crate::child_watch`. Re-exported for the same reason — the non-Linux build
// never reaches the watcher itself, so an in-binary copy would report its
// `pub` items dead there.
use scribe_server::beads_board;
use scribe_server::child_watch;
// The transfer gate + ledger is one small all-`pub` leaf shared by `mod
// handoff`, `mod state_dump`, and `mod ipc_server`; re-export the library's
// copy for the same single-compile reason as `tailnet`/`lan`.
use scribe_server::workspace_transfer;
mod github_ci;
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

const SERVER_USAGE: &str = "Usage: scribe-server [--upgrade] [--launchd-slot=primary|alternate]";

#[derive(Debug, PartialEq, Eq)]
enum ServerAction {
    RunServer { upgrade: bool, launchd_slot: Option<LaunchdSlot> },
    Help,
    Version,
    Unknown(String),
}

/// Decide the startup mode before environment, logging, or runtime setup.
fn parse_args<'a>(arguments: impl IntoIterator<Item = &'a str>) -> ServerAction {
    let args = arguments.into_iter().collect::<Vec<_>>();
    if args.contains(&"--help") {
        return ServerAction::Help;
    }
    if args.contains(&"--version") {
        return ServerAction::Version;
    }

    let launchd_slot = LaunchdSlot::from_args(args.iter().copied());
    if let Some(argument) = args.iter().find(|argument| {
        **argument != "--upgrade" && LaunchdSlot::from_argument(argument).is_none()
    }) {
        return ServerAction::Unknown((*argument).to_owned());
    }

    ServerAction::RunServer { upgrade: args.contains(&"--upgrade"), launchd_slot }
}

fn write_stdout(message: &str) {
    let mut stdout = std::io::stdout().lock();
    drop(stdout.write_all(message.as_bytes()));
    drop(stdout.write_all(b"\n"));
}

fn write_stderr(message: &str) {
    let mut stderr = std::io::stderr().lock();
    drop(stderr.write_all(message.as_bytes()));
    drop(stderr.write_all(b"\n"));
}

/// Entry point. Calls `setup_env()` before spawning the tokio runtime so that
/// `env::set_var("TERM", …)` runs while the process is still single-threaded.
/// `env::set_var` is unsound in multi-threaded contexts (Rust 1.81+).
fn main() -> Result<(), ScribeError> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let (upgrade_mode, launchd_slot) = match parse_args(args.iter().map(String::as_str)) {
        ServerAction::RunServer { upgrade, launchd_slot } => (upgrade, launchd_slot),
        ServerAction::Help => {
            write_stdout(SERVER_USAGE);
            return Ok(());
        }
        ServerAction::Version => {
            write_stdout(&format!("scribe-server {}", env!("CARGO_PKG_VERSION")));
            return Ok(());
        }
        ServerAction::Unknown(argument) => {
            write_stderr(&format!("error: unrecognized argument '{argument}'\n\n{SERVER_USAGE}"));
            return Err(ScribeError::IpcError { reason: String::from("invalid command line") });
        }
    };

    // Set TERM/COLORTERM before any threads are spawned.
    alacritty_terminal::tty::setup_env();
    #[cfg(target_os = "macos")]
    let _launchd_slot_guard = launchd_slot
        .map(|slot| {
            macos_launchd::acquire_slot_guard(scribe_common::app::current_identity(), slot)
                .map_err(|reason| ScribeError::IpcError { reason })
        })
        .transpose()?;

    let filter = EnvFilter::try_from_default_env().map_or(EnvFilter::new("info"), |filter| filter);

    // An upgrade server has no durable stdio of its own: Debian postinst
    // redirects it to a state-dir `upgrade.log`, while the macOS LaunchAgents
    // send stdout and stderr to `/dev/null`.
    // Mirror tracing into a file under the state dir so the successor's logs
    // survive; stdout stays active because the postinst watchdog greps it for
    // "IPC server listening".
    let log_file = if upgrade_mode { open_server_log_file() } else { None };
    let (file, log_path) = match log_file {
        Some((file, path)) => (Some(file), Some(path)),
        None => (None, None),
    };
    init_tracing(filter, file);

    if upgrade_mode {
        if let Some(path) = &log_path {
            info!(path = %path.display(), "upgrade-mode logs mirrored to state-dir file");
        } else {
            warn!("could not open state-dir log file; upgrade-mode logs go to stdio only");
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ScribeError::Io { source: e })?;

    let result = runtime.block_on(async {
        if let Some(slot) = launchd_slot {
            Box::pin(run_launchd_managed(slot)).await
        } else if upgrade_mode {
            let mut handoff_committed = false;
            Box::pin(run_upgrade_receiver(None, &mut handoff_committed)).await
        } else {
            Box::pin(run_normal_server(None)).await
        }
    });

    // `Runtime`'s own `Drop` waits on the blocking pool with no bound, so a
    // single blocking call that never returns holds the process open long
    // after `main` is done. Shut down explicitly instead: threads still busy
    // at the deadline are left to the exit.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);

    result
}

/// Install the global tracing subscriber: a stdout fmt layer always, plus a
/// second fmt layer appending to `file` when one is provided.
fn init_tracing(filter: EnvFilter, file: Option<std::fs::File>) {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let registry = tracing_subscriber::registry().with(filter).with(fmt::layer());
    match file {
        Some(file) => registry
            .with(fmt::layer().with_ansi(false).with_writer(std::sync::Arc::new(file)))
            .init(),
        None => registry.init(),
    }
}

/// Open (creating and rotating as needed) the append-mode `server.log` in the
/// app state dir. Returns `None` when the state dir is unavailable or the
/// file cannot be opened — tracing then stays on stdio alone.
fn open_server_log_file() -> Option<(std::fs::File, std::path::PathBuf)> {
    let dir = scribe_common::app::current_state_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("server.log");
    scribe_common::app::rotate_log_if_oversized(&path, scribe_common::app::STATE_LOG_MAX_BYTES);
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    Some((file, path))
}

/// Normal server mode: start IPC server + handoff listener, run until shutdown.
async fn run_normal_server(launchd_slot: Option<LaunchdSlot>) -> Result<(), ScribeError> {
    info!("scribe-server starting (normal mode)");

    let cfg = config::load_config()?;

    // Spec 020: mirror `terminal.images.enabled` into the process-wide master
    // switch before anything can advertise a capability. Nothing is latched
    // yet, so a disabled start simply never claims one.
    terminal_image_sharing::set_images_master_enabled(cfg.images_enabled);
    github_ci::set_github_ci_enabled(cfg.github_ci.enabled);

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

    // Acquire the server socket with singleton enforcement. The lock guard must
    // live until the server shuts down to hold the advisory flock.
    let (lock_guard, listener) = ipc_server::acquire_server_socket(&server_socket_path(), false)?;

    // A cold start is the only start that can be recovering from a crash, so
    // load the previous process's state dump here — bounded, because a wedged
    // keystore must not hold the accept loop hostage while dialing clients
    // queue in the already-bound listener's backlog. A timeout just means the
    // replayed panes come up blank, exactly as they did before dumps existed.
    let recovered_sessions =
        tokio::time::timeout(RECOVERY_LOAD_TIMEOUT, state_dump::load_recovered_sessions())
            .await
            .unwrap_or_else(|_| {
                warn!("state dump load timed out; starting without recovered content");
                std::collections::HashMap::new()
            });

    // Boxed: the loop future carries the whole server setup and sits over the
    // pedantic `large_futures` budget.
    Box::pin(run_server_loop(
        session_manager,
        workspace_manager,
        (lock_guard, listener),
        ServerLoopConfig {
            update: cfg.update,
            agent_api: cfg.agent_api,
            launchd_slot,
            restored_ci_windows: Vec::new(),
            recovered_sessions,
            restored_transfer_ledger: Vec::new(),
        },
    ))
    .await
}

/// How long a cold start waits on the keystore-backed recovery load before
/// serving without it.
const RECOVERY_LOAD_TIMEOUT: Duration = Duration::from_secs(2);

/// Start a launchd slot as either a crash-recovery owner or a warm successor.
async fn run_launchd_managed(slot: LaunchdSlot) -> Result<(), ScribeError> {
    if std::os::unix::net::UnixStream::connect(server_socket_path()).is_err() {
        match run_normal_server(Some(slot)).await {
            Ok(()) => return Ok(()),
            Err(error) if std::os::unix::net::UnixStream::connect(server_socket_path()).is_ok() => {
                // Both registered slots may bootstrap together at login. The
                // lock loser observes the winner's socket and becomes its warm
                // successor instead of exiting non-zero into launchd throttle.
                warn!(%error, slot = slot.name(), "another launchd slot won normal startup; switching to handoff");
            }
            Err(error) => return Err(error),
        }
    }

    let mut handoff_committed = false;
    match run_upgrade_receiver(Some(slot), &mut handoff_committed).await {
        Ok(()) => Ok(()),
        Err(error)
            if !handoff_committed
                && std::os::unix::net::UnixStream::connect(server_socket_path()).is_ok() =>
        {
            // The predecessor is still serving, most commonly because the
            // handoff versions are incompatible. A successful exit keeps this
            // inactive slot from crash-looping under `KeepAlive` while the UI
            // asks for explicit cold-restart approval.
            warn!(%error, slot = slot.name(), "warm handoff refused; predecessor remains active");
            #[cfg(target_os = "macos")]
            if let Err(cleanup_error) = spawn_inactive_slot_cleanup(slot.other()) {
                warn!(%cleanup_error, slot = slot.name(), "failed to unregister refused launchd slot");
            }
            Ok(())
        }
        Err(error) => {
            warn!(
                %error,
                slot = slot.name(),
                handoff_committed,
                "managed successor cannot continue; taking normal ownership"
            );
            run_normal_server(Some(slot)).await
        }
    }
}

/// Upgrade receiver mode: connect to old server, receive handoff, then serve.
///
/// The `--upgrade` process takes over from the old server: it receives the
/// PTY fds and session state, then starts serving on the IPC socket. The
/// old server exits after handoff. The `postinst` script runs this in the
/// background so it doesn't block the package install.
async fn run_upgrade_receiver(
    launchd_slot: Option<LaunchdSlot>,
    handoff_committed: &mut bool,
) -> Result<(), ScribeError> {
    info!("scribe-server starting (upgrade mode)");

    let cfg = config::load_config()?;

    // Spec 020: the successor decides its own image policy from the file it
    // just read, before restoring any handed-off session state.
    terminal_image_sharing::set_images_master_enabled(cfg.images_enabled);
    github_ci::set_github_ci_enabled(cfg.github_ci.enabled);

    // Receive handoff from the old server (blocking until complete). The IPC
    // socket comes back already claimed: `receive_handoff` takes it before it
    // acknowledges, so the path never points at a server that has exited while
    // the sessions below are still being rebuilt.
    let (state, fds, lock_guard, listener) = handoff::receive_handoff()?;
    *handoff_committed = true;

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
    let live_sessions = session_manager.pending_session_ids().await;
    let workspace_manager =
        Arc::new(RwLock::new(workspace_manager::WorkspaceManager::restore_from_handoff(
            cfg.workspace_roots,
            &state.workspaces,
            state.workspace_tree,
            &state.windows,
            &live_sessions,
        )));
    let restored_ci_windows = state.ci_windows.clone();
    let restored_transfer_ledger = state.transfer_ledger.clone();

    // Record completion only after restoration succeeds. A receiver that ACKed
    // but could not rebuild the handed-off state must not announce success.
    updater::post_upgrade::record_upgrade(env!("CARGO_PKG_VERSION"));
    if let Some(runtime_dir) = server_socket_path().parent() {
        updater::post_upgrade::reap_orphaned_stages(runtime_dir);
    }

    info!("session restoration complete — accepting connections");

    Box::pin(run_server_loop(
        session_manager,
        workspace_manager,
        (lock_guard, listener),
        ServerLoopConfig {
            update: cfg.update,
            agent_api: cfg.agent_api,
            launchd_slot,
            restored_ci_windows,
            // The handed-off sessions are alive — there is nothing to recover,
            // and a client reattaching to a surviving server replays nothing
            // that could consume an entry anyway.
            recovered_sessions: std::collections::HashMap::new(),
            restored_transfer_ledger,
        },
    ))
    .await
}

/// Run the IPC server, handoff listener, and signal handler concurrently.
///
/// Shared between normal and upgrade startup paths. Cleans up the IPC socket
/// on exit. Both the singleton lock guard and the listener are acquired by the
/// caller: the normal path binds before this call, and the upgrade path binds
/// inside the handoff, then acquires the lock after the predecessor exits.
/// `_lock_guard` must live until the server shuts down to hold the advisory
/// flock.
struct ServerLoopConfig {
    update: UpdateConfig,
    agent_api: AgentApiConfig,
    launchd_slot: Option<LaunchdSlot>,
    restored_ci_windows: Vec<github_ci::HandoffCiWindow>,
    /// Per-launch session content recovered from the previous process's state
    /// dump; consumed by cold-restart `CreateSession` replays.
    recovered_sessions:
        std::collections::HashMap<String, scribe_common::screen_replay::SessionReplay>,
    /// Workspace-transfer results carried across a handoff so a lost-ACK
    /// retry deduplicates on the successor; empty on cold start.
    restored_transfer_ledger: Vec<workspace_transfer::TransferLedgerEntry>,
}

#[allow(clippy::too_many_lines, reason = "server setup remains one ordered startup transaction")]
async fn run_server_loop(
    session_manager: Arc<session_manager::SessionManager>,
    workspace_manager: Arc<RwLock<workspace_manager::WorkspaceManager>>,
    (_lock_guard, listener): (ipc_server::ServerLock, tokio::net::UnixListener),
    config: ServerLoopConfig,
) -> Result<(), ScribeError> {
    let ServerLoopConfig {
        update,
        agent_api,
        launchd_slot,
        restored_ci_windows,
        recovered_sessions,
        restored_transfer_ledger,
    } = config;
    let live_sessions = ipc_server::new_live_session_registry();
    let workspace_transfers = workspace_transfer::restored_transfer_gate(restored_transfer_ledger);
    let window_shares = ipc_server::new_window_shares();
    let git_ref_watcher =
        Arc::new(git_ref_watcher::GitRefWatcherControl::new(github_ci::github_ci_enabled()));

    // The socket is already bound; queued client connections sit in the kernel
    // backlog until `start_ipc_server` begins accepting below, so nothing here
    // can present a connectable-but-dead socket to a client.

    // Activate sessions restored from a hot-reload handoff. Moves them from
    // SessionManager into the live registry and starts their PTY reader tasks
    // in detached mode. No-op for normal (non-upgrade) startup.
    ipc_server::activate_pending_sessions(
        &session_manager,
        &workspace_manager,
        &live_sessions,
        &window_shares,
        &git_ref_watcher,
    )
    .await;

    // Spawn the background updater. The handle is passed into the IPC server
    // so that TriggerUpdate / DismissUpdate messages can reach it.
    let updater_handle = Arc::new(updater::spawn_updater(Arc::clone(&window_shares), update));

    // No-op unless this process came up from an upgrade.
    tokio::spawn(updater::announce_upgrade_completion(Arc::clone(&window_shares)));

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
    let ci_dismissals = Arc::default();
    let github_ci_tracker = github_ci::spawn_tracker(
        Arc::clone(&git_ref_watcher),
        Arc::clone(&workspace_manager),
        Arc::clone(&window_shares),
        Arc::clone(&ci_dismissals),
        restored_ci_windows,
    );
    let server_state = ipc_server::IpcServerState {
        session_manager: Arc::clone(&session_manager),
        workspace_manager: Arc::clone(&workspace_manager),
        beads_boards: beads_board::BeadsBoardCache::default(),
        live_sessions: Arc::clone(&live_sessions),
        window_shares: Arc::clone(&window_shares),
        ci_dismissals,
        github_ci_tracker: github_ci_tracker.clone(),
        updater_handle: Arc::clone(&updater_handle),
        release_catalog: Arc::clone(&release_catalog),
        release_fetcher: Arc::clone(&release_fetcher),
        env_store: Arc::clone(&env_store),
        remote_control: Arc::clone(&remote_control),
        git_ref_watcher: Arc::clone(&git_ref_watcher),
        agent_api: agent_api::AgentApiState::new(agent_api),
        recovered_sessions: Arc::new(std::sync::Mutex::new(recovered_sessions)),
        workspace_transfers: Arc::clone(&workspace_transfers),
    };

    // Crash-recovery dump: checkpoint the live state on a dirty-gated interval
    // so a crash or SIGKILL loses at most one interval of terminal content.
    let dump_task = state_dump::spawn_dump_task(
        Arc::clone(&live_sessions),
        Arc::clone(&workspace_manager),
        github_ci_tracker.clone(),
        Arc::clone(&workspace_transfers),
    );
    // The handoff-listener arm consumes `github_ci_tracker`; the final dump
    // below runs after the select and needs its own handle.
    let shutdown_ci_tracker = github_ci_tracker.clone();

    // Spec 027: forward agent-activity lease transitions to each session's
    // window as `AgentActivity`, gated on the participant's `agent_api` bit.
    ipc_server::spawn_agent_activity_forwarder(&server_state);

    // Spec 027: tell capable windows when a capability prompt stopped being
    // answerable, so no consent dialog outlives the request it was gating.
    ipc_server::spawn_agent_prompt_dismiss_forwarder(&server_state);

    // Start the remote-control supervisor: it applies the current `[remote]`
    // config (a no-op when disabled — the default) and then rebinds/stops the
    // listener live on every `ConfigReloaded`. Spawned, not awaited, so a wedged
    // tailscaled cannot delay local serving; the server is never restarted.
    tokio::spawn(ipc_server::remote_supervisor(Arc::clone(&remote_control), server_state.clone()));

    // Debian postinst treats this exact line as successful hot-reload. Emit it
    // only after handoff restoration and session activation have succeeded,
    // immediately before the already-bound listener starts accepting. Normal
    // startup reaches the same readiness point through this shared path.
    if let Some(slot) = launchd_slot
        && let Err(error) =
            macos_launchd::record_active_slot(scribe_common::app::current_identity(), slot)
    {
        warn!(%error, slot = slot.name(), "failed to record active launchd slot");
    }
    #[cfg(target_os = "macos")]
    if let Some(slot) = launchd_slot
        && let Err(error) = spawn_inactive_slot_cleanup(slot)
    {
        warn!(%error, slot = slot.name(), "failed to start inactive launchd cleanup");
    }
    info!("IPC server listening");

    let handoff_triggered = tokio::select! {
        result = ipc_server::start_ipc_server(listener, server_state) => {
            result?;
            false
        }
        result = handoff::run_handoff_listener(
            Arc::clone(&workspace_manager),
            Arc::clone(&live_sessions),
            github_ci_tracker,
            Arc::clone(&workspace_transfers),
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
        () = wait_for_sigterm() => {
            // systemd stop, launchd job removal, and reboot all deliver
            // SIGTERM; handling it turns every ordinary service stop into the
            // same dump-then-exit path as Ctrl+C instead of a silent kill.
            info!("received SIGTERM");
            false
        }
    };

    dump_task.abort();
    if handoff_triggered {
        // Defuse Pty objects so the old server's exit doesn't send SIGHUP to
        // child processes. alacritty_terminal::Pty::drop() explicitly calls
        // kill(child_pid, SIGHUP) — the new server already has the master fds.
        // The readers are deliberately left running: the new server owns these
        // children now, so nothing here may cancel a reader into the exit
        // funnel and report their sessions dead. No final dump either: the
        // successor owns these sessions now and writes its own, so a stale
        // dump from this side could only shadow a fresher one.
        ipc_server::defuse_for_handoff(&live_sessions).await;
    } else {
        // The sessions die with this process, so checkpoint them first: this
        // is what makes an ordinary service stop or reboot content-preserving
        // for the next cold start's replay.
        state_dump::dump_now(
            &live_sessions,
            &workspace_manager,
            &shutdown_ci_tracker,
            &workspace_transfers,
        )
        .await;
        // Stop the readers before the runtime unwinds, under the same bounded
        // join the close paths use, so shutdown is not the one exit path that
        // abandons a task parked on a PTY read (spec 017 US1-3).
        ipc_server::shutdown_pty_readers(&live_sessions).await;
        // Only clean up the IPC socket if we're NOT handing off. During a
        // handoff the new server has already bound to the same socket path —
        // removing it would make the new server unreachable.
        cleanup_socket(&server_socket_path());
    }

    info!("scribe-server stopped");
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn_inactive_slot_cleanup(active: LaunchdSlot) -> Result<(), String> {
    use std::process::Stdio;

    let identity = scribe_common::app::current_identity();
    let client = std::env::current_exe()
        .map_err(|error| format!("cannot resolve server executable: {error}"))?
        .with_file_name(identity.client_binary_name());
    if !client.is_file() {
        return Err(format!("client binary not found at {}", client.display()));
    }
    std::process::Command::new(&client)
        .arg(active.inactive_unregistration_argument())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|error| format!("failed to spawn inactive-slot cleanup: {error}"))
}

/// Resolve when SIGTERM arrives; pends forever when the handler cannot be
/// installed, leaving Ctrl+C and the handoff listener as the exit paths.
async fn wait_for_sigterm() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(error) => {
            warn!(%error, "SIGTERM handler unavailable");
            std::future::pending::<()>().await;
        }
    }
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

#[cfg(test)]
mod server_log_tests {
    use scribe_common::{app::rotate_log_if_oversized, macos_launchd::LaunchdSlot};

    use super::{ServerAction, parse_args};

    #[test]
    fn parser_stops_non_startup_arguments_before_server_setup() {
        assert_eq!(parse_args(["--help"]), ServerAction::Help);
        assert_eq!(parse_args(["--version"]), ServerAction::Version);
        assert_eq!(parse_args(["--upgrde"]), ServerAction::Unknown("--upgrde".to_owned()));
        assert_eq!(parse_args([]), ServerAction::RunServer { upgrade: false, launchd_slot: None });
        assert_eq!(
            parse_args(["--upgrade", "--launchd-slot=alternate"]),
            ServerAction::RunServer { upgrade: true, launchd_slot: Some(LaunchdSlot::Alternate) }
        );
    }

    #[test]
    fn rotates_only_when_over_cap() {
        let dir =
            std::env::temp_dir().join(format!("scribe-server-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.log");
        let rotated = dir.join("server.log.1");

        // Under the cap: untouched, no rotation file appears.
        std::fs::write(&path, b"small").unwrap();
        rotate_log_if_oversized(&path, 16);
        assert!(path.exists());
        assert!(!rotated.exists());

        // Over the cap: renamed aside, replacing any prior rotation.
        std::fs::write(&path, vec![b'x'; 32]).unwrap();
        rotate_log_if_oversized(&path, 16);
        assert!(!path.exists());
        assert_eq!(std::fs::read(&rotated).unwrap().len(), 32);

        drop(std::fs::remove_dir_all(&dir));
    }
}
