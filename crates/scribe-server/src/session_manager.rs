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
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::{SessionContext, TerminalSize};
use scribe_common::screen::{
    CellFlags as ScreenCellFlags, CursorStyle as ScreenCursorStyle, ScreenCell, ScreenColor,
    ScreenSnapshot,
};
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
    /// Codex task label from handoff. `None` when unset for the session.
    pub codex_task_label: Option<String>,
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
    pub cwd: Option<std::path::PathBuf>,
    pub size: Option<TerminalSize>,
    pub command: Option<Vec<String>>,
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
            codex_task_label: None,
            cwd: None,
            context: None,
            ai_state: None,
            ai_provider_hint: self.ai_provider_hint,
            cell_width: self.geometry.cell_width,
            cell_height: self.geometry.cell_height,
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
        let launch = self.prepare_session_launch(session_id, request);
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
        let pty_options = build_pty_options(shell, request.cwd, &shell_binary, integration_enabled);

        PreparedSessionLaunch {
            session_id,
            workspace_id: request.workspace_id,
            ai_provider_hint,
            term,
            event_rx,
            shell_name,
            pty_options,
            geometry,
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
                codex_task_label: handoff_session.codex_task_label.clone(),
                cwd: handoff_session.cwd.clone(),
                context: handoff_session.context.clone(),
                ai_state: handoff_session.ai_state.clone(),
                ai_provider_hint: handoff_session.ai_provider_hint,
                cell_width: handoff_session.cell_width.max(1),
                cell_height: handoff_session.cell_height.max(1),
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

fn build_pty_options(
    shell: Option<alacritty_terminal::tty::Shell>,
    cwd: Option<std::path::PathBuf>,
    shell_binary: &str,
    integration_enabled: bool,
) -> PtyOptions {
    let mut env = HashMap::from([
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ("TERM_PROGRAM".to_owned(), "Scribe".to_owned()),
        ("TERM_PROGRAM_VERSION".to_owned(), env!("CARGO_PKG_VERSION").to_owned()),
    ]);
    if integration_enabled {
        inject_shell_integration_env(shell_binary, &mut env);
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
    if command_mentions_binary(parts, "codex") {
        Some(AiProvider::CodexCode)
    } else if command_mentions_binary(parts, "claude") {
        Some(AiProvider::ClaudeCode)
    } else {
        None
    }
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
