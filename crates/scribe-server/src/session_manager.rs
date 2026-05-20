use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alacritty_terminal::Term;
use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::tty::Options as PtyOptions;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tracing::info;
use vte::Parser as VteParser;
use vte::ansi::Processor as AnsiProcessor;

use scribe_common::ai_state::{AiProcessState, AiProvider};
use scribe_common::error::ScribeError;
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{SessionContext, TerminalSize};
use scribe_common::screen::{
    CellFlags as ScreenCellFlags, CursorStyle as ScreenCursorStyle, ScreenCell, ScreenColor,
    ScreenSnapshot,
};
use scribe_common::socket::server_socket_path;
use scribe_pty::async_fd::AsyncPtyFd;
use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};

use crate::handoff::HandoffState;
use crate::shell_integration;

/// Maximum number of active PTY sessions across all clients.
const MAX_SESSIONS: usize = 256;

/// Default terminal columns.
const DEFAULT_COLS: u16 = 80;

/// Default terminal rows.
const DEFAULT_ROWS: u16 = 24;

fn snapshot_line(index: usize) -> Line {
    Line(i32::try_from(index).unwrap_or(i32::MAX))
}

fn scrollback_line(offset: usize) -> Line {
    Line(-i32::try_from(offset).unwrap_or(i32::MAX))
}

fn snapshot_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn nonnegative_u16(value: i32) -> u16 {
    u16::try_from(value.max(0)).unwrap_or(u16::MAX)
}

fn snapshot_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn convert_named_color(named: alacritty_terminal::vte::ansi::NamedColor) -> u16 {
    use alacritty_terminal::vte::ansi::NamedColor;

    match named {
        NamedColor::Black => 0,
        NamedColor::Red => 1,
        NamedColor::Green => 2,
        NamedColor::Yellow => 3,
        NamedColor::Blue => 4,
        NamedColor::Magenta => 5,
        NamedColor::Cyan => 6,
        NamedColor::White => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        NamedColor::Foreground => 256,
        NamedColor::Background => 257,
        NamedColor::Cursor => 258,
        NamedColor::DimBlack => 259,
        NamedColor::DimRed => 260,
        NamedColor::DimGreen => 261,
        NamedColor::DimYellow => 262,
        NamedColor::DimBlue => 263,
        NamedColor::DimMagenta => 264,
        NamedColor::DimCyan => 265,
        NamedColor::DimWhite => 266,
        NamedColor::BrightForeground => 267,
        NamedColor::DimForeground => 268,
    }
}

/// Build the terminal core config used for live PTY sessions.
pub fn build_term_config(scrollback_lines: usize) -> TermConfig {
    TermConfig {
        scrolling_history: scrollback_lines,
        // Codex probes kitty keyboard mode during startup; enabling support
        // lets alacritty_terminal answer `CSI ? u` queries and mode updates.
        kitty_keyboard: true,
        ..TermConfig::default()
    }
}

/// A managed PTY session with terminal emulator state.
///
/// Fields are `pub` for crate-internal access (the module itself is private).
pub struct ManagedSession {
    pub pty_fd: AsyncPtyFd,
    /// Duplicate PTY master fd used for safe winsize updates and handoff fd passing.
    pub resize_fd: OwnedFd,
    pub child_pid: u32,
    pub term: Arc<Mutex<Term<ScribeEventListener>>>,
    /// ANSI processor for feeding bytes into `Term<ScribeEventListener>`.
    /// Uses `vte::ansi::Processor` which calls `Handler` methods on Term.
    pub ansi_processor: AnsiProcessor,
    /// VTE parser for the OSC interceptor (calls `Perform` on `OscInterceptor`).
    pub osc_parser: VteParser,
    pub event_rx: mpsc::UnboundedReceiver<SessionEvent>,
    pub workspace_id: WorkspaceId,
    pub shell_name: String,
    /// Keep the Pty object alive so the child process is not killed by SIGHUP
    /// when Pty's Drop impl runs. The Pty owns the child process handle.
    /// Owns the child process. Moved into `SessionHandle` by the IPC server.
    /// If dropped, sends SIGHUP to the child.
    ///
    /// `None` for sessions restored from a hot-reload handoff — the child stays
    /// alive because it holds the slave fd; we only need the master fd.
    pub pty: Option<alacritty_terminal::tty::Pty>,
    /// Screen snapshot from a hot-reload handoff. Sent to the first client
    /// that attaches (then cleared) so the pre-handoff screen content is
    /// restored instead of a blank terminal.
    pub handoff_snapshot: Option<ScreenSnapshot>,
    /// Title from handoff, used to restore tab name. `None` for fresh sessions.
    pub title: Option<String>,
    /// Provider task label from handoff. `None` when unset for the session.
    pub task_label: Option<String>,
    /// CWD from handoff, used to restore working directory. `None` for fresh sessions.
    pub cwd: Option<std::path::PathBuf>,
    /// Remote/tmux context from handoff. `None` for fresh sessions.
    pub context: Option<SessionContext>,
    /// AI state from handoff. `None` for fresh sessions.
    pub ai_state: Option<AiProcessState>,
    /// Launch-time AI provider hint derived from the session command.
    pub ai_provider_hint: Option<AiProvider>,
    /// Latest known terminal cell size in pixels for PTY winsize replies.
    pub cell_width: u16,
    pub cell_height: u16,
    /// Launch-record id (== env-envelope id) used to name this session's
    /// encrypted env envelope on disk. `Some` for cold-restart replays that
    /// re-issued a `LaunchRecord` via `CreateSession.env_envelope_id`; `None`
    /// for fresh first-time creations and for handoff-restored sessions
    /// (handoff keeps env on the existing PTY, no envelope handoff).
    ///
    /// Captured so the clean-close path in `ipc_server::handle_close_session`
    /// can find and delete the matching `<state_dir>/restore/env/<window_id>/
    /// <launch_id>.envz` file plus its keystore DEK without re-deriving the
    /// id from any client-supplied input.
    pub env_envelope_id: Option<String>,
}

/// Terminal dimensions implementing the `Dimensions` trait from `alacritty_terminal`.
struct TermDimensions {
    cols: usize,
    lines: usize,
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

struct SessionGeometry {
    dimensions: TermDimensions,
    window_size: WindowSize,
    cell_width: u16,
    cell_height: u16,
}

pub struct SessionLaunchRequest {
    pub workspace_id: WorkspaceId,
    /// The window that requested this session. Used to scope env-envelope
    /// lookups (envelopes live under `restore/env/<window_id>/`), so the
    /// restore-apply step can only consume envelopes owned by the
    /// requesting window per FR-005.
    pub window_id: WindowId,
    pub cwd: Option<std::path::PathBuf>,
    pub size: Option<TerminalSize>,
    pub command: Option<Vec<String>>,
    /// Optional launch-record id naming an encrypted env envelope to apply
    /// to the new PTY (cold-restart replay). `None` for normal first-time
    /// session creation and for handoff-restored sessions (env stays on the
    /// existing PTY across handoff).
    pub env_envelope_id: Option<String>,
}

struct PreparedSessionLaunch {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    ai_provider_hint: Option<AiProvider>,
    term: Term<ScribeEventListener>,
    event_rx: mpsc::UnboundedReceiver<SessionEvent>,
    shell_name: String,
    pty_options: PtyOptions,
    geometry: SessionGeometry,
    /// Carries the `launch_id` naming the env envelope (cold-restart restore-apply
    /// payload); `None` when the request did not name an envelope. Forwarded
    /// onto `ManagedSession` so the clean-close path can locate and delete the
    /// envelope without re-deriving the id.
    env_envelope_id: Option<String>,
}

impl PreparedSessionLaunch {
    fn spawn_pty(&self) -> Result<alacritty_terminal::tty::Pty, ScribeError> {
        alacritty_terminal::tty::new(&self.pty_options, self.geometry.window_size, 0).map_err(|e| {
            ScribeError::PtySpawnFailed { reason: format!("alacritty tty::new failed: {e}") }
        })
    }

    fn into_managed_session(
        self,
        pty: alacritty_terminal::tty::Pty,
    ) -> Result<ManagedSession, ScribeError> {
        let child_pid = pty.child().id();
        let master_file = pty.file().try_clone().map_err(|e| ScribeError::PtySpawnFailed {
            reason: format!("failed to clone PTY master fd: {e}"),
        })?;
        let master_fd: OwnedFd = master_file.into();
        let resize_fd = rustix::io::dup(&master_fd).map_err(|e| ScribeError::PtySpawnFailed {
            reason: format!("failed to duplicate PTY master fd: {e}"),
        })?;
        let pty_fd = AsyncPtyFd::new(master_fd).map_err(|e| ScribeError::PtySpawnFailed {
            reason: format!("AsyncPtyFd::new failed: {e}"),
        })?;
        let ansi_processor = AnsiProcessor::new();
        let osc_parser = VteParser::new();

        info!(%self.session_id, %self.workspace_id, "created new PTY session");

        Ok(ManagedSession {
            pty_fd,
            resize_fd,
            child_pid,
            term: Arc::new(Mutex::new(self.term)),
            ansi_processor,
            osc_parser,
            event_rx: self.event_rx,
            workspace_id: self.workspace_id,
            shell_name: self.shell_name,
            pty: Some(pty),
            handoff_snapshot: None,
            title: None,
            task_label: None,
            cwd: None,
            context: None,
            ai_state: None,
            ai_provider_hint: self.ai_provider_hint,
            cell_width: self.geometry.cell_width,
            cell_height: self.geometry.cell_height,
            env_envelope_id: self.env_envelope_id,
        })
    }
}

/// Manages all active PTY sessions.
pub struct SessionManager {
    sessions: Arc<tokio::sync::RwLock<HashMap<SessionId, ManagedSession>>>,
    /// Scrollback lines used when creating new sessions.
    scrollback_lines: AtomicUsize,
    /// Whether shell integration env injection is enabled.
    shell_integration_enabled: std::sync::atomic::AtomicBool,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::with_scrollback(10_000)
    }
}

impl SessionManager {
    /// Create a new `SessionManager` with a specific scrollback line count.
    #[must_use]
    pub fn with_scrollback(scrollback_lines: usize) -> Self {
        Self {
            sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            scrollback_lines: AtomicUsize::new(scrollback_lines),
            shell_integration_enabled: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Enable or disable shell integration env injection for new sessions.
    pub fn set_shell_integration_enabled(&self, enabled: bool) {
        self.shell_integration_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Update the scrollback line count used for new sessions and live sessions.
    pub fn set_scrollback_lines(&self, lines: usize) {
        self.scrollback_lines.store(lines, Ordering::Relaxed);
    }

    /// Create a new PTY session in the given workspace.
    ///
    /// Spawns a PTY via `alacritty_terminal::tty`, creates an `AsyncPtyFd`
    /// wrapper for epoll-driven I/O, and creates a `Term<ScribeEventListener>`
    /// for terminal state management. Uses the scrollback line count configured
    /// at construction time.
    pub async fn create_session(
        &self,
        request: SessionLaunchRequest,
    ) -> Result<SessionId, ScribeError> {
        let session_id = SessionId::new();
        self.reserve_session_slot().await?;

        // Cold-restart restore-apply (FR-005 / FR-008): if the launch names
        // an env envelope, decrypt it now and stage a per-spawn temp file
        // for the shell integration script to source. Fail-safe per FR-016:
        // any error here returns `None` so the session still spawns with rc
        // defaults instead of being blocked by the keystore.
        let restore_env_file = match request.env_envelope_id.as_deref() {
            Some(envelope_id) => {
                prepare_restore_env_file(request.window_id, session_id, envelope_id).await
            }
            None => None,
        };

        let launch = self.prepare_session_launch(session_id, request, restore_env_file.as_deref());
        let pty = launch.spawn_pty()?;
        let managed = launch.into_managed_session(pty)?;
        self.sessions.write().await.insert(session_id, managed);
        Ok(session_id)
    }

    async fn reserve_session_slot(&self) -> Result<(), ScribeError> {
        let sessions = self.sessions.read().await;
        if sessions.len() >= MAX_SESSIONS {
            return Err(ScribeError::IpcError {
                reason: "global session limit reached".to_owned(),
            });
        }
        Ok(())
    }

    fn prepare_session_launch(
        &self,
        session_id: SessionId,
        request: SessionLaunchRequest,
        restore_env_file: Option<&std::path::Path>,
    ) -> PreparedSessionLaunch {
        let scrollback_lines = self.scrollback_lines.load(Ordering::Relaxed);
        let ai_provider_hint = command_ai_provider_hint(request.command.as_deref());
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let event_listener = ScribeEventListener::new(session_id, event_tx);
        let term_config = build_term_config(scrollback_lines);
        let geometry = session_geometry(request.size);
        let term = Term::new(term_config, &geometry.dimensions, event_listener);
        let shell_binary = shell_binary_str(request.command.as_deref());
        let shell_name = Path::new(&shell_binary)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("shell")
            .to_owned();
        let kind = shell_integration::detect_shell(&shell_binary);
        let integration_enabled = self.shell_integration_enabled.load(Ordering::Relaxed);
        let integration_script = session_integration_script(&shell_binary, integration_enabled);
        let shell =
            build_shell(&shell_binary, request.command, kind, integration_script.as_deref());
        let pty_options = build_pty_options(PtyOptionsBuild {
            session_id,
            shell,
            cwd: request.cwd,
            shell_binary: &shell_binary,
            integration_enabled,
            restore_env_file,
        });

        PreparedSessionLaunch {
            session_id,
            workspace_id: request.workspace_id,
            ai_provider_hint,
            term,
            event_rx,
            shell_name,
            pty_options,
            geometry,
            env_envelope_id: request.env_envelope_id,
        }
    }

    /// Remove a session from the map and return it.
    ///
    /// This allows the IPC server to take ownership of the session for
    /// its read loop, avoiding lock contention on the sessions map during
    /// per-byte processing.
    pub async fn take_session(&self, session_id: SessionId) -> Option<ManagedSession> {
        self.sessions.write().await.remove(&session_id)
    }

    /// List all pending session IDs and their workspace IDs.
    ///
    /// "Pending" means the session exists in the manager but has not yet been
    /// taken by the IPC server. Used to activate handoff-restored sessions.
    pub async fn pending_session_ids(&self) -> Vec<(SessionId, WorkspaceId)> {
        self.sessions.read().await.iter().map(|(&id, s)| (id, s.workspace_id)).collect()
    }

    /// Reconstruct a `SessionManager` from handoff state and received PTY fds.
    ///
    /// Each fd in `fds` corresponds to the session at the same index in
    /// `state.sessions`. A fresh `Term` and metadata pipeline are created for
    /// each session.
    pub fn restore_from_handoff(
        state: &HandoffState,
        fds: Vec<OwnedFd>,
        scrollback: usize,
    ) -> Result<Self, ScribeError> {
        // shell_integration_enabled defaults to true; callers may override via
        // set_shell_integration_enabled after construction.
        let mut sessions_map = HashMap::new();

        for (handoff_session, owned_fd) in state.sessions.iter().zip(fds) {
            let cols = handoff_session.cols;
            let rows = handoff_session.rows;

            // Create metadata event channel.
            let (event_tx, event_rx) = mpsc::unbounded_channel();

            // Create event listener.
            let event_listener = ScribeEventListener::new(handoff_session.session_id, event_tx);

            // Create Term config with scrollback and the same terminal
            // protocol support used by newly spawned sessions.
            let term_config = build_term_config(scrollback);

            // Create Term with the session's dimensions.
            let dimensions = TermDimensions { cols: usize::from(cols), lines: usize::from(rows) };
            let mut term = Term::new(term_config, &dimensions, event_listener);

            let handoff_snapshot = apply_handoff_content(handoff_session, &mut term, scrollback);

            // Wrap the received fd for async I/O.
            let resize_fd =
                rustix::io::dup(&owned_fd).map_err(|e| ScribeError::PtySpawnFailed {
                    reason: format!(
                        "failed to duplicate restored PTY master fd for {}: {e}",
                        handoff_session.session_id
                    ),
                })?;
            let pty_fd = AsyncPtyFd::new(owned_fd).map_err(|e| ScribeError::PtySpawnFailed {
                reason: format!(
                    "AsyncPtyFd::new failed during restore for {}: {e}",
                    handoff_session.session_id
                ),
            })?;

            // Create parsers.
            let ansi_processor = AnsiProcessor::new();
            let osc_parser = VteParser::new();

            info!(
                session_id = %handoff_session.session_id,
                workspace_id = %handoff_session.workspace_id,
                child_pid = handoff_session.child_pid,
                cols,
                rows,
                v5_replay = handoff_session.session_replay.is_some(),
                "restored session from handoff"
            );

            // NOTE: We do NOT have a `Pty` object from alacritty_terminal here.
            // The child process stays alive because it holds the slave side of
            // the PTY. We only need the master fd (which we received). We must
            // create a ManagedSession without the `pty` field, which means we
            // need to make that field optional or restructure.
            //
            // For now we create a new PTY just to hold the child — but actually
            // we cannot: the child already exists. Instead we make `pty` an
            // Option. See the ManagedSession struct change.
            let managed = ManagedSession {
                pty_fd,
                resize_fd,
                child_pid: handoff_session.child_pid,
                term: Arc::new(Mutex::new(term)),
                ansi_processor,
                osc_parser,
                event_rx,
                workspace_id: handoff_session.workspace_id,
                shell_name: handoff_session.shell_name.clone(),
                pty: None,
                handoff_snapshot,
                title: handoff_session.title.clone(),
                task_label: handoff_session
                    .task_label
                    .clone()
                    .or_else(|| handoff_session.codex_task_label.clone()),
                cwd: handoff_session.cwd.clone(),
                context: handoff_session.context.clone(),
                ai_state: handoff_session.ai_state.clone(),
                ai_provider_hint: handoff_session.ai_provider_hint,
                cell_width: handoff_session.cell_width.max(1),
                cell_height: handoff_session.cell_height.max(1),
                // Handoff keeps env on the existing PTY; no envelope is
                // written for handoff-restored sessions, so close-time
                // delete has nothing to do.
                env_envelope_id: None,
            };

            sessions_map.insert(handoff_session.session_id, managed);
        }

        Ok(Self {
            sessions: Arc::new(tokio::sync::RwLock::new(sessions_map)),
            scrollback_lines: AtomicUsize::new(scrollback),
            shell_integration_enabled: std::sync::atomic::AtomicBool::new(true),
        })
    }
}

/// Populate a freshly-restored `Term` with the pre-handoff content.
///
/// - v5 path: decompress the `SessionReplay` and feed it through
///   `AnsiProcessor` into `term`, then trim the pseudo-scrollback pushed in
///   by the encoder's leading ED 2. Returns `None` because the Term now
///   owns the content.
/// - v4 fallback (or if v5 decompression fails): return the legacy
///   `ScreenSnapshot` so the first attach can deliver it.
fn apply_handoff_content(
    handoff_session: &crate::handoff::HandoffSession,
    term: &mut Term<ScribeEventListener>,
    scrollback: usize,
) -> Option<ScreenSnapshot> {
    let Some(replay) = handoff_session.session_replay.as_ref() else {
        return handoff_session.snapshot.clone();
    };

    let bytes = match scribe_common::screen_replay::decompress_session_replay(replay) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(
                session_id = %handoff_session.session_id,
                "v5 replay decompress failed, falling back to legacy snapshot: {e}"
            );
            return handoff_session.snapshot.clone();
        }
    };

    let mut processor: AnsiProcessor = AnsiProcessor::new();
    processor.advance(term, &bytes);

    // Trim the pseudo-scrollback the encoder's leading ED 2 pushes into
    // history on a fresh grid. The snapshot's true scrollback_rows survives.
    let kept = (replay.scrollback_rows as usize).min(scrollback);
    let grid = term.grid_mut();
    grid.update_history(kept);
    grid.update_history(scrollback);

    None
}

fn session_geometry(size: Option<TerminalSize>) -> SessionGeometry {
    let init_cols = size.and_then(|s| (s.cols > 0).then_some(s.cols)).unwrap_or(DEFAULT_COLS);
    let init_rows = size.and_then(|s| (s.rows > 0).then_some(s.rows)).unwrap_or(DEFAULT_ROWS);
    let cell_width = size.and_then(|s| (s.cell_width > 0).then_some(s.cell_width)).unwrap_or(1);
    let cell_height = size.and_then(|s| (s.cell_height > 0).then_some(s.cell_height)).unwrap_or(1);
    let dimensions = TermDimensions { cols: usize::from(init_cols), lines: usize::from(init_rows) };
    let window_size =
        WindowSize { num_lines: init_rows, num_cols: init_cols, cell_width, cell_height };

    SessionGeometry { dimensions, window_size, cell_width, cell_height }
}

/// Inputs to [`build_pty_options`]. Grouped into a struct so the call site
/// stays under Clippy's `too_many_arguments` threshold and remains readable
/// alongside the other prepared-launch fields.
struct PtyOptionsBuild<'a> {
    session_id: SessionId,
    shell: Option<alacritty_terminal::tty::Shell>,
    cwd: Option<std::path::PathBuf>,
    shell_binary: &'a str,
    integration_enabled: bool,
    restore_env_file: Option<&'a std::path::Path>,
}

fn build_pty_options(opts: PtyOptionsBuild<'_>) -> PtyOptions {
    let PtyOptionsBuild {
        session_id,
        shell,
        cwd,
        shell_binary,
        integration_enabled,
        restore_env_file,
    } = opts;
    let mut env = HashMap::from([
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ("TERM_PROGRAM".to_owned(), "Scribe".to_owned()),
        ("TERM_PROGRAM_VERSION".to_owned(), env!("CARGO_PKG_VERSION").to_owned()),
        // Hook channel discovery — see specs/003-ai-hook-channel/contracts/env-vars.md.
        // Both vars MUST be set together; absence of either signals "not under
        // Scribe" to `scribe-hook-helper`, which then exits 0 silently.
        ("SCRIBE_HOOK_SOCK".to_owned(), server_socket_path().to_string_lossy().into_owned()),
        ("SCRIBE_SESSION_ID".to_owned(), session_id.to_full_string()),
    ]);
    if integration_enabled {
        inject_shell_integration_env(shell_binary, &mut env);
    }

    // Per specs/006-persist-terminal-env/contracts/hook-event-additions.md, when
    // the spawn is restore-driven and an envelope decrypted successfully, point
    // the shell at the per-spawn temp file the integration script sources after
    // rc has run. Absence of the var leaves the shell with rc defaults.
    if let Some(path) = restore_env_file {
        env.insert("SCRIBE_RESTORE_ENV_DELTA_FILE".to_owned(), path.to_string_lossy().into_owned());
    }

    PtyOptions {
        shell,
        env,
        working_directory: cwd.filter(|p| p.is_dir()).or_else(dirs::home_dir),
        ..PtyOptions::default()
    }
}

fn session_integration_script(shell_binary: &str, integration_enabled: bool) -> Option<String> {
    if !integration_enabled {
        return None;
    }

    shell_integration::find_scripts_dir()
        .and_then(|dir| shell_integration::integration_script_path(shell_binary, &dir))
        .and_then(|path| path.to_str().map(String::from))
}

/// Extract the shell binary string from an optional command slice, falling
/// back to `$SHELL`, then the account login shell, then `"sh"`.
fn shell_binary_str(command: Option<&[String]>) -> String {
    command
        .and_then(|parts| parts.first())
        .cloned()
        .unwrap_or_else(scribe_common::shell::default_shell_program)
}

fn command_ai_provider_hint(command: Option<&[String]>) -> Option<AiProvider> {
    let parts = command?;
    AiProvider::all()
        .iter()
        .copied()
        .find(|provider| command_mentions_binary(parts, provider.binary_name()))
}

fn command_mentions_binary(parts: &[String], binary_name: &str) -> bool {
    parts.iter().any(|part| {
        if path_basename_eq(part, binary_name) {
            return true;
        }
        part.split_whitespace()
            .any(|token| path_basename_eq(token.trim_matches('\'').trim_matches('"'), binary_name))
    })
}

fn path_basename_eq(candidate: &str, expected: &str) -> bool {
    Path::new(candidate).file_name().and_then(|name| name.to_str()) == Some(expected)
}

/// Build the `Shell` for a PTY, adding `--rcfile` for bash.
///
/// When `command` is `None` (use the user's default shell) and the detected
/// shell is bash with shell integration enabled, we pass `--rcfile <script>`
/// so bash reads the integration script instead of `~/.bashrc` (the script
/// itself sources `~/.bashrc`).  We avoid `--posix` because POSIX mode
/// corrupts the history subsystem — even after `set +o posix`, `history -r`
/// only loads a handful of entries instead of the full `$HISTFILE`. For other
/// shells we still spawn the resolved default shell explicitly so GUI-launched
/// apps do not silently fall back to the server process environment's `SHELL`.
fn build_shell(
    shell_binary: &str,
    command: Option<Vec<String>>,
    kind: shell_integration::ShellKind,
    integration_script: Option<&str>,
) -> Option<alacritty_terminal::tty::Shell> {
    match command {
        Some(parts) => {
            let mut iter = parts.into_iter();
            let program = iter.next()?;
            let mut args: Vec<String> = iter.collect();
            match kind {
                shell_integration::ShellKind::Bash => {
                    if let Some(script) = integration_script {
                        args.insert(0, script.to_owned());
                        args.insert(0, "--rcfile".to_owned());
                    }
                }
                shell_integration::ShellKind::PowerShell => {
                    if let Some(script) = integration_script.filter(|_| args.is_empty()) {
                        args.splice(
                            0..0,
                            [
                                String::from("-NoLogo"),
                                String::from("-NoExit"),
                                String::from("-File"),
                                script.to_owned(),
                            ],
                        );
                    }
                }
                shell_integration::ShellKind::Zsh
                | shell_integration::ShellKind::Fish
                | shell_integration::ShellKind::Nushell
                | shell_integration::ShellKind::Unknown => {}
            }
            Some(alacritty_terminal::tty::Shell::new(program, args))
        }
        None => {
            match kind {
                shell_integration::ShellKind::Bash => {
                    let args = integration_script.map_or_else(Vec::new, |script| {
                        vec!["--rcfile".to_owned(), script.to_owned()]
                    });
                    Some(alacritty_terminal::tty::Shell::new(shell_binary.to_owned(), args))
                }
                shell_integration::ShellKind::PowerShell => {
                    let args = integration_script.map_or_else(Vec::new, |script| {
                        vec![
                            String::from("-NoLogo"),
                            String::from("-NoExit"),
                            String::from("-File"),
                            script.to_owned(),
                        ]
                    });
                    Some(alacritty_terminal::tty::Shell::new(shell_binary.to_owned(), args))
                }
                shell_integration::ShellKind::Zsh
                | shell_integration::ShellKind::Fish
                | shell_integration::ShellKind::Nushell
                | shell_integration::ShellKind::Unknown => {
                    // These shells rely on environment-based startup hooks, but
                    // we still spawn the resolved shell binary explicitly.
                    Some(alacritty_terminal::tty::Shell::new(shell_binary.to_owned(), Vec::new()))
                }
            }
        }
    }
}

/// Inject shell integration environment variables when a scripts directory
/// is available. Modifies `env` in place.
fn inject_shell_integration_env(shell_binary: &str, env: &mut HashMap<String, String>) {
    let Some(scripts_dir) = shell_integration::find_scripts_dir() else { return };
    let extra = shell_integration::build_env(shell_binary, &scripts_dir);
    env.extend(extra);
}

/// Create a `ScreenSnapshot` from a locked `Term`.
///
/// Iterates the visible grid (`screen_lines` x columns) and converts each
/// `alacritty_terminal` cell into our `ScreenCell` wire type.  Also captures
/// scrollback history so the client can restore it on reconnect.
pub fn snapshot_term(term: &Term<ScribeEventListener>) -> ScreenSnapshot {
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();

    // --- visible grid ---
    let mut cells = Vec::with_capacity(cols * rows);

    for line_idx in 0..rows {
        let line = snapshot_line(line_idx);
        let row = &grid[line];
        for col_idx in 0..cols {
            let cell = &row[Column(col_idx)];
            cells.push(convert_cell(cell));
        }
    }

    let cursor_point = grid.cursor.point;
    let cursor_style = term.cursor_style();
    let mode = term.mode();
    let cursor_visible = mode.contains(alacritty_terminal::term::TermMode::SHOW_CURSOR);
    let alt_screen = mode.contains(alacritty_terminal::term::TermMode::ALT_SCREEN);

    // --- scrollback history ---
    // Skip scrollback for alt screen: the alt grid's history is not meaningful
    // user content — it is a resize artifact from Grid::shrink_lines rotations
    // that Term::resize does not clamp.  Alt screen apps (vim, Claude Code)
    // redraw their own UI on reconnect anyway.
    let (scrollback, history) = if alt_screen {
        (Vec::new(), 0)
    } else {
        // Line(-1) is the most recent scrollback line (just above visible area),
        // Line(-history_size) is the oldest.  We iterate oldest-first so the
        // client can feed them in chronological order.
        let history = grid.history_size();
        let mut scrollback = Vec::with_capacity(cols * history);

        for i in (1..=history).rev() {
            let line = scrollback_line(i);
            let row = &grid[line];
            for col_idx in 0..cols {
                let cell = &row[Column(col_idx)];
                scrollback.push(convert_cell(cell));
            }
        }

        (scrollback, history)
    };

    tracing::debug!(cols, rows, alt_screen, scrollback_rows = history, "snapshot_term captured");

    ScreenSnapshot {
        cells,
        cols: snapshot_u16(cols),
        rows: snapshot_u16(rows),
        cursor_col: snapshot_u16(cursor_point.column.0),
        cursor_row: nonnegative_u16(cursor_point.line.0),
        cursor_style: convert_cursor_style(cursor_style),
        cursor_visible,
        alt_screen,
        scrollback,
        scrollback_rows: snapshot_u32(history),
    }
}

/// Convert an `alacritty_terminal` `Cell` to our `ScreenCell` wire type.
pub fn convert_cell(cell: &alacritty_terminal::term::cell::Cell) -> ScreenCell {
    ScreenCell {
        c: cell.c,
        fg: convert_color(cell.fg),
        bg: convert_color(cell.bg),
        flags: convert_flags(cell.flags),
    }
}

/// Convert an `alacritty_terminal` `Color` to our `ScreenColor`.
pub fn convert_color(color: alacritty_terminal::vte::ansi::Color) -> ScreenColor {
    match color {
        alacritty_terminal::vte::ansi::Color::Named(named) => {
            ScreenColor::Named(convert_named_color(named))
        }
        alacritty_terminal::vte::ansi::Color::Indexed(idx) => ScreenColor::Indexed(idx),
        alacritty_terminal::vte::ansi::Color::Spec(rgb) => {
            ScreenColor::Rgb { r: rgb.r, g: rgb.g, b: rgb.b }
        }
    }
}

/// Convert `alacritty_terminal` cell `Flags` to our `CellFlags`.
pub fn convert_flags(flags: CellFlags) -> ScreenCellFlags {
    ScreenCellFlags {
        emphasis: scribe_common::screen::CellEmphasisFlags {
            weight: scribe_common::screen::CellWeightFlags {
                bold: flags.contains(CellFlags::BOLD),
                dim: flags.contains(CellFlags::DIM),
            },
            italic: flags.contains(CellFlags::ITALIC),
        },
        decoration: scribe_common::screen::CellDecorationFlags {
            underline: flags.contains(CellFlags::UNDERLINE),
            strikethrough: flags.contains(CellFlags::STRIKEOUT),
        },
        presentation: scribe_common::screen::CellPresentationFlags {
            inverse: flags.contains(CellFlags::INVERSE),
            hidden: flags.contains(CellFlags::HIDDEN),
        },
        layout: scribe_common::screen::CellLayoutFlags {
            wide: flags.contains(CellFlags::WIDE_CHAR),
            wrap: flags.contains(CellFlags::WRAPLINE),
        },
    }
}

/// Convert `alacritty_terminal` `CursorStyle` to our `CursorStyle`.
pub fn convert_cursor_style(
    style: alacritty_terminal::vte::ansi::CursorStyle,
) -> ScreenCursorStyle {
    match style.shape {
        alacritty_terminal::vte::ansi::CursorShape::Underline => ScreenCursorStyle::Underline,
        alacritty_terminal::vte::ansi::CursorShape::Beam => ScreenCursorStyle::Beam,
        alacritty_terminal::vte::ansi::CursorShape::HollowBlock => ScreenCursorStyle::HollowBlock,
        // Block, Hidden, and any future variants all map to Block.
        _ => ScreenCursorStyle::Block,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use std::fs;
    #[cfg(target_os = "macos")]
    use std::path::{Path, PathBuf};
    #[cfg(target_os = "macos")]
    use std::process::Command;
    #[cfg(target_os = "macos")]
    use std::time::{SystemTime, UNIX_EPOCH};

    use alacritty_terminal::tty::Shell;

    use super::build_shell;
    use crate::shell_integration::ShellKind;

    #[test]
    fn build_shell_uses_explicit_resolved_shell_for_zsh_defaults() {
        let shell = build_shell("/bin/zsh", None, ShellKind::Zsh, None);

        assert_eq!(shell, Some(Shell::new(String::from("/bin/zsh"), Vec::new())));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_integration_sources_bash_profile_for_non_login_shells_on_macos() {
        let home = make_temp_home("profile");
        fs::write(home.join(".bash_profile"), "export PROFILE_SEEN=1\n")
            .expect("write .bash_profile");
        fs::write(home.join(".bashrc"), "export BASHRC_SEEN=1\n").expect("write .bashrc");

        let output = run_bash_integration_check(&home);
        cleanup_temp_home(&home);

        assert!(
            output.contains("PROFILE=1 BASHRC=0"),
            "expected bash profile to win on macOS, got output: {output}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_integration_falls_back_to_bashrc_when_no_profile_exists_on_macos() {
        let home = make_temp_home("bashrc");
        fs::write(home.join(".bashrc"), "export BASHRC_SEEN=1\n").expect("write .bashrc");

        let output = run_bash_integration_check(&home);
        cleanup_temp_home(&home);

        assert!(
            output.contains("PROFILE=0 BASHRC=1"),
            "expected bashrc fallback on macOS, got output: {output}"
        );
    }

    #[cfg(target_os = "macos")]
    fn run_bash_integration_check(home: &Path) -> String {
        let script = bash_integration_script_path();
        let output = Command::new("/bin/bash")
            .arg("--rcfile")
            .arg(&script)
            .arg("-ic")
            .arg("printf 'PROFILE=%s BASHRC=%s\\n' \"${PROFILE_SEEN:-0}\" \"${BASHRC_SEEN:-0}\"")
            .env("HOME", home)
            .env("TERM_PROGRAM", "Scribe")
            .env("SCRIBE_SHELL_INTEGRATION", "1")
            .output()
            .expect("run bash integration check");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[cfg(target_os = "macos")]
    fn bash_integration_script_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../dist/shell-integration/bash/scribe.bash")
            .canonicalize()
            .expect("canonicalize bash integration script path")
    }

    #[cfg(target_os = "macos")]
    fn make_temp_home(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("scribe-bash-startup-{name}-{nonce}"));
        fs::create_dir_all(&dir).expect("create temp home");
        dir
    }

    #[cfg(target_os = "macos")]
    fn cleanup_temp_home(home: &Path) {
        let _ignore = fs::remove_dir_all(home);
    }
}

// ---------------------------------------------------------------------------
// Cold-restart env restore-apply (see specs/006-persist-terminal-env/
// contracts/hook-event-additions.md and research.md::R1.3 / R3.5).
//
// The shell integration script sources `SCRIBE_RESTORE_ENV_DELTA_FILE` at
// the tail of its init — after rc has run, before the baseline-ready emit —
// then unlinks the file. The contents of that file (which we render here)
// drive what becomes the post-restore baseline. This step is intentionally
// skipped for handoff-restored sessions: per R3.5, handoff preserves the
// PTY's process so env stays intact and no apply is needed.
// ---------------------------------------------------------------------------

/// Decrypt the per-session env envelope, write a shell-source-compatible
/// temp file, and return the absolute path. The shell integration script
/// sources this path after rc has run, applies the deltas, and unlinks the
/// file.
///
/// Returns `None` (via early returns) when:
///   * persistence is disabled in config;
///   * no envelope exists for this launch (normal first-time session state);
///   * the keystore is unavailable / decrypt fails (FR-016 fail-safe);
///   * `XDG_RUNTIME_DIR` is unavailable; or
///   * writing the temp file fails.
///
/// In every failure case the session still spawns successfully with rc
/// defaults — graceful degradation per the fail-safe contract.
async fn prepare_restore_env_file(
    window_id: WindowId,
    session_id: SessionId,
    env_envelope_id: &str,
) -> Option<std::path::PathBuf> {
    // Feature-flag gate. Loading config off the hot path is fine here: this
    // helper only runs when the launch names an envelope (the cold-restart
    // path), not on every session creation.
    let enabled = match scribe_common::config::load_config() {
        Ok(cfg) => cfg.terminal.env_persistence.enabled,
        Err(e) => {
            tracing::warn!(
                target: "scribe_server::session_manager",
                error = ?e,
                "load_config failed during env restore; spawning without env apply"
            );
            return None;
        }
    };
    if !enabled {
        return None;
    }

    let delta = match crate::env_store::store::read_envelope(window_id, env_envelope_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                target: "scribe_server::session_manager",
                error = ?e,
                ?session_id,
                window_id = ?window_id,
                env_envelope_id,
                "read_envelope failed during restore; spawning without env apply (fail-safe)"
            );
            return None;
        }
    };

    let Some(runtime_dir) = runtime_dir_for_env_apply() else {
        tracing::warn!(
            target: "scribe_server::session_manager",
            "no XDG_RUNTIME_DIR available; env-restore deferred"
        );
        return None;
    };
    if let Err(e) = ensure_runtime_subdir(&runtime_dir).await {
        tracing::warn!(
            target: "scribe_server::session_manager",
            error = ?e,
            "create env-apply dir failed"
        );
        return None;
    }

    let pid = std::process::id();
    let file_name = format!("{session_id}-{pid}.sh");
    let path = runtime_dir.join(file_name);
    let body = render_shell_source(&delta);

    if let Err(e) = write_private_owner_only(&path, &body).await {
        tracing::warn!(
            target: "scribe_server::session_manager",
            error = ?e,
            "write env-apply file failed"
        );
        return None;
    }

    // Defensive cleanup: if the shell never sources/unlinks the file
    // (e.g., user pkill'd the shell before integration loaded), remove it
    // after a generous grace period so the runtime dir doesn't accumulate
    // cruft. The shell integration itself unlinks on the consume path —
    // this is only a safety net.
    let path_for_cleanup = path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        _ = tokio::fs::remove_file(&path_for_cleanup).await;
    });

    Some(path)
}

/// Per-user, per-flavor env-apply staging directory under
/// `$XDG_RUNTIME_DIR/<flavor>/env-apply/`. Flavor segment matches the
/// install-flavor slug used elsewhere (e.g. by `env_store::store`), so
/// stable and `scribe-dev` cannot collide on the same login user.
fn runtime_dir_for_env_apply() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from)?;
    let flavor = scribe_common::app::current_identity().slug();
    Some(base.join(flavor).join("env-apply"))
}

/// Create the env-apply directory (and any missing parents) with 0o700
/// perms. Idempotent — re-applies the mode if the dir already existed
/// with a wider mask.
async fn ensure_runtime_subdir(p: &std::path::Path) -> std::io::Result<()> {
    let p = p.to_owned();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&p)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&p)?.permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&p, perms)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| std::io::Error::other(format!("blocking panic: {e}")))?
}

/// Write `content` to `path` with create-or-truncate semantics and 0o600
/// perms (owner-only) on Unix. fsynced before returning so the temp file is
/// durable before the shell tries to source it.
async fn write_private_owner_only(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let path_owned = path.to_owned();
    let body = content.to_owned();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Write as _;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path_owned)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()
    })
    .await
    .map_err(|e| std::io::Error::other(format!("blocking panic: {e}")))?
}

/// Render a `TerminalEnvDelta` as a shell-source-compatible script.
///
/// Uses POSIX single-quote escaping so the output is safe across bash, zsh,
/// fish, nu, and pwsh — see `specs/006-persist-terminal-env/contracts/
/// hook-event-additions.md`. Inside a single-quoted string, single quotes
/// are escaped by closing the quote, inserting a backslash-quoted single
/// quote, and reopening — the canonical bash idiom `'\''`. Newlines, tabs,
/// spaces, slashes, and `$` are all literal inside single quotes and need
/// no further escaping.
fn render_shell_source(delta: &crate::env_store::delta::TerminalEnvDelta) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("# Scribe env restore — sourced by shell integration after rc, then unlinked.\n");
    for (name, value) in &delta.added {
        let escaped = value.replace('\'', "'\\''");
        _ = writeln!(out, "export {name}='{escaped}'");
    }
    for name in &delta.removed {
        _ = writeln!(out, "unset {name}");
    }
    out
}

#[cfg(test)]
mod tests_apply {
    use std::collections::{BTreeMap, BTreeSet};

    use crate::env_store::delta::TerminalEnvDelta;

    use super::render_shell_source;

    #[test]
    fn render_shell_source_quotes_values_correctly() {
        let mut added = BTreeMap::new();
        added.insert("FOO".to_owned(), "bar".to_owned());
        added.insert("PATH".to_owned(), "/a:/b".to_owned());
        added.insert("WITH_QUOTE".to_owned(), "it's value".to_owned());
        added.insert("WITH_SPACES".to_owned(), "hello world".to_owned());
        let mut removed = BTreeSet::new();
        removed.insert("STALE".to_owned());
        let delta = TerminalEnvDelta { added, removed };
        let s = render_shell_source(&delta);
        assert!(s.contains("export FOO='bar'"), "{s}");
        assert!(s.contains("export PATH='/a:/b'"), "{s}");
        assert!(s.contains("export WITH_QUOTE='it'\\''s value'"), "{s}");
        assert!(s.contains("export WITH_SPACES='hello world'"), "{s}");
        assert!(s.contains("unset STALE"), "{s}");
    }
}
