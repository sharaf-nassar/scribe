//! Session-lifecycle handling ported from the legacy client's reattach and
//! reconnect paths onto the display-only GPUI terminal.
//!
//! Five frozen-protocol server messages drive a pane's lifecycle after attach:
//! `SessionReplay` (zstd-decompress the reattach ANSI, then `feed_output`),
//! `ScreenSnapshot` (reset the terminal, then replay the `snapshot_to_ansi`
//! output), `TrimScrollback` (shift stored absolute prompt marks to track the
//! dropped scrollback rows), and `SessionCreated` / `SessionExited` (register
//! and retire panes). `SessionList` rebuilds the reconnect topology — sessions
//! grouped by workspace in first-seen order — and a takeover `Hello`'s
//! `Welcome` adopts the returned window id.
//!
//! Decompression and snapshot-to-ANSI conversion stay pure and happen in the
//! reader ahead of the coalescing drain (the drain only ever concatenates
//! bytes, per the IPC-bridge contract). A corrupt `SessionReplay` yields a
//! [`ReplayDecodeError`] the reader surfaces as pane status instead of tearing
//! down the loop, matching the legacy graceful-skip behaviour. The topology,
//! registry, and mark-shift helpers are pure so they can be exercised
//! independently of a live GPUI window.

use std::collections::HashMap;

use scribe_common::{
    ids::{SessionId, WindowId, WorkspaceId},
    protocol::SessionInfo,
    screen::ScreenSnapshot,
    screen_replay::{self, SessionReplay},
};

/// A `SessionReplay` could not be turned into terminal output.
///
/// Carries the offending session and a human-readable reason so the pane can
/// render an error banner. Producing this value never touches a terminal, so
/// the pane keeps whatever content it last showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayDecodeError {
    pub session_id: SessionId,
    pub reason: String,
}

impl std::fmt::Display for ReplayDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "replay decode failed: {}", self.reason)
    }
}

/// Decompress a `SessionReplay` into the ANSI byte stream fed to `feed_output`.
///
/// Zero-dimension replays are rejected up front (the legacy client skips them),
/// and a corrupt zstd stream is reported as a [`ReplayDecodeError`] rather than
/// panicking, so the caller can degrade to an error state without crashing.
///
/// # Errors
/// Returns [`ReplayDecodeError`] when the replay has zero dimensions or the
/// zstd payload fails to decompress.
pub fn decode_replay(
    session_id: SessionId,
    replay: &SessionReplay,
) -> Result<Vec<u8>, ReplayDecodeError> {
    if replay.cols == 0 || replay.rows == 0 {
        return Err(ReplayDecodeError {
            session_id,
            reason: "replay reported zero dimensions".to_owned(),
        });
    }
    screen_replay::decompress_session_replay(replay)
        .map_err(|error| ReplayDecodeError { session_id, reason: error.to_string() })
}

/// Build the byte stream that applies a `ScreenSnapshot` to a fresh terminal.
///
/// Prefixes RIS (`ESC c`) so the receiving terminal is reset before the
/// `snapshot_to_ansi` output replays, guaranteeing the tooling snapshot
/// replaces the pane's content instead of appending onto it.
#[must_use]
pub fn snapshot_reset_bytes(snapshot: &ScreenSnapshot) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + usize::from(snapshot.cols) * usize::from(snapshot.rows));
    bytes.extend_from_slice(b"\x1bc");
    bytes.extend_from_slice(&screen_replay::snapshot_to_ansi(snapshot));
    bytes
}

/// A prompt / command anchor stored as an absolute scrollback row.
///
/// `abs_pos` is "lines since the very top of scrollback" (0 = oldest), the
/// stable identifier a `TrimScrollback` shifts. Ported from the legacy
/// `CommandRecord`; only the anchor row matters for mark tracking here. The
/// scrollbar command-mark bead (scribe-38e.15, blocked on this one) populates
/// these from OSC 133; until then the [`SessionRegistry`] tracks an empty set
/// per session and still shifts it on every trim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandMark {
    pub abs_pos: usize,
}

/// Shift stored absolute scrollback positions after a `TrimScrollback` drops the
/// oldest `dropped_rows` rows.
///
/// A trim of `K` oldest rows shifts every surviving row's absolute index down by
/// `K`; marks anchored inside the trimmed region (`abs_pos < dropped_rows`) are
/// dropped because their row no longer exists. `input_start` is cleared in the
/// same case so downstream pin-height math falls back to the cursor heuristic
/// instead of pointing at a synthetic row 0. Ported byte-for-byte from the
/// legacy client's `shift_absolute_marks_after_trim`; driven by
/// [`SessionRegistry::on_trim_scrollback`].
pub fn shift_absolute_marks_after_trim(
    marks: &mut Vec<CommandMark>,
    input_start: &mut Option<(usize, usize)>,
    dropped_rows: usize,
) {
    if dropped_rows == 0 {
        return;
    }
    marks.retain_mut(|mark| {
        if mark.abs_pos >= dropped_rows {
            mark.abs_pos -= dropped_rows;
            true
        } else {
            false
        }
    });
    if let Some((line, col)) = *input_start {
        *input_start = if line >= dropped_rows { Some((line - dropped_rows, col)) } else { None };
    }
}

/// Order grouped reconnect tabs by a window's workspace order.
///
/// Only workspaces present in `workspace_order` that also have live sessions
/// appear, so stale tree leaves without sessions are pruned — mirroring the
/// legacy `ordered_workspace_tabs`.
#[must_use]
pub fn ordered_workspace_tabs(
    workspace_order: &[WorkspaceId],
    groups: &HashMap<WorkspaceId, Vec<SessionId>>,
) -> Vec<(WorkspaceId, Vec<SessionId>)> {
    workspace_order
        .iter()
        .filter_map(|ws_id| groups.get(ws_id).map(|sessions| (*ws_id, sessions.clone())))
        .collect()
}

/// Client-side registry of live sessions and window adoption state.
///
/// Rebuilt wholesale from a `SessionList` on reconnect and mutated incrementally
/// by `SessionCreated` / `SessionExited`. Sessions and their workspaces keep
/// first-seen order so [`SessionRegistry::reconnect_topology`] reproduces the
/// server's tab layout. `adopted_window` records the window id a takeover
/// `Hello`'s `Welcome` handed back, so the client knows it now owns the writer.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    session_order: Vec<SessionId>,
    workspace_order: Vec<WorkspaceId>,
    workspace: HashMap<SessionId, WorkspaceId>,
    adopted_window: Option<WindowId>,
    last_history: HashMap<SessionId, u32>,
    pane_marks: HashMap<SessionId, PaneMarks>,
}

/// Per-session prompt-mark state a `TrimScrollback` shifts.
///
/// `marks` are absolute scrollback anchors and `input_start` is the pending
/// input row/column; both stay empty until OSC 133 tracking lands, but the trim
/// handler shifts whatever is present.
#[derive(Debug, Default)]
struct PaneMarks {
    marks: Vec<CommandMark>,
    input_start: Option<(usize, usize)>,
}

impl SessionRegistry {
    /// Creates an empty registry with no adopted window.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the registry contents from a `SessionList` reconnect payload.
    ///
    /// Preserves the server's session and workspace order and drops any
    /// previously-tracked sessions the server no longer reports.
    pub fn rebuild_from_session_list(&mut self, sessions: &[SessionInfo]) {
        self.session_order.clear();
        self.workspace_order.clear();
        self.workspace.clear();
        for info in sessions {
            self.track(info.session_id, info.workspace_id);
        }
    }

    /// Register a freshly `SessionCreated` pane, appending it in arrival order.
    ///
    /// Re-announcing an existing session only refreshes its workspace mapping.
    pub fn on_session_created(&mut self, session_id: SessionId, workspace_id: WorkspaceId) {
        self.track(session_id, workspace_id);
    }

    /// Retire a `SessionExited` pane. Returns whether it was tracked.
    pub fn on_session_exited(&mut self, session_id: SessionId) -> bool {
        let existed = self.workspace.remove(&session_id).is_some();
        self.session_order.retain(|id| *id != session_id);
        self.last_history.remove(&session_id);
        self.pane_marks.remove(&session_id);
        existed
    }

    /// Handle a `TrimScrollback` for `session_id`, shifting the session's stored
    /// absolute marks to track the rows the server dropped. Returns the
    /// dropped-row count.
    ///
    /// `history_rows` is the server's post-trim scrollback size; the drop is the
    /// decrease from the previously-reported size (0 on the first report), which
    /// a display-only client mirrors as the number of oldest rows to shift past.
    pub fn on_trim_scrollback(&mut self, session_id: SessionId, history_rows: u32) -> usize {
        let previous = self.last_history.insert(session_id, history_rows).unwrap_or(history_rows);
        let dropped = usize::try_from(previous.saturating_sub(history_rows)).unwrap_or(usize::MAX);
        let entry = self.pane_marks.entry(session_id).or_default();
        shift_absolute_marks_after_trim(&mut entry.marks, &mut entry.input_start, dropped);
        dropped
    }

    /// Record the window id a takeover `Hello`'s `Welcome` adopted.
    pub fn adopt_window(&mut self, window_id: WindowId) {
        self.adopted_window = Some(window_id);
    }

    /// The window id this client adopted via takeover, if any.
    #[must_use]
    pub fn adopted_window(&self) -> Option<WindowId> {
        self.adopted_window
    }

    /// Number of tracked sessions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.session_order.len()
    }

    /// Reconnect topology: tracked sessions grouped by workspace in first-seen
    /// order, dropping workspaces that no longer have live sessions.
    #[must_use]
    pub fn reconnect_topology(&self) -> Vec<(WorkspaceId, Vec<SessionId>)> {
        let mut groups: HashMap<WorkspaceId, Vec<SessionId>> = HashMap::new();
        for session_id in &self.session_order {
            if let Some(ws_id) = self.workspace.get(session_id) {
                groups.entry(*ws_id).or_default().push(*session_id);
            }
        }
        ordered_workspace_tabs(&self.workspace_order, &groups)
    }

    /// Insert a session, recording first-seen session and workspace order.
    fn track(&mut self, session_id: SessionId, workspace_id: WorkspaceId) {
        if self.workspace.insert(session_id, workspace_id).is_none() {
            self.session_order.push(session_id);
        }
        if !self.workspace_order.contains(&workspace_id) {
            self.workspace_order.push(workspace_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use scribe_common::screen::{CellFlags, CursorStyle, ScreenCell, ScreenColor, ScreenSnapshot};
    use scribe_common::screen_replay::build_session_replay;

    use super::*;
    use crate::terminal::DisplayOnlyTerminal;

    fn blank_cell() -> ScreenCell {
        ScreenCell {
            c: ' ',
            fg: ScreenColor::Named(256),
            bg: ScreenColor::Named(257),
            flags: CellFlags::default(),
        }
    }

    fn snapshot_with_text(text: &str) -> ScreenSnapshot {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let mut cells = vec![blank_cell(); usize::from(cols) * usize::from(rows)];
        for (i, ch) in text.chars().enumerate() {
            if i >= cells.len() {
                break;
            }
            cells[i].c = ch;
        }
        ScreenSnapshot {
            cells,
            cols,
            rows,
            cursor_col: 0,
            cursor_row: 0,
            cursor_style: CursorStyle::Block,
            cursor_visible: true,
            alt_screen: false,
            active_dec_modes: Vec::new(),
            scrollback: Vec::new(),
            scrollback_rows: 0,
        }
    }

    fn first_row(terminal: &DisplayOnlyTerminal) -> String {
        terminal.content().rows.first().cloned().unwrap_or_default().trim_end().to_owned()
    }

    fn session_info(workspace_id: WorkspaceId) -> SessionInfo {
        SessionInfo {
            session_id: SessionId::new(),
            workspace_id,
            shell_name: "zsh".to_owned(),
            title: None,
            context: None,
            task_label: None,
            codex_task_label: None,
            cwd: None,
            git_branch: None,
            ai_state: None,
            ai_provider_hint: None,
        }
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Replay reattach applies]]
    #[gpui::test]
    fn replay_reattach_writes_decoded_output(_cx: &mut gpui::TestAppContext) {
        let mut terminal = DisplayOnlyTerminal::new(80, 24);
        let session = SessionId::new();
        let replay = build_session_replay(&snapshot_with_text("hello reattach")).unwrap();

        let bytes = decode_replay(session, &replay).expect("valid replay decodes");
        terminal.feed_output(&bytes);

        assert_eq!(first_row(&terminal), "hello reattach");
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Replay decode failure]]
    #[gpui::test]
    fn replay_decode_failure_shows_error_without_crashing(_cx: &mut gpui::TestAppContext) {
        let mut terminal = DisplayOnlyTerminal::new(80, 24);
        terminal.feed_output(b"live content");
        let session = SessionId::new();

        // A valid replay header with a corrupt zstd payload.
        let mut corrupt = build_session_replay(&snapshot_with_text("x")).unwrap();
        corrupt.replay_zstd = vec![0xff, 0x00, 0xde, 0xad, 0xbe, 0xef];

        let error = decode_replay(session, &corrupt).expect_err("corrupt replay is rejected");

        assert_eq!(error.session_id, session);
        assert!(error.to_string().contains("replay decode failed"));
        // Terminal is untouched and still usable — no panic, no poisoned state.
        assert_eq!(first_row(&terminal), "live content");
        terminal.feed_output(b"\r\nmore output");
        assert_eq!(first_row(&terminal), "live content");
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Snapshot resets before replay]]
    #[gpui::test]
    fn screen_snapshot_resets_before_replaying(_cx: &mut gpui::TestAppContext) {
        let mut terminal = DisplayOnlyTerminal::new(80, 24);
        terminal.feed_output(b"stale pane content that must be gone");

        terminal.feed_output(&snapshot_reset_bytes(&snapshot_with_text("fresh snapshot")));

        assert_eq!(first_row(&terminal), "fresh snapshot");
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Zero-dimension replay rejected]]
    #[test]
    fn zero_dimension_replay_is_rejected() {
        let mut replay = build_session_replay(&snapshot_with_text("x")).unwrap();
        replay.rows = 0;
        let error = decode_replay(SessionId::new(), &replay).unwrap_err();
        assert!(error.reason.contains("zero dimensions"));
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Trim shifts marks]]
    #[test]
    fn trim_shifts_and_drops_absolute_marks() {
        let mut marks = vec![
            CommandMark { abs_pos: 3 },
            CommandMark { abs_pos: 10 },
            CommandMark { abs_pos: 25 },
        ];
        let mut input_start = Some((25, 5));

        shift_absolute_marks_after_trim(&mut marks, &mut input_start, 5);

        assert_eq!(marks, vec![CommandMark { abs_pos: 5 }, CommandMark { abs_pos: 20 }]);
        assert_eq!(input_start, Some((20, 5)));
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Trim clears input below delta]]
    #[test]
    fn trim_clears_input_start_below_delta() {
        let mut marks = vec![CommandMark { abs_pos: 2 }];
        let mut input_start = Some((3, 1));
        shift_absolute_marks_after_trim(&mut marks, &mut input_start, 5);
        assert!(marks.is_empty());
        assert_eq!(input_start, None);
        // A zero-row trim is a no-op.
        let mut untouched = vec![CommandMark { abs_pos: 4 }];
        let mut start = Some((4, 0));
        shift_absolute_marks_after_trim(&mut untouched, &mut start, 0);
        assert_eq!(untouched, vec![CommandMark { abs_pos: 4 }]);
        assert_eq!(start, Some((4, 0)));
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Reconnect topology rebuild]]
    #[test]
    fn session_list_rebuilds_workspace_topology() {
        let ws_a = WorkspaceId::new();
        let ws_b = WorkspaceId::new();
        let a1 = session_info(ws_a);
        let b1 = session_info(ws_b);
        let a2 = session_info(ws_a);
        let sessions = vec![a1.clone(), b1.clone(), a2.clone()];

        let mut registry = SessionRegistry::new();
        registry.rebuild_from_session_list(&sessions);

        assert_eq!(registry.len(), 3);
        // First-seen workspace order is ws_a then ws_b; sessions keep arrival
        // order within each workspace bucket.
        assert_eq!(
            registry.reconnect_topology(),
            vec![(ws_a, vec![a1.session_id, a2.session_id]), (ws_b, vec![b1.session_id]),]
        );

        // A later rebuild replaces the topology and drops vanished sessions.
        registry.rebuild_from_session_list(std::slice::from_ref(&b1));
        assert_eq!(registry.reconnect_topology(), vec![(ws_b, vec![b1.session_id])]);
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Created and exited transitions]]
    #[test]
    fn session_created_and_exited_transitions() {
        let ws = WorkspaceId::new();
        let mut registry = SessionRegistry::new();
        let session = SessionId::new();

        registry.on_session_created(session, ws);
        assert_eq!(registry.reconnect_topology(), vec![(ws, vec![session])]);

        // Re-announcing does not duplicate.
        registry.on_session_created(session, ws);
        assert_eq!(registry.len(), 1);

        assert!(registry.on_session_exited(session));
        assert_eq!(registry.len(), 0);
        assert!(registry.reconnect_topology().is_empty());
        // Exiting an unknown session is a no-op.
        assert!(!registry.on_session_exited(SessionId::new()));
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Registry trims marks]]
    #[test]
    fn registry_trim_shifts_session_marks() {
        let mut registry = SessionRegistry::new();
        let session = SessionId::new();
        registry.on_session_created(session, WorkspaceId::new());
        // First report establishes the baseline history with no drop.
        assert_eq!(registry.on_trim_scrollback(session, 100), 0);

        // Seed a mark, then a trim that drops 40 rows shifts it down.
        registry.pane_marks.entry(session).or_default().marks.push(CommandMark { abs_pos: 50 });
        assert_eq!(registry.on_trim_scrollback(session, 60), 40);
        assert_eq!(
            registry.pane_marks.get(&session).unwrap().marks,
            vec![CommandMark { abs_pos: 10 }]
        );

        // Exiting the session clears its mark and history state.
        assert!(registry.on_session_exited(session));
        assert!(!registry.pane_marks.contains_key(&session));
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Takeover adoption]]
    #[test]
    fn takeover_welcome_adopts_window() {
        let mut registry = SessionRegistry::new();
        assert!(registry.adopted_window().is_none());
        let window = WindowId::new();
        registry.adopt_window(window);
        assert_eq!(registry.adopted_window(), Some(window));
    }
}
