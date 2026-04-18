//! Round-trip tests for ANSI replay fidelity.
//!
//! Contract: given a `Term` populated with content, running its snapshot
//! through `snapshot_to_ansi` and then through a fresh `AnsiProcessor` +
//! `Term` must reproduce the same grid + scrollback cells. This is the
//! foundation the v5 hot-reload handoff relies on.

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use scribe_common::ids::SessionId;
use scribe_common::screen_replay::{
    build_session_replay, decompress_session_replay, snapshot_to_ansi,
};
use scribe_pty::event_listener::ScribeEventListener;
use scribe_server::session_manager::{build_term_config, snapshot_term};
use tokio::sync::mpsc;
use vte::ansi::Processor as AnsiProcessor;

#[derive(Clone, Copy)]
struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
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

fn new_term(cols: usize, rows: usize, scrollback: usize) -> Term<ScribeEventListener> {
    let (tx, _rx) = mpsc::unbounded_channel();
    let listener = ScribeEventListener::new(SessionId::new(), tx);
    let config = build_term_config(scrollback);
    Term::new(config, &Dims { cols, rows }, listener)
}

/// Drive a Term with a byte stream via the same `AnsiProcessor` path the server
/// uses for real PTY bytes.
fn feed(term: &mut Term<ScribeEventListener>, bytes: &[u8]) {
    let mut processor: AnsiProcessor = AnsiProcessor::new();
    processor.advance(term, bytes);
}

/// Feed a compressed replay into a fresh Term and trim pseudo-scrollback
/// introduced by the encoder's leading `\x1b[2J` (which scrolls the blank
/// viewport into history on a fresh grid). Mirrors the trim the client
/// performs in response to the server's `TrimScrollback` event.
fn feed_replay(
    term: &mut Term<ScribeEventListener>,
    bytes: &[u8],
    expected_scrollback_rows: u32,
    max_scrollback: usize,
) {
    feed(term, bytes);

    let kept_rows = (expected_scrollback_rows as usize).min(max_scrollback);
    let grid = term.grid_mut();
    grid.update_history(kept_rows);
    grid.update_history(max_scrollback);
}

#[test]
fn roundtrip_ascii_text() {
    let mut src = new_term(80, 24, 100);
    feed(&mut src, b"hello world\r\nsecond line\r\n");

    let snap = snapshot_term(&src);
    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();
    assert_eq!(bytes, snapshot_to_ansi(&snap));

    let mut dst = new_term(80, 24, 100);
    feed_replay(&mut dst, &bytes, snap.scrollback_rows, 100);

    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells, "visible grid must match");
    assert_eq!(snap.scrollback, snap_dst.scrollback, "scrollback must match");
    assert_eq!(snap.cursor_row, snap_dst.cursor_row);
    assert_eq!(snap.cursor_col, snap_dst.cursor_col);
}

#[test]
fn roundtrip_sgr_attributes() {
    let mut src = new_term(80, 24, 100);
    feed(&mut src, b"\x1b[1mbold\x1b[0m normal \x1b[4;31munderlined red\x1b[0m\r\n");
    let snap = snapshot_term(&src);
    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();

    let mut dst = new_term(80, 24, 100);
    feed_replay(&mut dst, &bytes, snap.scrollback_rows, 100);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells);
}

#[test]
fn roundtrip_scrollback_overflow() {
    // Print 50 rows to force scrollback in a 10-row window.
    let mut src = new_term(80, 10, 100);
    for i in 0..50 {
        let line = format!("line {i:02}\r\n");
        feed(&mut src, line.as_bytes());
    }
    let snap = snapshot_term(&src);
    assert!(snap.scrollback_rows > 0, "scrollback must contain prior rows");

    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();

    let mut dst = new_term(80, 10, 100);
    feed_replay(&mut dst, &bytes, snap.scrollback_rows, 100);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.scrollback, snap_dst.scrollback);
    assert_eq!(snap.cells, snap_dst.cells);
}

#[test]
fn roundtrip_wide_chars() {
    let mut src = new_term(80, 24, 100);
    feed(&mut src, "hello 世界\r\n".as_bytes());
    let snap = snapshot_term(&src);
    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();

    let mut dst = new_term(80, 24, 100);
    feed_replay(&mut dst, &bytes, snap.scrollback_rows, 100);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells);
}

#[test]
fn roundtrip_soft_wrap() {
    // 20-col grid with a 50-char line forces a soft wrap (WRAPLINE flag).
    let mut src = new_term(20, 5, 100);
    let long: String = "a".repeat(50);
    feed(&mut src, long.as_bytes());
    feed(&mut src, b"\r\n");
    let snap = snapshot_term(&src);

    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();

    let mut dst = new_term(20, 5, 100);
    feed_replay(&mut dst, &bytes, snap.scrollback_rows, 100);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells, "soft-wrap content must match");
    assert_eq!(snap.scrollback, snap_dst.scrollback);
}
