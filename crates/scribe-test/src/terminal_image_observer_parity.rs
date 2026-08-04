//! Production-path parity probe for Alacritty-derived image observations.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use scribe_common::ids::SessionId;
use scribe_common::terminal_images::TerminalScreenKind;
use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
use scribe_server::session_manager::build_term_config;
use scribe_server::terminal_image_state::{
    ObservedTerminalGridEffect, PtyReaderIngressRejection, PtyTerminalImageState,
    SessionTerminalCommit, SessionTerminalError, SessionTerminalOutput, TerminalCursorObservation,
    TerminalGridObservation, TerminalGridObserverHandle, TerminalImageProcessPolicy,
    apply_observed_cursor_move, feed_terminal_image_result_observed,
    feed_terminal_image_result_with_observer, feed_terminal_observed, flush_terminal_observed,
    observe_terminal_resize, process_pty_reader_ingress,
};
use serde::Serialize;
use tokio::sync::mpsc;
use vte::ansi::Processor;

#[derive(Serialize)]
struct Evidence<'a> {
    schema_version: u32,
    status: &'a str,
    engine: &'a str,
    alacritty_terminal: &'a str,
    one_processor: bool,
    payload_free: bool,
    cases: BTreeMap<&'a str, &'a str>,
}

struct ProbeDimensions {
    columns: usize,
    rows: usize,
}

impl Dimensions for ProbeDimensions {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

struct Probe {
    images: PtyTerminalImageState,
    observer: TerminalGridObserverHandle,
    term: Term<ScribeEventListener>,
    processor: Processor,
    _event_rx: mpsc::UnboundedReceiver<SessionEvent>,
}

impl Probe {
    fn new(columns: usize, rows: usize) -> Self {
        Self::with_policy(columns, rows, TerminalImageProcessPolicy::v1())
    }

    fn with_policy(
        columns: usize,
        rows: usize,
        policy: std::sync::Arc<TerminalImageProcessPolicy>,
    ) -> Self {
        let session_id = SessionId::new();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let listener = ScribeEventListener::new(session_id, event_tx);
        let dimensions = ProbeDimensions { columns, rows };
        let term = Term::new(build_term_config(32), &dimensions, listener);
        let images = PtyTerminalImageState::new(policy);
        let observer = images.grid_observer();
        observer.set_cell_size(8, 16);
        Self { images, observer, term, processor: Processor::new(), _event_rx: event_rx }
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<SessionTerminalCommit, String> {
        let mut result = self.images.process_bytes(bytes);
        feed_terminal_image_result_observed(
            &mut self.images,
            &mut self.term,
            &mut self.processor,
            bytes,
            &mut result,
        );
        assert_active_matches(&self.term, &self.observer.observation())?;
        result.map_err(|error| error.to_string())
    }

    fn observation(&self) -> TerminalGridObservation {
        self.observer.observation()
    }

    fn resize(&mut self, columns: usize, rows: usize) {
        let before = (self.term.columns(), self.term.screen_lines());
        self.term.resize(ProbeDimensions { columns, rows });
        let changed = before != (self.term.columns(), self.term.screen_lines());
        observe_terminal_resize(&self.observer, &self.term, changed);
    }

    fn flush_sync(&mut self) -> Result<TerminalGridObservation, String> {
        let observation =
            flush_terminal_observed(&self.observer, &mut self.term, &mut self.processor);
        self.images.record_grid_observation(&observation);
        assert_active_matches(&self.term, &observation)?;
        Ok(observation)
    }
}

fn direct_cursor<T>(term: &Term<T>, saved: bool) -> TerminalCursorObservation {
    let cursor = if saved { &term.grid().saved_cursor } else { &term.grid().cursor };
    TerminalCursorObservation {
        row: cursor.point.line.0,
        column: u16::try_from(cursor.point.column.0).unwrap_or(u16::MAX),
        input_needs_wrap: cursor.input_needs_wrap,
    }
}

fn assert_active_matches<T>(
    term: &Term<T>,
    observation: &TerminalGridObservation,
) -> Result<(), String> {
    let screen = if term.mode().contains(TermMode::ALT_SCREEN) {
        TerminalScreenKind::Alternate
    } else {
        TerminalScreenKind::Primary
    };
    let active = match screen {
        TerminalScreenKind::Primary => observation.primary,
        TerminalScreenKind::Alternate => observation.alternate,
    };
    let actual_size = (
        u16::try_from(term.columns()).unwrap_or(u16::MAX),
        u16::try_from(term.screen_lines()).unwrap_or(u16::MAX),
    );
    if observation.active_screen != screen
        || active.cursor != Some(direct_cursor(term, false))
        || active.saved_cursor != Some(direct_cursor(term, true))
        || (active.size.columns, active.size.rows) != actual_size
        || observation.origin_mode != term.mode().contains(TermMode::ORIGIN)
        || observation.line_wrap_mode != term.mode().contains(TermMode::LINE_WRAP)
    {
        return Err(format!(
            "observer diverged from active Alacritty grid: screen={screen:?} actual_cursor={:?} actual_saved={:?} observation={observation:?}",
            direct_cursor(term, false),
            direct_cursor(term, true),
        ));
    }
    Ok(())
}

// @lat: [[test#Test Harness#Terminal Image Observer Parity#Production Alacritty Probe]]
pub fn run(evidence_path: &Path) -> Result<(), String> {
    verify_wrap_pending_and_image_move()?;
    verify_save_restore_and_1049()?;
    verify_margins_scroll_and_ed2()?;
    verify_ed1_pinned_semantics()?;
    verify_split_reads()?;
    verify_same_read_chronology()?;
    verify_same_span_modes_and_deccolm()?;
    verify_input_width_scroll_paths()?;
    verify_ordered_boundary_cuts()?;
    verify_image_error_observed_once()?;
    verify_synchronized_update_timeout()?;
    verify_both_grid_resize()?;

    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        engine: "scribe-server real Term observer",
        alacritty_terminal: "0.26.0-rc1",
        one_processor: true,
        payload_free: true,
        cases: [
            ("alternate_1049", "pass"),
            ("deccolm_same_span", "pass"),
            ("ed1_pinned_semantics", "pass"),
            ("ed2_half_open_scope", "pass"),
            ("image_error_observed_once", "pass"),
            ("input_width_scroll_paths", "pass"),
            ("ordered_boundary_cuts", "pass"),
            ("margins_and_scroll", "pass"),
            ("resize_active_and_inactive", "pass"),
            ("same_read_chronology", "pass"),
            ("same_span_live_wrap_mode", "pass"),
            ("save_restore_per_grid", "pass"),
            ("split_reads", "pass"),
            ("synchronized_update_timeout", "pass"),
            ("wrap_pending_and_image_move", "pass"),
        ]
        .into_iter()
        .collect(),
    };
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create observer evidence directory: {error}"))?;
    }
    let bytes = serde_json::to_vec_pretty(&evidence)
        .map_err(|error| format!("serialize observer evidence: {error}"))?;
    std::fs::write(evidence_path, bytes)
        .map_err(|error| format!("write observer evidence: {error}"))
}

fn verify_wrap_pending_and_image_move() -> Result<(), String> {
    let mut probe = Probe::new(5, 4);
    probe.feed(b"abcde")?;
    let before_move = probe.observation().primary.cursor;
    if before_move != Some(TerminalCursorObservation { row: 0, column: 4, input_needs_wrap: true })
    {
        return Err(format!("last-column wrap state drifted: {before_move:?}"));
    }
    let moved = apply_observed_cursor_move(&probe.observer, &mut probe.term, 2, 1);
    assert_active_matches(&probe.term, &moved)?;
    if moved.primary.cursor
        != Some(TerminalCursorObservation { row: 2, column: 1, input_needs_wrap: false })
    {
        return Err(format!("image cursor move left stale wrap state: {moved:?}"));
    }
    probe.feed(b"Z")?;
    Ok(())
}

fn verify_save_restore_and_1049() -> Result<(), String> {
    let mut probe = Probe::new(8, 5);
    probe.feed(b"\x1b[2;3H\x1b7\x1b[4;6H\x1b8")?;
    let primary = probe.observation().primary;
    if primary.cursor != primary.saved_cursor
        || primary.cursor
            != Some(TerminalCursorObservation { row: 1, column: 2, input_needs_wrap: false })
    {
        return Err(format!("primary save/restore drifted: {primary:?}"));
    }

    let enter = probe.feed(b"\x1b[?1049h")?;
    let entered = enter.grid_observations.last().ok_or("missing 1049 enter observation")?;
    if !entered.observation.effects.iter().any(|effect| {
        matches!(
            effect,
            ObservedTerminalGridEffect::SwitchScreen {
                from: TerminalScreenKind::Primary,
                to: TerminalScreenKind::Alternate,
            }
        )
    }) || entered.observation.primary.saved_cursor != primary.cursor
    {
        return Err(format!("1049 entry state drifted: {entered:?}"));
    }
    probe.feed(b"\x1b[5;8H\x1b7\x1b[3;4H")?;
    let alternate_saved = probe.observation().alternate.saved_cursor;
    let leave = probe.feed(b"\x1b[?1049l")?;
    let left = leave.grid_observations.last().ok_or("missing 1049 leave observation")?;
    if left.observation.primary.cursor != primary.cursor
        || left.observation.alternate.saved_cursor != alternate_saved
        || !left.observation.effects.iter().any(|effect| {
            matches!(
                effect,
                ObservedTerminalGridEffect::SwitchScreen {
                    from: TerminalScreenKind::Alternate,
                    to: TerminalScreenKind::Primary,
                }
            )
        })
    {
        return Err(format!("1049 leave state drifted: {left:?}"));
    }
    Ok(())
}

fn verify_margins_scroll_and_ed2() -> Result<(), String> {
    let mut probe = Probe::new(6, 5);
    probe.feed(b"\x1b[2;4r")?;
    let margins = probe.observation();
    if (margins.margin_top, margins.margin_bottom) != (1, 4) {
        return Err(format!("DECSTBM margins drifted: {margins:?}"));
    }
    let scroll = probe.feed(b"\x1b[4;1H\n")?;
    if !scroll.grid_observations.iter().flat_map(|span| &span.observation.effects).any(|effect| {
        matches!(
            effect,
            ObservedTerminalGridEffect::Scroll {
                screen: TerminalScreenKind::Primary,
                top: 1,
                bottom: 4,
                rows: 1,
            }
        )
    }) {
        return Err(format!("margin linefeed did not report half-open scroll: {scroll:?}"));
    }
    let ed2 = probe.feed(b"\x1b[2J")?;
    if !ed2.grid_observations.iter().flat_map(|span| &span.observation.effects).any(|effect| {
        matches!(
            effect,
            ObservedTerminalGridEffect::EraseDisplay { screen: TerminalScreenKind::Primary }
        )
    }) {
        return Err(format!("ED2 did not report display scope: {ed2:?}"));
    }
    Ok(())
}

fn verify_ed1_pinned_semantics() -> Result<(), String> {
    for row in [0_i32, 1, 2] {
        let mut probe = Probe::new(4, 4);
        probe.feed(b"\x1b[1;1HAAAA\x1b[2;1HBBBB\x1b[3;1HCCCC\x1b[4;1HDDDD")?;
        let sequence = format!("\x1b[{};2H\x1b[1J", row + 1);
        let commit = probe.feed(sequence.as_bytes())?;
        let effects: Vec<&ObservedTerminalGridEffect> =
            commit.grid_observations.iter().flat_map(|span| &span.observation.effects).collect();
        let cursor_row = ObservedTerminalGridEffect::EraseCells {
            screen: TerminalScreenKind::Primary,
            top: u16::try_from(row).unwrap_or(u16::MAX),
            left: 0,
            bottom: u16::try_from(row + 1).unwrap_or(u16::MAX),
            right: 2,
        };
        let expected = if row > 1 {
            vec![
                ObservedTerminalGridEffect::EraseCells {
                    screen: TerminalScreenKind::Primary,
                    top: 0,
                    left: 0,
                    bottom: u16::try_from(row).unwrap_or(u16::MAX),
                    right: 4,
                },
                cursor_row,
            ]
        } else {
            vec![cursor_row]
        };
        if effects != expected.iter().collect::<Vec<_>>() {
            return Err(format!("ED1 effects drifted at row {row}: {effects:?}"));
        }

        assert_ed1_cells(&probe.term, row)?;
    }
    Ok(())
}

fn assert_ed1_cells<T>(term: &Term<T>, cursor_row: i32) -> Result<(), String> {
    for row in 0_i32..4 {
        for column in 0_usize..4 {
            let actual = term.grid()[Line(row)][Column(column)].c;
            let expected_cell = expected_ed1_cell(cursor_row, row, column);
            if actual != expected_cell {
                return Err(format!(
                    "ED1 cell drifted at cursor row {cursor_row}, cell ({row},{column}): actual={actual:?} expected={expected_cell:?}"
                ));
            }
        }
    }
    Ok(())
}

fn expected_ed1_cell(cursor_row: i32, row: i32, column: usize) -> char {
    let cleared = (cursor_row > 1 && row < cursor_row) || (row == cursor_row && column <= 1);
    if cleared { ' ' } else { char::from(b'A' + u8::try_from(row).unwrap_or(0)) }
}

fn verify_split_reads() -> Result<(), String> {
    let mut probe = Probe::new(6, 4);
    let first = probe.feed(b"\x1b[2")?;
    if first.grid_observations.iter().any(|span| !span.observation.effects.is_empty()) {
        return Err(format!("partial CSI invented an effect: {first:?}"));
    }
    let second = probe.feed(b"J")?;
    if !second
        .grid_observations
        .iter()
        .flat_map(|span| &span.observation.effects)
        .any(|effect| matches!(effect, ObservedTerminalGridEffect::EraseDisplay { .. }))
    {
        return Err(format!("completed split ED2 missed its effect: {second:?}"));
    }

    let partial = probe.feed(b"\x1b_Ga=q,f=24,s=1,v=1,i=1;/wAA")?;
    if partial.grid_observations.iter().any(|span| !span.observation.effects.is_empty()) {
        return Err(format!("partial APC invented an effect: {partial:?}"));
    }
    let completed = probe.feed(b"\x1b\\")?;
    let completed_cursor =
        completed.grid_observations.first().map(|span| span.observation.primary.cursor);
    if completed.grid_observations.len() != 1
        || completed_cursor != Some(probe.observation().primary.cursor)
    {
        return Err(format!("split APC completion observation drifted: {completed:?}"));
    }
    Ok(())
}

fn verify_same_read_chronology() -> Result<(), String> {
    let mut probe = Probe::new(8, 4);
    let commit = probe.feed(b"A\x1b_Ga=q,f=24,s=1,v=1,i=7;/wAA\x1b\\B")?;
    let observed_columns: Vec<u16> = commit
        .grid_observations
        .iter()
        .filter_map(|span| span.observation.primary.cursor.map(|cursor| cursor.column))
        .collect();
    if observed_columns != [1, 2] {
        return Err(format!("same-read image chronology drifted: {commit:?}"));
    }
    Ok(())
}

fn verify_same_span_modes_and_deccolm() -> Result<(), String> {
    let mut probe = Probe::new(5, 3);
    probe.feed(b"\x1b[3;1Habcde")?;
    let wrap_off = probe.feed(b"\x1b[?7lZ")?;
    let wrap_off_effects: Vec<&ObservedTerminalGridEffect> =
        wrap_off.grid_observations.iter().flat_map(|span| &span.observation.effects).collect();
    if wrap_off_effects
        .iter()
        .any(|effect| matches!(effect, ObservedTerminalGridEffect::Scroll { .. }))
        || probe.observation().line_wrap_mode
        || probe.observation().primary.cursor
            != Some(TerminalCursorObservation { row: 2, column: 4, input_needs_wrap: true })
    {
        return Err(format!(
            "same-span DECRST 7 invented a wrap scroll: observation={:?} effects={wrap_off_effects:?}",
            probe.observation()
        ));
    }

    let wrap_on = probe.feed(b"\x1b[?7hQ")?;
    if !wrap_on.grid_observations.iter().flat_map(|span| &span.observation.effects).any(|effect| {
        matches!(
            effect,
            ObservedTerminalGridEffect::Scroll {
                screen: TerminalScreenKind::Primary,
                top: 0,
                bottom: 3,
                rows: 1,
            }
        )
    }) {
        return Err(format!("same-span DECSET 7 missed its real wrap scroll: {wrap_on:?}"));
    }

    probe.feed(b"X\x1b[2;2r")?;
    let set = probe.feed(b"\x1b[?3h")?;
    if (probe.observation().margin_top, probe.observation().margin_bottom) != (0, 3)
        || probe.term.grid()[Line(2)][Column(1)].c != ' '
        || !set.grid_observations.iter().flat_map(|span| &span.observation.effects).any(|effect| {
            matches!(
                effect,
                ObservedTerminalGridEffect::EraseDisplay { screen: TerminalScreenKind::Primary }
            )
        })
    {
        return Err(format!("DECSET 3 did not expose Alacritty grid reset: {set:?}"));
    }

    probe.feed(b"Y\x1b[2;2r")?;
    let unset = probe.feed(b"\x1b[?3l")?;
    if (probe.observation().margin_top, probe.observation().margin_bottom) != (0, 3)
        || probe.term.grid()[Line(0)][Column(0)].c != ' '
        || !unset
            .grid_observations
            .iter()
            .flat_map(|span| &span.observation.effects)
            .any(|effect| matches!(effect, ObservedTerminalGridEffect::EraseDisplay { .. }))
    {
        return Err(format!("DECRST 3 did not expose Alacritty grid reset: {unset:?}"));
    }
    Ok(())
}

fn verify_input_width_scroll_paths() -> Result<(), String> {
    let mut combining = Probe::new(4, 3);
    combining.feed(b"\x1b[3;4Hx")?;
    let history_before = combining.term.total_lines() - combining.term.screen_lines();
    let combined = combining.feed("\u{0301}".as_bytes())?;
    let history_after = combining.term.total_lines() - combining.term.screen_lines();
    let combined_effects: Vec<_> =
        combined.grid_observations.iter().flat_map(|span| &span.observation.effects).collect();
    if history_after != history_before
        || combining.observation().primary.cursor
            != Some(TerminalCursorObservation { row: 2, column: 3, input_needs_wrap: true })
        || combined_effects
            .iter()
            .any(|effect| matches!(effect, ObservedTerminalGridEffect::Scroll { .. }))
    {
        return Err(format!(
            "zero-width input did not preserve Alacritty's early-return path: history={history_before}->{history_after} effects={combined_effects:?} observation={:?}",
            combining.observation()
        ));
    }

    let mut wide = Probe::new(4, 3);
    wide.feed(b"\x1b[3;4H")?;
    let wide_history_before = wide.term.total_lines() - wide.term.screen_lines();
    let wrapped = wide.feed("界".as_bytes())?;
    let wide_history_after = wide.term.total_lines() - wide.term.screen_lines();
    let scrolls = wrapped
        .grid_observations
        .iter()
        .flat_map(|span| &span.observation.effects)
        .filter(|effect| {
            matches!(
                effect,
                ObservedTerminalGridEffect::Scroll {
                    screen: TerminalScreenKind::Primary,
                    top: 0,
                    bottom: 3,
                    rows: 1,
                }
            )
        })
        .count();
    if wide_history_after != wide_history_before.saturating_add(1)
        || scrolls != 1
        || wide.observation().primary.cursor
            != Some(TerminalCursorObservation { row: 2, column: 2, input_needs_wrap: false })
    {
        return Err(format!(
            "wide input did not expose Alacritty's last-column wrapline: history={wide_history_before}->{wide_history_after} scrolls={scrolls} observation={:?}",
            wide.observation()
        ));
    }
    Ok(())
}

fn verify_ordered_boundary_cuts() -> Result<(), String> {
    const BOUNDARIES: usize = 1_024;
    let mut probe = Probe::new(8, 4);
    let mut bytes = Vec::new();
    for _ in 0..BOUNDARIES {
        bytes.extend_from_slice(b"\x1b_Ga=q,f=24,s=1,v=1,i=1;/wAA\x1b\\");
    }
    let mut commit = probe.images.process_bytes(&bytes).map_err(|error| error.to_string())?;
    let original_outputs = std::mem::take(&mut commit.outputs);
    let mut duplicated = Vec::with_capacity(original_outputs.len().saturating_mul(2));
    for output in original_outputs {
        duplicated.push(output.clone());
        if matches!(output, SessionTerminalOutput::Image { .. }) {
            duplicated.push(output);
        }
    }
    commit.outputs = duplicated;
    feed_terminal_observed(
        &probe.observer,
        &mut probe.term,
        &mut probe.processor,
        &bytes,
        &mut commit,
    );
    if commit.grid_observations.len() != BOUNDARIES
        || commit.grid_observations.windows(2).any(|pair| match pair {
            [before, after] => before.range.end != after.range.start,
            _ => false,
        })
        || commit.grid_observations.last().map(|span| span.range.end)
            != Some(commit.input_range.end)
    {
        return Err(format!(
            "ordered boundary cuts were reordered or not linearly deduplicated: outputs={} observations={} final={:?}",
            commit.outputs.len(),
            commit.grid_observations.len(),
            commit.grid_observations.last().map(|span| span.range)
        ));
    }
    Ok(())
}

fn verify_image_error_observed_once() -> Result<(), String> {
    let policy = TerminalImageProcessPolicy::with_sequence_ceiling_for_validation(0);
    let mut probe = Probe::with_policy(8, 5, policy);
    let bytes = b"\x1b[?7l\x1b[2;4r\x1b[4;2H\n\x1b_Ga=q,f=24,s=1,v=1,i=9;/wAA\x1b\\";
    let client = Rc::new(RefCell::new(Vec::new()));
    let client_delivery_calls = Rc::new(Cell::new(0_u64));
    let term_feed_calls = Rc::new(Cell::new(0_u64));
    let rejections = Rc::new(RefCell::new(Vec::<PtyReaderIngressRejection>::new()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create ingress probe runtime: {error}"))?;
    let term = &mut probe.term;
    let processor = &mut probe.processor;
    let client_sink = Rc::clone(&client);
    let client_counter = Rc::clone(&client_delivery_calls);
    let term_counter = Rc::clone(&term_feed_calls);
    let rejection_sink = Rc::clone(&rejections);
    let result = runtime.block_on(process_pty_reader_ingress(
        &mut probe.images,
        bytes.to_vec(),
        move |delivered| {
            client_counter.set(client_counter.get().saturating_add(1));
            client_sink.borrow_mut().extend_from_slice(delivered);
        },
        move |observer, delivered, mut image_result| async move {
            term_counter.set(term_counter.get().saturating_add(1));
            let observation = feed_terminal_image_result_with_observer(
                &observer,
                term,
                processor,
                delivered.as_ref(),
                &mut image_result,
            );
            (image_result, Some(observation))
        },
        move |rejection| rejection_sink.borrow_mut().push(rejection),
    ));
    if !matches!(result, Err(SessionTerminalError::SequenceExhausted)) {
        return Err(format!("image rejection lost its typed error: {result:?}"));
    }
    assert_active_matches(&probe.term, &probe.observation())?;
    let observation = probe.observation();
    let rejections = rejections.borrow();
    let rejection_text = format!("{rejections:?}");
    if client_delivery_calls.get() != 1
        || client.borrow().as_slice() != bytes
        || term_feed_calls.get() != 1
        || rejections.as_slice()
            != [PtyReaderIngressRejection {
                error: SessionTerminalError::SequenceExhausted,
                image_sequence: scribe_common::terminal_images::TerminalOutputSequence(0),
            }]
        || rejection_text.contains("/wAA")
        || rejection_text.contains("Ga=q")
        || observation.line_wrap_mode
        || (observation.margin_top, observation.margin_bottom) != (1, 4)
        || observation.primary.cursor
            != Some(TerminalCursorObservation { row: 3, column: 1, input_needs_wrap: false })
        || !observation.effects.iter().any(|effect| {
            matches!(
                effect,
                ObservedTerminalGridEffect::Scroll {
                    screen: TerminalScreenKind::Primary,
                    top: 1,
                    bottom: 4,
                    rows: 1,
                }
            )
        })
    {
        return Err(format!(
            "rejected image chunk bypassed production ingress or duplicated a sink: client_calls={} client_bytes={} term_calls={} rejection_count={} observation={observation:?}",
            client_delivery_calls.get(),
            client.borrow().len(),
            term_feed_calls.get(),
            rejections.len(),
        ));
    }
    Ok(())
}

fn verify_synchronized_update_timeout() -> Result<(), String> {
    let mut probe = Probe::new(6, 5);
    probe.feed(b"\x1b[?2026h")?;
    let buffered = probe.feed(b"\x1b[2;4r\x1b[4;1H\n")?;
    if probe.processor.sync_bytes_count() == 0
        || buffered.grid_observations.iter().any(|span| !span.observation.effects.is_empty())
    {
        return Err(format!("synchronized update did not remain buffered: {buffered:?}"));
    }
    let flushed = probe.flush_sync()?;
    if probe.processor.sync_bytes_count() != 0
        || (flushed.margin_top, flushed.margin_bottom) != (1, 4)
        || !flushed.effects.iter().any(|effect| {
            matches!(
                effect,
                ObservedTerminalGridEffect::Scroll {
                    screen: TerminalScreenKind::Primary,
                    top: 1,
                    bottom: 4,
                    rows: 1,
                }
            )
        })
    {
        return Err(format!("timeout flush bypassed production observer: {flushed:?}"));
    }
    Ok(())
}

fn verify_both_grid_resize() -> Result<(), String> {
    let mut probe = Probe::new(10, 6);
    probe.feed(b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n\x1b[3;4H\x1b7")?;
    probe.feed(b"\x1b[?1049h\x1b[5;6H\x1b7")?;
    probe.resize(8, 4);
    let resized = probe.observation();
    if (resized.primary.size.columns, resized.primary.size.rows) != (8, 4)
        || (resized.alternate.size.columns, resized.alternate.size.rows) != (8, 4)
        || resized.primary.cursor.is_some()
        || resized.primary.saved_cursor.is_some()
        || resized.alternate.cursor.is_none()
        || resized.alternate.saved_cursor.is_none()
        || !matches!(resized.effects.as_slice(), [ObservedTerminalGridEffect::Resize { .. }])
    {
        return Err(format!("both-grid resize observation drifted: {resized:?}"));
    }
    probe.feed(b"\x1b[?1049l")?;
    assert_active_matches(&probe.term, &probe.observation())?;
    let primary = probe.observation().primary;
    if primary.cursor != Some(direct_cursor(&probe.term, false))
        || primary.saved_cursor != Some(direct_cursor(&probe.term, true))
    {
        return Err(format!(
            "activated primary did not refresh reflowed cursor facts: {primary:?}"
        ));
    }
    if (probe.term.columns(), probe.term.screen_lines()) != (8, 4) {
        return Err("inactive primary grid was not resized by real Term".to_owned());
    }

    probe.resize(7, 3);
    let primary_active = probe.observation();
    if primary_active.alternate.cursor.is_some() || primary_active.alternate.saved_cursor.is_some()
    {
        return Err(format!(
            "inactive alternate cursor was synthesized after resize: {primary_active:?}"
        ));
    }
    probe.feed(b"\x1b[?1049h")?;
    assert_active_matches(&probe.term, &probe.observation())?;
    let alternate = probe.observation().alternate;
    if alternate.cursor != Some(direct_cursor(&probe.term, false))
        || alternate.saved_cursor != Some(direct_cursor(&probe.term, true))
    {
        return Err(format!(
            "activated alternate did not refresh resized cursor facts: {alternate:?}"
        ));
    }
    if (probe.term.columns(), probe.term.screen_lines()) != (7, 3) {
        return Err("inactive alternate grid was not resized by real Term".to_owned());
    }
    Ok(())
}
