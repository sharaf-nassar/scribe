//! URL scanning and per-pane URL span cache.
//!
//! Scans the visible terminal grid for URLs and maintains a dirty-flag cache
//! so URL hit-testing can be performed without re-scanning every frame.
//!
//! Ported byte-for-byte from the legacy `scribe-client` `url_detect` module
//! onto Zed's Alacritty fork so URL/OSC 8 detection stays identical across the
//! GPUI cutover. The only structural change is that the two grid cell readers
//! ([`read_cell_char`], [`read_cell_flags`]) are defined locally here instead
//! of imported from `selection` — the selection port lands in a separate bead,
//! and these helpers carry no state.

use alacritty_terminal_gpui::event::VoidListener;
use alacritty_terminal_gpui::grid::Dimensions as _;
use alacritty_terminal_gpui::index::{Column, Line, Point};
use alacritty_terminal_gpui::term::Term;
use alacritty_terminal_gpui::term::cell::{Cell, Flags, Hyperlink};

/// Read a single cell from the terminal grid.
///
/// The `alacritty_terminal` grid only exposes `Index`, with no fallible
/// `.get()` alternative, so indexing is required here — matching the direct
/// grid indexing the display snapshot path already relies on.
fn read_cell(term: &Term<VoidListener>, line: Line, col: Column) -> &Cell {
    &term.grid()[line][col]
}

/// Read a single cell character from the terminal grid.
fn read_cell_char(term: &Term<VoidListener>, line: Line, col: Column) -> char {
    read_cell(term, line, col).c
}

/// Read the flags of a single cell from the terminal grid.
fn read_cell_flags(term: &Term<VoidListener>, line: Line, col: Column) -> Flags {
    read_cell(term, line, col).flags
}

/// Whether a detected span is a URL or a file-system path.
#[derive(Clone, Copy)]
pub enum SpanKind {
    /// An OSC 8 explicit hyperlink emitted by the program (spec 009 FR-002).
    /// Takes precedence over heuristic detection on overlapping cells
    /// (FR-004) and surfaces the verbatim URI in tooltips and the "Copy
    /// hyperlink address" context-menu entry (FR-006, FR-007).
    Osc8Hyperlink,
    /// A recognised URL (`https://`, `http://`, `ftp://`, `file://`).
    Url,
    /// A file-system path (`/abs`, `~/`, `./`, `../`, or bare `word/path`).
    Path,
}

/// Inclusive column range a span occupies on one grid row.
///
/// Spans are not rectangles: a hard-break continuation row starts at the
/// continuation indent (e.g. after a program-drawn gutter), not column 0,
/// so hit-testing and underline drawing consume these per-row segments
/// instead of deriving geometry from the span's bounding fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowSegment {
    /// Absolute grid row (0 = viewport top, negative = scrollback).
    pub row: i32,
    /// Column of the first span cell on this row (inclusive).
    pub col_start: usize,
    /// Column of the last span cell on this row (inclusive).
    pub col_end: usize,
}

/// A URL or file path found on the terminal grid.
#[derive(Clone)]
pub struct UrlSpan {
    /// Absolute grid row (0 = viewport top, negative = scrollback).
    pub row: i32,
    /// Column of the first character of the URL (inclusive).
    pub col_start: usize,
    /// Absolute grid row containing the last character of the URL.
    pub row_end: i32,
    /// Column of the last character of the URL (inclusive).
    pub col_end: usize,
    /// The URL or path text.
    pub url: String,
    /// Whether this span is a URL or a file path.
    pub kind: SpanKind,
    /// Exact per-row cell coverage; `row`/`col_start`/`row_end`/`col_end`
    /// remain the bounding endpoints for identity comparison and ordering.
    pub segments: Vec<RowSegment>,
}

/// Per-pane cache of detected URL spans.
pub struct PaneUrlCache {
    spans: Vec<UrlSpan>,
    dirty: bool,
}

impl Default for PaneUrlCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneUrlCache {
    pub fn new() -> Self {
        Self { spans: Vec::new(), dirty: true }
    }

    /// Mark the cache as needing a re-scan.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Re-scan visible rows if the cache is dirty, then clear the dirty flag.
    pub fn refresh(&mut self, term: &Term<VoidListener>) {
        if !self.dirty {
            return;
        }
        self.spans = scan_visible_urls(term);
        self.dirty = false;
    }

    /// Return the `UrlSpan` whose column range contains `col` on `row`, if any.
    ///
    /// When multiple spans cover the same cell, `Osc8Hyperlink` wins over
    /// `Url`/`Path` (FR-004 precedence). The OSC 8 pass in
    /// `scan_visible_urls` emits its spans first, so a single linear scan
    /// over `self.spans` naturally honours the precedence without needing
    /// a secondary index.
    pub fn url_at(&self, row: i32, col: usize) -> Option<&UrlSpan> {
        if let Some(span) = self.spans.iter().find(|span| {
            matches!(span.kind, SpanKind::Osc8Hyperlink) && span.contains_cell(row, col)
        }) {
            return Some(span);
        }
        self.spans.iter().find(|span| span.contains_cell(row, col))
    }

    /// All detected URL spans for the current viewport.
    pub fn visible_spans(&self) -> &[UrlSpan] {
        &self.spans
    }
}

impl UrlSpan {
    fn contains_cell(&self, row: i32, col: usize) -> bool {
        self.segments.iter().any(|seg| seg.row == row && col >= seg.col_start && col <= seg.col_end)
    }
}

/// Collapse an ordered run of matched logical cells into per-row segments.
///
/// Cells arrive in scan order (columns ascending within a row), so a new
/// segment starts whenever the row changes or a column gap appears.
fn segments_from_cells(cells: &[LogicalCell]) -> Vec<RowSegment> {
    let mut segments: Vec<RowSegment> = Vec::new();
    for cell in cells {
        push_segment_cell(&mut segments, cell.row, cell.col);
    }
    segments
}

/// Add one scanned cell to its exact per-row coverage segments.
fn push_segment_cell(segments: &mut Vec<RowSegment>, row: i32, col: usize) {
    if let Some(seg) = segments.last_mut()
        && seg.row == row
        && col == seg.col_end.saturating_add(1)
    {
        seg.col_end = col;
        return;
    }
    segments.push(RowSegment { row, col_start: col, col_end: col });
}

/// URL schemes recognised by the scanner.
const PREFIXES: &[&str] =
    &["https://", "http://", "ftp://", "file://", "mailto:", "ssh:", "telnet:"];

/// Characters that terminate a URL when encountered (in addition to whitespace).
const URL_TERMINATORS: &[char] = &['<', '>', '"', '\'', '`', '|'];

/// Punctuation that is stripped from the end of a URL when the corresponding
/// opening bracket is absent from the URL body.
const TRAILING_PUNCT: &[char] = &['.', ',', ')', ']', ';', ':', '!', '?'];

/// Bracket pairs checked when stripping trailing punctuation.
const BRACKET_PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']')];

/// Return `true` if `ch` ends URL collection.
fn is_url_terminator(ch: char) -> bool {
    ch.is_whitespace() || URL_TERMINATORS.contains(&ch)
}

fn grid_index_i32(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

#[derive(Clone, Copy)]
struct LogicalCell {
    ch: char,
    row: i32,
    col: usize,
}

// ---------------------------------------------------------------------------
// OSC 8 cell-walk pass — upstream URI-cap finding (spec 009 / T003 + T005)
// ---------------------------------------------------------------------------
// Upstream surface verified against the pinned crate source:
//   * `alacritty_terminal-0.26.0-rc1/src/term/cell.rs:128,202,219` —
//     `Cell::hyperlink() -> Option<Hyperlink>` and
//     `Cell::set_hyperlink(Option<Hyperlink>)` are public, ungated by any
//     feature flag.
//   * `vte-0.15.0/src/ansi.rs:1392-1419` parses `OSC 8 ; <params> ; <URI>
//     ST` and forwards the URI verbatim to `Handler::set_hyperlink`
//     (`alacritty_terminal-0.26.0-rc1/src/term/mod.rs:1873-1877`), which
//     stores it on each emitted cell.
//
// Upstream URI cap finding (FR-010): with `feature = "std"` (alacritty's
// default and Scribe's dependency edition) the VTE OSC raw buffer is an
// unbounded `Vec<u8>` (`vte-0.15.0/src/lib.rs:64`). `MAX_OSC_RAW = 1024`
// applies only to `no_std` builds (line 545-551). No upstream length check
// rejects long URIs in the std build. Scribe therefore enforces the
// FR-010 2 KiB cap itself below: URIs whose UTF-8 length exceeds 2048
// bytes are treated as absent (no `UrlSpan` emitted, the cell carries no
// OSC 8 URI in the cache, activation falls back to the heuristic
// detector).
//
// Span boundary rule: contiguous cells whose `cell.hyperlink()` compare
// equal merge into one `UrlSpan`. A later run with the same id + URI also
// joins that span when it starts no more than one row later, preserving
// multi-row links around unlinked gutter or filler cells. Other boundaries
// close the active span and start a new one if applicable.

/// Spec-mandated maximum URI length in bytes (FR-010, kitty-style 2 KiB cap).
const OSC8_MAX_URI_BYTES: usize = 2048;

/// Scan all visible rows of `term` for URLs and return their spans.
///
/// Row indices in the returned spans are **absolute grid lines**: screen row
/// minus `display_offset`, matching `alacritty_terminal`'s `Line` convention.
///
/// The OSC 8 cell-walk pass runs **first** so its spans take precedence in
/// the linear-scan `url_at` lookup (FR-004); the heuristic pass that
/// follows skips any cell already covered by an OSC 8 span (FR-014 — no
/// regression on cells outside OSC 8 spans).
fn scan_visible_urls(term: &Term<VoidListener>) -> Vec<UrlSpan> {
    let rows = term.grid().screen_lines();
    let cols = term.grid().columns();
    let display_offset = term.grid().display_offset();

    let mut spans = Vec::new();
    if rows == 0 || cols == 0 {
        return spans;
    }

    // Pass 1: OSC 8 hyperlinks. Runs before the heuristic pass so the
    // resulting `UrlSpan`s appear earlier in `spans`, which combined with
    // the precedence branch in `url_at` makes OSC 8 win on overlap.
    let osc8_ranges = scan_osc8_hyperlinks(term, &mut spans);

    // Pass 2: heuristic URL + path detection, skipping cells already
    // covered by an OSC 8 span.
    let continuation = ContinuationCtx { term, rows, cols, display_offset };
    let mut screen_row: usize = 0;
    while screen_row < rows {
        let logical_line =
            collect_wrapped_logical_line(term, screen_row, rows, cols, display_offset);
        let url_ranges = scan_logical_urls(&logical_line, &osc8_ranges, &continuation, &mut spans);
        scan_logical_paths(&logical_line, &url_ranges, &osc8_ranges, &mut spans);

        screen_row = logical_line.last().map_or(screen_row.saturating_add(1), |cell| {
            let row_with_offset = cell.row.saturating_add(grid_index_i32(display_offset)).max(0);
            usize::try_from(row_with_offset).unwrap_or(usize::MAX).saturating_add(1)
        });
    }

    spans
}

/// Exact cell coverage of one OSC 8 span (absolute grid coords).
///
/// A merged span can have unlinked gaps, so segments stay authoritative for
/// masking and hit-testing.
#[derive(Clone)]
struct Osc8CellRange {
    link: Option<Hyperlink>,
    row_end: i32,
    segments: Vec<RowSegment>,
}

impl Osc8CellRange {
    /// Test whether `(row, col)` falls inside this span.
    /// Coverage follows the scanned cells rather than a bounding rectangle.
    /// This keeps heuristic masking consistent with hover hit-testing when a
    /// merged OSC 8 run has a partial row between its endpoints.
    fn contains(&self, row: i32, col: usize) -> bool {
        self.segments
            .iter()
            .any(|segment| segment.row == row && col >= segment.col_start && col <= segment.col_end)
    }
}

/// Walk the visible grid via the alacritty display iterator and emit one
/// `UrlSpan { kind: Osc8Hyperlink, .. }` per contiguous run of cells sharing
/// the same `Hyperlink`, merging same-link runs separated by at most one row.
///
/// Returns inclusive cell ranges so the heuristic pass can skip them
/// (FR-004 precedence). URIs longer than `OSC8_MAX_URI_BYTES` are treated
/// as absent (FR-010) — the affected cells carry no OSC 8 URI in the cache
/// and fall through to the heuristic detector.
fn scan_osc8_hyperlinks(term: &Term<VoidListener>, out: &mut Vec<UrlSpan>) -> Vec<Osc8CellRange> {
    let mut ranges = Vec::new();
    let mut active: Option<Osc8SpanBuilder> = None;

    // `display_iter()` yields cells in row-major order across the visible
    // grid. `point.line.0` carries the same `Line(screen_row -
    // display_offset)` value the heuristic pass stores via `row_abs` in
    // `collect_wrapped_logical_line`, so no row-coordinate translation is
    // needed here.
    let iter = term.grid().display_iter();
    {
        let mut sink = Osc8WalkSink { active: &mut active, out, ranges: &mut ranges };
        for indexed in iter {
            let point: Point = indexed.point;
            let row_for_span = point.line.0;
            let col_for_span = point.column.0;
            let cell_link = indexed.cell.hyperlink();
            process_osc8_cell(&mut sink, cell_link, row_for_span, col_for_span);
        }
    }
    flush_osc8_span(&mut active, out, &mut ranges);
    tracing::trace!(span_count = ranges.len(), "osc8: scan_visible_urls");
    ranges
}

/// Sink for the OSC 8 cell-walk pass — bundles the per-call mutable
/// references so the boundary handler can stay below clippy's argument
/// count limit while still threading state across the iteration.
struct Osc8WalkSink<'a> {
    active: &'a mut Option<Osc8SpanBuilder>,
    out: &'a mut Vec<UrlSpan>,
    ranges: &'a mut Vec<Osc8CellRange>,
}

/// Exact geometry accumulated for an OSC 8 run while scanning the grid.
struct Osc8SpanBuilder {
    link: Hyperlink,
    row_start: i32,
    col_start: usize,
    row_end: i32,
    col_end: usize,
    segments: Vec<RowSegment>,
}

impl Osc8SpanBuilder {
    fn new(link: Hyperlink, row: i32, col: usize) -> Self {
        Self {
            link,
            row_start: row,
            col_start: col,
            row_end: row,
            col_end: col,
            segments: vec![RowSegment { row, col_start: col, col_end: col }],
        }
    }

    fn extend(&mut self, row: i32, col: usize) {
        self.row_end = row;
        self.col_end = col;
        push_segment_cell(&mut self.segments, row, col);
    }
}

/// Update the active-span aggregator with the hyperlink seen on the next
/// cell. Split out of the display-iter loop body to keep clippy's
/// excessive-nesting lint happy and to make the boundary rule
/// (`None ↔ Some` and Arc-change transitions) easy to inspect.
fn process_osc8_cell(
    sink: &mut Osc8WalkSink<'_>,
    cell_link: Option<Hyperlink>,
    row: i32,
    col: usize,
) {
    // Borrow the active hyperlink (if any) for comparison — avoids the
    // per-cell `Hyperlink::clone` (one atomic refcount op each) the
    // previous shape did unconditionally on every grid cell. The borrow
    // ends before any mutation of `sink.active`.
    let active_same = matches!((sink.active.as_ref(), cell_link.as_ref()), (Some(a), Some(b)) if hyperlink_same(&a.link, b));
    if active_same {
        if let Some(entry) = sink.active.as_mut() {
            entry.extend(row, col);
        }
        return;
    }
    match (sink.active.is_some(), cell_link) {
        (false, None) => {}
        (false, Some(link)) => {
            if hyperlink_uri_is_acceptable(&link) {
                *sink.active = Some(Osc8SpanBuilder::new(link, row, col));
            }
        }
        (true, Some(next)) => {
            flush_osc8_span(sink.active, sink.out, sink.ranges);
            if hyperlink_uri_is_acceptable(&next) {
                *sink.active = Some(Osc8SpanBuilder::new(next, row, col));
            }
        }
        (true, None) => {
            flush_osc8_span(sink.active, sink.out, sink.ranges);
        }
    }
}

/// Hyperlink equivalence used to detect a single OSC 8 open/close run.
///
/// Upstream wraps the URI + id pair in `Arc<HyperlinkInner>` so all cells
/// tagged inside one run share the same Arc identity. `PartialEq` on
/// `Hyperlink` compares via the inner Arc, so structurally-equal hyperlinks
/// emitted by two adjacent runs would compare equal too — but the upstream
/// auto-generated `_alacritty` id suffix increments per anonymous open
/// (see `term/cell.rs:88-94`), so the IDs differ and `PartialEq` separates
/// them correctly. For explicit `id=` reuse across non-adjacent runs the
/// `id` string still appears the same to upstream and they collapse into
/// one Arc; per FR-005 we treat that as the same span when contiguous,
/// and the contiguity check (cells must be adjacent in iteration order)
/// guarantees a later separately-opened run starts a new span anyway.
fn hyperlink_same(a: &Hyperlink, b: &Hyperlink) -> bool {
    a == b
}

fn hyperlink_uri_is_acceptable(link: &Hyperlink) -> bool {
    let uri = link.uri();
    !uri.is_empty() && uri.len() <= OSC8_MAX_URI_BYTES
}

fn flush_osc8_span(
    active: &mut Option<Osc8SpanBuilder>,
    out: &mut Vec<UrlSpan>,
    ranges: &mut Vec<Osc8CellRange>,
) {
    let Some(Osc8SpanBuilder { link, row_start, col_start, row_end, col_end, segments }) =
        active.take()
    else {
        return;
    };
    if let Some(previous) = ranges.last_mut().filter(|previous| {
        previous.link.as_ref().is_some_and(|previous_link| hyperlink_same(previous_link, &link))
            && row_start <= previous.row_end.saturating_add(1)
    }) && let Some(span) = out.last_mut()
    {
        span.row_end = row_end;
        span.col_end = col_end;
        span.segments.extend_from_slice(&segments);
        previous.row_end = row_end;
        previous.segments.extend_from_slice(&segments);
        return;
    }
    out.push(UrlSpan {
        row: row_start,
        col_start,
        row_end,
        col_end,
        url: link.uri().to_owned(),
        kind: SpanKind::Osc8Hyperlink,
        segments: segments.clone(),
    });
    ranges.push(Osc8CellRange { link: Some(link), row_end, segments });
}

/// Returns `true` when the cell at `(row, col)` falls inside any OSC 8 span.
fn cell_in_osc8_range(ranges: &[Osc8CellRange], row: i32, col: usize) -> bool {
    ranges.iter().any(|r| r.contains(row, col))
}

/// Returns `true` when `(row, col)` is already covered by an emitted span —
/// e.g. the continuation tail of a hard-break join produced while scanning
/// an earlier logical line. Later line scans must not re-match those cells
/// as fresh URLs or paths.
fn cell_in_existing_span(spans: &[UrlSpan], row: i32, col: usize) -> bool {
    spans.iter().any(|span| span.contains_cell(row, col))
}

fn collect_wrapped_logical_line(
    term: &Term<VoidListener>,
    start_screen_row: usize,
    rows: usize,
    cols: usize,
    display_offset: usize,
) -> Vec<LogicalCell> {
    let mut cells = Vec::new();
    let display_offset_i32 = grid_index_i32(display_offset);
    let last_col = Column(cols.saturating_sub(1));

    let mut screen_row = start_screen_row;
    while screen_row < rows {
        let row_abs = grid_index_i32(screen_row).saturating_sub(display_offset_i32);
        let line = Line(row_abs);

        let mut col_idx = 0usize;
        while col_idx < cols {
            cells.push(LogicalCell {
                ch: read_cell_char(term, line, Column(col_idx)),
                row: row_abs,
                col: col_idx,
            });
            col_idx = col_idx.saturating_add(1);
        }

        if !read_cell_flags(term, line, last_col).contains(Flags::WRAPLINE) {
            break;
        }
        screen_row = screen_row.saturating_add(1);
    }

    cells
}

/// Grid access needed to pull continuation rows for hard-break URL joins.
///
/// Programs that lay out their own text (Claude Code, pagers, line
/// editors) split long URLs with an explicit newline instead of letting
/// the terminal soft-wrap, so no `WRAPLINE` flag connects the rows. This
/// context lets the URL scanner fetch the logical line below a match that
/// ran through the final cell of its own logical line.
struct ContinuationCtx<'t> {
    term: &'t Term<VoidListener>,
    rows: usize,
    cols: usize,
    display_offset: usize,
}

impl ContinuationCtx<'_> {
    /// The WRAPLINE-joined logical line starting directly below the
    /// absolute grid row `after_row_abs`, or `None` when that row is
    /// outside the visible grid.
    fn logical_line_below(&self, after_row_abs: i32) -> Option<Vec<LogicalCell>> {
        let screen_row = after_row_abs.saturating_add(grid_index_i32(self.display_offset));
        let next_screen_row = usize::try_from(screen_row).ok()?.checked_add(1)?;
        if next_screen_row >= self.rows {
            return None;
        }
        Some(collect_wrapped_logical_line(
            self.term,
            next_screen_row,
            self.rows,
            self.cols,
            self.display_offset,
        ))
    }
}

/// Maximum number of hard-broken rows that may be appended to one URL.
/// Soft-wrapped rows inside a continuation line do not count against
/// this cap — only explicit line breaks do.
const MAX_HARD_JOIN_ROWS: usize = 3;

/// Everything the hard-break join policy may inspect for one decision.
///
/// The plumbing consults the policy whenever a URL match is the last
/// content on its row (nothing but blank filler follows it); the policy
/// decides whether the break was width-forced and the line below truly
/// continues the URL.
struct HardBreakContext<'a> {
    /// Grid column of the URL's last cell on the broken row.
    url_end_col: usize,
    /// Rightmost grid column (`columns - 1`), for flush-to-edge checks.
    last_col: usize,
    /// Full-width cells of the row the URL broke on, for comparing any
    /// leading gutter/indent run against the continuation row's.
    broken_row: &'a [LogicalCell],
    /// The WRAPLINE-joined logical line directly below the broken row,
    /// covering the full grid width.
    next_line: &'a [LogicalCell],
}

/// Maximum width of a program-drawn gutter/indent that may prefix a
/// continuation row.
const MAX_GUTTER_COLS: usize = 8;

/// Maximum pure-space alignment indent accepted after a width-forced hard
/// break. This is deliberately independent of [`MAX_GUTTER_COLS`]: alignment
/// indents carry no drawn-block semantics and commonly exceed a small gutter.
const MAX_ALIGNMENT_INDENT_COLS: usize = 32;

/// Characters treated as part of a program-drawn gutter: whitespace,
/// box-drawing (U+2500–U+257F), block elements (U+2580–U+259F), and the
/// email/markdown quote marker.
fn is_gutter_char(ch: char) -> bool {
    ch.is_whitespace() || ('\u{2500}'..='\u{259F}').contains(&ch) || ch == '>'
}

/// Decide whether the line below continues a URL across a hard line
/// break, and where the URL body resumes.
///
/// The policy follows kitty — the only major terminal that bridges hard
/// breaks, and it does so by default (`url_excluded_characters` docs:
/// newlines are "allowed (but stripped)… to accommodate programs such as
/// mutt that add hard line breaks even for continued lines"): a break is
/// bridged only when the URL ran **exactly to the terminal edge** and the
/// next row resumes with URL characters at column 0. Mid-row hard breaks
/// are never bridged (kitty#2927: the emulator cannot know whether such a
/// break is real). Alacritty's searcher, by contrast, hard-stops at
/// unwrapped row boundaries and its maintainers declined bridging
/// (alacritty#5453) — kitty's default is the richer standard and matches
/// Scribe's use case.
///
/// Scribe extends kitty in two safe directions:
/// * a continuation behind a program-drawn gutter (e.g. Claude Code's
///   banner bar `▏ `) is accepted when the broken row carries the
///   **identical** gutter run — the gutter cells stay outside the span;
/// * a next row that starts its own scheme prefix (`https://…`) is a new
///   link, never a continuation.
///
/// Returns the char index into `ctx.next_line` where the URL body
/// resumes, or `None` to refuse the join.
fn hard_break_continuation_start(ctx: &HardBreakContext<'_>) -> Option<usize> {
    // kitty rule: only a break at the terminal edge is width-forced.
    if ctx.url_end_col != ctx.last_col {
        return None;
    }

    let first = ctx.next_line.first()?.ch;
    let start = if !is_gutter_char(first) && !is_url_terminator(first) {
        // Continuation resumes at column 0 — the exact case kitty
        // bridges by default (mutt-style hard wrap).
        0
    } else if let Some(indent) = whitespace_alignment_indent(ctx.next_line) {
        // Programs that align table cells often place a continuation under
        // its column with spaces only. Unlike a drawn gutter this has no
        // corresponding prefix on the broken row, so do not compare runs.
        indent
    } else {
        matching_gutter_len(ctx.broken_row, ctx.next_line)?
    };

    let resume = ctx.next_line.get(start)?.ch;
    if is_url_terminator(resume) {
        return None;
    }
    let next_chars: Vec<char> = ctx.next_line.iter().map(|cell| cell.ch).collect();
    if match_prefix_chars(&next_chars, start).is_some() {
        return None;
    }
    Some(start)
}

/// Return the length of a safe pure-space alignment indent, if present.
///
/// Tabs and other whitespace remain part of the drawn-gutter path: accepting
/// only literal spaces prevents this relaxed branch from blurring structured
/// terminal decorations with an ordinary layout indent.
fn whitespace_alignment_indent(next_line: &[LogicalCell]) -> Option<usize> {
    let indent = next_line.iter().take_while(|cell| cell.ch == ' ').count();
    let resume = next_line.get(indent)?.ch;
    (indent > 0 && indent <= MAX_ALIGNMENT_INDENT_COLS && !is_url_terminator(resume))
        .then_some(indent)
}

/// Length of the gutter run shared verbatim by the broken row and the
/// continuation row, when the continuation row starts with one.
///
/// Returns `None` when the continuation row has no gutter run, the run
/// exceeds [`MAX_GUTTER_COLS`], or the broken row's leading cells differ —
/// differing gutters mean the rows belong to different drawn blocks.
fn matching_gutter_len(broken_row: &[LogicalCell], next_line: &[LogicalCell]) -> Option<usize> {
    let run = next_line.iter().take_while(|cell| is_gutter_char(cell.ch)).count();
    if run == 0 || run > MAX_GUTTER_COLS {
        return None;
    }
    let same = (0..run).all(
        |i| matches!((broken_row.get(i), next_line.get(i)), (Some(a), Some(b)) if a.ch == b.ch),
    );
    if same { Some(run) } else { None }
}

/// Extend a URL match that ended as the last content on its row across
/// hard line breaks, as permitted by [`hard_break_continuation_start`].
///
/// `raw_end_line` is the line-local char index one past the last raw
/// match char; the caller guarantees everything from there to the end of
/// the logical line is blank filler. Returns the owned match cells
/// (original URL cells plus any joined continuation cells) and the raw
/// match end index within them. With no join, the result is exactly the
/// original match and the URL text is unchanged.
///
/// OSC 8 precedence (FR-004): a continuation never absorbs cells covered
/// by an explicit hyperlink — the appended run is cut at the first such
/// cell, and joining stops there.
fn extend_url_across_hard_breaks(
    line_cells: &[LogicalCell],
    url_col_start: usize,
    raw_end_line: usize,
    ctx: &ContinuationCtx<'_>,
    osc8_ranges: &[Osc8CellRange],
) -> (Vec<LogicalCell>, usize) {
    let mut joined: Vec<LogicalCell> =
        line_cells.get(url_col_start..raw_end_line).unwrap_or(&[]).to_vec();
    let mut chars: Vec<char> = joined.iter().map(|cell| cell.ch).collect();
    let mut raw_end = chars.len();
    let mut hard_joins = 0usize;
    let last_col = ctx.cols.saturating_sub(1);
    // The full logical line the URL currently ends on; its final screen
    // row is the "broken row" the policy compares gutter prefixes against.
    let mut current_line: Vec<LogicalCell> = line_cells.to_vec();

    while raw_end == chars.len() && hard_joins < MAX_HARD_JOIN_ROWS {
        let Some(&last) = joined.last() else { break };
        let Some(next_line) = ctx.logical_line_below(last.row) else { break };
        let broken_row_start =
            current_line.iter().position(|cell| cell.row == last.row).unwrap_or(current_line.len());
        let policy_ctx = HardBreakContext {
            url_end_col: last.col,
            last_col,
            broken_row: current_line.get(broken_row_start..).unwrap_or(&[]),
            next_line: &next_line,
        };
        let Some(start) = hard_break_continuation_start(&policy_ctx) else { break };
        let tail = next_line.get(start..).unwrap_or(&[]);
        let osc8_cut = tail
            .iter()
            .position(|cell| cell_in_osc8_range(osc8_ranges, cell.row, cell.col))
            .unwrap_or(tail.len());
        let Some(tail) = tail.get(..osc8_cut).filter(|cells| !cells.is_empty()) else { break };

        joined.extend_from_slice(tail);
        chars.extend(tail.iter().map(|cell| cell.ch));
        raw_end = collect_url_end_chars(&chars, raw_end);
        current_line = next_line;

        // When the collected URL again runs to the last content cell of
        // this continuation row, drop the blank filler so the loop
        // condition sees a flush buffer and can attempt the next join.
        if chars.get(raw_end..).unwrap_or(&[]).iter().all(|ch| ch.is_whitespace()) {
            chars.truncate(raw_end);
            joined.truncate(raw_end);
        }
        hard_joins = hard_joins.saturating_add(1);
    }

    (joined, raw_end)
}

/// Scan a logical line's text for URLs and push found spans into `out`.
///
/// `cells` contains exactly one `char` per terminal column, joined across
/// WRAPLINE-connected rows. We work with char indices so multi-byte characters
/// never cause a slice at a non-char-boundary.
///
/// Cells already covered by an `Osc8Hyperlink` span (per `osc8_ranges`)
/// are skipped — heuristic URL detection MUST not produce a span over a
/// cell that already carries an OSC 8 URI (FR-004 precedence).
fn scan_logical_urls(
    cells: &[LogicalCell],
    osc8_ranges: &[Osc8CellRange],
    ctx: &ContinuationCtx<'_>,
    out: &mut Vec<UrlSpan>,
) -> Vec<(usize, usize)> {
    let chars: Vec<char> = cells.iter().map(|cell| cell.ch).collect();
    let char_count = chars.len();
    let mut char_pos = 0usize;
    let mut ranges = Vec::new();

    while char_pos < char_count {
        if cell_is_under_osc8(cells, char_pos, osc8_ranges) {
            char_pos = char_pos.saturating_add(1);
            continue;
        }
        if cells.get(char_pos).is_some_and(|cell| cell_in_existing_span(out, cell.row, cell.col)) {
            char_pos = char_pos.saturating_add(1);
            continue;
        }
        let Some(prefix_len_chars) = match_prefix_chars(&chars, char_pos) else {
            char_pos = char_pos.saturating_add(1);
            continue;
        };

        let url_col_start = char_pos;
        let raw_end_line = collect_url_end_chars(&chars, char_pos + prefix_len_chars);

        // A raw match followed only by blank filler to the end of the
        // logical line may have been split by a width-forced hard break
        // (program did its own wrapping — no WRAPLINE flag); try to
        // continue it on the logical line below.
        let tail_is_blank =
            chars.get(raw_end_line..).unwrap_or(&[]).iter().all(|ch| ch.is_whitespace());
        let (mut match_cells, raw): (Vec<LogicalCell>, String) = if tail_is_blank {
            let (mut joined, joined_end) =
                extend_url_across_hard_breaks(cells, url_col_start, raw_end_line, ctx, osc8_ranges);
            let text: String =
                joined.get(..joined_end).unwrap_or(&[]).iter().map(|cell| cell.ch).collect();
            joined.truncate(joined_end);
            (joined, text)
        } else {
            let slice = cells.get(url_col_start..raw_end_line).unwrap_or(&[]);
            (slice.to_vec(), slice.iter().map(|cell| cell.ch).collect())
        };

        let url = strip_trailing_punct(raw);
        let url_char_len = url.chars().count();
        match_cells.truncate(url_char_len);

        // The index one past the last match cell that lies on THIS
        // logical line — the path-scan exclusion ranges and the scan
        // cursor both live in line-local char space, so continuation
        // cells are clamped away.
        let line_end = url_col_start.saturating_add(url_char_len).min(char_count);

        // FR-004: if any cell within the heuristic match falls inside an
        // OSC 8 span, skip the entire match — OSC 8 wins. Continuation
        // cells are pre-cut at the first OSC 8 cell, so this can only
        // fire on the original line, exactly as before.
        let intersects_osc8 =
            match_cells.iter().any(|cell| cell_in_osc8_range(osc8_ranges, cell.row, cell.col));
        if intersects_osc8 {
            char_pos = char_pos.saturating_add(1);
            continue;
        }

        if url_char_len <= prefix_len_chars {
            char_pos = line_end.max(char_pos.saturating_add(1));
            continue;
        }
        let Some(start) = match_cells.first() else {
            char_pos = char_pos.saturating_add(1);
            continue;
        };
        let Some(end) = match_cells.last() else {
            char_pos = char_pos.saturating_add(1);
            continue;
        };
        out.push(UrlSpan {
            row: start.row,
            col_start: start.col,
            row_end: end.row,
            col_end: end.col,
            url,
            kind: SpanKind::Url,
            segments: segments_from_cells(&match_cells),
        });
        if line_end > url_col_start {
            ranges.push((url_col_start, line_end.saturating_sub(1)));
        }

        char_pos = line_end.max(char_pos.saturating_add(1));
    }

    ranges
}

/// Returns `true` when the logical cell at `char_pos` (a column index into
/// the WRAPLINE-joined logical line) maps to a grid cell covered by an
/// OSC 8 span.
fn cell_is_under_osc8(
    cells: &[LogicalCell],
    char_pos: usize,
    osc8_ranges: &[Osc8CellRange],
) -> bool {
    let Some(cell) = cells.get(char_pos) else {
        return false;
    };
    cell_in_osc8_range(osc8_ranges, cell.row, cell.col)
}

/// Match a URL prefix starting at `chars[pos]`, returning the prefix length in
/// chars if found.
fn match_prefix_chars(chars: &[char], pos: usize) -> Option<usize> {
    for prefix in PREFIXES {
        let prefix_len = prefix.chars().count();
        let matches = prefix
            .chars()
            .enumerate()
            .all(|(offset, prefix_char)| chars.get(pos + offset) == Some(&prefix_char));
        if matches {
            return Some(prefix_len);
        }
    }
    None
}

/// Walk forward from `start` (char index) collecting URL characters; return
/// the char index one past the last URL character.
fn collect_url_end_chars(chars: &[char], start: usize) -> usize {
    let mut end = start;
    while end < chars.len() {
        let Some(ch) = chars.get(end).copied() else {
            break;
        };
        if is_url_terminator(ch) {
            break;
        }
        end = end.saturating_add(1);
    }
    end
}

/// Strip trailing punctuation from a URL, respecting bracket pairs.
fn strip_trailing_punct(mut url: String) -> String {
    while let Some(last) = url.chars().next_back() {
        if !TRAILING_PUNCT.contains(&last) {
            break;
        }

        let should_strip = BRACKET_PAIRS
            .iter()
            .find(|(_, close)| *close == last)
            .is_none_or(|(open, _)| !url.contains(*open));

        if should_strip {
            url.truncate(url.len() - last.len_utf8());
        } else {
            break;
        }
    }
    url
}

/// Maximum lookahead for bare relative path detection (e.g. `src/main.rs`).
const BARE_PATH_LOOKAHEAD: usize = 30;

fn is_path_token_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '.' | '~' | '_' | '-')
}

/// Scan a logical line for file-system paths and push found spans into `out`.
///
/// `url_ranges` contains `(col_start, col_end)` pairs for URL spans already
/// detected on this logical line; any character position that falls inside
/// one of them is skipped to avoid overlaps. Cells covered by `osc8_ranges`
/// are likewise skipped so OSC 8 spans take precedence over heuristic path
/// detection (FR-004).
fn scan_logical_paths(
    cells: &[LogicalCell],
    url_ranges: &[(usize, usize)],
    osc8_ranges: &[Osc8CellRange],
    out: &mut Vec<UrlSpan>,
) {
    let chars: Vec<char> = cells.iter().map(|cell| cell.ch).collect();
    let char_count = chars.len();
    let mut char_pos = 0usize;

    while char_pos < char_count {
        // Skip positions that belong to a URL span.
        if url_ranges.iter().any(|(start, end)| char_pos >= *start && char_pos <= *end) {
            char_pos = char_pos.saturating_add(1);
            continue;
        }

        // Skip positions that belong to an OSC 8 span (FR-004 precedence).
        if cell_is_under_osc8(cells, char_pos, osc8_ranges) {
            char_pos = char_pos.saturating_add(1);
            continue;
        }

        // Skip positions already consumed by an emitted span (e.g. the
        // continuation tail of a hard-break join from an earlier line).
        if cells.get(char_pos).is_some_and(|cell| cell_in_existing_span(out, cell.row, cell.col)) {
            char_pos = char_pos.saturating_add(1);
            continue;
        }

        // Try to match a path prefix at this position.
        let Some((prefix_len, is_bare_relative)) = detect_path_prefix(&chars, char_pos) else {
            char_pos = char_pos.saturating_add(1);
            continue;
        };

        let path_col_start = char_pos;
        let body_start = char_pos + prefix_len;
        let raw_end = collect_url_end_chars(&chars, body_start);

        let raw: String = chars.get(path_col_start..raw_end).unwrap_or(&[]).iter().collect();
        let path = strip_trailing_punct(raw);
        let path_char_len = path.chars().count();
        let path_col_end_exclusive = path_col_start + path_char_len;

        // Bare relative paths must contain at least one '/' in the collected token.
        let valid = if is_bare_relative {
            path.contains('/') && path_char_len > prefix_len
        } else {
            path_char_len > prefix_len && path_col_end_exclusive <= char_count
        };

        // FR-004: skip the entire path match if any cell inside it lies
        // inside an OSC 8 span — OSC 8 wins on overlap.
        let intersects_osc8 = (path_col_start..path_col_end_exclusive)
            .any(|i| cell_is_under_osc8(cells, i, osc8_ranges));
        if intersects_osc8 {
            char_pos = char_pos.saturating_add(1);
            continue;
        }

        if valid && path_col_end_exclusive <= char_count {
            let Some(start) = cells.get(path_col_start) else {
                char_pos = char_pos.saturating_add(1);
                continue;
            };
            let Some(end) = cells.get(path_col_end_exclusive.saturating_sub(1)) else {
                char_pos = char_pos.saturating_add(1);
                continue;
            };
            let path_cells = cells.get(path_col_start..path_col_end_exclusive).unwrap_or(&[]);
            out.push(UrlSpan {
                row: start.row,
                col_start: start.col,
                row_end: end.row,
                col_end: end.col,
                url: path,
                kind: SpanKind::Path,
                segments: segments_from_cells(path_cells),
            });
            char_pos = path_col_end_exclusive.max(char_pos.saturating_add(1));
        } else {
            char_pos = char_pos.saturating_add(1);
        }
    }
}

/// Attempt to match a file-system path prefix starting at `chars[pos]`.
///
/// Returns `(prefix_len, is_bare_relative)` on success, `None` otherwise.
fn detect_path_prefix(chars: &[char], pos: usize) -> Option<(usize, bool)> {
    // `~/` — home-relative.
    if chars.get(pos) == Some(&'~') && chars.get(pos + 1) == Some(&'/') {
        return Some((2, false));
    }

    // `../` — explicit relative.
    if chars.get(pos) == Some(&'.')
        && chars.get(pos + 1) == Some(&'.')
        && chars.get(pos + 2) == Some(&'/')
    {
        return Some((3, false));
    }

    // `./` — explicit relative.
    if chars.get(pos) == Some(&'.') && chars.get(pos + 1) == Some(&'/') {
        return Some((2, false));
    }

    // `/something` — absolute path: must not continue a path token and the
    // character after `/` must not be another `/` or whitespace.
    if chars.get(pos) == Some(&'/') {
        let at_path_boundary =
            pos == 0 || chars.get(pos - 1).is_some_and(|c| !is_path_token_char(*c));
        let followed_ok = chars.get(pos + 1).is_some_and(|c| !c.is_whitespace() && *c != '/');
        if at_path_boundary && followed_ok {
            return Some((1, false));
        }
        return None;
    }

    // Bare relative word containing `/` within BARE_PATH_LOOKAHEAD chars.
    // The start character must itself be a valid path-token character (not
    // just alphanumeric) so a leading `.`, `~`, `_`, or `-` is not dropped —
    // `is_path_token_char` already allows these for interior characters, and
    // the start must accept the same set or the scanner silently advances
    // past it and reports a truncated path.
    if chars.get(pos).is_some_and(|c| is_path_token_char(*c)) {
        let look_end = (pos + BARE_PATH_LOOKAHEAD).min(chars.len());
        let window = chars.get(pos..look_end).unwrap_or(&[]);
        // Ensure there is a `/` in the window and only path-token characters
        // before it, so delimiters advance the scanner to the slash.
        let slash_pos = window.iter().position(|c| *c == '/');
        if let Some(rel_slash) = slash_pos {
            let path_token =
                window.get(..rel_slash).unwrap_or(&[]).iter().all(|c| is_path_token_char(*c));
            if path_token {
                return Some((0, true));
            }
        }
    }

    None
}

/// Open a file path with the system default application, optionally jumping
/// to a line number with VS Code when a `:N` suffix is present.
///
/// - Strips an optional `:N` line-number suffix from the end of `raw`.
/// - Expands `~/` to `$HOME/`.
/// - Resolves relative paths against `cwd` when provided.
/// - If a line number is present, tries `code --goto path:line` first;
///   falls back to `xdg-open` / `open` when VS Code is not found.
pub fn open_path(raw: &str, cwd: Option<&std::path::Path>) {
    use std::io::ErrorKind;

    // Parse optional :N line-number suffix.
    let (path_str, line_num) = parse_path_line_suffix(raw);
    let resolved = resolve_path(path_str, cwd);

    #[cfg(target_os = "linux")]
    let open_cmd = "xdg-open";
    #[cfg(target_os = "macos")]
    let open_cmd = "open";
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let open_cmd = "xdg-open";

    if let Some(line) = line_num {
        let goto_arg = format!("{resolved}:{line}");
        match std::process::Command::new("code").args(["--goto", &goto_arg]).spawn() {
            Ok(_child) => return,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // VS Code not installed — fall through to xdg-open.
            }
            Err(e) => {
                tracing::warn!("open_path: failed to spawn code: {e}");
                return;
            }
        }
    }

    match std::process::Command::new(open_cmd).arg(&resolved).spawn() {
        Ok(_child) => {}
        Err(e) => tracing::warn!("open_path: failed to spawn {open_cmd}: {e}"),
    }
}

/// Resolve a detected path into what the OS handler should be given: `~/`
/// expanded, a relative path joined onto the pane's working directory, and the
/// `./` the detector matched on dropped.
///
/// `cwd` is the directory of the shell the path was printed by, not this
/// process's — a relative path in a terminal means "relative to that pane", and
/// the client's own directory has nothing to do with it. Without one the path
/// is handed over as-is rather than resolved against a guess: the OS handler
/// failing on a relative path is a visible, correct failure, where opening the
/// wrong file silently is not.
///
/// `..` segments deliberately survive. Collapsing them lexically names a
/// different file whenever a symlink sits on the path, and the OS resolves them
/// correctly anyway; `.` carries no such risk, so it goes.
fn resolve_path(path: &str, cwd: Option<&std::path::Path>) -> String {
    let expanded: String = path.strip_prefix("~/").map_or_else(
        || path.to_owned(),
        |rel| {
            std::env::var("HOME").ok().map_or_else(
                || path.to_owned(),
                |home| format!("{}/{rel}", home.trim_end_matches('/')),
            )
        },
    );
    if expanded.starts_with('/') {
        return tidy_absolute(std::path::Path::new(&expanded));
    }
    // Only ever called with an absolute path: `std::path::absolute` would
    // otherwise resolve against *this* process's directory, which is the one
    // answer this function exists to avoid.
    match cwd {
        Some(base) if base.is_absolute() => tidy_absolute(&base.join(&expanded)),
        Some(base) => base.join(&expanded).to_string_lossy().into_owned(),
        None => expanded,
    }
}

/// Drop the `.` components from an already-absolute path.
fn tidy_absolute(path: &std::path::Path) -> String {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned()
}

/// Split a raw path string into `(path, optional_line_number)`.
///
/// A trailing `:N` suffix is recognised only when `N` is a non-empty string
/// of ASCII digits.
fn parse_path_line_suffix(raw: &str) -> (&str, Option<u32>) {
    if let Some(colon) = raw.rfind(':') {
        let suffix = &raw[colon + 1..];
        if !suffix.is_empty()
            && suffix.chars().all(|c| c.is_ascii_digit())
            && let Ok(n) = suffix.parse::<u32>()
        {
            return (&raw[..colon], Some(n));
        }
    }
    (raw, None)
}

/// Returns `true` when `uri` starts with one of the outbound URL
/// allowlist **prefixes** (`https://`, `http://`, `ftp://`, `file://`,
/// `mailto:`, `ssh:`, `telnet:`).
///
/// Used by the OSC 8 activation router (spec 009 FR-009 / FR-015) to
/// decide whether an OSC 8 URI may open directly or must first prompt
/// the user via the disallowed-scheme dialog.
///
/// **Note:** despite the name, the check is a prefix match — each entry
/// in `PREFIXES` includes its scheme delimiter (`://` or `:`). As long as
/// `PREFIXES` only contains scheme+delimiter entries, this is
/// functionally identical to a scheme-name check; the short name is kept
/// to match the call sites.
pub fn is_allowed_scheme(uri: &str) -> bool {
    PREFIXES.iter().any(|p| uri.starts_with(p))
}

/// Extract the URI scheme (everything up to the first `:`) when present.
///
/// Returns `None` when `uri` does not contain a `:` or when the substring
/// before `:` is empty.
pub fn extract_scheme(uri: &str) -> Option<String> {
    let colon = uri.find(':')?;
    let scheme = uri.get(..colon)?;
    if scheme.is_empty() { None } else { Some(scheme.to_owned()) }
}

/// Open a URL in the system default browser.
///
/// Only URL schemes that Scribe recognizes in terminal text are accepted.
/// The child process is spawned and not awaited (fire-and-forget).
pub fn open_url(url: &str) {
    if !PREFIXES.iter().any(|p| url.starts_with(p)) {
        tracing::warn!("open_url: refusing to open unsupported URL scheme");
        return;
    }
    open_uri_unguarded(url);
}

/// Open `uri` with the OS handler without the scheme-allowlist guard.
///
/// Used by the OSC 8 disallowed-scheme confirmation dialog (spec 009
/// FR-015) after the user has explicitly chosen "Open Anyway". Allowed
/// schemes go through `open_url` instead.
pub fn open_uri_unguarded(uri: &str) {
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let cmd = "xdg-open";

    match std::process::Command::new(cmd).arg(uri).spawn() {
        Ok(_child) => {}
        Err(e) => tracing::warn!("open_uri_unguarded: failed to spawn {cmd}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal_gpui::event::VoidListener;
    use alacritty_terminal_gpui::grid::Dimensions;
    use alacritty_terminal_gpui::term::Config;
    use alacritty_terminal_gpui::term::Term;
    use vte::ansi::Processor;

    use super::{
        HardBreakContext, LogicalCell, Osc8CellRange, PaneUrlCache, RowSegment, SpanKind,
        hard_break_continuation_start, resolve_path, segments_from_cells,
    };

    #[derive(Clone, Copy)]
    struct TestDims {
        cols: usize,
        rows: usize,
    }

    impl Dimensions for TestDims {
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

    fn term_with_output(cols: usize, rows: usize, output: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &TestDims { cols, rows }, VoidListener);
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, output);
        term
    }

    fn detected_paths(text: &str) -> Vec<String> {
        let term = term_with_output(text.chars().count().saturating_add(1), 1, text.as_bytes());
        let mut cache = PaneUrlCache::new();
        cache.refresh(&term);
        cache
            .visible_spans()
            .iter()
            .filter(|span| matches!(span.kind, SpanKind::Path))
            .map(|span| span.url.clone())
            .collect()
    }

    fn detected_urls(text: &str) -> Vec<String> {
        let term = term_with_output(text.chars().count().saturating_add(1), 1, text.as_bytes());
        let mut cache = PaneUrlCache::new();
        cache.refresh(&term);
        cache
            .visible_spans()
            .iter()
            .filter(|span| matches!(span.kind, SpanKind::Url))
            .map(|span| span.url.clone())
            .collect()
    }

    // @lat: [[test#GPUI URL Detection#Delimited absolute paths retain their root]]
    #[test]
    fn delimited_absolute_paths_retain_their_root() {
        for input in [
            "'/tmp/example'",
            "\"/tmp/example\"",
            "`/tmp/example`",
            "(/tmp/example)",
            "path=/tmp/example",
        ] {
            assert_eq!(detected_paths(input), ["/tmp/example"], "input: {input}");
        }

        assert_eq!(detected_paths("PATH=/usr/bin:/opt/bin"), ["/usr/bin:/opt/bin"]);
        assert_eq!(
            detected_paths("src/main.rs ./build.sh ../parent ~/notes foo/bar"),
            ["src/main.rs", "./build.sh", "../parent", "~/notes", "foo/bar"]
        );
    }

    // @lat: [[test#GPUI URL Detection#Bare relative paths keep their leading punctuation]]
    #[test]
    fn bare_relative_paths_keep_leading_punctuation() {
        assert_eq!(
            detected_paths(
                ".impeccable/mocks/beads-board-signal-theme.html _private/notes -draft/notes ~alice/notes"
            ),
            [
                ".impeccable/mocks/beads-board-signal-theme.html",
                "_private/notes",
                "-draft/notes",
                "~alice/notes",
            ]
        );
    }

    // @lat: [[test#GPUI URL Detection#Backticks terminate detected URLs]]
    #[test]
    fn backticks_terminate_detected_urls() {
        assert_eq!(detected_urls("`https://example.com/path`"), ["https://example.com/path"]);
    }

    #[test]
    fn joins_whitespace_indented_table_urls_across_hard_breaks() {
        let output = concat!(
            " Links     Durable search (https://www.pathofexile.com/\r\n",
            "          trade2/search/poe2/Runes%20of%20Aldur/KlOw5Pz\r\n",
            "          gi5) · Exact item (https://www.pathofexile.co\r\n",
            "          m/trade2/search/poe2/Runes%20of%20Aldur/KlOw5\r\n",
            "          odLh5)"
        );
        let term = term_with_output(55, 6, output.as_bytes());
        let mut cache = PaneUrlCache::new();
        cache.refresh(&term);

        let urls: Vec<_> = cache
            .visible_spans()
            .iter()
            .filter(|span| matches!(span.kind, SpanKind::Url))
            .collect();
        assert_eq!(urls.len(), 2);
        assert_eq!(
            urls[0].url,
            "https://www.pathofexile.com/trade2/search/poe2/Runes%20of%20Aldur/KlOw5Pzgi5"
        );
        assert_eq!(urls[0].segments.len(), 3);
        assert_eq!(urls[0].segments[1].col_start, 10);
        assert_eq!(urls[0].segments[2].col_start, 10);
        assert_eq!(
            urls[1].url,
            "https://www.pathofexile.com/trade2/search/poe2/Runes%20of%20Aldur/KlOw5odLh5"
        );
        assert_eq!(urls[1].segments.len(), 3);
        assert_eq!(urls[1].segments[1].col_start, 10);
        assert_eq!(urls[1].segments[2].col_start, 10);
    }

    fn cells(text: &str) -> Vec<LogicalCell> {
        text.chars().enumerate().map(|(col, ch)| LogicalCell { ch, row: 0, col }).collect()
    }

    #[test]
    fn preserves_col_zero_new_scheme_and_drawn_gutter_hard_break_rules() {
        let broken = cells("https");
        let col_zero = cells("continuation");
        assert_eq!(
            hard_break_continuation_start(&HardBreakContext {
                url_end_col: 4,
                last_col: 4,
                broken_row: &broken,
                next_line: &col_zero,
            }),
            Some(0)
        );

        let new_link = cells("https://new.example");
        assert_eq!(
            hard_break_continuation_start(&HardBreakContext {
                url_end_col: 4,
                last_col: 4,
                broken_row: &broken,
                next_line: &new_link,
            }),
            None
        );

        let gutter_broken = cells("▏ https");
        let gutter_next = cells("▏ continuation");
        assert_eq!(
            hard_break_continuation_start(&HardBreakContext {
                url_end_col: 6,
                last_col: 6,
                broken_row: &gutter_broken,
                next_line: &gutter_next,
            }),
            Some(2)
        );
    }

    // @lat: [[test#GPUI URL Detection#Explicit hyperlink segment geometry]]
    #[test]
    fn osc8_segments_keep_full_and_partial_middle_rows_exact() {
        let full_middle = segments_from_cells(&[
            LogicalCell { ch: 'a', row: 10, col: 6 },
            LogicalCell { ch: 'b', row: 10, col: 7 },
            LogicalCell { ch: 'c', row: 10, col: 8 },
            LogicalCell { ch: 'd', row: 11, col: 0 },
            LogicalCell { ch: 'e', row: 11, col: 1 },
            LogicalCell { ch: 'f', row: 11, col: 2 },
            LogicalCell { ch: 'g', row: 11, col: 3 },
            LogicalCell { ch: 'h', row: 11, col: 4 },
            LogicalCell { ch: 'i', row: 11, col: 5 },
            LogicalCell { ch: 'j', row: 11, col: 6 },
            LogicalCell { ch: 'k', row: 11, col: 7 },
            LogicalCell { ch: 'l', row: 12, col: 0 },
            LogicalCell { ch: 'm', row: 12, col: 1 },
        ]);
        assert_eq!(
            full_middle,
            vec![
                RowSegment { row: 10, col_start: 6, col_end: 8 },
                RowSegment { row: 11, col_start: 0, col_end: 7 },
                RowSegment { row: 12, col_start: 0, col_end: 1 },
            ]
        );

        let partial_middle = Osc8CellRange {
            link: None,
            row_end: 22,
            segments: vec![
                RowSegment { row: 20, col_start: 5, col_end: 7 },
                RowSegment { row: 21, col_start: 2, col_end: 4 },
                RowSegment { row: 22, col_start: 0, col_end: 1 },
            ],
        };
        assert!(partial_middle.contains(21, 3));
        assert!(!partial_middle.contains(21, 1));
        assert!(!partial_middle.contains(21, 5));
    }

    fn osc8_spans(term: &Term<VoidListener>) -> Vec<(String, Vec<RowSegment>)> {
        let mut cache = PaneUrlCache::new();
        cache.refresh(term);
        cache
            .visible_spans()
            .iter()
            .filter(|span| matches!(span.kind, SpanKind::Osc8Hyperlink))
            .map(|span| (span.url.clone(), span.segments.clone()))
            .collect()
    }

    #[test]
    fn merges_gutter_split_osc8_runs_with_the_same_id_and_uri() {
        let output = concat!(
            "\x1b]8;id=split;https://example.com/target\x1b\\FIRST\x1b]8;;\x1b\\\r\n",
            "GUTTER \x1b]8;id=split;https://example.com/target\x1b\\SECOND\x1b]8;;\x1b\\"
        );
        let term = term_with_output(20, 2, output.as_bytes());
        let spans = osc8_spans(&term);

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, "https://example.com/target");
        assert_eq!(spans[0].1.len(), 2);
        assert_eq!(spans[0].1[0].row, 0);
        assert_eq!(spans[0].1[1].row, 1);
        assert_eq!(spans[0].1[1].col_start, 7);
    }

    #[test]
    fn merges_hard_wrap_split_osc8_runs_around_blank_filler() {
        let output = concat!(
            "\x1b]8;id=split;https://example.com/target\x1b\\FIRST\r\n",
            "SECOND\x1b]8;;\x1b\\"
        );
        let term = term_with_output(20, 2, output.as_bytes());
        let spans = osc8_spans(&term);

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1.len(), 2);
        assert_eq!(spans[0].1[0].col_start, 0);
        assert_eq!(spans[0].1[1].col_start, 0);
    }

    #[test]
    fn keeps_distant_same_uri_anonymous_osc8_links_separate() {
        let output = concat!(
            "\x1b]8;;https://example.com/target\x1b\\FIRST\x1b]8;;\x1b\\\r\n\r\n\r\n",
            "\x1b]8;;https://example.com/target\x1b\\SECOND\x1b]8;;\x1b\\"
        );
        let term = term_with_output(20, 4, output.as_bytes());
        let spans = osc8_spans(&term);

        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(|span| span.0 == "https://example.com/target"));
    }

    // @lat: [[test#GPUI URL Detection#A relative path resolves against the pane's CWD]]
    #[test]
    fn a_relative_path_resolves_against_the_panes_cwd() {
        let cwd = std::path::Path::new("/srv/project");

        // The detector matches `./name`, so the `./` has to come back off
        // before the path reaches the OS handler.
        assert_eq!(resolve_path("./build.sh", Some(cwd)), "/srv/project/build.sh");
        assert_eq!(resolve_path("docs/index.md", Some(cwd)), "/srv/project/docs/index.md");

        // `..` survives: collapsing it here would name a different file
        // whenever a symlink sits on the path.
        assert_eq!(resolve_path("../sibling/x", Some(cwd)), "/srv/project/../sibling/x");

        // An absolute path ignores the CWD entirely but still loses its `.`.
        assert_eq!(resolve_path("/etc/./hosts", Some(cwd)), "/etc/hosts");

        // With no CWD the path is handed over unresolved rather than resolved
        // against this process's directory, which is not the shell's.
        assert_eq!(resolve_path("./build.sh", None), "./build.sh");

        // `~/` expands from `$HOME` and ignores the pane's CWD, so the result
        // is whatever this environment's home is — asserted as a prefix rather
        // than a literal so the test does not mutate process-global state.
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(resolve_path("~/notes.md", Some(cwd)), format!("{home}/notes.md"));
        }
    }
}
