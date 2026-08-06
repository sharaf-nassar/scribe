//! Round-trip tests for ANSI replay fidelity.
//!
//! Contract: given a `Term` populated with content, running its snapshot
//! through `snapshot_to_ansi` and then through a fresh `AnsiProcessor` +
//! `Term` must reproduce the same grid + scrollback cells. This is the
//! foundation the v5 hot-reload handoff relies on.

use std::fmt::Write as _;

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::TermMode;
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

#[test]
fn roundtrip_ascii_text() {
    let mut src = new_term(80, 24, 100);
    feed(&mut src, b"hello world\r\nsecond line\r\n");

    let snap = snapshot_term(&src);
    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();
    assert_eq!(bytes, snapshot_to_ansi(&snap));

    let mut dst = new_term(80, 24, 100);
    feed(&mut dst, &bytes);

    let snap_dst = snapshot_term(&dst);
    assert_eq!(dst.history_size(), 0, "zero-history replay must not synthesize scrollback");
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
    feed(&mut dst, &bytes);
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
    feed(&mut dst, &bytes);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(dst.history_size(), snap.scrollback_rows as usize);
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
    feed(&mut dst, &bytes);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells);
}

#[test]
fn roundtrip_truecolor_dense_screen() {
    // Every cell gets its own 24-bit fg/bg, so the encoder's SGR diff never
    // coalesces and the replay inflates to ~30 bytes per cell. The retired
    // `cols * rows * 8` decompression bound rejected exactly this payload,
    // which surfaced as a blank session on the far side of a handoff.
    const COLS: usize = 120;
    const ROWS: usize = 30;
    const SCROLLBACK: usize = 200;

    let mut src = new_term(COLS, ROWS, SCROLLBACK);
    let mut painted = String::new();
    for row in 0..(ROWS + SCROLLBACK) {
        for col in 0..COLS {
            let seed = u32::try_from(row * COLS + col).unwrap_or(u32::MAX);
            let [r, g, b, _] = seed.to_le_bytes();
            let ch = char::from(b'!' + u8::try_from(seed % 90).unwrap_or_default());
            write!(painted, "\x1b[38;2;{r};{g};{b}m\x1b[48;2;{b};{r};{g}m{ch}").unwrap();
        }
        painted.push_str("\x1b[0m\r\n");
    }
    feed(&mut src, painted.as_bytes());

    let snap = snapshot_term(&src);
    assert_eq!(snap.scrollback_rows as usize, SCROLLBACK, "fixture must fill scrollback");

    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();
    assert_eq!(bytes, snapshot_to_ansi(&snap));

    let retired_bound = COLS * (ROWS + snap.scrollback_rows as usize) * 8;
    assert!(
        bytes.len() > retired_bound,
        "fixture must exceed the retired 8-bytes-per-cell bound: {} vs {retired_bound}",
        bytes.len()
    );

    let mut dst = new_term(COLS, ROWS, SCROLLBACK);
    feed(&mut dst, &bytes);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells, "visible grid must survive a dense replay");
    assert_eq!(snap.scrollback, snap_dst.scrollback, "scrollback must survive a dense replay");
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
    feed(&mut dst, &bytes);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells, "soft-wrap content must match");
    assert_eq!(snap.scrollback, snap_dst.scrollback);
}

// @lat: [[test#Test Harness#Replay Observation#Replay replaces terminal state]]
#[test]
fn replay_replaces_dirty_primary_and_alternate_destinations() {
    let mut primary_src = new_term(12, 4, 20);
    feed(&mut primary_src, b"fresh primary");
    let primary = snapshot_term(&primary_src);

    let mut dirty_alt = new_term(12, 4, 20);
    for row in 0..8 {
        feed(&mut dirty_alt, format!("old {row}\r\n").as_bytes());
    }
    feed(&mut dirty_alt, b"\x1b[2;3r\x1b[?6h\x1b[?1049hdirty alt");
    feed(&mut dirty_alt, &snapshot_to_ansi(&primary));

    assert!(!dirty_alt.mode().contains(TermMode::ALT_SCREEN));
    assert!(!dirty_alt.mode().contains(TermMode::ORIGIN));
    assert_eq!(dirty_alt.history_size(), 0);
    let rebuilt_primary = snapshot_term(&dirty_alt);
    assert_eq!(rebuilt_primary.cells, primary.cells);
    assert_eq!(rebuilt_primary.cursor_row, primary.cursor_row);
    assert_eq!(rebuilt_primary.cursor_col, primary.cursor_col);

    let mut alt_src = new_term(12, 4, 20);
    feed(&mut alt_src, b"\x1b[?1049halt snapshot");
    let alternate = snapshot_term(&alt_src);
    assert!(alternate.alt_screen);

    let mut dirty_primary = new_term(12, 4, 20);
    for row in 0..8 {
        feed(&mut dirty_primary, format!("stale {row}\r\n").as_bytes());
    }
    feed(&mut dirty_primary, b"\x1b[2;3r\x1b[?6h");
    feed(&mut dirty_primary, &snapshot_to_ansi(&alternate));

    assert!(dirty_primary.mode().contains(TermMode::ALT_SCREEN));
    assert!(!dirty_primary.mode().contains(TermMode::ORIGIN));
    assert_eq!(dirty_primary.history_size(), 0);
    let rebuilt_alternate = snapshot_term(&dirty_primary);
    assert_eq!(rebuilt_alternate.cells, alternate.cells);
    assert_eq!(rebuilt_alternate.cursor_row, alternate.cursor_row);
    assert_eq!(rebuilt_alternate.cursor_col, alternate.cursor_col);
}

#[test]
fn short_snapshot_clears_omitted_dirty_cells() {
    let mut src = new_term(10, 4, 20);
    feed(&mut src, b"Q");
    let mut snapshot = snapshot_term(&src);
    snapshot.cells.truncate(1);

    let mut dst = new_term(10, 4, 20);
    feed(&mut dst, b"XXXXXXXXXX\r\nXXXXXXXXXX\r\nXXXXXXXXXX\r\nXXXXXXXXXX");
    feed(&mut dst, &snapshot_to_ansi(&snapshot));

    let rebuilt = snapshot_term(&dst);
    assert_eq!(rebuilt.cells[0].c, 'Q');
    assert!(rebuilt.cells[1..].iter().all(|cell| cell.c == ' '));
    assert_eq!(dst.history_size(), 0);
}
