//! Session-lifecycle handling ported from the legacy client's reattach and
//! reconnect paths onto the display-only GPUI terminal.
//!
//! Six frozen-protocol server messages drive a pane's lifecycle after attach:
//! `SessionReplay` (zstd-decompress the reattach ANSI, then present it),
//! `ScreenSnapshot` (reset the terminal, then replay the `snapshot_to_ansi`
//! output), `TrimScrollback` (shift stored absolute prompt marks to track the
//! dropped scrollback rows), `PromptMark` (fold one OSC 133 mark into the
//! session's command records), and `SessionCreated` / `SessionExited` (register
//! and retire panes). `SessionList` rebuilds the reconnect topology — sessions
//! grouped by workspace in first-seen order — and a takeover `Hello`'s
//! `Welcome` adopts the returned window id.
//!
//! Prompt marks live in [`PromptMarks`] rather than on the registry, because
//! two threads need them: the IPC drain anchors each mark against the live grid
//! it has just written output into, and the GPUI key path reads them back to
//! resolve `prompt_jump_up` / `prompt_jump_down` / `jump_to_failure`.
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
    protocol::{PromptMarkKind, SessionInfo},
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

/// Decompress a `SessionReplay` into the ANSI byte stream fed to the pane.
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

/// The command-boundary record type, owned by the scrollbar module.
///
/// The store here and the overlay scrollbar are two ends of one record: the
/// drain writes `abs_pos`/`status` from the server's OSC 133 stream and the
/// paint pass renders each entry as a status-coloured tick, so they share the
/// type rather than each keeping a copy that has to be converted (and kept in
/// step) on the way to the screen. `Unknown` is both the initial state of an
/// open record and the resting state of a command whose shell reported no exit
/// code — FR-012/SC-006: an unreported exit is never a failure, so
/// `jump_to_failure` skips it and the tick stays neutral.
pub use scribe_client::scrollbar::{CommandMark, CommandStatus};

/// The grid geometry a prompt mark is anchored against.
///
/// Read off the live [`crate::terminal::DisplayOnlyTerminal`] at the moment the
/// mark is applied, which is after every output byte the server sent ahead of
/// it, so the anchor names the row the shell actually drew the prompt on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PromptAnchor {
    /// Rows of scrollback above the live screen.
    pub history: usize,
    /// Rows in the live screen.
    pub screen_lines: usize,
    /// The cursor's row within the live screen.
    pub cursor_row: usize,
    /// The cursor's column.
    pub cursor_col: usize,
}

/// Which way a prompt jump walks the mark list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpDirection {
    /// Toward older scrollback: the newest mark strictly above the viewport top.
    Up,
    /// Toward the live bottom: the oldest mark strictly below the viewport top.
    Down,
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
/// [`PromptMarks::on_trim`] with the drop count
/// [`SessionRegistry::on_trim_scrollback`] measures.
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

/// Per-session prompt-mark state, shared between the IPC drain that ingests
/// OSC 133 marks and the GPUI thread that jumps between them.
///
/// The legacy client kept this on each `Pane`; the display-only client has no
/// pane struct, so the same state lives here behind the one mutex both the
/// drain and the key path already have to cross. Every position is absolute
/// ("rows since the oldest scrollback row"), which is what makes a
/// `TrimScrollback` a pure subtraction instead of a re-scan.
#[derive(Debug, Default)]
pub struct PromptMarks {
    panes: HashMap<SessionId, PaneMarks>,
}

/// One session's prompt-mark state: the command records plus the pending input
/// row/column the `PromptEnd` mark records and a `TrimScrollback` shifts.
#[derive(Debug, Default)]
struct PaneMarks {
    marks: Vec<CommandMark>,
    input_start: Option<(usize, usize)>,
}

impl PromptMarks {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one OSC 133 mark into `session_id`'s command records.
    ///
    /// Ported from the legacy client's `handle_prompt_mark` (research D7):
    ///
    /// - `A` (`PromptStart`) opens a new [`CommandMark`] anchored at the prompt
    ///   row with [`CommandStatus::Unknown`], then prunes records whose anchor
    ///   has fallen out of the grid. A second `A` before a `D` simply leaves the
    ///   earlier record `Unknown`.
    /// - `B` (`PromptEnd`) records the input start row/column.
    /// - `C` (`CommandStart`) clears it.
    /// - `D` (`CommandEnd`) clears it and resolves the most-recent still-open
    ///   record from `exit_code`: `Some(0)` → `Success`, `Some(≠0)` →
    ///   `Failure`, `None` stays `Unknown`. A `D` with no open record is
    ///   ignored.
    ///
    /// Returns how many records the session holds afterwards, which the caller
    /// logs so the E2E can assert ingestion happened at all.
    pub fn record(
        &mut self,
        session_id: SessionId,
        kind: PromptMarkKind,
        exit_code: Option<i32>,
        anchor: PromptAnchor,
    ) -> usize {
        let pane = self.panes.entry(session_id).or_default();
        match kind {
            PromptMarkKind::PromptStart => {
                let abs_pos = anchor.history.saturating_add(anchor.cursor_row);
                pane.marks.push(CommandMark { abs_pos, status: CommandStatus::Unknown });
                // The highest addressable absolute row is history + screen_lines
                // - 1; anything past that ceiling names a row the grid no longer
                // holds, so it is dropped rather than jumped to.
                let max_valid = anchor.history.saturating_add(anchor.screen_lines);
                pane.marks.retain(|mark| mark.abs_pos <= max_valid);
                pane.input_start = None;
            }
            PromptMarkKind::PromptEnd => {
                pane.input_start =
                    Some((anchor.history.saturating_add(anchor.cursor_row), anchor.cursor_col));
            }
            PromptMarkKind::CommandStart => pane.input_start = None,
            PromptMarkKind::CommandEnd => {
                pane.input_start = None;
                if let Some(record) = pane.marks.last_mut()
                    && record.status == CommandStatus::Unknown
                {
                    record.status = match exit_code {
                        Some(0) => CommandStatus::Success,
                        Some(_) => CommandStatus::Failure,
                        None => CommandStatus::Unknown,
                    };
                }
            }
        }
        pane.marks.len()
    }

    /// Shift `session_id`'s marks after a `TrimScrollback` dropped
    /// `dropped_rows` of the oldest scrollback.
    pub fn on_trim(&mut self, session_id: SessionId, dropped_rows: usize) {
        if dropped_rows == 0 {
            return;
        }
        let pane = self.panes.entry(session_id).or_default();
        shift_absolute_marks_after_trim(&mut pane.marks, &mut pane.input_start, dropped_rows);
    }

    /// Drop everything tracked for a session that exited.
    pub fn forget(&mut self, session_id: SessionId) {
        self.panes.remove(&session_id);
    }

    /// The session's command records, oldest first.
    #[must_use]
    pub fn marks(&self, session_id: SessionId) -> &[CommandMark] {
        self.panes.get(&session_id).map_or(&[], |pane| pane.marks.as_slice())
    }

    /// The absolute row a prompt jump from `viewport_top_abs` should land on.
    ///
    /// `Up` takes the newest mark strictly above the current viewport top and
    /// `Down` the oldest strictly below it, so repeated presses walk the prompt
    /// list one command at a time and neither direction can re-select the mark
    /// the viewport is already parked on.
    #[must_use]
    pub fn jump_target(
        &self,
        session_id: SessionId,
        viewport_top_abs: usize,
        direction: JumpDirection,
    ) -> Option<usize> {
        let marks = self.marks(session_id);
        match direction {
            JumpDirection::Up => {
                marks.iter().rev().map(|mark| mark.abs_pos).find(|&pos| pos < viewport_top_abs)
            }
            JumpDirection::Down => {
                marks.iter().map(|mark| mark.abs_pos).find(|&pos| pos > viewport_top_abs)
            }
        }
    }

    /// The absolute row of the most recent failed command, if any.
    #[must_use]
    pub fn failure_target(&self, session_id: SessionId) -> Option<usize> {
        self.marks(session_id)
            .iter()
            .rev()
            .find(|mark| mark.status == CommandStatus::Failure)
            .map(|mark| mark.abs_pos)
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
        existed
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
        terminal.content().row_text(0).trim_end().to_owned()
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

    fn mark(abs_pos: usize) -> CommandMark {
        CommandMark { abs_pos, status: CommandStatus::Unknown }
    }

    /// A prompt anchor at `cursor_row` on an 80x24 grid with `history` rows of
    /// scrollback behind it.
    fn anchor_at(history: usize, cursor_row: usize) -> PromptAnchor {
        PromptAnchor { history, screen_lines: 24, cursor_row, cursor_col: 0 }
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Trim shifts marks]]
    #[test]
    fn trim_shifts_and_drops_absolute_marks() {
        let mut marks = vec![mark(3), mark(10), mark(25)];
        let mut input_start = Some((25, 5));

        shift_absolute_marks_after_trim(&mut marks, &mut input_start, 5);

        assert_eq!(marks, vec![mark(5), mark(20)]);
        assert_eq!(input_start, Some((20, 5)));
    }

    // @lat: [[client#GPUI Client Spike#Session Lifecycle#Trim clears input below delta]]
    #[test]
    fn trim_clears_input_start_below_delta() {
        let mut marks = vec![mark(2)];
        let mut input_start = Some((3, 1));
        shift_absolute_marks_after_trim(&mut marks, &mut input_start, 5);
        assert!(marks.is_empty());
        assert_eq!(input_start, None);
        // A zero-row trim is a no-op.
        let mut untouched = vec![mark(4)];
        let mut start = Some((4, 0));
        shift_absolute_marks_after_trim(&mut untouched, &mut start, 0);
        assert_eq!(untouched, vec![mark(4)]);
        assert_eq!(start, Some((4, 0)));
    }

    // @lat: [[client#GPUI Client Spike#Prompt Marks And Jumps#Mark state machine resolves exits]]
    #[test]
    fn prompt_mark_state_machine_resolves_exit_codes() {
        let mut marks = PromptMarks::new();
        let session = SessionId::new();

        // A: opens a record anchored at history + cursor row.
        assert_eq!(marks.record(session, PromptMarkKind::PromptStart, None, anchor_at(100, 4)), 1);
        assert_eq!(marks.marks(session), [mark(104)]);

        // B and C only move the pending input start, leaving the record open.
        marks.record(
            session,
            PromptMarkKind::PromptEnd,
            None,
            PromptAnchor { history: 100, screen_lines: 24, cursor_row: 4, cursor_col: 7 },
        );
        marks.record(session, PromptMarkKind::CommandStart, None, anchor_at(100, 5));
        assert_eq!(marks.marks(session)[0].status, CommandStatus::Unknown);

        // D with a non-zero exit resolves the open record as a failure.
        marks.record(session, PromptMarkKind::CommandEnd, Some(1), anchor_at(100, 6));
        assert_eq!(marks.marks(session)[0].status, CommandStatus::Failure);

        // A second command that exits 0 resolves as a success…
        marks.record(session, PromptMarkKind::PromptStart, None, anchor_at(100, 8));
        marks.record(session, PromptMarkKind::CommandEnd, Some(0), anchor_at(100, 9));
        assert_eq!(marks.marks(session)[1].status, CommandStatus::Success);

        // …and a third whose shell reported no exit code stays Unknown, so it
        // is never mistaken for a failure.
        marks.record(session, PromptMarkKind::PromptStart, None, anchor_at(100, 10));
        marks.record(session, PromptMarkKind::CommandEnd, None, anchor_at(100, 11));
        assert_eq!(marks.marks(session)[2].status, CommandStatus::Unknown);

        // A D that arrives once every record is resolved cannot rewrite one: it
        // only ever touches a still-open record, so the success above survives.
        marks.record(session, PromptMarkKind::CommandEnd, Some(3), anchor_at(100, 12));
        marks.record(session, PromptMarkKind::CommandEnd, Some(0), anchor_at(100, 13));
        assert_eq!(marks.marks(session)[1].status, CommandStatus::Success);
        assert_eq!(marks.marks(session)[2].status, CommandStatus::Failure);
    }

    // @lat: [[client#GPUI Client Spike#Prompt Marks And Jumps#Jump picks the neighbouring mark]]
    #[test]
    fn jump_targets_walk_marks_one_at_a_time() {
        let mut marks = PromptMarks::new();
        let session = SessionId::new();
        for row in [10_usize, 40, 70] {
            marks.record(session, PromptMarkKind::PromptStart, None, anchor_at(row, 0));
        }

        // From the live bottom (viewport top at 70) the first jump up lands on
        // 40, then 10, then reports nothing left.
        assert_eq!(marks.jump_target(session, 70, JumpDirection::Up), Some(40));
        assert_eq!(marks.jump_target(session, 40, JumpDirection::Up), Some(10));
        assert_eq!(marks.jump_target(session, 10, JumpDirection::Up), None);
        // Down walks the same list back toward the live bottom.
        assert_eq!(marks.jump_target(session, 10, JumpDirection::Down), Some(40));
        assert_eq!(marks.jump_target(session, 40, JumpDirection::Down), Some(70));
        assert_eq!(marks.jump_target(session, 70, JumpDirection::Down), None);
        // An unknown session has nothing to jump to.
        assert_eq!(marks.jump_target(SessionId::new(), 0, JumpDirection::Down), None);
    }

    // @lat: [[client#GPUI Client Spike#Prompt Marks And Jumps#Failure jump picks the newest failure]]
    #[test]
    fn failure_target_picks_the_newest_failure() {
        let mut marks = PromptMarks::new();
        let session = SessionId::new();
        assert_eq!(marks.failure_target(session), None);

        for (row, exit) in [(10_usize, Some(1)), (40, Some(0)), (70, Some(2)), (100, None)] {
            marks.record(session, PromptMarkKind::PromptStart, None, anchor_at(row, 0));
            marks.record(session, PromptMarkKind::CommandEnd, exit, anchor_at(row, 1));
        }

        assert_eq!(marks.failure_target(session), Some(70));
        // A trim that drops the oldest 50 rows re-anchors the survivor.
        marks.on_trim(session, 50);
        assert_eq!(marks.failure_target(session), Some(20));
        // An exited session forgets everything.
        marks.forget(session);
        assert_eq!(marks.failure_target(session), None);
    }

    // @lat: [[client#GPUI Client Spike#Prompt Marks And Jumps#Evicted anchors are pruned]]
    #[test]
    fn marks_past_the_grid_ceiling_are_pruned() {
        let mut marks = PromptMarks::new();
        let session = SessionId::new();
        // Two marks anchored high in a long scrollback…
        marks.record(session, PromptMarkKind::PromptStart, None, anchor_at(900, 3));
        marks.record(session, PromptMarkKind::PromptStart, None, anchor_at(910, 3));
        assert_eq!(marks.marks(session).len(), 2);
        // …then the grid shrinks to 20 rows of history, so both old anchors sit
        // past `history + screen_lines` and only the new one survives.
        marks.record(session, PromptMarkKind::PromptStart, None, anchor_at(20, 2));
        assert_eq!(marks.marks(session), [mark(22)]);
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
