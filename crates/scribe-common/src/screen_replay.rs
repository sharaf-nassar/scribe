//! ANSI replay encoding for `ScreenSnapshot`.
//!
//! Produces a byte stream that, when fed through a VTE parser into a fresh
//! `Term`, reconstructs the snapshot's scrollback + visible grid, SGR state,
//! cursor, and alt-screen flag. Used on both the client reconnect path and
//! the server hot-reload handoff path.

use serde::{Deserialize, Serialize};

use crate::screen::{CellFlags, CursorStyle, ScreenCell, ScreenColor, ScreenSnapshot};

// ── Wire type for hot-reload handoff ────────────────────────────────

/// Per-session replay payload for v5+ hot-reload handoff.
///
/// Transports the session's visible grid plus scrollback as a zstd-compressed
/// ANSI byte stream produced by `snapshot_to_ansi`. The receiver feeds the
/// decompressed bytes through `vte::ansi::Processor::advance` into a fresh
/// `Term`, which reconstructs the grid and scrollback durably.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReplay {
    pub cols: u16,
    pub rows: u16,
    pub scrollback_rows: u32,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cursor_style: CursorStyle,
    pub cursor_visible: bool,
    pub alt_screen: bool,
    /// zstd-compressed ANSI replay bytes (output of `snapshot_to_ansi`).
    pub replay_zstd: Vec<u8>,
}

/// Compression level. Level 3 is the zstd default; tuned for fast encode with
/// good ratio on repetitive terminal content.
const ZSTD_LEVEL: i32 = 3;

/// Build a `SessionReplay` from a `ScreenSnapshot`.
///
/// Runs `snapshot_to_ansi` and compresses the result with zstd at level 3.
///
/// # Errors
/// Returns an `io::Error` if zstd fails to compress the input. The in-memory
/// `zstd::bulk::compress` path only fails on allocator errors in practice.
pub fn build_session_replay(snapshot: &ScreenSnapshot) -> std::io::Result<SessionReplay> {
    let ansi = snapshot_to_ansi(snapshot);
    let replay_zstd = zstd::bulk::compress(&ansi, ZSTD_LEVEL)?;
    Ok(SessionReplay {
        cols: snapshot.cols,
        rows: snapshot.rows,
        scrollback_rows: snapshot.scrollback_rows,
        cursor_col: snapshot.cursor_col,
        cursor_row: snapshot.cursor_row,
        cursor_style: snapshot.cursor_style,
        cursor_visible: snapshot.cursor_visible,
        alt_screen: snapshot.alt_screen,
        replay_zstd,
    })
}

/// Decompress a `SessionReplay`'s replay bytes into a plain ANSI byte buffer.
///
/// # Errors
/// Returns an `io::Error` if zstd decompression fails (corrupted stream or
/// capacity exhausted).
pub fn decompress_session_replay(replay: &SessionReplay) -> std::io::Result<Vec<u8>> {
    // Capacity hint: ~8 bytes per cell upper bound, minimum 64 KiB to avoid
    // thrashing on small payloads.
    let total_rows = usize::from(replay.rows).saturating_add(replay.scrollback_rows as usize);
    let hint = usize::from(replay.cols).saturating_mul(total_rows).saturating_mul(8);
    let capacity = hint.max(64 * 1024);
    zstd::bulk::decompress(&replay.replay_zstd, capacity)
}

// ── SGR diff state ──────────────────────────────────────────────────

/// Tracks the "current" SGR state while emitting ANSI for a snapshot.
///
/// Allows diff-based emission: only emit a new SGR escape when the next cell's
/// attributes differ from the currently-active attributes, avoiding a full
/// `\x1b[0m` reset for every cell.
struct SgrState {
    fg: ScreenColor,
    bg: ScreenColor,
    flags: CellFlags,
}

impl SgrState {
    /// Initial state: all flags off, colors are the terminal defaults
    /// (`Named(256)` = Foreground, `Named(257)` = Background in alacritty's
    /// `NamedColor` numbering).
    fn default_state() -> Self {
        Self {
            fg: ScreenColor::Named(256),
            bg: ScreenColor::Named(257),
            flags: CellFlags::default(),
        }
    }

    /// Returns `true` if the cell's attributes exactly match the current state.
    fn matches(&self, cell: &ScreenCell) -> bool {
        self.fg == cell.fg
            && self.bg == cell.bg
            && self.flags.bold() == cell.flags.bold()
            && self.flags.dim() == cell.flags.dim()
            && self.flags.italic() == cell.flags.italic()
            && self.flags.underline() == cell.flags.underline()
            && self.flags.inverse() == cell.flags.inverse()
            && self.flags.hidden() == cell.flags.hidden()
            && self.flags.strikethrough() == cell.flags.strikethrough()
    }

    /// Update state to match the given cell's attributes.
    fn update(&mut self, cell: &ScreenCell) {
        self.fg = cell.fg;
        self.bg = cell.bg;
        self.flags.set_bold(cell.flags.bold());
        self.flags.set_dim(cell.flags.dim());
        self.flags.set_italic(cell.flags.italic());
        self.flags.set_underline(cell.flags.underline());
        self.flags.set_inverse(cell.flags.inverse());
        self.flags.set_hidden(cell.flags.hidden());
        self.flags.set_strikethrough(cell.flags.strikethrough());
    }
}

// ── Encoder ─────────────────────────────────────────────────────────

/// Convert a `ScreenSnapshot` to ANSI escape sequences that reproduce the
/// visible screen content when fed through a VTE parser.
///
/// Used by the client on reconnect replay and by the server's hot-reload
/// handoff sender to build a compact, human-inspectable representation that
/// can be fed back through `vte::ansi::Processor` to rebuild the grid and
/// scrollback history durably.
#[must_use]
pub fn snapshot_to_ansi(snapshot: &ScreenSnapshot) -> Vec<u8> {
    let cols = usize::from(snapshot.cols);
    let scrollback_rows = usize::try_from(snapshot.scrollback_rows).unwrap_or(usize::MAX);
    let visible_rows = usize::from(snapshot.rows);

    let mut buf = String::with_capacity((scrollback_rows + visible_rows) * cols * 4);

    // If the server was in alternate screen mode, switch the client into it
    // so that subsequent PTY output (which assumes alt screen) lands in the
    // correct buffer.  Without this, apps like Claude Code that use alt screen
    // produce ghost cursors and broken exit behaviour after reconnect.
    if snapshot.alt_screen {
        buf.push_str("\x1b[?1049h");
    }

    // Hide cursor, move home, clear screen, reset attributes.
    buf.push_str("\x1b[?25l\x1b[H\x1b[2J\x1b[0m");

    let mut wrote_row = false;
    let mut previous_row_wrapped = false;

    // SGR diff state: start from the known-reset state (we just emitted \x1b[0m
    // above), so the first cell will only emit SGR if it differs from defaults.
    let mut sgr_state = SgrState::default_state();

    // --- Scrollback lines (oldest first) ---
    // As these overflow the visible area, they naturally flow into the
    // receiving Term's scrollback buffer — the same mechanism as normal use.
    for row in 0..scrollback_rows {
        if wrote_row && !previous_row_wrapped {
            buf.push_str("\r\n");
        }
        write_snapshot_row(&mut buf, &snapshot.scrollback, row, cols, &mut sgr_state);
        previous_row_wrapped = row_wraps(&snapshot.scrollback, row, cols);
        wrote_row = true;
    }

    // --- Visible lines ---
    for row in 0..visible_rows {
        if wrote_row && !previous_row_wrapped {
            buf.push_str("\r\n");
        }
        write_snapshot_row(&mut buf, &snapshot.cells, row, cols, &mut sgr_state);
        previous_row_wrapped = row_wraps(&snapshot.cells, row, cols);
        wrote_row = true;
    }

    // Reset attributes, position cursor, show cursor if visible.
    buf.push_str("\x1b[0m");
    write_string(
        &mut buf,
        format_args!(
            "\x1b[{};{}H",
            u32::from(snapshot.cursor_row) + 1,
            u32::from(snapshot.cursor_col) + 1,
        ),
    );
    // For alt screen snapshots, leave the cursor hidden and skip DECSCUSR —
    // the alt screen app (e.g. Claude Code, vim) will control cursor
    // visibility and shape through its own live PTY output.  Emitting them
    // here causes a "double cursor": the terminal cursor overlaps with the
    // app's own drawn cursor.
    if !snapshot.alt_screen {
        if snapshot.cursor_visible {
            buf.push_str("\x1b[?25h");
        }
        // Restore cursor shape via DECSCUSR so reconnect preserves the style
        // that was active in the session (e.g. beam in a text editor).
        let decscusr = match snapshot.cursor_style {
            crate::screen::CursorStyle::Block => "\x1b[2 q",
            crate::screen::CursorStyle::Beam => "\x1b[6 q",
            crate::screen::CursorStyle::Underline => "\x1b[4 q",
            crate::screen::CursorStyle::HollowBlock => "\x1b[1 q",
        };
        buf.push_str(decscusr);
    }

    buf.into_bytes()
}

/// Write a single row of cells as ANSI escape sequences.
///
/// `sgr_state` tracks the currently-active SGR attributes across calls so that
/// unchanged runs of cells can skip emitting a redundant escape sequence.
fn write_snapshot_row(
    buf: &mut String,
    cells: &[ScreenCell],
    row: usize,
    cols: usize,
    sgr_state: &mut SgrState,
) {
    for col in 0..cols {
        let idx = row * cols + col;
        let Some(cell) = cells.get(idx) else { break };

        // Skip spacer cells for wide characters.
        let is_wide_spacer =
            col > 0 && cells.get(row * cols + col - 1).is_some_and(|c| c.flags.wide());
        if is_wide_spacer {
            continue;
        }

        // Only emit SGR when this cell's attributes differ from the current
        // state.  Terminals preserve SGR across line breaks, so the state
        // carries over between rows without resetting.
        if !sgr_state.matches(cell) {
            write_sgr(buf, cell);
            sgr_state.update(cell);
        }

        // Write the character (space for null/empty cells).
        if cell.c == '\0' || cell.c == ' ' {
            buf.push(' ');
        } else {
            buf.push(cell.c);
        }
    }
}

/// Whether the given row soft-wraps into the next row.
fn row_wraps(cells: &[ScreenCell], row: usize, cols: usize) -> bool {
    if cols == 0 {
        return false;
    }

    row.checked_mul(cols)
        .and_then(|base| base.checked_add(cols - 1))
        .and_then(|idx| cells.get(idx))
        .is_some_and(|cell| cell.flags.wrap())
}

/// Write SGR escape sequences for a cell's foreground, background, and flags.
fn write_sgr(buf: &mut String, cell: &ScreenCell) {
    buf.push_str("\x1b[0"); // reset, then append attributes

    let f = &cell.flags;
    if f.bold() {
        buf.push_str(";1");
    }
    if f.dim() {
        buf.push_str(";2");
    }
    if f.italic() {
        buf.push_str(";3");
    }
    if f.underline() {
        buf.push_str(";4");
    }
    if f.inverse() {
        buf.push_str(";7");
    }
    if f.hidden() {
        buf.push_str(";8");
    }
    if f.strikethrough() {
        buf.push_str(";9");
    }

    write_color_sgr(buf, cell.fg, true);
    write_color_sgr(buf, cell.bg, false);

    buf.push('m');
}

/// Append the SGR parameters for a single color (foreground or background).
///
/// `NamedColor` values: 0–7 = normal ANSI, 8–15 = bright ANSI,
/// 256 = Foreground, 257 = Background, 258 = Cursor, 259–266 = dim variants.
/// Values >= 16 use the terminal default colour (SGR 39/49).
fn write_color_sgr(buf: &mut String, color: ScreenColor, foreground: bool) {
    match color {
        ScreenColor::Named(n) if n < 8 => {
            let base: u32 = if foreground { 30 } else { 40 };
            write_string(buf, format_args!(";{}", base + u32::from(n)));
        }
        ScreenColor::Named(n) if n < 16 => {
            let base: u32 = if foreground { 90 } else { 100 };
            write_string(buf, format_args!(";{}", base + u32::from(n - 8)));
        }
        ScreenColor::Named(_) => {
            // Foreground (256), Background (257), Cursor (258), Dim* (259+)
            // — use the terminal's default colour.
            buf.push_str(if foreground { ";39" } else { ";49" });
        }
        ScreenColor::Indexed(idx) => {
            let prefix = if foreground { "38" } else { "48" };
            write_string(buf, format_args!(";{prefix};5;{idx}"));
        }
        ScreenColor::Rgb { r, g, b } => {
            let prefix = if foreground { "38" } else { "48" };
            write_string(buf, format_args!(";{prefix};2;{r};{g};{b}"));
        }
    }
}

fn write_string(buf: &mut String, args: std::fmt::Arguments<'_>) {
    use std::fmt::Write as _;

    let write_result = buf.write_fmt(args);
    debug_assert!(write_result.is_ok(), "writing to String cannot fail");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{CellFlags, CursorStyle, ScreenCell, ScreenColor, ScreenSnapshot};

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
            scrollback: Vec::new(),
            scrollback_rows: 0,
        }
    }

    #[test]
    fn session_replay_round_trip_preserves_ansi_bytes() {
        let snapshot = snapshot_with_text("hello world");
        let replay = build_session_replay(&snapshot).expect("build_session_replay");
        let decoded = decompress_session_replay(&replay).expect("decompress");
        let direct = snapshot_to_ansi(&snapshot);
        assert_eq!(decoded, direct);
    }

    #[test]
    fn session_replay_compresses_spaces_well() {
        // 80x24 of spaces should zstd down to a few hundred bytes at most.
        let snapshot = snapshot_with_text("");
        let replay = build_session_replay(&snapshot).unwrap();
        assert!(
            replay.replay_zstd.len() < 1024,
            "expected <1024 compressed bytes for blank screen, got {}",
            replay.replay_zstd.len()
        );
    }

    #[test]
    fn session_replay_preserves_metadata_fields() {
        let mut snapshot = snapshot_with_text("x");
        snapshot.cursor_row = 5;
        snapshot.cursor_col = 10;
        snapshot.cursor_style = CursorStyle::Beam;
        snapshot.alt_screen = true;
        snapshot.scrollback_rows = 7;

        let replay = build_session_replay(&snapshot).unwrap();
        assert_eq!(replay.cols, snapshot.cols);
        assert_eq!(replay.rows, snapshot.rows);
        assert_eq!(replay.cursor_row, 5);
        assert_eq!(replay.cursor_col, 10);
        assert!(matches!(replay.cursor_style, CursorStyle::Beam));
        assert!(replay.alt_screen);
        assert_eq!(replay.scrollback_rows, 7);
    }
}
