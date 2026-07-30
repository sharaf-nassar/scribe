//! ANSI replay encoding for `ScreenSnapshot`.
//!
//! Produces a byte stream that, when fed through a VTE parser into a fresh
//! `Term`, reconstructs the snapshot's scrollback + visible grid, SGR state,
//! cursor, and alt-screen flag. Used on both the client reconnect path and
//! the server hot-reload handoff path.

use std::io::Read as _;

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

/// Absolute post-inflate ceiling for one replay payload, in bytes.
///
/// Replays reach a receiver over paths an untrusted peer can drive — LAN client
/// attach and server-to-server handoff — so the decoder must never size its
/// output from anything the sender *declares*: `cols`, `rows`, and
/// `scrollback_rows` are set independently of the bytes actually shipped. The
/// decode streams instead and stops the moment the inflated stream would cross
/// this line. 64 MiB matches [`crate::framing::MAX_MESSAGE_SIZE`]: a replay that
/// could not have fit in one IPC frame has no legitimate consumer.
pub const MAX_REPLAY_INFLATED_BYTES: usize = crate::framing::MAX_MESSAGE_SIZE as usize;

/// Bytes pulled from the decoder per read during a streamed inflate.
const REPLAY_DECODE_CHUNK: usize = 64 * 1024;

/// Floor for the initial output allocation, so small payloads do not start from
/// a handful of bytes and re-grow several times.
const REPLAY_DECODE_MIN_CAPACITY: usize = 64 * 1024;

/// Cap on the *initial* output allocation. Past this the buffer only grows as
/// bytes actually arrive, so a small frame can never reserve the ceiling up
/// front; a legitimate multi-megabyte replay pays a handful of doublings.
const REPLAY_DECODE_MAX_INITIAL_CAPACITY: usize = 4 * 1024 * 1024;

/// First-guess inflate ratio applied to the encoded length to size the output
/// buffer. Purely an allocation hint — the only enforced bound is
/// [`MAX_REPLAY_INFLATED_BYTES`].
const REPLAY_DECODE_SIZE_HINT_RATIO: usize = 8;

/// Largest back-reference window the decoder accepts, as a log2 distance.
///
/// Streaming decode allocates the window the *frame header* declares, so a
/// hostile peer could otherwise buy a multi-gigabyte allocation with a few
/// bytes. [`ZSTD_LEVEL`] caps the encoder's window log at 21 (2 MiB), so 23
/// leaves two doublings of headroom. Raising [`ZSTD_LEVEL`] past a window log
/// of 23 needs an N/N-1 release window first: receivers on the old value would
/// reject the new encoder's frames outright.
const REPLAY_DECODE_WINDOW_LOG_MAX: u32 = 23;

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
/// Streams the zstd frame in fixed chunks and refuses to produce more than
/// [`MAX_REPLAY_INFLATED_BYTES`]. Nothing about the bound comes from the
/// replay's declared geometry, so a dense truecolor screen — which the encoder
/// emits at 40+ bytes per cell — decodes fully instead of degrading to a blank
/// session, while a hostile peer still cannot force an unbounded allocation.
///
/// # Errors
/// Returns an `io::Error` if the zstd stream is corrupt or truncated, or if it
/// inflates past [`MAX_REPLAY_INFLATED_BYTES`].
pub fn decompress_session_replay(replay: &SessionReplay) -> std::io::Result<Vec<u8>> {
    inflate_bounded(&replay.replay_zstd, MAX_REPLAY_INFLATED_BYTES)
}

/// Streamed zstd inflate of `encoded`, refusing to emit more than `ceiling`
/// bytes. Split out from [`decompress_session_replay`] so the bound itself is
/// testable without materialising a 64 MiB buffer.
fn inflate_bounded(encoded: &[u8], ceiling: usize) -> std::io::Result<Vec<u8>> {
    let mut decoder = zstd::stream::read::Decoder::with_buffer(encoded)?;
    decoder.window_log_max(REPLAY_DECODE_WINDOW_LOG_MAX)?;

    let mut out = Vec::with_capacity(initial_inflate_capacity(encoded.len(), ceiling));
    let mut chunk = vec![0_u8; REPLAY_DECODE_CHUNK];

    loop {
        // A frame that ends mid-stream surfaces here as an error rather than a
        // short read, so a truncated payload can never look like a clean EOF.
        let read = decoder.read(&mut chunk)?;
        if read == 0 {
            return Ok(out);
        }
        if read > ceiling - out.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "replay inflates past the {ceiling}-byte ceiling (encoded {} bytes)",
                    encoded.len()
                ),
            ));
        }
        let Some(filled) = chunk.get(..read) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "zstd reported more bytes read than the chunk holds",
            ));
        };
        reserve_within_ceiling(&mut out, read, ceiling);
        out.extend_from_slice(filled);
    }
}

/// Initial output allocation for a streamed inflate.
///
/// Derived from the encoded length — the only size the receiver has actually
/// observed — never from the replay's declared grid, which an untrusted sender
/// controls independently of the payload.
fn initial_inflate_capacity(encoded_len: usize, ceiling: usize) -> usize {
    let upper = REPLAY_DECODE_MAX_INITIAL_CAPACITY.min(ceiling);
    let lower = REPLAY_DECODE_MIN_CAPACITY.min(upper);
    encoded_len.saturating_mul(REPLAY_DECODE_SIZE_HINT_RATIO).clamp(lower, upper)
}

/// Make room for `additional` more bytes, keeping capacity at or under
/// `ceiling` so amortised doubling cannot overshoot the bound the stream is
/// being held to. Callers must already have checked that `out.len() +
/// additional` fits.
fn reserve_within_ceiling(out: &mut Vec<u8>, additional: usize, ceiling: usize) {
    if out.capacity() - out.len() >= additional {
        return;
    }
    let target =
        out.capacity().saturating_mul(2).max(out.len().saturating_add(additional)).min(ceiling);
    out.reserve_exact(target - out.len());
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

    // Re-assert DEC private modes (mouse, bracketed paste, focus, app
    // cursor/keypad) so a reattached TUI keeps them; these set Term modes
    // without altering rendered content.
    for mode in &snapshot.active_dec_modes {
        buf.push_str(mode.set_sequence());
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
    use crate::screen::{
        CellFlags, CursorStyle, DecPrivateMode, ScreenCell, ScreenColor, ScreenSnapshot,
    };

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

    /// A snapshot whose every cell carries its own 24-bit foreground and
    /// background, so the SGR diff never coalesces and the encoder emits 30+
    /// bytes per cell — the density the old 8-bytes-per-cell bound truncated.
    fn truecolor_dense_snapshot(cols: u16, rows: u16) -> ScreenSnapshot {
        let count = usize::from(cols) * usize::from(rows);
        let mut cells = Vec::with_capacity(count);
        for idx in 0..count {
            let seed = u32::try_from(idx).unwrap_or(u32::MAX);
            let [r, g, b, _] = seed.to_le_bytes();
            cells.push(ScreenCell {
                c: char::from(b'a' + u8::try_from(idx % 26).unwrap_or_default()),
                fg: ScreenColor::Rgb { r, g, b },
                bg: ScreenColor::Rgb { r: b, g: r, b: g },
                flags: CellFlags::default(),
            });
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

    #[test]
    fn decompress_bound_ignores_declared_geometry() {
        // Dense enough that the retired `cols * rows * 8` capacity — and its
        // 64 KiB floor — would have failed the decode outright.
        let snapshot = truecolor_dense_snapshot(200, 60);
        let ansi = snapshot_to_ansi(&snapshot);
        assert!(ansi.len() > 64 * 1024, "fixture must exceed the retired floor: {}", ansi.len());

        let mut replay = build_session_replay(&snapshot).expect("build_session_replay");
        // Declared geometry is sender-controlled and unrelated to the payload:
        // shrink it to a single cell and the decode must be unaffected.
        replay.cols = 1;
        replay.rows = 1;
        replay.scrollback_rows = 0;

        let decoded = decompress_session_replay(&replay).expect("decompress");
        assert_eq!(decoded, ansi);
    }

    #[test]
    fn inflate_accepts_a_stream_that_lands_exactly_on_the_ceiling() {
        let ceiling = 100_000;
        let encoded = zstd::bulk::compress(&vec![b'x'; ceiling], ZSTD_LEVEL).unwrap();

        let decoded = inflate_bounded(&encoded, ceiling).expect("inflate at the ceiling");
        assert_eq!(decoded.len(), ceiling);
    }

    #[test]
    fn inflate_rejects_a_stream_one_byte_past_the_ceiling() {
        let ceiling = 100_000;
        let encoded = zstd::bulk::compress(&vec![b'x'; ceiling + 1], ZSTD_LEVEL).unwrap();

        let error = inflate_bounded(&encoded, ceiling).expect_err("must refuse the overflow");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("ceiling"), "{error}");
    }

    #[test]
    fn inflate_rejects_a_decompression_bomb() {
        // 32 MiB of zeros compresses to a few hundred bytes; a peer-declared
        // size plays no part, so the streamed bound is what stops it.
        let encoded = zstd::bulk::compress(&vec![0_u8; 32 * 1024 * 1024], ZSTD_LEVEL).unwrap();
        assert!(encoded.len() < 64 * 1024, "bomb fixture must stay tiny: {}", encoded.len());

        let error = inflate_bounded(&encoded, 1024 * 1024).expect_err("must refuse the bomb");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn inflate_rejects_a_truncated_frame() {
        let encoded = zstd::bulk::compress(&vec![b'y'; 200_000], ZSTD_LEVEL).unwrap();
        let truncated = &encoded[..encoded.len() / 2];

        // A short read must not read back as a clean end of stream: replaying
        // half an ANSI stream would leave a garbled grid, not a blank one.
        inflate_bounded(truncated, MAX_REPLAY_INFLATED_BYTES)
            .expect_err("truncated frame must not decode");
    }

    #[test]
    fn inflate_rejects_an_oversized_declared_window() {
        use std::io::Write as _;

        // Streaming decode allocates whatever window the frame header declares,
        // so a header past the cap must be refused before that allocation.
        let mut wide = zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL).unwrap();
        wide.window_log(REPLAY_DECODE_WINDOW_LOG_MAX + 1).unwrap();
        wide.write_all(b"tiny").unwrap();
        let encoded = wide.finish().unwrap();

        inflate_bounded(&encoded, MAX_REPLAY_INFLATED_BYTES)
            .expect_err("window log past the cap must be refused");
    }

    #[test]
    fn replay_ceiling_matches_the_ipc_frame_limit() {
        assert_eq!(MAX_REPLAY_INFLATED_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_REPLAY_INFLATED_BYTES, crate::framing::MAX_MESSAGE_SIZE as usize);
    }

    #[test]
    fn initial_capacity_never_reserves_the_whole_ceiling() {
        // A 1 MiB frame that could inflate to the ceiling still starts small.
        let capacity = initial_inflate_capacity(1024 * 1024, MAX_REPLAY_INFLATED_BYTES);
        assert!(capacity <= REPLAY_DECODE_MAX_INITIAL_CAPACITY, "{capacity}");
        assert!(capacity >= REPLAY_DECODE_MIN_CAPACITY, "{capacity}");
        // A tiny ceiling must clamp the hint rather than over-reserve.
        assert_eq!(initial_inflate_capacity(16, 4096), 4096);
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

    /// Returns true if `haystack` contains `needle` as a contiguous subslice.
    fn contains_seq(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// Every DEC-private-mode emission the encoder supports, paired with the
    /// `ScreenSnapshot` flag that gates it. Exercised by the all-on / all-off
    /// tests below so the full set stays covered.
    const DEC_PRIVATE_MODE_SEQS: &[&[u8]] = &[
        b"\x1b[?1000h", // mouse_report_click
        b"\x1b[?1002h", // mouse_button_event
        b"\x1b[?1003h", // mouse_any_motion
        b"\x1b[?1006h", // sgr_mouse
        b"\x1b[?1005h", // utf8_mouse
        b"\x1b[?1007h", // alternate_scroll
        b"\x1b[?2004h", // bracketed_paste
        b"\x1b[?1004h", // focus_event
        b"\x1b[?1h",    // app_cursor (DECCKM)
        b"\x1b=",       // app_keypad (DECPAM)
    ];

    #[test]
    fn snapshot_to_ansi_emits_enabled_dec_private_modes() {
        let mut snapshot = snapshot_with_text("x");
        // Enable ALL ten DEC private modes.
        snapshot.active_dec_modes = vec![
            DecPrivateMode::MouseReportClick,
            DecPrivateMode::MouseButtonEvent,
            DecPrivateMode::MouseAnyMotion,
            DecPrivateMode::SgrMouse,
            DecPrivateMode::Utf8Mouse,
            DecPrivateMode::AlternateScroll,
            DecPrivateMode::BracketedPaste,
            DecPrivateMode::FocusEvent,
            DecPrivateMode::AppCursor,
            DecPrivateMode::AppKeypad,
        ];

        let ansi = snapshot_to_ansi(&snapshot);
        for seq in DEC_PRIVATE_MODE_SEQS {
            assert!(
                contains_seq(&ansi, seq),
                "expected DEC private mode sequence {seq:?} when its flag is set"
            );
        }
    }

    #[test]
    fn snapshot_to_ansi_omits_disabled_dec_private_modes() {
        // Default helper leaves `active_dec_modes` empty.
        let snapshot = snapshot_with_text("x");

        let ansi = snapshot_to_ansi(&snapshot);
        for seq in DEC_PRIVATE_MODE_SEQS {
            assert!(
                !contains_seq(&ansi, seq),
                "unexpected DEC private mode sequence {seq:?} when all flags are false"
            );
        }
    }
}
