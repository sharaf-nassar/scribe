//! Find-in-scrollback: the overlay the find chord opens, plus the local regex
//! matcher cribbed from Zed's `terminal.rs`.
//!
//! Two matchers live here because the GPUI client has two grids to search.
//! [`TerminalSearch`] wraps the Alacritty fork's [`RegexSearch`] / [`RegexIter`]
//! for a locally-owned `Term`, collecting every match across scrollback and the
//! viewport and cycling a highlighted "current" match with wraparound.
//! [`FindOverlayView`] is the live surface: this client owns no PTY, so the
//! authoritative scrollback lives on the server and every keystroke in the
//! overlay raises a `ClientMessage::SearchRequest` whose `SearchResults` reply
//! lands in [`FindResults`] and is projected onto the painted viewport as
//! [`MatchHighlight`] spans.

use alacritty_terminal_gpui::Term;
use alacritty_terminal_gpui::event::VoidListener;
use alacritty_terminal_gpui::grid::Dimensions as _;
use alacritty_terminal_gpui::index::{Boundary, Column, Direction, Point};
use alacritty_terminal_gpui::term::search::{Match, RegexIter, RegexSearch};
use gpui::{Context, EventEmitter, FocusHandle, Rgba, div, prelude::*, px};
use scribe_common::protocol::SearchMatch as ServerMatch;
use scribe_common::theme::ChromeColors;

use crate::selection::SelectionPoint;
use crate::tab_bar::srgba;

/// A single regex match on the grid, in absolute grid coordinates.
///
/// `start`/`end` are inclusive cell endpoints in reading order, matching the
/// [`crate::selection::SelectionRange`] coordinate convention (row 0 = viewport
/// top, negative rows = scrollback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
}

impl SearchMatch {
    fn from_range(range: &Match) -> Self {
        let start = range.start();
        let end = range.end();
        Self {
            start: SelectionPoint { row: start.line.0, col: start.column.0 },
            end: SelectionPoint { row: end.line.0, col: end.column.0 },
        }
    }
}

/// Compiled find-in-terminal state: the ordered match set plus the index of the
/// currently highlighted match.
#[derive(Debug, Clone)]
pub struct TerminalSearch {
    query: String,
    matches: Vec<SearchMatch>,
    current: Option<usize>,
}

impl TerminalSearch {
    /// Compile `query` and collect every match across the whole terminal grid.
    ///
    /// Returns `None` when the regex fails to compile. An empty query, or a
    /// valid regex with no matches, yields an empty (but valid) search.
    pub fn new(term: &Term<VoidListener>, query: &str) -> Option<Self> {
        if query.is_empty() {
            return Some(Self { query: String::new(), matches: Vec::new(), current: None });
        }

        let mut regex = RegexSearch::new(query).ok()?;
        let matches = collect_matches(term, &mut regex);
        let current = if matches.is_empty() { None } else { Some(0) };
        Some(Self { query: query.to_owned(), matches, current })
    }

    /// The query this search was compiled from.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// All matches in reading order (top-to-bottom, left-to-right).
    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    /// Total number of matches.
    pub fn len(&self) -> usize {
        self.matches.len()
    }

    /// `true` when the search found no matches.
    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    /// Index of the currently highlighted match, if any.
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// The currently highlighted match, if any.
    pub fn current(&self) -> Option<SearchMatch> {
        self.current.and_then(|index| self.matches.get(index).copied())
    }

    /// Advance the highlight to the next match downward, wrapping from the last
    /// match back to the first. Returns the newly selected match.
    pub fn select_next(&mut self) -> Option<SearchMatch> {
        self.cycle(Direction::Right)
    }

    /// Advance the highlight to the previous match upward, wrapping from the
    /// first match back to the last. Returns the newly selected match.
    pub fn select_prev(&mut self) -> Option<SearchMatch> {
        self.cycle(Direction::Left)
    }

    fn cycle(&mut self, direction: Direction) -> Option<SearchMatch> {
        if self.matches.is_empty() {
            self.current = None;
            return None;
        }

        let len = self.matches.len();
        let next = self.current.map_or(0, |index| match direction {
            Direction::Right => (index + 1) % len,
            Direction::Left => (index + len - 1) % len,
        });
        self.current = Some(next);
        self.matches.get(next).copied()
    }
}

/// Iterate every regex match over the full grid, from the topmost scrollback
/// line to the bottom of the viewport, in reading order.
fn collect_matches(term: &Term<VoidListener>, regex: &mut RegexSearch) -> Vec<SearchMatch> {
    let grid = term.grid();
    let last_column = Column(grid.columns().saturating_sub(1));
    let start = Point::new(grid.topmost_line(), Column(0));
    let end = Point::new(grid.bottommost_line(), last_column);

    // Clamp defensively so a degenerate grid never yields an inverted range.
    if start > end {
        return Vec::new();
    }
    let start = start.grid_clamp(term, Boundary::Grid);
    let end = end.grid_clamp(term, Boundary::Grid);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .map(|range| SearchMatch::from_range(&range))
        .collect()
}

/// Maximum matches one `SearchRequest` asks the server for. Ported verbatim
/// from the winit client's `SEARCH_RESULT_LIMIT`.
pub const SEARCH_RESULT_LIMIT: u32 = 256;

/// Blend factor applied to a non-current match's own cell background, so the
/// accent tints it without hiding the text underneath. Ported from the winit
/// renderer's `PASSIVE_MATCH_BLEND`.
const PASSIVE_MATCH_BLEND: f32 = 0.4;

/// The server's answer to the newest `SearchRequest`, handed from the IPC
/// reader thread to the GPUI view.
///
/// This client owns no PTY and therefore no authoritative scrollback, so the
/// match set is whatever the server most recently reported. `version` is bumped
/// on every accepted reply so the view can tell a fresh result set from the one
/// it already adopted without comparing match vectors each frame.
#[derive(Debug, Default)]
pub struct FindResults {
    query: String,
    matches: Vec<ServerMatch>,
    version: u64,
}

impl FindResults {
    /// Record one `SearchResults` reply, superseding any earlier answer.
    pub fn accept(&mut self, query: String, matches: Vec<ServerMatch>) {
        self.query = query;
        self.matches = matches;
        self.version = self.version.wrapping_add(1);
    }

    /// Monotonic counter identifying the currently stored reply.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// The query the stored matches answer.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The stored matches, in the server's reading order.
    #[must_use]
    pub fn matches(&self) -> &[ServerMatch] {
        &self.matches
    }

    /// Clone the stored reply for the view to adopt.
    #[must_use]
    pub fn snapshot(&self) -> (String, Vec<ServerMatch>) {
        (self.query.clone(), self.matches.clone())
    }
}

/// One painted match span, already projected onto the visible viewport.
///
/// Rows and columns are viewport indices, so the paint path can apply the
/// highlight without knowing anything about scrollback coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchHighlight {
    /// Viewport row, `0` at the top of the painted grid.
    pub row: usize,
    /// First highlighted column, inclusive.
    pub start_col: usize,
    /// Last highlighted column, inclusive.
    pub end_col: usize,
    /// Whether this is the match the overlay's `n/m` counter points at.
    pub current: bool,
}

/// Resolved colours for a painted match span.
///
/// The current match takes the opaque accent with a contrast foreground; every
/// other match keeps its own foreground and blends the accent into its
/// background at [`PASSIVE_MATCH_BLEND`]. Ported from the winit
/// `search_highlight_colors` / `blend_search_bg` pair.
#[derive(Debug, Clone, Copy)]
pub struct MatchHighlightColors {
    /// Background of the current match.
    pub current_bg: Rgba,
    /// Foreground of the current match, chosen for contrast against the accent.
    pub current_fg: Rgba,
    /// Accent blended into a non-current match's background.
    pub accent: Rgba,
}

impl MatchHighlightColors {
    /// Derive the highlight colours from the theme's accent.
    #[must_use]
    pub fn from_chrome(chrome: &ChromeColors) -> Self {
        let accent = chrome.accent;
        let luminance =
            0.2126f32.mul_add(accent[0], 0.7152f32.mul_add(accent[1], 0.0722 * accent[2]));
        let current_fg = if luminance > 0.45 {
            Rgba { r: 0.05, g: 0.05, b: 0.05, a: 1.0 }
        } else {
            Rgba { r: 0.98, g: 0.98, b: 0.98, a: 1.0 }
        };
        let opaque_accent = Rgba { a: 1.0, ..srgba(accent) };
        Self { current_bg: opaque_accent, current_fg, accent: opaque_accent }
    }

    /// Blend `bg` towards the accent for a non-current match.
    #[must_use]
    pub fn blend_passive(&self, bg: Rgba) -> Rgba {
        let keep = 1.0 - PASSIVE_MATCH_BLEND;
        Rgba {
            r: bg.r.mul_add(keep, self.accent.r * PASSIVE_MATCH_BLEND),
            g: bg.g.mul_add(keep, self.accent.g * PASSIVE_MATCH_BLEND),
            b: bg.b.mul_add(keep, self.accent.b * PASSIVE_MATCH_BLEND),
            a: 1.0,
        }
    }
}

/// Project `matches` onto a `rows` x `cols` viewport.
///
/// The server reports absolute grid rows: negative rows are scrollback lines
/// above the viewport and `0..rows` are the visible screen. This client paints
/// the active viewport only, so an off-screen match contributes no span rather
/// than being clamped onto a row it does not occupy. Columns are clamped to the
/// last painted column because the server's grid width can lag a resize.
#[must_use]
pub fn visible_highlights(
    matches: &[ServerMatch],
    current: usize,
    rows: usize,
    cols: usize,
) -> Vec<MatchHighlight> {
    if rows == 0 || cols == 0 {
        return Vec::new();
    }
    let last_col = cols - 1;
    matches
        .iter()
        .enumerate()
        .filter_map(|(index, hit)| {
            let row = usize::try_from(hit.row).ok()?;
            if row >= rows {
                return None;
            }
            let start_col = usize::from(hit.col_start).min(last_col);
            let end_col = usize::from(hit.col_end).min(last_col);
            if end_col < start_col {
                return None;
            }
            Some(MatchHighlight { row, start_col, end_col, current: index == current })
        })
        .collect()
}

/// How long the overlay waits after the last query edit before asking the
/// server (spec 017 US8-2).
///
/// Each edit costs the server a full-scrollback scan, and at the default 10,000
/// lines the first snapshot behind it is tens of megabytes; a typed word should
/// buy one round trip, not one per character. 150 ms sits under the ~200 ms
/// gap that reads as a deliberate pause, so a fluent typist never sees it and a
/// hesitant one gets intermediate results.
pub const FIND_QUERY_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// What the find overlay asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindOverlayEvent {
    /// The query settled after [`FIND_QUERY_DEBOUNCE`]; the shell issues a fresh
    /// `SearchRequest` for a non-empty query and simply stops highlighting for
    /// an empty one.
    QueryChanged(String),
    /// The overlay was dismissed (Escape or a backdrop click).
    Dismissed,
}

/// Resolved GPUI colours for the find overlay box.
#[derive(Clone, Copy)]
pub struct FindOverlayColors {
    /// Box background.
    pub bg: Rgba,
    /// Query field background.
    pub input_bg: Rgba,
    /// Border / prefix accent colour.
    pub border: Rgba,
    /// Header text ("Find  3/12").
    pub header_fg: Rgba,
    /// Typed query text.
    pub query_fg: Rgba,
    /// Placeholder text shown before anything is typed.
    pub placeholder_fg: Rgba,
}

impl From<&ChromeColors> for FindOverlayColors {
    fn from(chrome: &ChromeColors) -> Self {
        let mut bg = srgba(chrome.tab_bar_active_bg);
        bg.a = 0.96;
        let mut input_bg = srgba(chrome.status_bar_bg);
        input_bg.a = 0.98;
        let query_fg = srgba(chrome.status_bar_text);
        Self {
            bg,
            input_bg,
            border: srgba(chrome.accent),
            header_fg: srgba(chrome.tab_text_active),
            query_fg,
            placeholder_fg: Rgba { a: query_fg.a * 0.7, ..query_fg },
        }
    }
}

/// The find-in-scrollback overlay: a query field, the server's match set, and
/// the highlighted match index.
///
/// Ported from the winit `SearchOverlay`, with the GPU-quad painter replaced by
/// GPUI elements and the results plumbed in from the IPC reader rather than a
/// `UiEvent`. Editing the query emits [`FindOverlayEvent::QueryChanged`] so the
/// shell — the only holder of the `IpcSink` and the attached session — is what
/// actually puts a `SearchRequest` on the wire.
pub struct FindOverlayView {
    colors: FindOverlayColors,
    query: String,
    matches: Vec<ServerMatch>,
    current: usize,
    /// [`FindResults::version`] of the reply last adopted, so a redraw that
    /// arrives before a new reply does not reset the highlighted index.
    adopted: u64,
    /// Bumped by every query edit so an already-elapsed debounce timer can tell
    /// it was superseded.
    debounce: u64,
    /// The scheduled request. Dropped — and therefore cancelled — whenever a
    /// newer edit replaces it.
    pending: Option<gpui::Task<()>>,
    focus_handle: FocusHandle,
}

impl EventEmitter<FindOverlayEvent> for FindOverlayView {}

impl FindOverlayView {
    /// Open a fresh overlay with an empty query and no matches.
    pub fn new(colors: FindOverlayColors, adopted: u64, cx: &mut Context<Self>) -> Self {
        Self {
            colors,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            adopted,
            debounce: 0,
            pending: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// The query currently typed into the overlay.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The matches the overlay is highlighting.
    #[must_use]
    pub fn matches(&self) -> &[ServerMatch] {
        &self.matches
    }

    /// Index of the highlighted match within [`Self::matches`].
    #[must_use]
    pub const fn current_index(&self) -> usize {
        self.current
    }

    /// Total number of matches the server reported for the current query.
    #[must_use]
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// The visible spans the paint path highlights for a `rows` x `cols` grid.
    #[must_use]
    pub fn highlights(&self, rows: usize, cols: usize) -> Vec<MatchHighlight> {
        visible_highlights(&self.matches, self.current, rows, cols)
    }

    /// Adopt a newer `SearchResults` reply, if there is one.
    ///
    /// A reply answering a stale query is dropped rather than shown: the user
    /// has typed on since it was requested, and its own answer is already in
    /// flight. Returns `true` when the overlay changed and needs a repaint.
    pub fn adopt_results(&mut self, results: &FindResults, cx: &mut Context<Self>) -> bool {
        if results.version() == self.adopted {
            return false;
        }
        self.adopted = results.version();
        if results.query() != self.query {
            return false;
        }
        let (_, matches) = results.snapshot();
        self.matches = matches;
        self.current = 0;
        cx.notify();
        true
    }

    /// Append a typed character and re-issue the search.
    pub fn push_char(&mut self, c: char, cx: &mut Context<Self>) {
        if c.is_control() {
            return;
        }
        self.query.push(c);
        self.restart_search(cx);
    }

    /// Append pasted text, dropping control characters, and re-issue the search.
    pub fn push_str(&mut self, text: &str, cx: &mut Context<Self>) {
        let before = self.query.len();
        self.query.extend(text.chars().filter(|c| !c.is_control()));
        if self.query.len() != before {
            self.restart_search(cx);
        }
    }

    /// Remove the last query character and re-issue the search.
    pub fn pop_char(&mut self, cx: &mut Context<Self>) {
        if self.query.pop().is_some() {
            self.restart_search(cx);
        }
    }

    /// Clear the query entirely (Delete) and drop every highlight.
    pub fn clear_query(&mut self, cx: &mut Context<Self>) {
        if self.query.is_empty() {
            return;
        }
        self.query.clear();
        self.restart_search(cx);
    }

    /// Highlight the next match, wrapping past the last one.
    pub fn next_match(&mut self, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        self.current = (self.current + 1) % self.matches.len();
        cx.notify();
    }

    /// Highlight the previous match, wrapping past the first one.
    pub fn prev_match(&mut self, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let count = self.matches.len();
        self.current = (self.current + count - 1) % count;
        cx.notify();
    }

    /// Dismiss the overlay, clearing its query so a later reopen starts fresh.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.query.clear();
        self.matches.clear();
        self.current = 0;
        // Retire the scheduled request: a search for a query the overlay no
        // longer shows would land after the shell released the server's cached
        // snapshot and re-take it for nobody.
        self.pending = None;
        self.debounce = self.debounce.wrapping_add(1);
        cx.emit(FindOverlayEvent::Dismissed);
    }

    /// Drop the stale match set and schedule a fresh request.
    ///
    /// Clearing first matters: the old query's highlights must not linger on
    /// screen while the new request is in flight, or the grid would contradict
    /// the query field for a frame. The request itself waits out
    /// [`FIND_QUERY_DEBOUNCE`] so a typed word costs the server one
    /// full-scrollback scan rather than one per character (spec 017 US8-2).
    fn restart_search(&mut self, cx: &mut Context<Self>) {
        self.matches.clear();
        self.current = 0;
        self.debounce = self.debounce.wrapping_add(1);
        let generation = self.debounce;
        // Assigning a new task drops the previous one, cancelling its timer.
        self.pending = Some(cx.spawn(async move |this, app| {
            app.background_executor().timer(FIND_QUERY_DEBOUNCE).await;
            this.update(app, |this, ecx| this.emit_if_current(generation, ecx)).ok();
        }));
        cx.notify();
    }

    /// Ask the shell for results only when the timer that woke us is still the
    /// current one — a later edit or a dismiss supersedes an in-flight timer.
    fn emit_if_current(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.debounce != generation {
            return;
        }
        self.pending = None;
        cx.emit(FindOverlayEvent::QueryChanged(self.query.clone()));
    }

    /// The `Find  n/m` header text for the current state.
    fn header(&self) -> String {
        if self.query.is_empty() {
            "Find".to_owned()
        } else if self.matches.is_empty() {
            "Find  no matches".to_owned()
        } else {
            format!("Find  {}/{}", self.current + 1, self.matches.len())
        }
    }
}

impl Render for FindOverlayView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        let query_empty = self.query.is_empty();
        let query_text =
            if query_empty { "Type to search scrollback".to_owned() } else { self.query.clone() };
        let query_color = if query_empty { colors.placeholder_fg } else { colors.query_fg };

        // The box hugs the top-right corner exactly as the winit overlay did, so
        // it never covers the prompt the user is searching from. The backdrop
        // stays click-through except for its own dismiss handler.
        div()
            .track_focus(&self.focus_handle)
            .absolute()
            .inset_0()
            .flex()
            .justify_end()
            // `items_start` keeps the box at its own content height: the
            // backdrop spans the window, and a stretched child would paint a
            // full-height panel down the right edge of the grid.
            .items_start()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _win, ctx| this.dismiss(ctx)),
            )
            .child(
                div()
                    .mt(px(14.0))
                    .mr(px(14.0))
                    .w(px(360.0))
                    .flex()
                    .flex_col()
                    .bg(colors.bg)
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .shadow_lg()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
                    .child(
                        div()
                            .px_2()
                            .pt_1()
                            .text_xs()
                            .text_color(colors.header_fg)
                            .child(self.header()),
                    )
                    .child(
                        div()
                            .mx_1()
                            .mt_1()
                            .mb_1()
                            .px_2()
                            .py_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .bg(colors.input_bg)
                            .rounded_sm()
                            .child(div().text_sm().text_color(colors.border).child("/"))
                            .child(div().text_sm().text_color(query_color).child(query_text)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal_gpui::event::VoidListener;
    use alacritty_terminal_gpui::grid::Dimensions;
    use alacritty_terminal_gpui::term::{Config, Term};
    use vte::ansi::Processor;

    use super::TerminalSearch;

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

    // @lat: [[test#GPUI Terminal Search#Cycles matches with wraparound]]
    #[gpui::test]
    fn cycles_matches_with_wraparound() {
        let term = term_with_output(40, 4, b"foo bar foo baz foo\r\nqux foo end");
        let mut search = TerminalSearch::new(&term, "foo").expect("valid regex");
        assert_eq!(search.len(), 4);
        assert_eq!(search.current_index(), Some(0));

        // Matches are returned top-to-bottom, left-to-right.
        let starts: Vec<_> = search.matches().iter().map(|m| (m.start.row, m.start.col)).collect();
        assert_eq!(starts, vec![(0, 0), (0, 8), (0, 16), (1, 4)]);

        // Forward cycling advances then wraps to the first match.
        assert_eq!(search.select_next().map(|m| m.start.col), Some(8));
        assert_eq!(search.select_next().map(|m| m.start.col), Some(16));
        assert_eq!(search.select_next().map(|m| (m.start.row, m.start.col)), Some((1, 4)));
        assert_eq!(search.current_index(), Some(3));
        assert_eq!(search.select_next().map(|m| (m.start.row, m.start.col)), Some((0, 0)));

        // Backward cycling wraps from the first match to the last.
        assert_eq!(search.select_prev().map(|m| (m.start.row, m.start.col)), Some((1, 4)));
        assert_eq!(search.select_prev().map(|m| m.start.col), Some(16));
    }

    // @lat: [[test#GPUI Terminal Search#Match endpoints cover the whole hit]]
    #[gpui::test]
    fn match_endpoints_cover_whole_hit() {
        let term = term_with_output(40, 2, b"see error_42 here");
        let search = TerminalSearch::new(&term, "error_[0-9]+").expect("valid regex");
        let hit = search.current().expect("one match");
        assert_eq!((hit.start.row, hit.start.col), (0, 4));
        assert_eq!((hit.end.row, hit.end.col), (0, 11));
    }

    // @lat: [[test#GPUI Terminal Search#Empty and unmatched queries stay valid]]
    #[gpui::test]
    fn empty_and_unmatched_queries_stay_valid() {
        let term = term_with_output(20, 2, b"nothing here");

        let empty = TerminalSearch::new(&term, "").expect("empty query is valid");
        assert!(empty.is_empty());
        assert_eq!(empty.current_index(), None);

        let mut unmatched = TerminalSearch::new(&term, "zzz").expect("valid regex");
        assert!(unmatched.is_empty());
        assert_eq!(unmatched.select_next(), None);
        assert_eq!(unmatched.current_index(), None);

        assert!(TerminalSearch::new(&term, "(unclosed").is_none());
    }
}

#[cfg(test)]
mod overlay_tests {
    use std::sync::{Arc, Mutex};

    use gpui::{AppContext as _, Entity, TestAppContext};
    use scribe_common::protocol::SearchMatch as ServerMatch;
    use scribe_common::theme::minimal_dark;

    use super::{
        FIND_QUERY_DEBOUNCE, FindOverlayColors, FindOverlayEvent, FindOverlayView, FindResults,
        visible_highlights,
    };

    fn hit(row: i32, col_start: u16, col_end: u16) -> ServerMatch {
        ServerMatch { row, col_start, col_end }
    }

    type EventLog = Arc<Mutex<Vec<FindOverlayEvent>>>;

    fn overlay(cx: &mut TestAppContext) -> (Entity<FindOverlayView>, EventLog) {
        let colors = FindOverlayColors::from(&minimal_dark().chrome);
        let view = cx.new(|cx| FindOverlayView::new(colors, 0, cx));
        let log: EventLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        cx.update(|app| {
            app.subscribe(&view, move |_, event: &FindOverlayEvent, _| record(&sink, event))
                .detach();
        });
        cx.update(|_| {});
        (view, log)
    }

    fn record(log: &EventLog, event: &FindOverlayEvent) {
        if let Ok(mut guard) = log.lock() {
            guard.push(event.clone());
        }
    }

    fn drain(log: &EventLog, cx: &mut TestAppContext) -> Vec<FindOverlayEvent> {
        cx.update(|_| {});
        let mut guard = log.lock().expect("event log");
        std::mem::take(&mut *guard)
    }

    /// Let the debounce window elapse so a settled query reaches the shell.
    fn settle(cx: &mut TestAppContext) {
        cx.executor().advance_clock(FIND_QUERY_DEBOUNCE);
        cx.run_until_parked();
    }

    // @lat: [[test#GPUI Client Headless Suites#Find overlay#A typed query asks the server once]]
    #[gpui::test]
    fn query_edits_coalesce_into_one_request(cx: &mut TestAppContext) {
        let (view, log) = overlay(cx);

        // Two edits inside one debounce window are one request for the final text.
        view.update(cx, |overlay, ctx| overlay.push_char('e', ctx));
        cx.executor().advance_clock(FIND_QUERY_DEBOUNCE / 2);
        cx.run_until_parked();
        view.update(cx, |overlay, ctx| overlay.push_str("rr", ctx));
        cx.executor().advance_clock(FIND_QUERY_DEBOUNCE / 2);
        cx.run_until_parked();
        assert!(drain(&log, cx).is_empty(), "a restarted timer never fired mid-burst");

        settle(cx);
        assert_eq!(drain(&log, cx), vec![FindOverlayEvent::QueryChanged("err".to_owned())]);
        assert_eq!(view.read_with(cx, |o, _| o.query().to_owned()), "err");

        // Backspace and Delete are query edits too, so each re-asks once settled.
        view.update(cx, FindOverlayView::pop_char);
        settle(cx);
        view.update(cx, FindOverlayView::clear_query);
        settle(cx);
        assert_eq!(
            drain(&log, cx),
            vec![
                FindOverlayEvent::QueryChanged("er".to_owned()),
                FindOverlayEvent::QueryChanged(String::new()),
            ]
        );

        // A no-op edit sends nothing: the query did not change.
        view.update(cx, |overlay, ctx| {
            overlay.pop_char(ctx);
            overlay.clear_query(ctx);
            overlay.push_char('\u{1b}', ctx);
        });
        settle(cx);
        assert!(drain(&log, cx).is_empty());

        // Dismissing retires the scheduled request: it would search a query the
        // overlay no longer shows, after the shell released the server's snapshot.
        view.update(cx, |overlay, ctx| overlay.push_str("err", ctx));
        view.update(cx, FindOverlayView::dismiss);
        settle(cx);
        assert_eq!(drain(&log, cx), vec![FindOverlayEvent::Dismissed]);
    }

    // @lat: [[test#GPUI Client Headless Suites#Find overlay#A stale reply never replaces live matches]]
    #[gpui::test]
    fn only_the_reply_for_the_typed_query_is_adopted(cx: &mut TestAppContext) {
        let (view, _log) = overlay(cx);
        view.update(cx, |overlay, ctx| overlay.push_str("err", ctx));

        // The answer to the earlier "er" is already in flight when "err" is
        // typed; adopting it would highlight cells the user is no longer
        // searching for.
        let mut results = FindResults::default();
        results.accept("er".to_owned(), vec![hit(1, 0, 1)]);
        assert!(!view.update(cx, |overlay, ctx| overlay.adopt_results(&results, ctx)));
        assert_eq!(view.read_with(cx, |o, _| o.match_count()), 0);

        results.accept("err".to_owned(), vec![hit(1, 0, 2), hit(3, 4, 6)]);
        assert!(view.update(cx, |overlay, ctx| overlay.adopt_results(&results, ctx)));
        assert_eq!(view.read_with(cx, |o, _| o.match_count()), 2);
        // The same reply is adopted exactly once, so a redraw cannot reset the
        // highlighted match the user cycled to.
        view.update(cx, FindOverlayView::next_match);
        assert!(!view.update(cx, |overlay, ctx| overlay.adopt_results(&results, ctx)));
        assert_eq!(view.read_with(cx, |o, _| o.current_index()), 1);
    }

    // @lat: [[test#GPUI Client Headless Suites#Find overlay#Cycling wraps and drives the counter]]
    #[gpui::test]
    fn cycling_wraps_in_both_directions(cx: &mut TestAppContext) {
        let (view, _log) = overlay(cx);
        view.update(cx, |overlay, ctx| overlay.push_str("x", ctx));
        let mut results = FindResults::default();
        results.accept("x".to_owned(), vec![hit(0, 0, 0), hit(1, 2, 2), hit(2, 4, 4)]);
        view.update(cx, |overlay, ctx| overlay.adopt_results(&results, ctx));

        view.update(cx, FindOverlayView::prev_match);
        assert_eq!(view.read_with(cx, |o, _| o.current_index()), 2);
        view.update(cx, FindOverlayView::next_match);
        assert_eq!(view.read_with(cx, |o, _| o.current_index()), 0);
        assert_eq!(view.read_with(cx, |o, _| o.header()), "Find  1/3");

        // A query with no matches says so rather than pretending to be at 0/0.
        view.update(cx, |overlay, ctx| overlay.push_str("y", ctx));
        assert_eq!(view.read_with(cx, |o, _| o.header()), "Find  no matches");
        view.update(cx, FindOverlayView::next_match);
        assert_eq!(view.read_with(cx, |o, _| o.current_index()), 0);
    }

    // @lat: [[test#GPUI Client Headless Suites#Find overlay#Only on-screen matches are highlighted]]
    #[test]
    fn scrollback_matches_are_projected_off_the_viewport() {
        let matches = vec![hit(-4, 0, 2), hit(0, 1, 3), hit(2, 70, 90), hit(9, 0, 1)];
        let spans = visible_highlights(&matches, 1, 4, 80);

        // The scrollback hit (row -4) and the below-viewport hit (row 9) paint
        // nothing; the wide hit is clamped to the last painted column.
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].row, spans[0].start_col, spans[0].end_col), (0, 1, 3));
        assert!(spans[0].current, "the overlay's current index carries into the paint path");
        assert_eq!((spans[1].row, spans[1].start_col, spans[1].end_col), (2, 70, 79));
        assert!(!spans[1].current);

        // A degenerate grid highlights nothing rather than panicking on the
        // last-column arithmetic.
        assert!(visible_highlights(&matches, 0, 0, 0).is_empty());
    }
}
