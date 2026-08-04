//! Server-owned ordering seam for one terminal session's image state.
//!
//! The seam consumes the production PTY graphics framer and returns typed,
//! caller-owned boundaries. Live IPC fanout and PTY reply write-back remain
//! downstream integration work.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::term::TermMode;
use cursor_icon::CursorIcon;
use scribe_common::terminal_images::{
    ImageLimits, TerminalImageDefinition, TerminalImageGeneration, TerminalImageId,
    TerminalImagePlacement, TerminalOutputSequence, TerminalPlacementId, TerminalScreenKind,
};
use scribe_pty::graphics_framing::{
    GraphicsEvent, GraphicsFailure, GraphicsFramer, KittyCommand, PendingGraphicsTransfer,
    RawByteRange, RawBytes, SixelCommand, SixelMode,
};
use unicode_width::UnicodeWidthChar;
use vte::ansi::Processor as AnsiProcessor;
use vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, NamedPrivateMode,
    PrivateMode, Rgb, ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

/// Cursor facts read directly from one Alacritty grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalCursorObservation {
    pub row: i32,
    pub column: u16,
    pub input_needs_wrap: bool,
}

/// Dimensions of one Alacritty grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalGridSizeObservation {
    pub columns: u16,
    pub rows: u16,
}

/// Payload-free facts retained independently for each screen grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalScreenObservation {
    pub size: TerminalGridSizeObservation,
    /// Real cursor facts from the last time this grid was active. `None`
    /// means a resize reflowed this grid while inactive, so Alacritty exposes
    /// no public state from which an exact post-resize value can be read.
    pub cursor: Option<TerminalCursorObservation>,
    /// Real saved-cursor facts, unavailable under the same condition as
    /// [`Self::cursor`]. Activating the grid refreshes both values.
    pub saved_cursor: Option<TerminalCursorObservation>,
}

/// Typed semantic consequence observed while the real Alacritty `Term` handled
/// one source span. All row and column bounds are half-open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservedTerminalGridEffect {
    Scroll { screen: TerminalScreenKind, top: u16, bottom: u16, rows: i32 },
    EraseCells { screen: TerminalScreenKind, top: u16, left: u16, bottom: u16, right: u16 },
    EraseDisplay { screen: TerminalScreenKind },
    Resize { primary: TerminalGridSizeObservation, alternate: TerminalGridSizeObservation },
    SwitchScreen { from: TerminalScreenKind, to: TerminalScreenKind },
    SoftReset,
    HardReset,
}

/// Alacritty-derived terminal facts after one ordered source span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalGridObservation {
    pub active_screen: TerminalScreenKind,
    pub primary: TerminalScreenObservation,
    pub alternate: TerminalScreenObservation,
    pub margin_top: u16,
    pub margin_bottom: u16,
    pub origin_mode: bool,
    pub line_wrap_mode: bool,
    pub cell_width_pixels: u16,
    pub cell_height_pixels: u16,
    pub effects: Vec<ObservedTerminalGridEffect>,
}

impl Default for TerminalGridObservation {
    fn default() -> Self {
        let size = TerminalGridSizeObservation { columns: 80, rows: 24 };
        Self {
            active_screen: TerminalScreenKind::Primary,
            primary: TerminalScreenObservation { size, ..TerminalScreenObservation::default() },
            alternate: TerminalScreenObservation { size, ..TerminalScreenObservation::default() },
            margin_top: 0,
            margin_bottom: 24,
            origin_mode: false,
            line_wrap_mode: true,
            cell_width_pixels: 1,
            cell_height_pixels: 1,
            effects: Vec::new(),
        }
    }
}

/// One observation aligned to a half-open range of the original PTY stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalGridSpanObservation {
    pub range: RawByteRange,
    pub observation: TerminalGridObservation,
}

#[derive(Debug, Default)]
struct TerminalGridObserverState {
    observation: TerminalGridObservation,
    initialized: bool,
}

/// Cloneable session-owned handle shared only with production feed and resize
/// call sites. It never retains cells or image payloads.
#[derive(Clone, Debug)]
pub struct TerminalGridObserverHandle(Arc<Mutex<TerminalGridObserverState>>);

impl Default for TerminalGridObserverHandle {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(TerminalGridObserverState::default())))
    }
}

impl TerminalGridObserverHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, TerminalGridObserverState> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Return the latest payload-free production observation.
    #[must_use]
    pub fn observation(&self) -> TerminalGridObservation {
        self.lock().observation.clone()
    }

    /// Update terminal cell pixel metrics used by image coordinate capture.
    pub fn set_cell_size(&self, width: u16, height: u16) {
        let mut state = self.lock();
        state.observation.cell_width_pixels = width.max(1);
        state.observation.cell_height_pixels = height.max(1);
    }
}

fn terminal_screen<T>(term: &Term<T>) -> TerminalScreenKind {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        TerminalScreenKind::Alternate
    } else {
        TerminalScreenKind::Primary
    }
}

fn observed_cursor<T>(term: &Term<T>, saved: bool) -> TerminalCursorObservation {
    let cursor = if saved { &term.grid().saved_cursor } else { &term.grid().cursor };
    TerminalCursorObservation {
        row: cursor.point.line.0,
        column: u16::try_from(cursor.point.column.0).unwrap_or(u16::MAX),
        input_needs_wrap: cursor.input_needs_wrap,
    }
}

fn observed_size<T>(term: &Term<T>) -> TerminalGridSizeObservation {
    TerminalGridSizeObservation {
        columns: u16::try_from(term.columns()).unwrap_or(u16::MAX),
        rows: u16::try_from(term.screen_lines()).unwrap_or(u16::MAX),
    }
}

impl TerminalGridObserverState {
    fn initialize<T>(&mut self, term: &Term<T>) {
        if self.initialized {
            return;
        }
        let size = observed_size(term);
        self.observation.primary.size = size;
        self.observation.alternate.size = size;
        self.observation.margin_top = 0;
        self.observation.margin_bottom = size.rows;
        self.sync_active(term);
        self.initialized = true;
    }

    fn sync_active<T>(&mut self, term: &Term<T>) {
        let screen = terminal_screen(term);
        let grid = TerminalScreenObservation {
            size: observed_size(term),
            cursor: Some(observed_cursor(term, false)),
            saved_cursor: Some(observed_cursor(term, true)),
        };
        match screen {
            TerminalScreenKind::Primary => self.observation.primary = grid,
            TerminalScreenKind::Alternate => self.observation.alternate = grid,
        }
        self.observation.active_screen = screen;
        self.observation.origin_mode = term.mode().contains(TermMode::ORIGIN);
        self.observation.line_wrap_mode = term.mode().contains(TermMode::LINE_WRAP);
    }

    fn finish_span<T>(&mut self, term: &Term<T>, effects: Vec<ObservedTerminalGridEffect>) {
        self.sync_active(term);
        self.observation.effects = effects;
    }

    fn observe_resize<T>(&mut self, term: &Term<T>, changed: bool) {
        self.initialize(term);
        if !changed {
            self.sync_active(term);
            self.observation.effects.clear();
            return;
        }
        let size = observed_size(term);
        self.observation.primary.size = size;
        self.observation.alternate.size = size;
        self.observation.margin_top = 0;
        self.observation.margin_bottom = size.rows;
        let inactive = match terminal_screen(term) {
            TerminalScreenKind::Primary => &mut self.observation.alternate,
            TerminalScreenKind::Alternate => &mut self.observation.primary,
        };
        inactive.cursor = None;
        inactive.saved_cursor = None;
        for grid in [&mut self.observation.primary, &mut self.observation.alternate] {
            grid.size = size;
        }
        self.sync_active(term);
        self.observation.effects = vec![ObservedTerminalGridEffect::Resize {
            primary: self.observation.primary.size,
            alternate: self.observation.alternate.size,
        }];
    }
}

/// Delegating observer around the production `Term` handler. The existing VTE
/// processor invokes this once; every callback is forwarded to the same real
/// `Term`, while image-relevant effects are captured from typed callbacks and
/// actual pre/post cursor state.
struct ObservedTermHandler<'a, T> {
    term: &'a mut Term<T>,
    state: &'a mut TerminalGridObserverState,
    effects: Vec<ObservedTerminalGridEffect>,
}

impl<'a, T> ObservedTermHandler<'a, T> {
    fn new(term: &'a mut Term<T>, state: &'a mut TerminalGridObserverState) -> Self {
        state.initialize(term);
        state.observation.effects.clear();
        Self { term, state, effects: Vec::new() }
    }

    fn screen(&self) -> TerminalScreenKind {
        terminal_screen(self.term)
    }

    fn active(&self) -> TerminalScreenObservation {
        TerminalScreenObservation {
            size: observed_size(self.term),
            cursor: Some(observed_cursor(self.term, false)),
            saved_cursor: Some(observed_cursor(self.term, true)),
        }
    }

    fn scroll(&mut self, top: u16, bottom: u16, rows: i32) {
        let height = i32::from(bottom.saturating_sub(top));
        let rows = rows.clamp(-height, height);
        if height > 0 && rows != 0 {
            self.effects.push(ObservedTerminalGridEffect::Scroll {
                screen: self.screen(),
                top,
                bottom,
                rows,
            });
        }
    }

    fn erase(&mut self, top: u16, left: u16, bottom: u16, right: u16) {
        let size = observed_size(self.term);
        let bottom = bottom.min(size.rows);
        let right = right.min(size.columns);
        if top < bottom && left < right {
            self.effects.push(ObservedTerminalGridEffect::EraseCells {
                screen: self.screen(),
                top,
                left,
                bottom,
                right,
            });
        }
    }

    fn implicit_scroll_after_cursor_action(&mut self, before: TerminalScreenObservation) {
        let margin_bottom = self.state.observation.margin_bottom;
        if before
            .cursor
            .is_some_and(|cursor| cursor.row.saturating_add(1) == i32::from(margin_bottom))
        {
            self.scroll(self.state.observation.margin_top, margin_bottom, 1);
        }
    }

    fn observe_deccolm(&mut self) {
        let size = observed_size(self.term);
        self.state.observation.margin_top = 0;
        self.state.observation.margin_bottom = size.rows;
        self.effects.push(ObservedTerminalGridEffect::EraseDisplay { screen: self.screen() });
    }

    fn finish(mut self) -> TerminalGridObservation {
        let effects = std::mem::take(&mut self.effects);
        self.state.finish_span(self.term, effects);
        self.state.observation.clone()
    }
}

impl<T: EventListener> Handler for ObservedTermHandler<'_, T> {
    fn input(&mut self, c: char) {
        let before = self.active();
        let width = c.width();
        let columns = self.term.columns();
        let line_wrap = self.term.mode().contains(TermMode::LINE_WRAP);
        Handler::input(self.term, c);

        // Match the pinned Alacritty 0.26.0-rc1 `Term::input` path. A
        // zero-width character returns before deferred wrapping, while a wide
        // character which does not fit emits a leading spacer and wraps even
        // without a pre-existing deferred wrap. This uses the same width
        // implementation and live Term mode as Alacritty, then records only
        // wrapline calls which actually hit the bottom scroll margin.
        let Some(width) = width.filter(|width| *width > 0) else { return };
        if !line_wrap {
            return;
        }
        let Some(cursor) = before.cursor else { return };
        let margin_bottom = i32::from(self.state.observation.margin_bottom);
        let mut row = cursor.row;
        let column = if cursor.input_needs_wrap {
            if row.saturating_add(1) == margin_bottom {
                self.scroll(
                    self.state.observation.margin_top,
                    self.state.observation.margin_bottom,
                    1,
                );
            } else if row.saturating_add(1)
                < i32::try_from(self.term.screen_lines()).unwrap_or(i32::MAX)
            {
                row = row.saturating_add(1);
            }
            0
        } else {
            usize::from(cursor.column)
        };
        if width == 2
            && column.saturating_add(1) >= columns
            && row.saturating_add(1) == margin_bottom
        {
            self.scroll(self.state.observation.margin_top, self.state.observation.margin_bottom, 1);
        }
    }

    fn linefeed(&mut self) {
        let before = self.active();
        Handler::linefeed(self.term);
        self.implicit_scroll_after_cursor_action(before);
    }

    fn newline(&mut self) {
        let before = self.active();
        Handler::newline(self.term);
        self.implicit_scroll_after_cursor_action(before);
    }

    fn reverse_index(&mut self) {
        let before = self.active();
        Handler::reverse_index(self.term);
        if before
            .cursor
            .is_some_and(|cursor| cursor.row == i32::from(self.state.observation.margin_top))
        {
            self.scroll(
                self.state.observation.margin_top,
                self.state.observation.margin_bottom,
                -1,
            );
        }
    }

    fn scroll_up(&mut self, rows: usize) {
        Handler::scroll_up(self.term, rows);
        self.scroll(
            self.state.observation.margin_top,
            self.state.observation.margin_bottom,
            i32::try_from(rows).unwrap_or(i32::MAX),
        );
    }

    fn scroll_down(&mut self, rows: usize) {
        Handler::scroll_down(self.term, rows);
        self.scroll(
            self.state.observation.margin_top,
            self.state.observation.margin_bottom,
            -i32::try_from(rows).unwrap_or(i32::MAX),
        );
    }

    fn insert_blank_lines(&mut self, rows: usize) {
        let before = self.active();
        Handler::insert_blank_lines(self.term, rows);
        let row =
            before.cursor.and_then(|cursor| u16::try_from(cursor.row).ok()).unwrap_or(u16::MAX);
        let bottom = self.state.observation.margin_bottom;
        if row >= self.state.observation.margin_top && row < bottom {
            self.scroll(row, bottom, -i32::try_from(rows).unwrap_or(i32::MAX));
        }
    }

    fn delete_lines(&mut self, rows: usize) {
        let before = self.active();
        Handler::delete_lines(self.term, rows);
        let row =
            before.cursor.and_then(|cursor| u16::try_from(cursor.row).ok()).unwrap_or(u16::MAX);
        let bottom = self.state.observation.margin_bottom;
        if row >= self.state.observation.margin_top && row < bottom {
            self.scroll(row, bottom, i32::try_from(rows).unwrap_or(i32::MAX));
        }
    }

    fn erase_chars(&mut self, count: usize) {
        let before = self.active();
        Handler::erase_chars(self.term, count);
        let Some(cursor) = before.cursor else { return };
        let row = u16::try_from(cursor.row).unwrap_or(u16::MAX);
        let right = cursor.column.saturating_add(u16::try_from(count).unwrap_or(u16::MAX));
        self.erase(row, cursor.column, row.saturating_add(1), right);
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        let before = self.active();
        let Some(cursor) = before.cursor else {
            Handler::clear_line(self.term, mode);
            return;
        };
        let row = u16::try_from(cursor.row).unwrap_or(u16::MAX);
        let (left, right) = match mode {
            LineClearMode::Right if cursor.input_needs_wrap => {
                Handler::clear_line(self.term, LineClearMode::Right);
                return;
            }
            LineClearMode::Right => {
                Handler::clear_line(self.term, LineClearMode::Right);
                (cursor.column, before.size.columns)
            }
            LineClearMode::Left => {
                Handler::clear_line(self.term, LineClearMode::Left);
                (0, cursor.column.saturating_add(1))
            }
            LineClearMode::All => {
                Handler::clear_line(self.term, LineClearMode::All);
                (0, before.size.columns)
            }
        };
        self.erase(row, left, row.saturating_add(1), right);
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        let before = self.active();
        let Some(cursor) = before.cursor else {
            Handler::clear_screen(self.term, mode);
            return;
        };
        let row = u16::try_from(cursor.row).unwrap_or(u16::MAX);
        match mode {
            ClearMode::All => {
                self.effects
                    .push(ObservedTerminalGridEffect::EraseDisplay { screen: self.screen() });
                Handler::clear_screen(self.term, ClearMode::All);
            }
            ClearMode::Below => {
                self.erase(row, cursor.column, row.saturating_add(1), before.size.columns);
                self.erase(row.saturating_add(1), 0, before.size.rows, before.size.columns);
                Handler::clear_screen(self.term, ClearMode::Below);
            }
            ClearMode::Above => {
                if cursor.row > 1 {
                    self.erase(0, 0, row, before.size.columns);
                }
                self.erase(row, 0, row.saturating_add(1), cursor.column.saturating_add(1));
                Handler::clear_screen(self.term, ClearMode::Above);
            }
            ClearMode::Saved => Handler::clear_screen(self.term, ClearMode::Saved),
        }
    }

    fn reset_state(&mut self) {
        Handler::reset_state(self.term);
        let size = observed_size(self.term);
        self.state.observation.primary =
            TerminalScreenObservation { size, ..TerminalScreenObservation::default() };
        self.state.observation.alternate =
            TerminalScreenObservation { size, ..TerminalScreenObservation::default() };
        self.state.observation.margin_top = 0;
        self.state.observation.margin_bottom = size.rows;
        self.effects.push(ObservedTerminalGridEffect::HardReset);
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        let before_screen = self.screen();
        let before = self.active();
        Handler::set_private_mode(self.term, mode);
        if matches!(mode, PrivateMode::Named(NamedPrivateMode::ColumnMode)) {
            self.observe_deccolm();
        }
        let after_screen = self.screen();
        if before_screen != after_screen {
            match before_screen {
                TerminalScreenKind::Primary => {
                    self.state.observation.primary = before;
                    // `Term::swap_alt` overwrites primary saved cursor with
                    // current cursor before swapping to the alternate grid.
                    self.state.observation.primary.saved_cursor = before.cursor;
                }
                TerminalScreenKind::Alternate => self.state.observation.alternate = before,
            }
            self.state.sync_active(self.term);
            self.effects.push(ObservedTerminalGridEffect::SwitchScreen {
                from: before_screen,
                to: after_screen,
            });
        }
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        let before_screen = self.screen();
        let before = self.active();
        Handler::unset_private_mode(self.term, mode);
        if matches!(mode, PrivateMode::Named(NamedPrivateMode::ColumnMode)) {
            self.observe_deccolm();
        }
        let after_screen = self.screen();
        if before_screen != after_screen {
            match before_screen {
                TerminalScreenKind::Primary => self.state.observation.primary = before,
                TerminalScreenKind::Alternate => self.state.observation.alternate = before,
            }
            self.state.sync_active(self.term);
            self.effects.push(ObservedTerminalGridEffect::SwitchScreen {
                from: before_screen,
                to: after_screen,
            });
        }
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        Handler::set_scrolling_region(self.term, top, bottom);
        let rows = observed_size(self.term).rows;
        let bottom =
            u16::try_from(bottom.unwrap_or(usize::from(rows))).unwrap_or(u16::MAX).min(rows);
        let top = u16::try_from(top.saturating_sub(1)).unwrap_or(u16::MAX).min(rows);
        if top < bottom {
            self.state.observation.margin_top = top;
            self.state.observation.margin_bottom = bottom;
        }
    }

    fn set_title(&mut self, value: Option<String>) {
        Handler::set_title(self.term, value);
    }
    fn set_cursor_style(&mut self, value: Option<CursorStyle>) {
        Handler::set_cursor_style(self.term, value);
    }
    fn set_cursor_shape(&mut self, value: CursorShape) {
        Handler::set_cursor_shape(self.term, value);
    }
    fn goto(&mut self, line: i32, column: usize) {
        Handler::goto(self.term, line, column);
    }
    fn goto_line(&mut self, line: i32) {
        Handler::goto_line(self.term, line);
    }
    fn goto_col(&mut self, column: usize) {
        Handler::goto_col(self.term, column);
    }
    fn insert_blank(&mut self, count: usize) {
        Handler::insert_blank(self.term, count);
    }
    fn move_up(&mut self, rows: usize) {
        Handler::move_up(self.term, rows);
    }
    fn move_down(&mut self, rows: usize) {
        Handler::move_down(self.term, rows);
    }
    fn identify_terminal(&mut self, intermediate: Option<char>) {
        Handler::identify_terminal(self.term, intermediate);
    }
    fn device_status(&mut self, status: usize) {
        Handler::device_status(self.term, status);
    }
    fn move_forward(&mut self, columns: usize) {
        Handler::move_forward(self.term, columns);
    }
    fn move_backward(&mut self, columns: usize) {
        Handler::move_backward(self.term, columns);
    }
    fn move_down_and_cr(&mut self, rows: usize) {
        Handler::move_down_and_cr(self.term, rows);
    }
    fn move_up_and_cr(&mut self, rows: usize) {
        Handler::move_up_and_cr(self.term, rows);
    }
    fn put_tab(&mut self, count: u16) {
        Handler::put_tab(self.term, count);
    }
    fn backspace(&mut self) {
        Handler::backspace(self.term);
    }
    fn carriage_return(&mut self) {
        Handler::carriage_return(self.term);
    }
    fn bell(&mut self) {
        Handler::bell(self.term);
    }
    fn substitute(&mut self) {
        Handler::substitute(self.term);
    }
    fn set_horizontal_tabstop(&mut self) {
        Handler::set_horizontal_tabstop(self.term);
    }
    fn delete_chars(&mut self, count: usize) {
        Handler::delete_chars(self.term, count);
    }
    fn move_backward_tabs(&mut self, count: u16) {
        Handler::move_backward_tabs(self.term, count);
    }
    fn move_forward_tabs(&mut self, count: u16) {
        Handler::move_forward_tabs(self.term, count);
    }
    fn save_cursor_position(&mut self) {
        Handler::save_cursor_position(self.term);
    }
    fn restore_cursor_position(&mut self) {
        Handler::restore_cursor_position(self.term);
    }
    fn clear_tabs(&mut self, mode: TabulationClearMode) {
        Handler::clear_tabs(self.term, mode);
    }
    fn set_tabs(&mut self, interval: u16) {
        Handler::set_tabs(self.term, interval);
    }
    fn terminal_attribute(&mut self, attr: Attr) {
        Handler::terminal_attribute(self.term, attr);
    }
    fn set_mode(&mut self, mode: Mode) {
        Handler::set_mode(self.term, mode);
    }
    fn unset_mode(&mut self, mode: Mode) {
        Handler::unset_mode(self.term, mode);
    }
    fn report_mode(&mut self, mode: Mode) {
        Handler::report_mode(self.term, mode);
    }
    fn report_private_mode(&mut self, mode: PrivateMode) {
        Handler::report_private_mode(self.term, mode);
    }
    fn set_keypad_application_mode(&mut self) {
        Handler::set_keypad_application_mode(self.term);
    }
    fn unset_keypad_application_mode(&mut self) {
        Handler::unset_keypad_application_mode(self.term);
    }
    fn set_active_charset(&mut self, index: CharsetIndex) {
        Handler::set_active_charset(self.term, index);
    }
    fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
        Handler::configure_charset(self.term, index, charset);
    }
    fn set_color(&mut self, index: usize, color: Rgb) {
        Handler::set_color(self.term, index, color);
    }
    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        Handler::dynamic_color_sequence(self.term, prefix, index, terminator);
    }
    fn reset_color(&mut self, index: usize) {
        Handler::reset_color(self.term, index);
    }
    fn clipboard_store(&mut self, clipboard: u8, bytes: &[u8]) {
        Handler::clipboard_store(self.term, clipboard, bytes);
    }
    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        Handler::clipboard_load(self.term, clipboard, terminator);
    }
    fn decaln(&mut self) {
        Handler::decaln(self.term);
    }
    fn push_title(&mut self) {
        Handler::push_title(self.term);
    }
    fn pop_title(&mut self) {
        Handler::pop_title(self.term);
    }
    fn text_area_size_pixels(&mut self) {
        Handler::text_area_size_pixels(self.term);
    }
    fn text_area_size_chars(&mut self) {
        Handler::text_area_size_chars(self.term);
    }
    fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        Handler::set_hyperlink(self.term, hyperlink);
    }
    fn set_mouse_cursor_icon(&mut self, icon: CursorIcon) {
        Handler::set_mouse_cursor_icon(self.term, icon);
    }
    fn report_keyboard_mode(&mut self) {
        Handler::report_keyboard_mode(self.term);
    }
    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        Handler::push_keyboard_mode(self.term, mode);
    }
    fn pop_keyboard_modes(&mut self, count: u16) {
        Handler::pop_keyboard_modes(self.term, count);
    }
    fn set_keyboard_mode(&mut self, mode: KeyboardModes, behavior: KeyboardModesApplyBehavior) {
        Handler::set_keyboard_mode(self.term, mode, behavior);
    }
    fn set_modify_other_keys(&mut self, mode: ModifyOtherKeys) {
        Handler::set_modify_other_keys(self.term, mode);
    }
    fn report_modify_other_keys(&mut self) {
        Handler::report_modify_other_keys(self.term);
    }
    fn set_scp(&mut self, path: ScpCharPath, mode: ScpUpdateMode) {
        Handler::set_scp(self.term, path, mode);
    }
}

/// Immutable process policy shared by every session image seam.
#[derive(Debug)]
pub struct TerminalImageProcessPolicy {
    limits: ImageLimits,
    output_sequence_ceiling: u64,
}

impl TerminalImageProcessPolicy {
    /// Construct the frozen terminal-images-v1 process policy.
    #[must_use]
    pub fn v1() -> Arc<Self> {
        static POLICY: OnceLock<Arc<TerminalImageProcessPolicy>> = OnceLock::new();
        Arc::clone(POLICY.get_or_init(|| {
            Arc::new(Self { limits: ImageLimits::V1, output_sequence_ceiling: u64::MAX })
        }))
    }

    /// Construct immutable v1 policy with a smaller sequence ceiling for
    /// deterministic exhaustion validation through the production seam.
    #[must_use]
    pub fn with_sequence_ceiling_for_validation(output_sequence_ceiling: u64) -> Arc<Self> {
        Arc::new(Self { limits: ImageLimits::V1, output_sequence_ceiling })
    }

    /// Return a copy of the immutable process limits.
    #[must_use]
    pub fn limits(&self) -> ImageLimits {
        self.limits
    }
}

/// Image-side meaning of one ordered graphics boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalImageBoundary {
    Kitty(KittyCommand),
    Sixel(SixelCommand),
    SixelMode { mode: SixelMode, enabled: bool },
    Failure(GraphicsFailure),
}

/// One output from production framing, in original PTY byte order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionTerminalOutput {
    /// Bytes to feed to the ordinary terminal exactly once.
    Raw(RawBytes),
    /// An image boundary assigned the session's monotonic output sequence.
    Image { sequence: TerminalOutputSequence, range: RawByteRange, boundary: TerminalImageBoundary },
}

/// Caller-owned result of one PTY read; no output is fanned out by the seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTerminalCommit {
    pub generation: TerminalImageGeneration,
    pub through_sequence: TerminalOutputSequence,
    pub outputs: Vec<SessionTerminalOutput>,
    pub input_range: RawByteRange,
    pub grid_observations: Vec<TerminalGridSpanObservation>,
}

/// Failure at the session ordering seam before any image state is committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTerminalError {
    SequenceExhausted,
}

impl fmt::Display for SessionTerminalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SequenceExhausted => {
                formatter.write_str("terminal image output sequence exhausted")
            }
        }
    }
}

impl std::error::Error for SessionTerminalError {}

/// Payload-free state facts used by production inspection and functional gates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionTerminalState {
    pub generation: TerminalImageGeneration,
    pub sequence: TerminalOutputSequence,
    pub active_screen: TerminalScreenKind,
    pub definition_count: usize,
    pub placement_count: usize,
    pub pending_transfer: Option<PendingGraphicsTransfer>,
}

/// Exact payload-free work counters for the framing commit strategy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionTerminalFramingWork {
    /// Reads parsed directly after the input-length boundary preflight.
    pub direct_reads: u64,
    /// Reads parsed against a cloned framer because sequence space was tight.
    pub speculative_clone_reads: u64,
}

/// Authoritative image-state ownership seam for one server terminal session.
// @lat: [[terminal-images#Terminal Images#Server-Owned Session State Seam]]
pub struct SessionTerminal {
    policy: Arc<TerminalImageProcessPolicy>,
    framer: GraphicsFramer,
    generation: TerminalImageGeneration,
    sequence: TerminalOutputSequence,
    active_screen: TerminalScreenKind,
    definitions: BTreeMap<TerminalImageId, TerminalImageDefinition>,
    placements: BTreeMap<
        (TerminalScreenKind, TerminalImageId, TerminalPlacementId),
        TerminalImagePlacement,
    >,
    pending_transfer: Option<PendingGraphicsTransfer>,
    framing_work: SessionTerminalFramingWork,
    grid_observer: TerminalGridObserverHandle,
}

impl SessionTerminal {
    /// Construct a session from its process owner's shared immutable policy.
    #[must_use]
    pub fn new(policy: Arc<TerminalImageProcessPolicy>) -> Self {
        let max_control_string_bytes =
            usize::try_from(policy.limits.max_control_string_bytes).unwrap_or(usize::MAX);
        Self {
            policy,
            framer: GraphicsFramer::with_max_control_string_bytes(max_control_string_bytes),
            generation: TerminalImageGeneration(1),
            sequence: TerminalOutputSequence(0),
            active_screen: TerminalScreenKind::Primary,
            definitions: BTreeMap::new(),
            placements: BTreeMap::new(),
            pending_transfer: None,
            framing_work: SessionTerminalFramingWork::default(),
            grid_observer: TerminalGridObserverHandle::default(),
        }
    }

    /// Consume one PTY read and return raw/image boundaries without fanout.
    pub fn process_bytes(
        &mut self,
        bytes: &[u8],
    ) -> Result<SessionTerminalCommit, SessionTerminalError> {
        let input_start = self.framer.offset();
        let input_end = input_start.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let input_range = RawByteRange { start: input_start, end: input_end };
        // Every input byte can complete at most one non-Raw boundary: active
        // completion/failure consumes that byte, candidate fallback can only
        // reprocess it as ground input, and SixelMode's companion Raw event is
        // not sequenced. When this conservative bound fits, mutate the framer
        // directly and avoid cloning a retained transfer of up to 16 MiB.
        let boundary_upper_bound =
            u64::try_from(bytes.len()).map_err(|_| SessionTerminalError::SequenceExhausted)?;
        let direct_through = self
            .sequence
            .0
            .checked_add(boundary_upper_bound)
            .filter(|sequence| *sequence <= self.policy.output_sequence_ceiling);

        if direct_through.is_some() {
            let events = self.framer.push(bytes);
            self.record_direct_read();
            return Ok(self.commit_events(events, None, input_range));
        }

        // Only reads close enough to sequence exhaustion to fail the safe
        // upper bound need rollback parsing. The original framer and all
        // canonical state remain untouched when actual emitted events exceed
        // the remaining sequence capacity.
        let mut candidate_framer = self.framer.clone();
        self.record_speculative_clone();
        let events = candidate_framer.push(bytes);
        let through_sequence = self.preflight_sequence(&events)?;
        self.framer = candidate_framer;
        Ok(self.commit_events(events, Some(through_sequence), input_range))
    }

    fn commit_events(
        &mut self,
        events: Vec<GraphicsEvent>,
        admitted_sequence: Option<TerminalOutputSequence>,
        input_range: RawByteRange,
    ) -> SessionTerminalCommit {
        let mut output_sequence = self.sequence;
        let mut outputs = Vec::with_capacity(events.len());
        for event in events {
            self.append_event(event, &mut output_sequence, &mut outputs);
        }
        if let Some(admitted_sequence) = admitted_sequence {
            assert_eq!(
                output_sequence, admitted_sequence,
                "sequence preflight must equal committed image boundary count"
            );
        }
        self.sequence = output_sequence;
        self.pending_transfer = self.framer.pending_transfer();
        SessionTerminalCommit {
            generation: self.generation,
            through_sequence: self.sequence,
            outputs,
            input_range,
            grid_observations: Vec::new(),
        }
    }

    fn record_direct_read(&mut self) {
        let direct_reads = self.framing_work.direct_reads.checked_add(1);
        assert!(direct_reads.is_some(), "framing work counter exhausted");
        self.framing_work.direct_reads = direct_reads.unwrap_or(self.framing_work.direct_reads);
    }

    fn record_speculative_clone(&mut self) {
        let speculative_clone_reads = self.framing_work.speculative_clone_reads.checked_add(1);
        assert!(speculative_clone_reads.is_some(), "framing work counter exhausted");
        self.framing_work.speculative_clone_reads =
            speculative_clone_reads.unwrap_or(self.framing_work.speculative_clone_reads);
    }

    /// Record the screen selected by the production terminal observer.
    pub fn observe_active_screen(&mut self, screen: TerminalScreenKind) {
        self.active_screen = screen;
    }

    /// Return the session-owned production terminal observation handle.
    #[must_use]
    pub fn grid_observer(&self) -> TerminalGridObserverHandle {
        self.grid_observer.clone()
    }

    /// Return payload-free ownership facts.
    #[must_use]
    pub fn state(&self) -> SessionTerminalState {
        SessionTerminalState {
            generation: self.generation,
            sequence: self.sequence,
            active_screen: self.active_screen,
            definition_count: self.definitions.len(),
            placement_count: self.placements.len(),
            pending_transfer: self.pending_transfer,
        }
    }

    /// Return exact work counters without exposing retained image payloads.
    #[must_use]
    pub fn framing_work(&self) -> SessionTerminalFramingWork {
        self.framing_work
    }

    /// Confirm that two sessions use the exact same process policy object.
    #[must_use]
    pub fn shares_process_policy_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.policy, &other.policy)
    }

    fn preflight_sequence(
        &self,
        events: &[GraphicsEvent],
    ) -> Result<TerminalOutputSequence, SessionTerminalError> {
        let image_boundaries =
            events.iter().filter(|event| !matches!(event, GraphicsEvent::Raw(_))).count();
        let image_boundaries =
            u64::try_from(image_boundaries).map_err(|_| SessionTerminalError::SequenceExhausted)?;
        self.sequence
            .0
            .checked_add(image_boundaries)
            .filter(|sequence| *sequence <= self.policy.output_sequence_ceiling)
            .map(TerminalOutputSequence)
            .ok_or(SessionTerminalError::SequenceExhausted)
    }

    fn append_event(
        &self,
        event: GraphicsEvent,
        sequence: &mut TerminalOutputSequence,
        outputs: &mut Vec<SessionTerminalOutput>,
    ) {
        match event {
            GraphicsEvent::Raw(raw) => outputs.push(SessionTerminalOutput::Raw(raw)),
            GraphicsEvent::Kitty { range, command } => {
                self.append_image(range, TerminalImageBoundary::Kitty(command), sequence, outputs);
            }
            GraphicsEvent::Sixel { range, command } => {
                self.append_image(range, TerminalImageBoundary::Sixel(command), sequence, outputs);
            }
            GraphicsEvent::SixelMode(change) => {
                let range = change.raw.range;
                outputs.push(SessionTerminalOutput::Raw(change.raw));
                self.append_image(
                    range,
                    TerminalImageBoundary::SixelMode { mode: change.mode, enabled: change.enabled },
                    sequence,
                    outputs,
                );
            }
            GraphicsEvent::Failure(failure) => {
                let range = failure.range;
                self.append_image(
                    range,
                    TerminalImageBoundary::Failure(failure),
                    sequence,
                    outputs,
                );
            }
        }
    }

    fn append_image(
        &self,
        range: RawByteRange,
        boundary: TerminalImageBoundary,
        sequence: &mut TerminalOutputSequence,
        outputs: &mut Vec<SessionTerminalOutput>,
    ) {
        let next =
            sequence.0.checked_add(1).filter(|next| *next <= self.policy.output_sequence_ceiling);
        assert!(next.is_some(), "sequence preflight admitted every image boundary");
        let next = next.unwrap_or(sequence.0);
        *sequence = TerminalOutputSequence(next);
        outputs.push(SessionTerminalOutput::Image { sequence: *sequence, range, boundary });
    }
}

/// Production PTY-reader ownership for exactly one terminal-image seam.
// @lat: [[terminal-images#Terminal Images#Server-Owned Session State Seam]]
pub struct PtyTerminalImageState {
    terminal: SessionTerminal,
}

impl PtyTerminalImageState {
    /// Construct the reader-owned seam from process policy.
    #[must_use]
    pub fn new(policy: Arc<TerminalImageProcessPolicy>) -> Self {
        Self { terminal: SessionTerminal::new(policy) }
    }

    /// Return payload-free state facts for diagnostics and evidence.
    #[must_use]
    pub fn state(&self) -> SessionTerminalState {
        self.terminal.state()
    }

    /// Return exact framing work counters for allocation evidence.
    #[must_use]
    pub fn framing_work(&self) -> SessionTerminalFramingWork {
        self.terminal.framing_work()
    }

    /// Frame one effective PTY read through the integrated session seam.
    pub fn process_bytes(
        &mut self,
        bytes: &[u8],
    ) -> Result<SessionTerminalCommit, SessionTerminalError> {
        self.terminal.process_bytes(bytes)
    }

    /// Return the observer shared with the production resize path.
    #[must_use]
    pub fn grid_observer(&self) -> TerminalGridObserverHandle {
        self.terminal.grid_observer()
    }

    /// Synchronize the seam's screen summary with a committed grid span.
    pub fn record_grid_observation(&mut self, observation: &TerminalGridObservation) {
        self.terminal.observe_active_screen(observation.active_screen);
    }
}

/// Feed one read through the existing Alacritty processor and real `Term`,
/// splitting only at completed image boundaries. Every input byte is consumed
/// exactly once; split control strings do not synthesize observations.
pub fn feed_terminal_observed<T: EventListener>(
    observer: &TerminalGridObserverHandle,
    term: &mut Term<T>,
    ansi_processor: &mut AnsiProcessor,
    bytes: &[u8],
    commit: &mut SessionTerminalCommit,
) {
    let mut state = observer.lock();
    let mut absolute_start = commit.input_range.start;
    for output in &commit.outputs {
        let SessionTerminalOutput::Image { range, .. } = output else { continue };
        if range.end <= commit.input_range.start || range.end > commit.input_range.end {
            continue;
        }
        debug_assert!(
            range.end >= absolute_start,
            "graphics boundaries must remain in source order"
        );
        let absolute_end = range.end;
        if absolute_end <= absolute_start {
            continue;
        }
        let relative_start =
            usize::try_from(absolute_start - commit.input_range.start).unwrap_or(bytes.len());
        let relative_end =
            usize::try_from(absolute_end - commit.input_range.start).unwrap_or(bytes.len());
        let Some(span) = bytes.get(relative_start..relative_end) else {
            break;
        };
        let mut handler = ObservedTermHandler::new(term, &mut state);
        ansi_processor.advance(&mut handler, span);
        let observation = handler.finish();
        commit.grid_observations.push(TerminalGridSpanObservation {
            range: RawByteRange { start: absolute_start, end: absolute_end },
            observation,
        });
        absolute_start = absolute_end;
    }
    if absolute_start < commit.input_range.end {
        let relative_start =
            usize::try_from(absolute_start - commit.input_range.start).unwrap_or(bytes.len());
        if let Some(span) = bytes.get(relative_start..) {
            let mut handler = ObservedTermHandler::new(term, &mut state);
            ansi_processor.advance(&mut handler, span);
            let observation = handler.finish();
            commit.grid_observations.push(TerminalGridSpanObservation {
                range: RawByteRange { start: absolute_start, end: commit.input_range.end },
                observation,
            });
        }
    }
}

/// Feed one unsplittable source span through the delegating observer.
///
/// This is the image-rejection path: framing produced no trustworthy image
/// cuts, but ordinary terminal bytes must still reach the real `Term` once and
/// leave the observer synchronized with it.
pub fn feed_terminal_observed_full_span<T: EventListener>(
    observer: &TerminalGridObserverHandle,
    term: &mut Term<T>,
    ansi_processor: &mut AnsiProcessor,
    bytes: &[u8],
) -> TerminalGridObservation {
    let mut state = observer.lock();
    let mut handler = ObservedTermHandler::new(term, &mut state);
    ansi_processor.advance(&mut handler, bytes);
    handler.finish()
}

/// Feed bytes once according to the production image-framing result.
///
/// Successful commits retain their completed-image cuts. Rejected commits use
/// one full-span observation because no cut metadata was committed. Both paths
/// update the same session observer and active-screen summary.
pub fn feed_terminal_image_result_observed<T: EventListener>(
    terminal_images: &mut PtyTerminalImageState,
    term: &mut Term<T>,
    ansi_processor: &mut AnsiProcessor,
    bytes: &[u8],
    image_result: &mut Result<SessionTerminalCommit, SessionTerminalError>,
) -> TerminalGridObservation {
    let observer = terminal_images.grid_observer();
    let observation = feed_terminal_image_result_with_observer(
        &observer,
        term,
        ansi_processor,
        bytes,
        image_result,
    );
    terminal_images.record_grid_observation(&observation);
    observation
}

/// Feed one framing result through a caller-owned production observer.
///
/// This form lets the reader orchestration seam retain the only mutable image
/// state borrow while its real terminal feed is awaited.
pub fn feed_terminal_image_result_with_observer<T: EventListener>(
    observer: &TerminalGridObserverHandle,
    term: &mut Term<T>,
    ansi_processor: &mut AnsiProcessor,
    bytes: &[u8],
    image_result: &mut Result<SessionTerminalCommit, SessionTerminalError>,
) -> TerminalGridObservation {
    match image_result {
        Ok(commit) => {
            feed_terminal_observed(observer, term, ansi_processor, bytes, commit);
            commit
                .grid_observations
                .last()
                .map_or_else(|| observer.observation(), |span| span.observation.clone())
        }
        Err(_) => feed_terminal_observed_full_span(observer, term, ansi_processor, bytes),
    }
}

/// Flush VTE's buffered synchronized update through the production observer.
///
/// The buffered callbacks change terminal state, but timeout expiry consumes no
/// new source bytes, so this returns a state/effect observation without a
/// fabricated [`RawByteRange`].
pub fn flush_terminal_observed<T: EventListener>(
    observer: &TerminalGridObserverHandle,
    term: &mut Term<T>,
    ansi_processor: &mut AnsiProcessor,
) -> TerminalGridObservation {
    let mut state = observer.lock();
    let mut handler = ObservedTermHandler::new(term, &mut state);
    ansi_processor.stop_sync(&mut handler);
    handler.finish()
}

/// Synchronize the session observer after production `Term::resize`. Alacritty
/// resizes active and inactive grids in the same call; both dimensions are
/// published in one typed effect.
pub fn observe_terminal_resize<T>(
    observer: &TerminalGridObserverHandle,
    term: &Term<T>,
    changed: bool,
) {
    observer.lock().observe_resize(term, changed);
}

/// Apply an image-derived cursor movement to the real terminal and observer
/// once. Alacritty clears deferred wrap in `goto`; raw text is never replayed.
pub fn apply_observed_cursor_move<T: EventListener>(
    observer: &TerminalGridObserverHandle,
    term: &mut Term<T>,
    row: i32,
    column: u16,
) -> TerminalGridObservation {
    let mut state = observer.lock();
    let mut handler = ObservedTermHandler::new(term, &mut state);
    Handler::goto(&mut handler, row, usize::from(column));
    handler.finish()
}

/// Route one effective PTY chunk through the production image, client, and
/// terminal sinks exactly once and in that order.
///
/// Image rejection does not suppress ordinary terminal delivery. Image fanout
/// and PTY reply write-back remain deliberately absent from this shared path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtyReaderIngressRejection {
    /// Typed rejection retained from the image-state seam.
    pub error: SessionTerminalError,
    /// Last committed payload-free sequence after transactional rejection.
    pub image_sequence: TerminalOutputSequence,
}

pub async fn process_pty_reader_ingress<Bytes, Deliver, Feed, FeedFuture, Reject>(
    terminal_images: &mut PtyTerminalImageState,
    bytes: Bytes,
    deliver: Deliver,
    feed: Feed,
    reject: Reject,
) -> Result<SessionTerminalCommit, SessionTerminalError>
where
    Bytes: AsRef<[u8]>,
    Deliver: FnOnce(&[u8]),
    Feed: FnOnce(
        TerminalGridObserverHandle,
        Bytes,
        Result<SessionTerminalCommit, SessionTerminalError>,
    ) -> FeedFuture,
    FeedFuture: Future<
        Output = (
            Result<SessionTerminalCommit, SessionTerminalError>,
            Option<TerminalGridObservation>,
        ),
    >,
    Reject: FnOnce(PtyReaderIngressRejection),
{
    let image_result = terminal_images.terminal.process_bytes(bytes.as_ref());
    let observer = terminal_images.grid_observer();
    deliver(bytes.as_ref());
    let (image_result, observation) = feed(observer, bytes, image_result).await;
    if let Some(observation) = observation {
        terminal_images.record_grid_observation(&observation);
    }
    if let Err(error) = image_result {
        reject(PtyReaderIngressRejection {
            error,
            image_sequence: terminal_images.state().sequence,
        });
    }
    image_result
}
