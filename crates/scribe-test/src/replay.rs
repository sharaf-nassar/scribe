//! Local inflation and application of `SessionReplay` frames.
//!
//! The daemon used to drop `SessionReplay` on the floor, which made the whole
//! attach path invisible to e2e assertions: everything the harness could see
//! came from `RequestSnapshot`, which the server answers from its own `Term`
//! regardless of what it put on the wire. A replay that arrived out of order,
//! lost the bytes emitted between snapshot and sink install, or never arrived
//! at all looked identical to a correct one.
//!
//! This module gives the harness the receiving half a real client implements:
//! zstd-inflate the frame, feed the ANSI through `vte`'s processor into a fresh
//! `Term`, and keep that `Term` fed with the session's subsequent `PtyOutput`.
//! The result is a locally-derived screen — the *replayed view* — that can be
//! read back as a `ScreenSnapshot` and compared against the server's own view.
//! Ordering is observable alongside it: every applied frame records how many
//! live output bytes preceded it, so a script can tell replayed content from
//! content that arrived after the replay.
//!
//! The terminal core, `TermConfig`, event listener, and snapshot conversion are
//! the server's own (`scribe_server::session_manager`), so the replayed view
//! never disagrees with the server for reasons that live in the harness.

use std::io::{self, Write as _};
use std::str::FromStr as _;

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use scribe_common::ids::SessionId;
use scribe_common::screen::ScreenSnapshot;
use scribe_common::screen_replay::{SessionReplay, decompress_session_replay};
use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
use scribe_server::session_manager::{build_term_config, snapshot_term};
use tokio::sync::mpsc;
use vte::ansi::Processor as AnsiProcessor;

use crate::TestError;
use crate::cmd_socket::{DaemonRequest, DaemonResponse, send_request};

/// Scrollback rows the replayed view retains.
///
/// Matches the server's default scrollback budget closely enough that a replay
/// carrying history is applied whole; the exact figure only bounds how much of
/// the replayed history a `replay screen` can show.
const VIEW_SCROLLBACK_ROWS: usize = 10_000;

// ---------------------------------------------------------------------------
// Replayed view
// ---------------------------------------------------------------------------

/// Grid geometry handed to `Term::new` / `Term::resize`.
#[derive(Clone, Copy)]
struct ViewDims {
    cols: usize,
    rows: usize,
}

impl Dimensions for ViewDims {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// A terminal the harness rebuilds from a `SessionReplay` and then keeps fed
/// with the session's live output, mirroring what an attached client shows.
pub struct ReplayView {
    term: Term<ScribeEventListener>,
    processor: AnsiProcessor,
    dims: ViewDims,
}

impl std::fmt::Debug for ReplayView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The terminal core and its VTE processor are not `Debug`; geometry is
        // the part a daemon log line is ever interested in anyway.
        f.debug_struct("ReplayView")
            .field("cols", &self.dims.cols)
            .field("rows", &self.dims.rows)
            .finish_non_exhaustive()
    }
}

impl ReplayView {
    /// Inflate `replay` and apply it to a fresh terminal, returning the view and
    /// the inflated ANSI byte count.
    ///
    /// # Errors
    /// Returns a message when the frame declares zero dimensions (a client
    /// skips those) or its zstd payload fails to inflate.
    pub fn apply(session_id: SessionId, replay: &SessionReplay) -> Result<(Self, usize), String> {
        if replay.cols == 0 || replay.rows == 0 {
            return Err("replay reported zero dimensions".to_owned());
        }
        let ansi = decompress_session_replay(replay).map_err(|e| e.to_string())?;

        let dims = ViewDims { cols: usize::from(replay.cols), rows: usize::from(replay.rows) };
        // The event channel's receiver is dropped on purpose: the harness has no
        // PTY to answer a clipboard or colour query on, and the listener already
        // treats a closed receiver as "nobody is listening".
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<SessionEvent>();
        let listener = ScribeEventListener::new(session_id, event_tx);
        let mut view = Self {
            term: Term::new(build_term_config(VIEW_SCROLLBACK_ROWS), &dims, listener),
            processor: AnsiProcessor::new(),
            dims,
        };

        view.feed(&ansi);
        view.trim_pseudo_scrollback(replay.scrollback_rows);
        Ok((view, ansi.len()))
    }

    /// Feed live PTY bytes into the replayed view, in arrival order.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// Track a session resize so the view keeps the server's geometry.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.dims = ViewDims { cols: usize::from(cols), rows: usize::from(rows) };
        self.term.resize(self.dims);
    }

    /// Read the view back as the same `ScreenSnapshot` wire type the server
    /// produces from its own `Term`.
    #[must_use]
    pub fn snapshot(&self) -> ScreenSnapshot {
        snapshot_term(&self.term)
    }

    /// Drop the blank history the replay's own `ESC [ 2J` scrolls into a fresh
    /// grid, leaving only the rows the frame actually carried.
    ///
    /// A real client reaches the same state through the server's
    /// `TrimScrollback`; without the trim the replayed view reports scrollback
    /// the session never had, and every history-sensitive comparison drifts.
    fn trim_pseudo_scrollback(&mut self, replay_scrollback_rows: u32) {
        let kept = usize::try_from(replay_scrollback_rows)
            .unwrap_or(VIEW_SCROLLBACK_ROWS)
            .min(VIEW_SCROLLBACK_ROWS);
        let grid = self.term.grid_mut();
        grid.update_history(kept);
        grid.update_history(VIEW_SCROLLBACK_ROWS);
    }
}

// ---------------------------------------------------------------------------
// Snapshot text
// ---------------------------------------------------------------------------

/// Render a snapshot's visible grid as plain text lines, trailing blanks
/// trimmed, so shell tests can `grep` the replayed content directly.
#[must_use]
pub fn snapshot_text(snapshot: &ScreenSnapshot) -> String {
    let cols = usize::from(snapshot.cols);
    if cols == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(snapshot.cells.len());
    for row in snapshot.cells.chunks(cols) {
        let line: String =
            row.iter().map(|cell| if cell.c == '\0' { ' ' } else { cell.c }).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// CLI commands
// ---------------------------------------------------------------------------

/// Print the replay bookkeeping for a session, optionally waiting for frames.
///
/// `min_frames` blocks until that many frames have been applied (the attach
/// path races the next CLI invocation otherwise); `expect_frames` turns the
/// count into an assertion, which is how a test states that a *fresh* session
/// must never be sent a replay at all.
pub fn status(
    session_id: &str,
    min_frames: u32,
    expect_frames: Option<u32>,
    timeout_ms: u64,
) -> Result<(), TestError> {
    let id = parse_session_id(session_id)?;
    let request = DaemonRequest::ReplayStatus { session_id: id, min_frames, timeout_ms };
    let response = send_request(&request).map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::ReplayStatus { applied, failed, live_bytes, last } => {
            print_status(applied, failed, live_bytes, last.as_ref())?;
            if applied < min_frames {
                return Err(TestError::TestFailure(format!(
                    "timed out after {timeout_ms}ms waiting for {min_frames} replay frame(s); {applied} applied"
                )));
            }
            match expect_frames {
                Some(expected) if applied != expected => Err(TestError::TestFailure(format!(
                    "replay frames: expected {expected} but found {applied}"
                ))),
                _ => Ok(()),
            }
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Write the status block callers grep: one `key: value` line per fact, plus a
/// single line describing the most recent frame.
fn print_status(
    applied: u32,
    failed: u32,
    live_bytes: u64,
    last: Option<&crate::cmd_socket::ReplayFrameInfo>,
) -> Result<(), TestError> {
    let frame_line = last.map_or_else(
        || "last-frame: none".to_owned(),
        |frame| {
            format!(
                "last-frame: index={} cols={} rows={} scrollback={} cursor={},{} alt={} compressed={} inflated={} live-before={} live-after={}",
                frame.index,
                frame.cols,
                frame.rows,
                frame.scrollback_rows,
                frame.cursor_row,
                frame.cursor_col,
                frame.alt_screen,
                frame.compressed_bytes,
                frame.inflated_bytes,
                frame.live_bytes_before,
                live_bytes.saturating_sub(frame.live_bytes_before),
            )
        },
    );

    write!(
        io::stdout().lock(),
        "frames: {applied}\nfailed: {failed}\nlive-bytes: {live_bytes}\n{frame_line}\n"
    )
    .map_err(|e| TestError::InfraError(format!("failed to write status: {e}")))
}

/// Print the replayed view as text, or write it out as snapshot JSON.
pub fn screen(session_id: &str, json_path: Option<&std::path::Path>) -> Result<(), TestError> {
    let id = parse_session_id(session_id)?;
    let response = send_request(&DaemonRequest::ReplayScreen { session_id: id })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::ScreenshotData { snapshot } => {
            if let Some(path) = json_path {
                let json = serde_json::to_string_pretty(&*snapshot).map_err(|e| {
                    TestError::InfraError(format!("failed to serialize snapshot: {e}"))
                })?;
                std::fs::write(path, json)
                    .map_err(|e| TestError::InfraError(format!("failed to write file: {e}")))
            } else {
                let text = snapshot_text(&snapshot);
                io::stdout()
                    .lock()
                    .write_all(text.as_bytes())
                    .map_err(|e| TestError::InfraError(format!("failed to write screen: {e}")))
            }
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Assert that the replayed view matches the server's own screen.
///
/// This is the losslessness oracle: the view holds the replay plus every
/// `PtyOutput` byte that followed it, so a gap between the server's snapshot and
/// its sink install — or a duplicated flush — shows up as a cell mismatch here
/// while `RequestSnapshot` alone stays green.
pub fn assert_matches(session_id: &str) -> Result<(), TestError> {
    let id = parse_session_id(session_id)?;
    let response = send_request(&DaemonRequest::AssertReplayMatchesScreen { session_id: id })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::AssertFailed { message } => Err(TestError::TestFailure(message)),
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

fn parse_session_id(session_id: &str) -> Result<SessionId, TestError> {
    SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))
}
