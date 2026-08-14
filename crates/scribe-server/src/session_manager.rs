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
use scribe_common::protocol::{AiLaunchSpec, AiResumeMode, SessionContext, TerminalSize};
use scribe_common::screen::{
    CellFlags as ScreenCellFlags, CursorStyle as ScreenCursorStyle, DecPrivateMode, ScreenCell,
    ScreenColor, ScreenSnapshot,
};
use scribe_common::socket::server_socket_path;
use scribe_pty::async_fd::AsyncPtyFd;
use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};

use crate::handoff::{HandoffSession, HandoffState};
use crate::pty_guard::PtyGuard;
use crate::shell_integration::{self, ShellKind};

/// Maximum number of live PTY sessions across all clients.
pub const MAX_SESSIONS: usize = 256;

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
        // Spec 010: alacritty's default `Osc52::OnlyCopy` silently drops
        // OSC 52 read sequences inside the terminal core, so the read
        // never surfaces as a `SessionEvent::ClipboardLoad` and the
        // policy gating in `ipc_server::handle_clipboard_load` never
        // runs. Forward both directions to the gating layer and let
        // `ClipboardPolicyConfig` (FR-001 / FR-004) make the
        // deny / allow / prompt decision per request.
        osc52: alacritty_terminal::term::Osc52::CopyPaste,
        ..TermConfig::default()
    }
}

/// One reservation against the global [`MAX_SESSIONS`] budget (spec 017 US7-1).
///
/// The permit is taken atomically before a session's PTY is spawned and is
/// released by this value's `Drop`, so the population the cap bounds is
/// exactly the set of sessions that still exist — not the transient contents
/// of any one map. The slot therefore has to travel with the session from
/// [`SessionManager::create_session`] through [`SessionManager::take_session`]
/// and into the live-session registry entry: dropping it any earlier hands the
/// slot back while the session is still running and lets the next create
/// overshoot the cap.
pub struct SessionSlot {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// A managed PTY session with terminal emulator state.
///
/// Fields are `pub` for crate-internal access (the module itself is private).
pub struct ManagedSession {
    /// This session's reservation against the global [`MAX_SESSIONS`] budget
    /// (spec 017 US7-1). Acquired before the PTY is spawned and moved onward
    /// into the live registry entry, which is what makes the cap count live
    /// sessions; see [`SessionSlot`].
    pub slot: SessionSlot,
    pub pty_fd: AsyncPtyFd,
    /// Duplicate PTY master fd used for safe winsize updates and handoff fd passing.
    pub resize_fd: OwnedFd,
    pub child_pid: u32,
    /// `pidfd` for the child, opened at spawn so the child-exit watcher can
    /// wait on the process itself instead of inferring death from master EOF
    /// (spec 017 US1-2).
    ///
    /// `None` for handoff-restored sessions — their child belongs to the
    /// previous server process and was reparented when it exited, so this
    /// process cannot wait on it — and on platforms without `pidfd`. Those
    /// sessions stay on the EOF path with `exit_code: None`.
    pub child_pidfd: Option<OwnedFd>,
    /// Per-boot identity token for `child_pid` (spec 017 US7-2), read at spawn
    /// for fresh sessions and carried on the wire for handoff-restored ones.
    /// Unlike `child_pidfd` this one survives the handoff, which is what lets
    /// the successor prove an inherited PID before signalling it. `None` means
    /// the PID cannot be proven, making the close-time SIGHUP for
    /// handoff-restored sessions a no-op.
    pub child_identity: Option<crate::child_identity::ChildIdentity>,
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
    /// Tearing the guard down sends SIGHUP to the child and reaps it off any
    /// Tokio worker; see [`crate::pty_guard::PtyGuard`].
    ///
    /// `None` for sessions restored from a hot-reload handoff — the child stays
    /// alive because it holds the slave fd; we only need the master fd.
    pub pty: Option<PtyGuard>,
    /// Screen snapshot from a hot-reload handoff. Sent to the first client
    /// that attaches (then cleared) so the pre-handoff screen content is
    /// restored instead of a blank terminal.
    pub handoff_snapshot: Option<ScreenSnapshot>,
    /// Title from handoff, used to restore tab name. `None` for fresh sessions.
    pub title: Option<String>,
    /// Icon/tab title from handoff. `None` for fresh sessions.
    pub icon_title: Option<String>,
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
    /// Prompt history from handoff, kept next to `ai_state` so a restored
    /// session still answers `SessionList` with the bar the client had.
    /// `None` for fresh sessions.
    pub prompt_state: Option<scribe_common::protocol::SessionPromptState>,
    /// Latest known terminal cell size in pixels for PTY winsize replies.
    pub cell_width: u16,
    pub cell_height: u16,
    /// Stable env-envelope owner carried by handoff. `None` for fresh sessions
    /// and older handoffs; `start_session` then uses its existing window id.
    pub env_window_id: Option<WindowId>,
    /// Launch-record id (== env-envelope id) used to name this session's
    /// encrypted env envelope on disk. `Some` for cold-restart replays that
    /// re-issued a `LaunchRecord` via `CreateSession.env_envelope_id`, and for
    /// handoff-restored sessions whose predecessor already had an envelope.
    ///
    /// Captured so the clean-close path in `ipc_server::handle_close_session`
    /// can find and delete the matching `<state_dir>/restore/env/<window_id>/
    /// <launch_id>.envz` file plus its keystore DEK without re-deriving the
    /// id from any client-supplied input.
    pub env_envelope_id: Option<String>,
    /// Committed terminal-image state a predecessor exported for this session,
    /// staged onto the fresh reader seam before the first byte is read. `None`
    /// for every freshly spawned session and for any handoff that carried no
    /// image state.
    // @lat: [[terminal-images#Terminal Images#Image State Across Handoff]]
    pub image_state: Option<Box<crate::terminal_image_handoff::SessionImageHandoff>>,
}

/// The per-session values [`SessionManager::restore_from_handoff`] builds
/// before it can assemble a [`ManagedSession`].
struct RestoredSessionParts {
    slot: SessionSlot,
    pty_fd: AsyncPtyFd,
    resize_fd: OwnedFd,
    term: Term<ScribeEventListener>,
    ansi_processor: AnsiProcessor,
    osc_parser: VteParser,
    event_rx: mpsc::UnboundedReceiver<SessionEvent>,
    handoff_snapshot: Option<ScreenSnapshot>,
}

/// Assemble one handoff-restored session.
///
/// There is no `Pty` to carry: the child stays alive on the slave side and this
/// process only ever received the master fd, so `pty` is `None` and the
/// close-time SIGHUP is arbitrated by `child_identity` instead.
fn restored_managed_session(
    handoff_session: &HandoffSession,
    parts: RestoredSessionParts,
) -> ManagedSession {
    ManagedSession {
        slot: parts.slot,
        pty_fd: parts.pty_fd,
        resize_fd: parts.resize_fd,
        child_pid: handoff_session.child_pid,
        // Inherited child: this process never spawned it, so it arms no
        // child-exit watcher and keeps EOF-based exit detection.
        child_pidfd: None,
        // Carried, never re-derived: re-reading the token here would certify
        // whatever process holds the PID right now, which is exactly the
        // assumption the check exists to reject. A sender that predates the
        // field leaves this `None`, and the session is then exempt from the
        // close-time SIGHUP.
        child_identity: handoff_session.child_identity,
        term: Arc::new(Mutex::new(parts.term)),
        ansi_processor: parts.ansi_processor,
        osc_parser: parts.osc_parser,
        event_rx: parts.event_rx,
        workspace_id: handoff_session.workspace_id,
        shell_name: handoff_session.shell_name.clone(),
        pty: None,
        handoff_snapshot: parts.handoff_snapshot,
        title: handoff_session.title.clone(),
        icon_title: handoff_session.icon_title.clone(),
        task_label: handoff_session
            .task_label
            .clone()
            .or_else(|| handoff_session.codex_task_label.clone()),
        cwd: handoff_session.cwd.clone(),
        context: handoff_session.context.clone(),
        ai_state: handoff_session.ai_state.clone(),
        ai_provider_hint: handoff_session.ai_provider_hint,
        prompt_state: handoff_session.prompt_state.clone(),
        cell_width: handoff_session.cell_width.max(1),
        cell_height: handoff_session.cell_height.max(1),
        env_window_id: handoff_session.env_window_id,
        env_envelope_id: handoff_session.env_envelope_id.clone(),
        image_state: handoff_session.image_state.clone().map(Box::new),
    }
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
    /// Structured AI launch intent used by the server-owned argv builder.
    /// When present, this is authoritative and the dual-written legacy
    /// `command` is ignored.
    pub ai_launch: Option<AiLaunchSpec>,
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
        slot: SessionSlot,
    ) -> Result<ManagedSession, ScribeError> {
        let child_pid = pty.child().id();
        // Read the identity token now, while the child is unquestionably ours:
        // it has just been forked and nothing has reaped it, so the PID cannot
        // yet name anything else (spec 017 US7-2).
        let child_identity = crate::child_identity::read_child_identity(child_pid);
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
            slot,
            pty_fd,
            resize_fd,
            child_pid,
            child_pidfd: crate::child_watch::open_child_pidfd(child_pid),
            child_identity,
            term: Arc::new(Mutex::new(self.term)),
            ansi_processor,
            osc_parser,
            event_rx: self.event_rx,
            workspace_id: self.workspace_id,
            shell_name: self.shell_name,
            pty: Some(PtyGuard::new(pty)),
            handoff_snapshot: None,
            title: None,
            icon_title: None,
            task_label: None,
            cwd: None,
            context: None,
            ai_state: None,
            ai_provider_hint: self.ai_provider_hint,
            prompt_state: None,
            cell_width: self.geometry.cell_width,
            cell_height: self.geometry.cell_height,
            env_window_id: None,
            env_envelope_id: self.env_envelope_id,
            image_state: None,
        })
    }
}

/// Manages all active PTY sessions.
pub struct SessionManager {
    sessions: Arc<tokio::sync::RwLock<HashMap<SessionId, ManagedSession>>>,
    /// Admission control for [`MAX_SESSIONS`] (spec 017 US7-1): one permit per
    /// live session, held by the session itself for its whole lifetime.
    ///
    /// `sessions` is only a staging area — the IPC server takes each session
    /// out of it moments after creation — so its length was never a count of
    /// anything and could not enforce a cap. The semaphore is the count, and
    /// because a permit is taken with a single non-blocking `try_acquire`, a
    /// burst of concurrent creates admits exactly the number of free slots.
    slots: Arc<tokio::sync::Semaphore>,
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
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_SESSIONS)),
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
        // Reserved first and held across every fallible step below: an error
        // after this point drops `slot` and returns the reservation, so a
        // failed spawn never leaks a slot.
        let slot = self.reserve_session_slot()?;
        let session_id = SessionId::new();

        let shell = ResolvedShell::for_request(request.command.as_deref());

        // One config read per spawn drives both the restore-apply decision
        // and the shell-side gate var, so a launch cannot see the feature as
        // enabled in one half and disabled in the other.
        let env_persistence = env_persistence_enabled();

        // Cold-restart restore-apply (FR-005 / FR-008): if the launch names
        // an env envelope, decrypt it now and stage a per-spawn temp file
        // for the shell integration script to source. Fail-safe per FR-016:
        // any error here returns `None` so the session still spawns with rc
        // defaults instead of being blocked by the keystore.
        let integration_enabled = self.shell_integration_enabled.load(Ordering::Relaxed);
        let restore_env_file = match request.env_envelope_id.as_deref() {
            Some(envelope_id) if env_persistence => {
                prepare_restore_env_file(request.window_id, session_id, envelope_id, shell.kind)
                    .await
            }
            _ => None,
        };

        let launch = self.prepare_session_launch(
            session_id,
            request,
            &shell,
            EnvLaunchContext {
                restore_file: restore_env_file.as_deref(),
                persistence_enabled: env_persistence,
                integration_enabled,
            },
        );
        let pty = launch.spawn_pty()?;
        let managed = launch.into_managed_session(pty, slot)?;
        self.sessions.write().await.insert(session_id, managed);
        Ok(session_id)
    }

    /// Claim one of the [`MAX_SESSIONS`] slots, or refuse immediately.
    ///
    /// `try_acquire_owned` makes the test and the take a single atomic step,
    /// which is the whole point: N concurrent creates admit exactly the number
    /// of free slots and every loser gets [`ScribeError::SessionLimitReached`]
    /// straight away instead of parking until some unrelated session closes.
    fn reserve_session_slot(&self) -> Result<SessionSlot, ScribeError> {
        Arc::clone(&self.slots)
            .try_acquire_owned()
            .map(|permit| SessionSlot { _permit: permit })
            .map_err(|_| {
                tracing::warn!(max = MAX_SESSIONS, "session limit reached, refusing create");
                ScribeError::SessionLimitReached { limit: MAX_SESSIONS }
            })
    }

    fn prepare_session_launch(
        &self,
        session_id: SessionId,
        request: SessionLaunchRequest,
        shell: &ResolvedShell,
        env: EnvLaunchContext<'_>,
    ) -> PreparedSessionLaunch {
        let shell_binary = shell.binary.as_str();
        let scrollback_lines = self.scrollback_lines.load(Ordering::Relaxed);
        let ai_provider_hint = request
            .ai_launch
            .as_ref()
            .map(|launch| launch.provider)
            .or_else(|| command_ai_provider_hint(request.command.as_deref()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let event_listener = ScribeEventListener::new(session_id, event_tx);
        let term_config = build_term_config(scrollback_lines);
        let geometry = session_geometry(request.size);
        let term = Term::new(term_config, &geometry.dimensions, event_listener);
        let shell_name = Path::new(shell_binary)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("shell")
            .to_owned();
        let integration_enabled = env.integration_enabled;
        let integration_script = session_integration_script(shell.kind, integration_enabled);
        let pty_shell = build_launch_shell(
            shell_binary,
            request.command,
            shell.kind,
            integration_script.as_deref(),
            request.ai_launch.as_ref(),
        )
        .map(|(program, args)| alacritty_terminal::tty::Shell::new(program, args));
        let kitty_window_id = codex_kitty_window_id(
            request.ai_launch.as_ref(),
            crate::terminal_image_sharing::images_master_enabled(),
        );
        let pty_options = build_pty_options(PtyOptionsBuild {
            session_id,
            shell: pty_shell,
            cwd: request.cwd,
            shell_kind: shell.kind,
            env,
            kitty_window_id,
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
    ///
    /// Restored sessions consume [`MAX_SESSIONS`] slots exactly like freshly
    /// created ones (spec 017 US7-1) — otherwise a hot reload would reset the
    /// cap to zero used and let the successor run at twice the budget. The
    /// predecessor enforced the same cap, so an over-budget payload means a
    /// corrupt or hostile sender: the excess is refused rather than admitted,
    /// which also keeps a handoff peer from allocating up to `MAX_FDS`
    /// terminals' worth of state in this process.
    pub fn restore_from_handoff(
        state: &HandoffState,
        fds: Vec<OwnedFd>,
        scrollback: usize,
    ) -> Result<Self, ScribeError> {
        Self::restore_within_cap(state, fds, scrollback, MAX_SESSIONS)
    }

    /// [`Self::restore_from_handoff`] against an explicit slot budget. Split
    /// out so the over-budget branch is reachable in a test without opening
    /// `MAX_SESSIONS + 1` PTY pairs.
    fn restore_within_cap(
        state: &HandoffState,
        fds: Vec<OwnedFd>,
        scrollback: usize,
        cap: usize,
    ) -> Result<Self, ScribeError> {
        // shell_integration_enabled defaults to true; callers may override via
        // set_shell_integration_enabled after construction.
        let mut sessions_map = HashMap::new();
        let slots = Arc::new(tokio::sync::Semaphore::new(cap));

        for (handoff_session, owned_fd) in state.sessions.iter().zip(fds) {
            let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                tracing::error!(
                    offered = state.sessions.len(),
                    max = cap,
                    "handoff carried more sessions than the cap; refusing the excess"
                );
                break;
            };
            let slot = SessionSlot { _permit: permit };
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

            let managed = restored_managed_session(
                handoff_session,
                RestoredSessionParts {
                    slot,
                    pty_fd,
                    resize_fd,
                    term,
                    ansi_processor,
                    osc_parser,
                    event_rx,
                    handoff_snapshot,
                },
            );

            sessions_map.insert(handoff_session.session_id, managed);
        }

        Ok(Self {
            sessions: Arc::new(tokio::sync::RwLock::new(sessions_map)),
            slots,
            scrollback_lines: AtomicUsize::new(scrollback),
            shell_integration_enabled: std::sync::atomic::AtomicBool::new(true),
        })
    }
}

/// Populate a freshly-restored `Term` with the pre-handoff content.
///
/// - v5 path: decompress the `SessionReplay` and feed it through
///   `AnsiProcessor` into `term`, then normalize history for a replay produced
///   by a pre-RIS server. Returns `None` because the Term now owns the content.
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

    // N-1 handoff can carry the former ED-2 replay prefix. Normalize that
    // synthetic row while preserving the snapshot's true scrollback_rows.
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

/// The env-persistence half of a launch, resolved from one config read.
///
/// `restore_file` is `Some` only when the launch named an envelope that
/// decrypted; `persistence_enabled` is what the shells' gate var carries.
/// The two travel together so they cannot disagree about one spawn.
#[derive(Clone, Copy)]
struct EnvLaunchContext<'a> {
    restore_file: Option<&'a std::path::Path>,
    persistence_enabled: bool,
    integration_enabled: bool,
}

/// Inputs to [`build_pty_options`]. Grouped into a struct so the call site
/// stays under Clippy's `too_many_arguments` threshold and remains readable
/// alongside the other prepared-launch fields.
struct PtyOptionsBuild<'a> {
    session_id: SessionId,
    shell: Option<alacritty_terminal::tty::Shell>,
    cwd: Option<std::path::PathBuf>,
    shell_kind: ShellKind,
    env: EnvLaunchContext<'a>,
    kitty_window_id: bool,
}

fn build_pty_options(opts: PtyOptionsBuild<'_>) -> PtyOptions {
    let PtyOptionsBuild {
        session_id,
        shell,
        cwd,
        shell_kind,
        env: EnvLaunchContext { restore_file, persistence_enabled, integration_enabled },
        kitty_window_id,
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
    // launchd starts the server with PATH=/usr/bin:/bin:/usr/sbin:/sbin, so
    // without a floor every PTY session would miss Homebrew — /opt/homebrew/bin
    // (Apple Silicon) and /usr/local/bin (Intel) are added by `brew shellenv`
    // in the user's login profile, never by /etc/paths. This is the single env
    // funnel for all session types (plain, AI, SSH-local), so normalizing here
    // covers every shell, including fish/nushell/powershell which have no
    // login-profile emulation in their integration scripts.
    #[cfg(target_os = "macos")]
    env.insert("PATH".to_owned(), path_with_macos_baseline(std::env::var("PATH").ok().as_deref()));
    // Packaged layouts do not put `scribe-hook-helper` on `PATH`, so hand the
    // shells and the `ai-hook-*.sh` adapters an absolute path when we can
    // resolve one. Injected unconditionally: AI hooks run even with shell
    // integration disabled. Absence leaves the scripts on their PATH fallback.
    if let Some(helper) = shell_integration::find_hook_helper() {
        env.insert("SCRIBE_HOOK_HELPER".to_owned(), helper.to_string_lossy().into_owned());
    }

    // Codex checks `KITTY_WINDOW_ID` instead of probing. Keep that
    // compatibility marker out of ordinary PTYs, where applications such as
    // Yazi correctly treat it as terminal identity and skip capability probes.
    if kitty_window_id {
        env.insert("KITTY_WINDOW_ID".to_owned(), "1".to_owned());
    }

    // Spawn-time persistence gate. With the feature off the server drops
    // every `EnvChanged` at the ingress gate, so the shells' baseline
    // snapshot, per-prompt snapshot/diff, and helper fork are pure waste;
    // the scripts skip all of it when this reads `0`. The value is fixed at
    // spawn because no server-to-running-shell channel exists — a live
    // config flip binds newly started shells only, which is the documented
    // "restart or re-init required" semantic in both directions.
    env.insert(
        "SCRIBE_ENV_PERSIST".to_owned(),
        if persistence_enabled { "1" } else { "0" }.to_owned(),
    );

    if integration_enabled {
        inject_shell_integration_env(shell_kind, &mut env);
    }

    // Per specs/006-persist-terminal-env/contracts/hook-event-additions.md, when
    // the spawn is restore-driven and an envelope decrypted successfully, point
    // the shell at the per-spawn temp file consumed after login/rc processing.
    // Plain shells and AI bash use integration; AI zsh/fish use the server-built
    // preamble. Shell kinds with no consumer are filtered before staging.
    if let Some(path) = restore_file {
        env.insert("SCRIBE_RESTORE_ENV_DELTA_FILE".to_owned(), path.to_string_lossy().into_owned());
    }

    PtyOptions {
        shell,
        env,
        working_directory: cwd.filter(|p| p.is_dir()).or_else(dirs::home_dir),
        ..PtyOptions::default()
    }
}

fn codex_kitty_window_id(ai_launch: Option<&AiLaunchSpec>, images_enabled: bool) -> bool {
    images_enabled && ai_launch.is_some_and(|launch| launch.provider == AiProvider::CodexCode)
}

/// Return a PATH guaranteed to contain the macOS baseline directories.
///
/// Missing Homebrew prefixes (`/opt/homebrew/bin`, `/usr/local/bin`) are
/// prepended ahead of the inherited entries, matching `brew shellenv`
/// semantics; missing system directories are appended. Non-empty inherited
/// entries keep their order and are never duplicated, so the function is
/// idempotent. Empty entries (POSIX implicit-cwd, e.g. `::` or a leading
/// or trailing `:`) are deliberately dropped as a safety measure.
///
/// Compiled on every platform (macOS call site aside) so the unit tests
/// run everywhere.
#[cfg(any(target_os = "macos", test))]
fn path_with_macos_baseline(inherited: Option<&str>) -> String {
    const HOMEBREW_PREFIXES: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];
    const SYSTEM_DIRS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

    let existing: Vec<&str> =
        inherited.unwrap_or_default().split(':').filter(|entry| !entry.is_empty()).collect();
    let mut entries: Vec<&str> =
        HOMEBREW_PREFIXES.iter().copied().filter(|prefix| !existing.contains(prefix)).collect();
    entries.extend(&existing);
    entries.extend(SYSTEM_DIRS.iter().copied().filter(|dir| !existing.contains(dir)));
    entries.join(":")
}

fn session_integration_script(kind: ShellKind, integration_enabled: bool) -> Option<String> {
    if !integration_enabled {
        return None;
    }

    shell_integration::find_scripts_dir()
        .and_then(|dir| shell_integration::integration_script_path(kind, &dir))
        .and_then(|path| path.to_str().map(String::from))
}

/// The shell a launch will spawn, resolved once up front.
///
/// `create_session` needs the kind before it renders the cold-restart
/// restore file — that file's syntax and extension are shell-specific —
/// so detection cannot stay inside `prepare_session_launch`. It is also the
/// launch's only `detect_shell` call: the startup-script and integration-env
/// paths both take this kind instead of re-deriving it from the binary path.
struct ResolvedShell {
    binary: String,
    kind: ShellKind,
}

impl ResolvedShell {
    fn for_request(command: Option<&[String]>) -> Self {
        let binary = shell_binary_str(command);
        let kind = shell_integration::detect_shell(&binary);
        if kind == ShellKind::Unknown {
            tracing::debug!(shell = %binary, "unknown shell, skipping integration env");
        }
        Self { binary, kind }
    }
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

/// Build the `Shell` for a plain or legacy-command PTY.
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
) -> Option<(String, Vec<String>)> {
    let Some(parts) = command else {
        let args = match kind {
            shell_integration::ShellKind::Bash => integration_script
                .map_or_else(Vec::new, |script| vec!["--rcfile".to_owned(), script.to_owned()]),
            shell_integration::ShellKind::PowerShell => {
                integration_script.map_or_else(Vec::new, |script| {
                    vec![
                        String::from("-NoLogo"),
                        String::from("-NoExit"),
                        String::from("-File"),
                        script.to_owned(),
                    ]
                })
            }
            // These shells rely on environment-based startup hooks, but we
            // still spawn the resolved shell binary explicitly.
            shell_integration::ShellKind::Zsh
            | shell_integration::ShellKind::Fish
            | shell_integration::ShellKind::Nushell
            | shell_integration::ShellKind::Unknown => Vec::new(),
        };
        return Some((shell_binary.to_owned(), args));
    };

    let mut iter = parts.into_iter();
    let program = iter.next()?;
    let mut args: Vec<String> = iter.collect();
    match kind {
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
        shell_integration::ShellKind::Bash
        | shell_integration::ShellKind::Zsh
        | shell_integration::ShellKind::Fish
        | shell_integration::ShellKind::Nushell
        | shell_integration::ShellKind::Unknown => {}
    }
    Some((program, args))
}

/// The PTY shell for a launch, with or without structured AI intent.
///
/// An AI tab is deliberately not its own session class: it is the ordinary
/// [`build_shell`] invocation — same binary, same integration attachment, same
/// startup files — with a command appended that runs the provider. The shell
/// reads the user's rc exactly as every other tab does, so an AI tab can never
/// resolve a different `PATH` than the tab beside it.
fn build_launch_shell(
    shell_binary: &str,
    command: Option<Vec<String>>,
    kind: ShellKind,
    integration_script: Option<&str>,
    ai_launch: Option<&AiLaunchSpec>,
) -> Option<(String, Vec<String>)> {
    let Some(launch) = ai_launch else {
        return build_shell(shell_binary, command, kind, integration_script);
    };
    // PowerShell runs the provider through `-Command`, which is exclusive with
    // the `-File` integration attachment and has to be the final argument, so
    // an AI launch drops the script rather than ship an argv where one flag
    // eats another. Every other shell keeps its attachment.
    let script = integration_script.filter(|_| kind != ShellKind::PowerShell);
    let (program, mut args) = build_shell(shell_binary, command, kind, script)?;
    args.extend(ai_exec_args(kind, launch));
    Some((program, args))
}

/// Trailing argv that runs the provider and ends the session with it.
///
/// POSIX-family shells `exec` the provider over themselves, so the CLI becomes
/// the PTY's direct child and quitting it closes the tab instead of dropping
/// the user at a stray prompt.
fn ai_exec_args(kind: ShellKind, launch: &AiLaunchSpec) -> Vec<String> {
    let exec = ai_exec_command(kind, launch);
    match kind {
        // Nushell rejects the grouped short form and takes no integration
        // under any `-c` variant (its vendor autoload is REPL-only).
        ShellKind::Nushell => vec![String::from("-i"), String::from("-c"), exec],
        // Zsh and fish schedule their restore-delta apply for the first
        // `precmd` so it lands after the user's rc; an AI tab execs before any
        // prompt, so that consumer never runs and both the delta and its temp
        // file would be left behind. The `-c` command is the only point that
        // is still after user rc and still before `exec`, so it consumes the
        // file itself. Bash needs no equivalent — `scribe.bash` applies the
        // delta inline while it is being sourced, which is already post-rc.
        ShellKind::Zsh => vec![
            String::from("-ic"),
            format!(
                "[ -n \"${{SCRIBE_RESTORE_ENV_DELTA_FILE:-}}\" ] && [ -f \"$SCRIBE_RESTORE_ENV_DELTA_FILE\" ] && . \"$SCRIBE_RESTORE_ENV_DELTA_FILE\" && command rm -f \"$SCRIBE_RESTORE_ENV_DELTA_FILE\"; unset SCRIBE_RESTORE_ENV_DELTA_FILE; {exec}"
            ),
        ],
        ShellKind::Fish => vec![
            String::from("-ic"),
            format!(
                "if test -n \"$SCRIBE_RESTORE_ENV_DELTA_FILE\"; and test -f \"$SCRIBE_RESTORE_ENV_DELTA_FILE\"; source \"$SCRIBE_RESTORE_ENV_DELTA_FILE\"; command rm -f \"$SCRIBE_RESTORE_ENV_DELTA_FILE\"; end; set -e SCRIBE_RESTORE_ENV_DELTA_FILE; {exec}"
            ),
        ],
        // PowerShell has neither `-i` nor `exec`. `-Command` is the provider's
        // only job and pwsh exits when it returns, so the tab still ends with
        // the CLI without an exec to replace the process.
        ShellKind::PowerShell => vec![String::from("-NoLogo"), String::from("-Command"), exec],
        // An unknown shell is most likely POSIX-family; `-ic` is the portable
        // guess and is what `sh` itself accepts.
        ShellKind::Bash | ShellKind::Unknown => vec![String::from("-ic"), exec],
    }
}

fn ai_exec_command(kind: ShellKind, launch: &AiLaunchSpec) -> String {
    let binary = launch.provider.binary_name();
    let mut command =
        if kind == ShellKind::PowerShell { binary.to_owned() } else { format!("exec {binary}") };
    if launch.resume_mode == AiResumeMode::Resume {
        for arg in launch.provider.resume_args() {
            command.push(' ');
            command.push_str(arg);
        }
        if let Some(conversation_id) = launch.conversation_id.as_deref() {
            command.push(' ');
            command.push_str(&shell_single_quote(kind, conversation_id));
        }
    }
    command
}

/// Quote one argument in the command language spoken by the resolved shell.
fn shell_single_quote(kind: ShellKind, value: &str) -> String {
    match kind {
        ShellKind::Fish => format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'")),
        ShellKind::Nushell => nushell_single_quote(value),
        ShellKind::PowerShell => format!("'{}'", value.replace('\'', "''")),
        ShellKind::Bash | ShellKind::Zsh | ShellKind::Unknown => {
            format!("'{}'", value.replace('\'', "'\"'\"'"))
        }
    }
}

fn nushell_single_quote(value: &str) -> String {
    if !value.contains('\'') {
        return format!("'{value}'");
    }

    for hashes in 1..=8 {
        let marker = "#".repeat(hashes);
        if !value.contains(&format!("'{marker}")) {
            return format!("r{marker}'{value}'{marker}");
        }
    }

    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Inject shell integration environment variables when a scripts directory
/// is available. Modifies `env` in place.
fn inject_shell_integration_env(kind: ShellKind, env: &mut HashMap<String, String>) {
    let Some(scripts_dir) = shell_integration::find_scripts_dir() else { return };
    let extra = shell_integration::build_env(kind, &scripts_dir);
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

    let restore_modes = active_dec_modes(*mode);
    tracing::debug!(
        cols,
        rows,
        alt_screen,
        scrollback_rows = history,
        ?restore_modes,
        "snapshot_term captured"
    );

    ScreenSnapshot {
        cells,
        cols: snapshot_u16(cols),
        rows: snapshot_u16(rows),
        cursor_col: snapshot_u16(cursor_point.column.0),
        cursor_row: nonnegative_u16(cursor_point.line.0),
        cursor_style: convert_cursor_style(cursor_style),
        cursor_visible,
        alt_screen,
        // Capture the DEC private modes so the client can re-emit them on
        // reattach; otherwise a restored vim/tmux/Claude-Code session silently
        // loses mouse reporting, bracketed paste, focus reporting, and app
        // cursor/keypad.
        active_dec_modes: restore_modes,
        scrollback,
        scrollback_rows: snapshot_u32(history),
    }
}

/// Map the enabled DEC private modes from a `Term`'s mode flags into the
/// wire-side [`DecPrivateMode`] list restored on reattach.
fn active_dec_modes(mode: alacritty_terminal::term::TermMode) -> Vec<DecPrivateMode> {
    use DecPrivateMode as M;
    use alacritty_terminal::term::TermMode;
    [
        (TermMode::MOUSE_REPORT_CLICK, M::MouseReportClick),
        (TermMode::MOUSE_DRAG, M::MouseButtonEvent),
        (TermMode::MOUSE_MOTION, M::MouseAnyMotion),
        (TermMode::SGR_MOUSE, M::SgrMouse),
        (TermMode::UTF8_MOUSE, M::Utf8Mouse),
        (TermMode::ALTERNATE_SCROLL, M::AlternateScroll),
        (TermMode::BRACKETED_PASTE, M::BracketedPaste),
        (TermMode::FOCUS_IN_OUT, M::FocusEvent),
        (TermMode::APP_CURSOR, M::AppCursor),
        (TermMode::APP_KEYPAD, M::AppKeypad),
    ]
    .into_iter()
    .filter(|(f, _)| mode.contains(*f))
    .map(|(_, m)| m)
    .collect()
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
mod tests_session_cap {
    use crate::handoff::{HandoffSession, HandoffState};
    use std::os::fd::OwnedFd;
    use std::sync::Arc;

    use scribe_common::error::ScribeError;
    use scribe_common::ids::{SessionId, WorkspaceId};

    use super::{MAX_SESSIONS, SessionManager, SessionSlot};

    /// Build a handoff payload of `count` sessions, each backed by a real PTY
    /// pair. The slave fds are returned so the caller keeps them open for the
    /// duration of the test.
    fn handoff_state(count: usize) -> (HandoffState, Vec<OwnedFd>, Vec<OwnedFd>) {
        let mut sessions = Vec::with_capacity(count);
        let mut masters = Vec::with_capacity(count);
        let mut slaves = Vec::with_capacity(count);

        for _ in 0..count {
            let pty = nix::pty::openpty(None, None).expect("openpty");
            sessions.push(HandoffSession {
                session_id: SessionId::new(),
                workspace_id: WorkspaceId::new(),
                child_pid: std::process::id(),
                child_identity: None,
                cols: 80,
                rows: 24,
                cell_width: 1,
                cell_height: 1,
                snapshot: None,
                session_replay: None,
                title: None,
                icon_title: None,
                shell_name: String::from("zsh"),
                task_label: None,
                codex_task_label: None,
                cwd: None,
                context: None,
                ai_state: None,
                ai_provider_hint: None,
                prompt_state: None,
                env_window_id: None,
                env_envelope_id: None,
                image_state: None,
            });
            masters.push(pty.master);
            slaves.push(pty.slave);
        }

        let state = HandoffState {
            version: 5,
            sessions,
            workspaces: vec![],
            workspace_tree: None,
            windows: vec![],
            ci_windows: vec![],
        };
        (state, masters, slaves)
    }

    /// Park a reservation on `barrier` so every task in the storm contends for
    /// the same instant.
    fn spawn_reservation(
        manager: Arc<SessionManager>,
        barrier: Arc<tokio::sync::Barrier>,
    ) -> tokio::task::JoinHandle<Result<SessionSlot, ScribeError>> {
        tokio::spawn(async move {
            barrier.wait().await;
            manager.reserve_session_slot()
        })
    }

    fn is_cap_error(outcome: &Result<SessionSlot, ScribeError>) -> bool {
        matches!(outcome, Err(ScribeError::SessionLimitReached { limit }) if *limit == MAX_SESSIONS)
    }

    /// The reservation storm: `2 * MAX_SESSIONS` tasks released simultaneously
    /// must admit exactly `MAX_SESSIONS` of them, and every loser must get the
    /// typed cap error. A check-then-act cap would instead let an arbitrary
    /// number of them all observe "under the limit" before any of them
    /// recorded a session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_creates_stop_exactly_at_the_session_cap() {
        let manager = Arc::new(SessionManager::with_scrollback(100));
        let storm = MAX_SESSIONS * 2;
        let barrier = Arc::new(tokio::sync::Barrier::new(storm));

        let tasks: Vec<_> = (0..storm)
            .map(|_| spawn_reservation(Arc::clone(&manager), Arc::clone(&barrier)))
            .collect();

        let mut outcomes = Vec::with_capacity(storm);
        for task in tasks {
            outcomes.push(task.await.expect("reservation task"));
        }

        let refused = outcomes.iter().filter(|outcome| is_cap_error(outcome)).count();
        let mut admitted: Vec<_> = outcomes.into_iter().flatten().collect();

        assert_eq!(admitted.len(), MAX_SESSIONS);
        assert_eq!(refused, storm - MAX_SESSIONS, "every refusal must be the typed cap error");
        assert!(manager.reserve_session_slot().is_err(), "cap must stay closed while full");

        // Ending one session hands its slot straight back to the next create.
        admitted.pop();
        manager.reserve_session_slot().expect("slot freed by the ended session");
    }

    /// Handoff-restored sessions occupy cap slots too: a successor that
    /// restored `n` sessions may admit only `MAX_SESSIONS - n` new ones, so a
    /// hot reload cannot silently double the live-session budget.
    #[tokio::test]
    async fn handoff_restored_sessions_occupy_cap_slots() {
        const RESTORED: usize = 3;
        let (state, masters, _slaves) = handoff_state(RESTORED);
        let manager = SessionManager::restore_from_handoff(&state, masters, 100).unwrap();
        assert_eq!(manager.pending_session_ids().await.len(), RESTORED);

        // Bound, not discarded: a dropped `SessionSlot` returns its permit, so
        // the remaining budget only shrinks while the reservations are held.
        let _held: Vec<_> = (0..(MAX_SESSIONS - RESTORED))
            .map(|_| manager.reserve_session_slot().expect("slot below the cap"))
            .collect();
        assert!(
            manager.reserve_session_slot().is_err(),
            "restored sessions must count against the cap"
        );
    }

    /// A handoff payload claiming more sessions than the budget is truncated
    /// rather than admitted: the predecessor enforced the same limit, so an
    /// over-budget payload is corrupt or hostile and must not be allowed to
    /// start the successor already above its cap.
    #[tokio::test]
    async fn over_cap_handoff_payload_is_truncated_at_the_cap() {
        let (state, masters, _slaves) = handoff_state(4);
        let manager = SessionManager::restore_within_cap(&state, masters, 100, 2).unwrap();

        assert_eq!(manager.pending_session_ids().await.len(), 2);
        assert!(manager.reserve_session_slot().is_err(), "truncated restore must fill the cap");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use scribe_common::ids::SessionId;

    use super::{
        AiLaunchSpec, AiProvider, AiResumeMode, EnvLaunchContext, PtyOptionsBuild,
        build_launch_shell, build_pty_options, build_shell, codex_kitty_window_id,
        path_with_macos_baseline,
    };
    use crate::shell_integration::ShellKind;

    const MACOS_BASELINE_PATH: &str =
        "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

    #[test]
    fn macos_path_baseline_covers_empty_and_unset_path() {
        assert_eq!(path_with_macos_baseline(None), MACOS_BASELINE_PATH);
        assert_eq!(path_with_macos_baseline(Some("")), MACOS_BASELINE_PATH);
    }

    #[test]
    fn macos_path_baseline_prepends_homebrew_ahead_of_existing_entries() {
        assert_eq!(
            path_with_macos_baseline(Some("/usr/bin:/bin:/usr/sbin:/sbin")),
            MACOS_BASELINE_PATH
        );
        assert_eq!(
            path_with_macos_baseline(Some("/custom/bin:/usr/bin:/bin")),
            "/opt/homebrew/bin:/usr/local/bin:/custom/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
    }

    #[test]
    fn macos_path_baseline_never_moves_entries_already_present() {
        assert_eq!(
            path_with_macos_baseline(Some("/usr/bin:/opt/homebrew/bin:/bin")),
            "/usr/local/bin:/usr/bin:/opt/homebrew/bin:/bin:/usr/sbin:/sbin"
        );
    }

    #[test]
    fn macos_path_baseline_drops_empty_entries() {
        let path = path_with_macos_baseline(Some("/usr/bin::"));
        assert_eq!(path, MACOS_BASELINE_PATH);
        assert!(path.split(':').all(|entry| !entry.is_empty()));
    }

    #[test]
    fn macos_path_baseline_is_idempotent() {
        let once = path_with_macos_baseline(Some("/custom/bin:/usr/bin"));
        assert_eq!(path_with_macos_baseline(Some(&once)), once);
        assert_eq!(path_with_macos_baseline(Some(MACOS_BASELINE_PATH)), MACOS_BASELINE_PATH);
    }

    fn pty_env(kitty_window_id: bool) -> std::collections::HashMap<String, String> {
        build_pty_options(PtyOptionsBuild {
            session_id: SessionId::new(),
            shell: None,
            cwd: None,
            shell_kind: ShellKind::Bash,
            env: EnvLaunchContext {
                restore_file: None,
                persistence_enabled: false,
                integration_enabled: false,
            },
            kitty_window_id,
        })
        .env
    }

    // @lat: [[terminal-images#Terminal Images#Kitty Environment Marker]]
    #[test]
    fn kitty_window_id_env_is_limited_to_image_enabled_codex_tabs() {
        let codex = AiLaunchSpec {
            provider: AiProvider::CodexCode,
            resume_mode: AiResumeMode::New,
            conversation_id: None,
        };
        let claude = AiLaunchSpec { provider: AiProvider::ClaudeCode, ..codex.clone() };

        assert!(codex_kitty_window_id(Some(&codex), true));
        assert!(!codex_kitty_window_id(Some(&codex), false));
        assert!(!codex_kitty_window_id(Some(&claude), true));
        assert!(!codex_kitty_window_id(None, true));
        assert_eq!(pty_env(true).get("KITTY_WINDOW_ID").map(String::as_str), Some("1"));
        assert_eq!(pty_env(false).get("KITTY_WINDOW_ID"), None);
    }

    #[test]
    fn build_shell_uses_explicit_resolved_shell_for_zsh_defaults() {
        let shell = build_shell("/bin/zsh", None, ShellKind::Zsh, None);

        assert_eq!(shell, Some((String::from("/bin/zsh"), Vec::new())));
    }

    fn ai_argv(
        kind: ShellKind,
        integration_script: Option<&str>,
        launch: &AiLaunchSpec,
    ) -> Vec<String> {
        build_launch_shell("/bin/shell", None, kind, integration_script, Some(launch))
            .expect("argv")
            .1
    }

    // @lat: [[server#Server#Sessions#Session Creation#AI tabs are plain tabs that exec]]
    #[test]
    fn ai_argv_is_the_plain_tab_argv_plus_an_interactive_exec() {
        let new = AiLaunchSpec {
            provider: AiProvider::ClaudeCode,
            resume_mode: AiResumeMode::New,
            conversation_id: None,
        };
        let resume = AiLaunchSpec { resume_mode: AiResumeMode::Resume, ..new.clone() };
        let targeted =
            AiLaunchSpec { conversation_id: Some(String::from("it's mine")), ..resume.clone() };

        // Bash keeps the plain `--rcfile` attachment ahead of the command, so
        // the AI tab reads exactly the startup files a plain tab reads.
        assert_eq!(
            ai_argv(ShellKind::Bash, Some("/s/scribe.bash"), &new),
            ["--rcfile", "/s/scribe.bash", "-ic", "exec claude"]
        );
        // Env-hook shells carry no startup argv at all, plain or AI, but zsh
        // and fish prepend the restore-delta consumer their prompt never runs.
        let zsh = ai_argv(ShellKind::Zsh, None, &resume);
        assert_eq!(zsh[0], "-ic");
        assert!(zsh[1].contains("SCRIBE_RESTORE_ENV_DELTA_FILE"), "{}", zsh[1]);
        assert!(zsh[1].ends_with("exec claude --resume"), "{}", zsh[1]);
        // A conversation id is quoted in the language the shell speaks.
        let fish = ai_argv(ShellKind::Fish, None, &targeted);
        assert!(fish[1].ends_with("exec claude --resume 'it\\'s mine'"), "{}", fish[1]);
        // Nushell rejects the grouped short form.
        assert_eq!(ai_argv(ShellKind::Nushell, None, &new), ["-i", "-c", "exec claude"]);
        // PowerShell speaks neither `-i` nor `exec`, and its `-File` attachment
        // has to be last, so an AI launch drops the script for `-Command`.
        assert_eq!(
            ai_argv(ShellKind::PowerShell, Some("/s/scribe.ps1"), &new),
            ["-NoLogo", "-Command", "claude"]
        );
        // Without AI intent the very same call is the untouched plain-tab argv.
        assert_eq!(
            build_launch_shell(
                "/bin/shell",
                None,
                ShellKind::PowerShell,
                Some("/s/scribe.ps1"),
                None
            ),
            Some((
                String::from("/bin/shell"),
                vec![
                    String::from("-NoLogo"),
                    String::from("-NoExit"),
                    String::from("-File"),
                    String::from("/s/scribe.ps1"),
                ]
            ))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bash_integration_sources_bash_profile_for_non_login_shells_on_macos() {
        let home = make_temp_home("bash-startup-profile");
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
        let home = make_temp_home("bash-startup-bashrc");
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

    // The zsh integration's Darwin login-profile emulation gates on
    // `uname -s`, so these tests shim `uname` on PATH and run the real
    // script through `/bin/zsh`, making the Darwin branch testable on any
    // Unix host.

    #[test]
    fn zsh_integration_sources_zprofile_for_non_login_shells_on_darwin() {
        let Some(output) = run_zsh_integration_check("darwin", "Darwin", "") else { return };
        assert!(
            output.contains("ZPROFILE=1 GUARD=1"),
            "expected ~/.zprofile to be sourced on Darwin, got output: {output}"
        );
    }

    #[test]
    fn zsh_integration_login_profile_guard_prevents_double_sourcing() {
        let Some(output) =
            run_zsh_integration_check("guard", "Darwin", "_SCRIBE_LOGIN_PROFILE_SOURCED=1; ")
        else {
            return;
        };
        assert!(
            output.contains("ZPROFILE=0 GUARD=1"),
            "expected guard to skip ~/.zprofile, got output: {output}"
        );
    }

    #[test]
    fn zsh_integration_skips_login_profile_off_darwin() {
        let Some(output) = run_zsh_integration_check("linux", "Linux", "") else { return };
        assert!(
            output.contains("ZPROFILE=0 GUARD=0"),
            "expected no login-profile emulation off Darwin, got output: {output}"
        );
    }

    /// Source the shipped zsh integration in a non-login `/bin/zsh` with an
    /// isolated HOME containing a marker `~/.zprofile`, and report whether
    /// the marker and the double-source guard are set afterwards. Returns
    /// `None` (skipping the test) when `/bin/zsh` is not installed.
    fn run_zsh_integration_check(name: &str, uname_reports: &str, prelude: &str) -> Option<String> {
        if !Path::new("/bin/zsh").exists() {
            return None;
        }
        let home = make_temp_home(&format!("zsh-startup-{name}"));
        fs::write(home.join(".zprofile"), "export ZPROFILE_SEEN=1\n").expect("write .zprofile");

        let shim_dir = home.join("shim-bin");
        fs::create_dir_all(&shim_dir).expect("create uname shim dir");
        let shim = shim_dir.join("uname");
        fs::write(&shim, format!("#!/bin/sh\necho {uname_reports}\n")).expect("write uname shim");
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))
                .expect("make uname shim executable");
        }

        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../dist/shell-integration/zsh/scribe.zsh")
            .canonicalize()
            .expect("canonicalize zsh integration script path");
        let output = Command::new("/bin/zsh")
            .arg("-c")
            .arg(format!(
                "{prelude}source '{}'; printf 'ZPROFILE=%s GUARD=%s\\n' \
                 \"${{ZPROFILE_SEEN:-0}}\" \"${{_SCRIBE_LOGIN_PROFILE_SOURCED:-0}}\"",
                script.display()
            ))
            .env_clear()
            .env("HOME", &home)
            .env("PATH", format!("{}:/usr/bin:/bin", shim_dir.display()))
            .env("TERM_PROGRAM", "Scribe")
            .env("SCRIBE_ENV_PERSIST", "0")
            .output()
            .expect("run zsh integration check");
        cleanup_temp_home(&home);
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn make_temp_home(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("scribe-{name}-{nonce}"));
        fs::create_dir_all(&dir).expect("create temp home");
        dir
    }

    fn cleanup_temp_home(home: &Path) {
        let _ignore = fs::remove_dir_all(home);
    }
}

// ---------------------------------------------------------------------------
// Cold-restart env restore-apply (see specs/006-persist-terminal-env/
// contracts/hook-event-additions.md and research.md::R1.3 / R3.5).
//
// Plain-shell integration consumes `SCRIBE_RESTORE_ENV_DELTA_FILE` after user
// startup: at the integration tail for bash/nushell/PowerShell, or from the
// first prompt event for zsh/fish, whose integration loads before user rc
// files. Structured AI launches instead consume it after login from either
// the bash AI-mode integration script or the zsh/fish server preamble. Every
// supported path applies then unlinks it; unsupported shell kinds never stage
// it. This step is intentionally skipped for handoff-restored sessions: per
// R3.5, handoff preserves the PTY's process so env stays intact.
// ---------------------------------------------------------------------------

/// Read `terminal.env_persistence.enabled` once for a spawn.
///
/// Fails safe to `false`, matching the `EnvChanged` ingress gate: a config
/// we cannot read means every env event is dropped server-side, so neither
/// the restore-apply nor the shells' snapshot machinery should run. Reading
/// from disk here is fine — this is session creation, not a hot path.
fn env_persistence_enabled() -> bool {
    match scribe_common::config::load_config() {
        Ok(cfg) => cfg.terminal.env_persistence.enabled,
        Err(e) => {
            tracing::warn!(
                target: "scribe_server::session_manager",
                error = ?e,
                "load_config failed during spawn; env persistence gated off for this shell"
            );
            false
        }
    }
}

/// Decrypt the per-session env envelope, write a shell-source-compatible
/// temp file, and return the absolute path. The launch's supported post-startup
/// consumer — shell integration for plain sessions and AI bash, or the server
/// preamble for AI zsh/fish — applies the delta and unlinks the file.
///
/// The caller feature-gates the call; this helper returns `None` (via early
/// returns) when:
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
    kind: ShellKind,
) -> Option<std::path::PathBuf> {
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
    let extension = restore_env_file_extension(kind);
    let file_name = format!("{session_id}-{pid}.{extension}");
    let path = runtime_dir.join(file_name);
    let body = render_restore_env_source(kind, &delta);

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
        tokio::time::sleep(std::time::Duration::from_mins(1)).await;
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

const RESTORE_HEADER: &str = "# Scribe env restore — applied after shell startup, then unlinked.\n";

/// File-name extension for the staged restore file.
///
/// PowerShell is the reason this varies at all: `.` on a path whose
/// extension is not `.ps1` is resolved as a native command instead of a
/// script, which for a non-executable POSIX file is a silent no-op rather
/// than an error. Fish and Nushell are given honest extensions for the
/// same reason a `.sh` would be misleading — the bodies are not POSIX.
fn restore_env_file_extension(kind: ShellKind) -> &'static str {
    match kind {
        ShellKind::Fish => "fish",
        ShellKind::Nushell => "json",
        ShellKind::PowerShell => "ps1",
        ShellKind::Bash | ShellKind::Zsh | ShellKind::Unknown => "sh",
    }
}

/// Render a `TerminalEnvDelta` in the syntax the target shell actually
/// speaks — see `specs/006-persist-terminal-env/contracts/
/// hook-event-additions.md`.
///
/// Fish has no `export`/`unset`, PowerShell has neither plus a different
/// quoting rule, and Nushell cannot `source` a runtime-computed path at
/// all, so it gets JSON that its integration script parses and feeds to
/// `load-env`/`hide-env`.
fn render_restore_env_source(
    kind: ShellKind,
    delta: &crate::env_store::delta::TerminalEnvDelta,
) -> String {
    match kind {
        ShellKind::Fish => render_fish_restore(delta),
        ShellKind::Nushell => render_nushell_restore(delta),
        ShellKind::PowerShell => render_powershell_restore(delta),
        ShellKind::Bash | ShellKind::Zsh | ShellKind::Unknown => render_posix_restore(delta),
    }
}

/// Reject names that are not `[A-Za-z_][A-Za-z0-9_]*`.
///
/// The delta is built from whatever the shell reported as exported, which
/// on bash includes exported-function entries such as `BASH_FUNC_foo%%`.
/// No shell can assign those by name, and interpolating them into a file
/// the shell then sources would turn a variable name into executable
/// syntax, so they are dropped instead of rendered.
fn is_assignable_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// POSIX `export`/`unset` for bash and zsh. Inside a single-quoted string
/// single quotes are escaped by closing the quote, inserting a
/// backslash-quoted single quote, and reopening — the canonical `'\''`
/// idiom. Newlines, tabs, spaces, slashes, and `$` are literal there.
fn render_posix_restore(delta: &crate::env_store::delta::TerminalEnvDelta) -> String {
    use std::fmt::Write as _;
    let mut out = String::from(RESTORE_HEADER);
    for (name, value) in delta.added.iter().filter(|(name, _)| is_assignable_env_name(name)) {
        let escaped = value.replace('\'', "'\\''");
        _ = writeln!(out, "export {name}='{escaped}'");
    }
    for name in delta.removed.iter().filter(|name| is_assignable_env_name(name)) {
        _ = writeln!(out, "unset {name}");
    }
    out
}

/// Fish `set -gx` / `set -e`. Fish single quotes recognise exactly two
/// escapes, `\\` and `\'`; everything else — newlines included — is
/// literal, so backslash must be doubled before quotes are escaped.
fn render_fish_restore(delta: &crate::env_store::delta::TerminalEnvDelta) -> String {
    use std::fmt::Write as _;
    let mut out = String::from(RESTORE_HEADER);
    for (name, value) in delta.added.iter().filter(|(name, _)| is_assignable_env_name(name)) {
        let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
        _ = writeln!(out, "set -gx {name} '{escaped}'");
    }
    for name in delta.removed.iter().filter(|name| is_assignable_env_name(name)) {
        _ = writeln!(out, "set -e {name}");
    }
    out
}

/// PowerShell env-drive assignment. A single-quoted PowerShell string is
/// verbatim apart from `'`, which doubles; the `${env:NAME}` form is used
/// over `$env:NAME` so a name is never re-parsed as an expression.
fn render_powershell_restore(delta: &crate::env_store::delta::TerminalEnvDelta) -> String {
    use std::fmt::Write as _;
    let mut out = String::from(RESTORE_HEADER);
    for (name, value) in delta.added.iter().filter(|(name, _)| is_assignable_env_name(name)) {
        let escaped = value.replace('\'', "''");
        _ = writeln!(out, "${{env:{name}}} = '{escaped}'");
    }
    for name in delta.removed.iter().filter(|name| is_assignable_env_name(name)) {
        _ = writeln!(out, "Remove-Item -LiteralPath 'env:{name}' -ErrorAction SilentlyContinue");
    }
    out
}

/// Nushell reads the delta as JSON rather than as script.
///
/// `source` in nushell resolves at parse time and refuses a runtime path,
/// so the integration script cannot dot-source anything; it used to
/// hand-parse the POSIX file instead, which lost `'\''` sequences and any
/// value spanning more than one line. JSON removes the parser entirely.
fn render_nushell_restore(delta: &crate::env_store::delta::TerminalEnvDelta) -> String {
    let filtered = crate::env_store::delta::TerminalEnvDelta {
        added: delta
            .added
            .iter()
            .filter(|(name, _)| is_assignable_env_name(name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        removed: delta
            .removed
            .iter()
            .filter(|name| is_assignable_env_name(name))
            .cloned()
            .collect(),
    };
    // Serializing a `BTreeMap<String, String>` + `BTreeSet<String>` cannot
    // fail; an empty object still parses to an empty delta on the nu side.
    serde_json::to_string(&filtered).unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
mod tests_apply {
    use std::collections::{BTreeMap, BTreeSet};

    use crate::env_store::delta::TerminalEnvDelta;
    use crate::shell_integration::ShellKind;

    use super::{render_restore_env_source, restore_env_file_extension};

    fn sample_delta() -> TerminalEnvDelta {
        let mut added = BTreeMap::new();
        added.insert("FOO".to_owned(), "bar".to_owned());
        added.insert("PATH".to_owned(), "/a:/b".to_owned());
        added.insert("WITH_BACKSLASH".to_owned(), r"C:\tmp\x".to_owned());
        added.insert("WITH_MULTILINE".to_owned(), "one\ntwo".to_owned());
        added.insert("WITH_QUOTE".to_owned(), "it's value".to_owned());
        added.insert("WITH_SPACES".to_owned(), "hello world".to_owned());
        let mut removed = BTreeSet::new();
        removed.insert("STALE".to_owned());
        TerminalEnvDelta { added, removed }
    }

    #[test]
    fn posix_restore_quotes_values_correctly() {
        let s = render_restore_env_source(ShellKind::Bash, &sample_delta());
        assert!(s.contains("export FOO='bar'"), "{s}");
        assert!(s.contains("export PATH='/a:/b'"), "{s}");
        assert!(s.contains("export WITH_QUOTE='it'\\''s value'"), "{s}");
        assert!(s.contains("export WITH_SPACES='hello world'"), "{s}");
        assert!(s.contains("export WITH_MULTILINE='one\ntwo'"), "{s}");
        assert!(s.contains(r"export WITH_BACKSLASH='C:\tmp\x'"), "{s}");
        assert!(s.contains("unset STALE"), "{s}");
        assert_eq!(restore_env_file_extension(ShellKind::Zsh), "sh");
    }

    #[test]
    fn fish_restore_uses_set_and_escapes_backslashes() {
        let s = render_restore_env_source(ShellKind::Fish, &sample_delta());
        assert!(s.contains("set -gx FOO 'bar'"), "{s}");
        assert!(s.contains("set -gx WITH_QUOTE 'it\\'s value'"), "{s}");
        assert!(s.contains("set -gx WITH_MULTILINE 'one\ntwo'"), "{s}");
        assert!(s.contains(r"set -gx WITH_BACKSLASH 'C:\\tmp\\x'"), "{s}");
        assert!(s.contains("set -e STALE"), "{s}");
        assert!(!s.contains("export "), "fish has no export builtin: {s}");
        assert_eq!(restore_env_file_extension(ShellKind::Fish), "fish");
    }

    #[test]
    fn powershell_restore_uses_env_drive_and_doubles_quotes() {
        let s = render_restore_env_source(ShellKind::PowerShell, &sample_delta());
        assert!(s.contains("${env:FOO} = 'bar'"), "{s}");
        assert!(s.contains("${env:WITH_QUOTE} = 'it''s value'"), "{s}");
        assert!(s.contains("${env:WITH_MULTILINE} = 'one\ntwo'"), "{s}");
        assert!(s.contains(r"${env:WITH_BACKSLASH} = 'C:\tmp\x'"), "{s}");
        assert!(
            s.contains("Remove-Item -LiteralPath 'env:STALE' -ErrorAction SilentlyContinue"),
            "{s}"
        );
        assert_eq!(restore_env_file_extension(ShellKind::PowerShell), "ps1");
    }

    #[test]
    fn nushell_restore_is_json() {
        let s = render_restore_env_source(ShellKind::Nushell, &sample_delta());
        let parsed: TerminalEnvDelta = serde_json::from_str(&s).expect("nu payload is JSON");
        assert_eq!(parsed, sample_delta());
        assert_eq!(restore_env_file_extension(ShellKind::Nushell), "json");
    }

    /// `compgen -e` reports bash's exported functions as `BASH_FUNC_x%%`,
    /// whose value is a function body. Rendering that as an assignment
    /// would splice shell syntax into a file the shell then sources.
    #[test]
    fn unassignable_names_are_dropped_from_every_dialect() {
        let mut added = BTreeMap::new();
        added.insert("BASH_FUNC_evil%%".to_owned(), "() { :; }".to_owned());
        added.insert("KEEP".to_owned(), "ok".to_owned());
        let mut removed = BTreeSet::new();
        removed.insert("BASH_FUNC_gone%%".to_owned());
        let delta = TerminalEnvDelta { added, removed };
        for kind in [ShellKind::Bash, ShellKind::Fish, ShellKind::PowerShell, ShellKind::Nushell] {
            let s = render_restore_env_source(kind, &delta);
            assert!(!s.contains("BASH_FUNC"), "{kind:?} rendered a function export: {s}");
            assert!(s.contains("KEEP"), "{kind:?} dropped a legitimate name: {s}");
        }
    }
}

/// Round-trips a rendered restore file through the real interpreter of
/// every supported shell. String assertions alone cannot catch this
/// finding class: pwsh dot-sourcing a POSIX `.sh` file raised no error
/// and applied nothing, and fish silently ignored `export`.
#[cfg(test)]
mod tests_apply_shells {
    use std::collections::{BTreeMap, BTreeSet};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::env_store::delta::TerminalEnvDelta;
    use crate::shell_integration::ShellKind;
    use crate::shell_integration::desktop_isolation::{scrub_desktop_env, seal_child};

    use super::{render_restore_env_source, restore_env_file_extension};

    const QUOTE_VALUE: &str = "it's \"value\"";
    const MULTI_VALUE: &str = "one\ntwo";
    const BACKSLASH_VALUE: &str = r"C:\tmp\x";
    const UNSET_MARKER: &str = "!unset";

    fn probe_delta() -> TerminalEnvDelta {
        let mut added = BTreeMap::new();
        added.insert("SCRIBE_PROBE_QUOTE".to_owned(), QUOTE_VALUE.to_owned());
        added.insert("SCRIBE_PROBE_MULTI".to_owned(), MULTI_VALUE.to_owned());
        added.insert("SCRIBE_PROBE_BS".to_owned(), BACKSLASH_VALUE.to_owned());
        let mut removed = BTreeSet::new();
        removed.insert("SCRIBE_PROBE_STALE".to_owned());
        TerminalEnvDelta { added, removed }
    }

    fn interpreter_available(binary: &str) -> bool {
        let mut command = Command::new(binary);
        command.arg("--version").stdout(Stdio::null()).stderr(Stdio::null());
        scrub_desktop_env(&mut command);
        command.status().is_ok_and(|status| status.success())
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("scribe-restore-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// A child process inherits exported variables only, so reading the
    /// probes back out of one proves the restore file exported rather
    /// than merely assigned them.
    fn write_recorder(dir: &Path) -> PathBuf {
        let recorder = dir.join("recorder.sh");
        std::fs::write(
            &recorder,
            "#!/bin/sh\nprintf '%s\\0%s\\0%s\\0%s\\0' \
             \"${SCRIBE_PROBE_QUOTE-!unset}\" \"${SCRIBE_PROBE_MULTI-!unset}\" \
             \"${SCRIBE_PROBE_BS-!unset}\" \"${SCRIBE_PROBE_STALE-!unset}\" > \"$1\"\n",
        )
        .expect("write recorder");
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755))
            .expect("chmod recorder");
        recorder
    }

    fn stage_restore_file(dir: &Path, kind: ShellKind, extension: &str) -> PathBuf {
        let path = dir.join(format!("restore.{extension}"));
        std::fs::write(&path, render_restore_env_source(kind, &probe_delta()))
            .expect("write restore file");
        path
    }

    /// Runs `driver` under `binary` and returns the four probe values the
    /// recorder observed, in `[quote, multi, backslash, stale]` order.
    /// The driver keeps a shell-appropriate extension because `pwsh -File`
    /// refuses anything but `.ps1`.
    fn run_driver(
        binary: &str,
        args: &[&str],
        dir: &Path,
        driver_name: &str,
        driver: &str,
    ) -> [String; 4] {
        let driver_path = dir.join(driver_name);
        std::fs::write(&driver_path, driver).expect("write driver");
        let out = dir.join("record.bin");
        _ = std::fs::remove_file(&out);

        let mut command = Command::new(binary);
        command
            .args(args)
            .arg(&driver_path)
            .env_remove("SCRIBE_PROBE_QUOTE")
            .env_remove("SCRIBE_PROBE_MULTI")
            .env_remove("SCRIBE_PROBE_BS")
            .env_remove("SCRIBE_PROBE_STALE")
            .env("SCRIBE_RECORD_PATH", &out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        seal_child(&mut command, dir);
        let result = command.output().expect("run driver");
        assert!(
            result.status.success(),
            "{binary} driver exited with {}: {}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        );

        let raw = std::fs::read(&out).unwrap_or_default();
        let fields: Vec<String> = raw
            .split(|byte| *byte == 0)
            .map(|field| String::from_utf8_lossy(field).into_owned())
            .collect();
        let at = |i: usize| fields.get(i).cloned().unwrap_or_default();
        [at(0), at(1), at(2), at(3)]
    }

    fn assert_probes_restored(shell: &str, probes: &[String; 4]) {
        assert_eq!(probes[0], QUOTE_VALUE, "{shell} lost the quote-bearing value");
        assert_eq!(probes[1], MULTI_VALUE, "{shell} lost the multi-line value");
        assert_eq!(probes[2], BACKSLASH_VALUE, "{shell} lost the backslash value");
        assert_eq!(probes[3], UNSET_MARKER, "{shell} failed to erase the removed variable");
    }

    fn run_posix_case(shell: &str, kind: ShellKind, args: &[&str]) {
        if !interpreter_available(shell) {
            return;
        }
        let dir = scratch_dir(shell);
        let recorder = write_recorder(&dir);
        let restore = stage_restore_file(&dir, kind, restore_env_file_extension(kind));
        let driver = format!(
            "export SCRIBE_PROBE_STALE=preexisting\n. '{}'\n'{}' \"$SCRIBE_RECORD_PATH\"\n",
            restore.display(),
            recorder.display(),
        );
        let probes = run_driver(shell, args, &dir, "driver.sh", &driver);
        std::fs::remove_dir_all(&dir).expect("clean scratch dir");
        assert_probes_restored(shell, &probes);
    }

    #[test]
    fn bash_applies_rendered_restore_file() {
        run_posix_case("bash", ShellKind::Bash, &["--norc", "--noprofile"]);
    }

    #[test]
    fn zsh_applies_rendered_restore_file() {
        run_posix_case("zsh", ShellKind::Zsh, &["--no-rcs"]);
    }

    #[test]
    fn fish_applies_rendered_restore_file() {
        if !interpreter_available("fish") {
            return;
        }
        let dir = scratch_dir("fish");
        let recorder = write_recorder(&dir);
        let restore = stage_restore_file(&dir, ShellKind::Fish, "fish");
        let driver = format!(
            "set -gx SCRIBE_PROBE_STALE preexisting\nbuiltin source '{}'\n'{}' \
             \"$SCRIBE_RECORD_PATH\"\n",
            restore.display(),
            recorder.display(),
        );
        let probes = run_driver("fish", &["--no-config"], &dir, "driver.fish", &driver);
        std::fs::remove_dir_all(&dir).expect("clean scratch dir");
        assert_probes_restored("fish", &probes);
    }

    #[test]
    fn powershell_applies_rendered_restore_file_only_with_a_ps1_extension() {
        if !interpreter_available("pwsh") {
            return;
        }
        let dir = scratch_dir("pwsh");
        let recorder = write_recorder(&dir);
        let restore = stage_restore_file(&dir, ShellKind::PowerShell, "ps1");
        let driver = format!(
            "$env:SCRIBE_PROBE_STALE = 'preexisting'\n. '{}'\n& '{}' $env:SCRIBE_RECORD_PATH\n",
            restore.display(),
            recorder.display(),
        );
        let probes = run_driver("pwsh", &["-NoProfile", "-File"], &dir, "driver.ps1", &driver);
        assert_probes_restored("pwsh", &probes);

        // Same body, wrong extension: pwsh resolves the dot-source target
        // as a native command instead of a script and applies nothing,
        // without raising so much as a warning. Resolving it that way also
        // reaches .NET's shell-execute fallback, so this is the case that
        // needs `seal_child` to keep a desktop opener out of the loop.
        let misnamed = stage_restore_file(&dir, ShellKind::PowerShell, "sh");
        let misnamed_driver = format!(
            "$env:SCRIBE_PROBE_STALE = 'preexisting'\ntry {{ . '{}' }} catch {{ }}\n& '{}' \
             $env:SCRIBE_RECORD_PATH\n",
            misnamed.display(),
            recorder.display(),
        );
        let misnamed_probes =
            run_driver("pwsh", &["-NoProfile", "-File"], &dir, "driver.ps1", &misnamed_driver);
        std::fs::remove_dir_all(&dir).expect("clean scratch dir");
        assert_eq!(
            misnamed_probes[0], UNSET_MARKER,
            "pwsh unexpectedly dot-sourced a non-.ps1 file; the per-shell extension is moot"
        );
    }

    /// Drives the shipped `scribe.nu` applier, not a reimplementation of
    /// it: nushell cannot `source` a runtime path, so the JSON payload
    /// and its parser are a matched pair that has to be tested together.
    #[test]
    fn nushell_applies_rendered_restore_file() {
        if !interpreter_available("nu") {
            return;
        }
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../dist/shell-integration/nushell/vendor/autoload/scribe.nu");
        let dir = scratch_dir("nu");
        let recorder = write_recorder(&dir);
        let restore = stage_restore_file(&dir, ShellKind::Nushell, "json");
        let driver = format!(
            "source '{}'\n$env.SCRIBE_PROBE_STALE = 'preexisting'\n__scribe-apply-restore '{}'\n\
             run-external '{}' $env.SCRIBE_RECORD_PATH\n",
            script.display(),
            restore.display(),
            recorder.display(),
        );
        let probes = run_driver("nu", &["--no-config-file"], &dir, "driver.nu", &driver);
        std::fs::remove_dir_all(&dir).expect("clean scratch dir");
        assert_probes_restored("nu", &probes);
    }
}
