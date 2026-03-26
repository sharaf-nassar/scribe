use std::collections::HashMap;
use std::os::fd::OwnedFd;
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

use scribe_common::ai_state::AiProcessState;
use scribe_common::error::ScribeError;
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::screen::{
    CellFlags as ScreenCellFlags, CursorStyle as ScreenCursorStyle, ScreenCell, ScreenColor,
    ScreenSnapshot,
};
use scribe_pty::async_fd::AsyncPtyFd;
use scribe_pty::event_listener::ScribeEventListener;
use scribe_pty::metadata::{MetadataEvent, MetadataParser};

use crate::handoff::HandoffState;

/// Maximum number of active PTY sessions across all clients.
const MAX_SESSIONS: usize = 256;

/// Default terminal columns.
const DEFAULT_COLS: u16 = 80;

/// Default terminal rows.
const DEFAULT_ROWS: u16 = 24;

/// A managed PTY session with terminal emulator state.
///
/// Fields are `pub` for crate-internal access (the module itself is private).
pub struct ManagedSession {
    pub pty_fd: AsyncPtyFd,
    pub child_pid: u32,
    pub term: Arc<Mutex<Term<ScribeEventListener>>>,
    /// ANSI processor for feeding bytes into `Term<ScribeEventListener>`.
    /// Uses `vte::ansi::Processor` which calls `Handler` methods on Term.
    pub ansi_processor: AnsiProcessor,
    /// VTE parser for the OSC interceptor (calls `Perform` on `OscInterceptor`).
    pub osc_parser: VteParser,
    pub metadata_parser: MetadataParser,
    pub metadata_rx: mpsc::UnboundedReceiver<MetadataEvent>,
    pub workspace_id: WorkspaceId,
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
    /// CWD from handoff, used to restore working directory. `None` for fresh sessions.
    pub cwd: Option<std::path::PathBuf>,
    /// AI state from handoff. `None` for fresh sessions.
    pub ai_state: Option<AiProcessState>,
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

/// Manages all active PTY sessions.
pub struct SessionManager {
    sessions: Arc<tokio::sync::RwLock<HashMap<SessionId, ManagedSession>>>,
    /// Scrollback lines used when creating new sessions.
    scrollback_lines: AtomicUsize,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::with_scrollback(10_000)
    }
}

impl SessionManager {
    /// Create a new `SessionManager` with a default scrollback of 10 000 lines.
    #[must_use]
    #[allow(
        dead_code,
        reason = "public constructor retained for API symmetry with with_scrollback"
    )]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new `SessionManager` with a specific scrollback line count.
    #[must_use]
    pub fn with_scrollback(scrollback_lines: usize) -> Self {
        Self {
            sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            scrollback_lines: AtomicUsize::new(scrollback_lines),
        }
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
        workspace_id: WorkspaceId,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<SessionId, ScribeError> {
        let scrollback_lines = self.scrollback_lines.load(Ordering::Relaxed);
        {
            let sessions = self.sessions.read().await;
            if sessions.len() >= MAX_SESSIONS {
                return Err(ScribeError::IpcError {
                    reason: "global session limit reached".to_owned(),
                });
            }
        }

        let session_id = SessionId::new();

        // 1. Create metadata event channel.
        let (event_tx, metadata_rx) = mpsc::unbounded_channel();

        // 2. Create event listener for `alacritty_terminal` events.
        let event_listener = ScribeEventListener::new(session_id, event_tx);

        // 3. Create Term config with scrollback.
        let term_config =
            TermConfig { scrolling_history: scrollback_lines, ..TermConfig::default() };

        // 4. Create Term with default 80x24 dimensions.
        let dimensions =
            TermDimensions { cols: usize::from(DEFAULT_COLS), lines: usize::from(DEFAULT_ROWS) };
        let term = Term::new(term_config, &dimensions, event_listener);

        // 5. Create PTY using `alacritty_terminal::tty`.
        let window_size = WindowSize {
            num_lines: DEFAULT_ROWS,
            num_cols: DEFAULT_COLS,
            cell_width: 1,
            cell_height: 1,
        };
        let pty_options = PtyOptions {
            env: HashMap::from([
                ("TERM".to_owned(), "xterm-256color".to_owned()),
                ("COLORTERM".to_owned(), "truecolor".to_owned()),
                ("TERM_PROGRAM".to_owned(), "Scribe".to_owned()),
                ("TERM_PROGRAM_VERSION".to_owned(), env!("CARGO_PKG_VERSION").to_owned()),
            ]),
            working_directory: cwd.filter(|p| p.is_dir()).or_else(dirs::home_dir),
            ..PtyOptions::default()
        };

        let pty = alacritty_terminal::tty::new(&pty_options, window_size, 0).map_err(|e| {
            ScribeError::PtySpawnFailed { reason: format!("alacritty tty::new failed: {e}") }
        })?;

        // 6. Extract child PID and master fd.
        let child_pid = pty.child().id();
        let master_file = pty.file().try_clone().map_err(|e| ScribeError::PtySpawnFailed {
            reason: format!("failed to clone PTY master fd: {e}"),
        })?;
        let master_fd: OwnedFd = master_file.into();

        // 7. Wrap in `AsyncPtyFd` for epoll-driven I/O.
        let pty_fd = AsyncPtyFd::new(master_fd).map_err(|e| ScribeError::PtySpawnFailed {
            reason: format!("AsyncPtyFd::new failed: {e}"),
        })?;

        // 8. Create `MetadataParser` and parsers.
        let metadata_parser = MetadataParser::new(session_id);
        let ansi_processor = AnsiProcessor::new();
        let osc_parser = VteParser::new();

        info!(%session_id, %workspace_id, "created new PTY session");

        let managed = ManagedSession {
            pty_fd,
            child_pid,
            term: Arc::new(Mutex::new(term)),
            ansi_processor,
            osc_parser,
            metadata_parser,
            metadata_rx,
            workspace_id,
            pty: Some(pty),
            handoff_snapshot: None,
            title: None,
            cwd: None,
            ai_state: None,
        };

        self.sessions.write().await.insert(session_id, managed);
        Ok(session_id)
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
        let mut sessions_map = HashMap::new();

        for (handoff_session, owned_fd) in state.sessions.iter().zip(fds) {
            let cols = handoff_session.cols;
            let rows = handoff_session.rows;

            // Create metadata event channel.
            let (event_tx, metadata_rx) = mpsc::unbounded_channel();

            // Create event listener.
            let event_listener = ScribeEventListener::new(handoff_session.session_id, event_tx);

            // Create Term config with scrollback.
            let term_config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };

            // Create Term with the session's dimensions.
            let dimensions = TermDimensions { cols: usize::from(cols), lines: usize::from(rows) };
            let term = Term::new(term_config, &dimensions, event_listener);

            // Wrap the received fd for async I/O.
            let pty_fd = AsyncPtyFd::new(owned_fd).map_err(|e| ScribeError::PtySpawnFailed {
                reason: format!(
                    "AsyncPtyFd::new failed during restore for {}: {e}",
                    handoff_session.session_id
                ),
            })?;

            // Create parsers.
            let metadata_parser = MetadataParser::new(handoff_session.session_id);
            let ansi_processor = AnsiProcessor::new();
            let osc_parser = VteParser::new();

            info!(
                session_id = %handoff_session.session_id,
                workspace_id = %handoff_session.workspace_id,
                child_pid = handoff_session.child_pid,
                cols,
                rows,
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
                child_pid: handoff_session.child_pid,
                term: Arc::new(Mutex::new(term)),
                ansi_processor,
                osc_parser,
                metadata_parser,
                metadata_rx,
                workspace_id: handoff_session.workspace_id,
                pty: None,
                handoff_snapshot: handoff_session.snapshot.clone(),
                title: handoff_session.title.clone(),
                cwd: handoff_session.cwd.clone(),
                ai_state: handoff_session.ai_state.clone(),
            };

            sessions_map.insert(handoff_session.session_id, managed);
        }

        Ok(Self {
            sessions: Arc::new(tokio::sync::RwLock::new(sessions_map)),
            scrollback_lines: AtomicUsize::new(scrollback),
        })
    }
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
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap,
            reason = "terminal rows are always within i32 range (max ~65535)"
        )]
        let line = Line(line_idx as i32);
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
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_possible_wrap,
                reason = "scrollback index bounded by history_size (≤ 100_000), fits in i32"
            )]
            let line = Line(-(i as i32));
            let row = &grid[line];
            for col_idx in 0..cols {
                let cell = &row[Column(col_idx)];
                scrollback.push(convert_cell(cell));
            }
        }

        (scrollback, history)
    };

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "terminal dimensions and cursor position are always within u16 range; \
                  history bounded by scrollback_lines (≤ 100_000) fits u32"
    )]
    ScreenSnapshot {
        cells,
        cols: cols as u16,
        rows: rows as u16,
        cursor_col: cursor_point.column.0 as u16,
        cursor_row: cursor_point.line.0.max(0) as u16,
        cursor_style: convert_cursor_style(cursor_style),
        cursor_visible,
        alt_screen,
        scrollback,
        scrollback_rows: history as u32,
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
        alacritty_terminal::vte::ansi::Color::Named(named) =>
        {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "NamedColor max variant (DimCyan = 266) fits in u16"
            )]
            ScreenColor::Named(named as u16)
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
        bold: flags.contains(CellFlags::BOLD),
        italic: flags.contains(CellFlags::ITALIC),
        underline: flags.contains(CellFlags::UNDERLINE),
        strikethrough: flags.contains(CellFlags::STRIKEOUT),
        dim: flags.contains(CellFlags::DIM),
        inverse: flags.contains(CellFlags::INVERSE),
        hidden: flags.contains(CellFlags::HIDDEN),
        wide: flags.contains(CellFlags::WIDE_CHAR),
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
