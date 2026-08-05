//! Server-owned ordering seam for one terminal session's image state.
//!
//! The seam consumes the production PTY graphics framer and returns typed,
//! caller-owned boundaries. Live IPC fanout and PTY reply write-back remain
//! downstream integration work.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::terminal_image_handoff::{
    ExportedSessionImages, PendingKittyHandoff, SessionImageHandoff,
};
use crate::terminal_image_mutations::{
    CanonicalImageMutation, CanonicalImageState, CanonicalRestoreCursor, DecodedImageMeta,
    MutationContext, MutationLog,
};
use crate::terminal_image_publication::{
    DefinitionPayload, PublicationInputs, publish as publish_burst,
};
use crate::terminal_image_replay::{ReplayInputs, plan_replay};
use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::term::TermMode;
use cursor_icon::CursorIcon;
use icy_sixel_decoder::{DcsSettings, DecodeError as SixelDecodeError, decode_sixel_payload};
use scribe_common::kitty_decode::{
    KittyCompression as DecodeKittyCompression, KittyDataParams, KittyFormat as DecodeKittyFormat,
    KittyTransfer, KittyTransport,
};
use scribe_common::terminal_images::{
    ImageBoundError, ImageLimits, TerminalImageCellClip, TerminalImageDefinition,
    TerminalImageGeneration, TerminalImageId, TerminalImageLiveMessage, TerminalImagePlacement,
    TerminalImageRejectionReason, TerminalImageReplayMessage, TerminalOutputSequence,
    TerminalScreenKind,
};
use scribe_image_decode::{
    DecodeAdmissionError, DecodeAllocationClass, DecodeBudget, DecodeBuffer, DecodeCeilings,
    DecodeLimits, DecodePermit, DecodeRequest, DecodeScheduler, DecodeSessionId, DecodeStorage,
    DecodeStorageLease, DecodeTarget, NoopHooks, StorageProcess, StorageValidation,
    StorageValidationPause, StorageValidationRejection,
};
use scribe_pty::graphics_framing::{
    GraphicsEvent, GraphicsFailure, GraphicsFailureCategory, GraphicsFramer, GraphicsProtocol,
    GraphicsStorageBudget, GraphicsStorageClass, GraphicsStorageRejection, GraphicsStorageVec,
    KittyAction, KittyChunkState, KittyCommand, KittyCommandControls, KittyCompression,
    KittyControlPresence, KittyFormat, PendingGraphicsTransfer, RawByteRange, RawBytes,
    SixelCommand, SixelMode,
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
///
/// Payload-free scalars only. Hostile-input-proportional effect lists travel
/// with their own paired-ledger ownership in [`ObservedTerminalGridSpan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        }
    }
}

/// Terminal facts plus the accounted effects observed over one source span.
#[derive(Debug, PartialEq, Eq)]
pub struct ObservedTerminalGridSpan {
    pub observation: TerminalGridObservation,
    effects: Option<GraphicsStorageVec<ObservedTerminalGridEffect>>,
    /// Typed paired-ledger rejection that truncated this span's effect list.
    pub storage_rejection: Option<GraphicsStorageRejection>,
}

impl ObservedTerminalGridSpan {
    /// Borrow the ordered effects while their storage ownership lives.
    #[must_use]
    pub fn effects(&self) -> &[ObservedTerminalGridEffect] {
        self.effects.as_ref().map_or(&[], GraphicsStorageVec::as_slice)
    }
}

/// One observation aligned to a half-open range of the original PTY stream.
#[derive(Debug, PartialEq, Eq)]
pub struct TerminalGridSpanObservation {
    pub range: RawByteRange,
    pub observation: TerminalGridObservation,
    effects: Option<GraphicsStorageVec<ObservedTerminalGridEffect>>,
}

impl TerminalGridSpanObservation {
    /// Borrow the ordered effects while their storage ownership lives.
    #[must_use]
    pub fn effects(&self) -> &[ObservedTerminalGridEffect] {
        self.effects.as_ref().map_or(&[], GraphicsStorageVec::as_slice)
    }
}

#[derive(Debug, Default)]
struct TerminalGridObserverState {
    observation: TerminalGridObservation,
    initialized: bool,
}

/// Cloneable session-owned handle shared only with production feed and resize
/// call sites. It never retains cells or image payloads.
#[derive(Clone, Debug)]
pub struct TerminalGridObserverHandle {
    state: Arc<Mutex<TerminalGridObserverState>>,
    budget: Arc<GraphicsStorageBudget>,
}

impl TerminalGridObserverHandle {
    /// Bind one observer to the session/process ledger pair that owns every
    /// grid-observation allocation it produces.
    #[must_use]
    pub fn new(budget: Arc<GraphicsStorageBudget>) -> Self {
        Self { state: Arc::new(Mutex::new(TerminalGridObserverState::default())), budget }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TerminalGridObserverState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Return the latest payload-free production observation.
    #[must_use]
    pub fn observation(&self) -> TerminalGridObservation {
        self.lock().observation
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

    fn observe_resize<T>(&mut self, term: &Term<T>, changed: bool) -> bool {
        self.initialize(term);
        if !changed {
            self.sync_active(term);
            return false;
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
        true
    }
}

/// Delegating observer around the production `Term` handler. The existing VTE
/// processor invokes this once; every callback is forwarded to the same real
/// `Term`, while image-relevant effects are captured from typed callbacks and
/// actual pre/post cursor state.
struct ObservedTermHandler<'a, T> {
    term: &'a mut Term<T>,
    state: &'a mut TerminalGridObserverState,
    budget: &'a Arc<GraphicsStorageBudget>,
    effects: Option<GraphicsStorageVec<ObservedTerminalGridEffect>>,
    storage_rejection: Option<GraphicsStorageRejection>,
}

impl<'a, T> ObservedTermHandler<'a, T> {
    fn new(
        term: &'a mut Term<T>,
        state: &'a mut TerminalGridObserverState,
        budget: &'a Arc<GraphicsStorageBudget>,
    ) -> Self {
        state.initialize(term);
        Self { term, state, budget, effects: None, storage_rejection: None }
    }

    /// Append one observed effect, reserving its storage before it is stored.
    /// Storage pressure truncates this payload-free list; the already-fed
    /// terminal is never rewound and the ledger stays exact.
    fn push_effect(&mut self, effect: ObservedTerminalGridEffect) {
        if self.storage_rejection.is_some() {
            return;
        }
        let effects = match self.effects.as_mut() {
            Some(effects) => effects,
            None => {
                match GraphicsStorageVec::new(
                    Arc::clone(self.budget),
                    GraphicsStorageClass::GridObservations,
                ) {
                    Ok(effects) => self.effects.insert(effects),
                    Err(error) => {
                        self.storage_rejection = Some(error);
                        return;
                    }
                }
            }
        };
        if let Err(error) = effects.push(effect) {
            self.storage_rejection = Some(error);
        }
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
            self.push_effect(ObservedTerminalGridEffect::Scroll {
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
            self.push_effect(ObservedTerminalGridEffect::EraseCells {
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
        self.push_effect(ObservedTerminalGridEffect::EraseDisplay { screen: self.screen() });
    }

    fn finish(mut self) -> ObservedTerminalGridSpan {
        let effects = self.effects.take();
        self.state.sync_active(self.term);
        ObservedTerminalGridSpan {
            observation: self.state.observation,
            effects,
            storage_rejection: self.storage_rejection,
        }
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
                let screen = self.screen();
                self.push_effect(ObservedTerminalGridEffect::EraseDisplay { screen });
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
        self.push_effect(ObservedTerminalGridEffect::HardReset);
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
            self.push_effect(ObservedTerminalGridEffect::SwitchScreen {
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
            self.push_effect(ObservedTerminalGridEffect::SwitchScreen {
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

pub use scribe_image_decode::{
    StorageClass as StorageAllocationClass, StorageClassCounters as ImageStorageClassCounters,
    StorageCounters as ImageStorageCounters, StorageLedgerOperation, StorageLedgerScope,
    StorageLedgerValidationFault, StorageSnapshotValidationFault,
};

struct OwnedImageStorage {
    bytes: Vec<u8>,
    reservation: DecodeStorageLease,
}

impl OwnedImageStorage {
    fn from_slices(
        budget: &Arc<DecodeStorage>,
        class: StorageAllocationClass,
        slices: &[&[u8]],
    ) -> Result<Self, GraphicsStorageRejection> {
        let requested = slices.iter().try_fold(0_usize, |total, slice| {
            total.checked_add(slice.len()).ok_or(GraphicsStorageRejection::CounterOverflow)
        })?;
        let mut reservation = budget.reserve(class, requested)?;
        reservation.record_allocation_attempt()?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(requested)
            .map_err(|_| GraphicsStorageRejection::AllocationFailed)?;
        let observed = budget.observe_allocation_capacity(bytes.capacity())?;
        reservation.reconcile_observed(observed)?;
        for slice in slices {
            bytes.extend_from_slice(slice);
        }
        Ok(Self { bytes, reservation })
    }

    fn requested_bytes(&self) -> usize {
        self.reservation.requested_bytes()
    }

    fn observed_bytes(&self) -> usize {
        self.reservation.observed_bytes()
    }
}

enum StagedStorage {
    Unchanged,
    Clear,
    Replace(OwnedImageStorage),
}

enum StagedDecodeStorage {
    Unchanged,
    Replace(DecodeBuffer),
}

struct StagedRead {
    sequence: TerminalOutputSequence,
    outputs: GraphicsStorageVec<SessionTerminalOutput>,
    sixel_body: StagedStorage,
    kitty_decoded: StagedDecodeStorage,
    sixel_decoded: StagedDecodeStorage,
    completed_kitty_transfer: Option<PendingKittyDecode>,
}

struct PendingKittyDecode {
    transfer: KittyTransfer,
    controls: KittyCommandControls,
    presence: KittyControlPresence,
    /// Raw span from the transfer's first chunk through its latest chunk, so a
    /// retirement failure names the bytes it discarded.
    range: RawByteRange,
}

enum KittyTransferPreparation {
    Ready,
    Passthrough,
    HandledFailure,
}

impl StagedStorage {
    fn apply(self, slot: &mut Option<OwnedImageStorage>) {
        match self {
            Self::Unchanged => {}
            Self::Clear => *slot = None,
            Self::Replace(storage) => *slot = Some(storage),
        }
    }
}

impl StagedDecodeStorage {
    /// Decoded pixels land behind an `Arc` so the canonical retention map can
    /// hold the same bytes the slot does. One buffer, one lease, two owners:
    /// retaining a committed image costs nothing the decode already paid, and
    /// the lease is released when the last owner drops it.
    // @lat: [[terminal-images#Terminal Images#Retained Canonical Pixels]]
    fn apply(self, slot: &mut Option<Arc<DecodeBuffer>>) {
        match self {
            Self::Unchanged => {}
            Self::Replace(storage) => *slot = Some(Arc::new(storage)),
        }
    }
}

/// Payload-free ownership by production retention path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageStorageOwnership {
    pub pending_kitty_requested: usize,
    pub pending_kitty_observed: usize,
    pub completed_kitty_requested: usize,
    pub completed_kitty_observed: usize,
    pub sixel_body_requested: usize,
    pub sixel_body_observed: usize,
    pub kitty_decoded_requested: usize,
    pub kitty_decoded_observed: usize,
    pub sixel_decoded_requested: usize,
    pub sixel_decoded_observed: usize,
}

/// Payload-free stable digests for deterministic canonical rollback evidence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageStorageDigests {
    pub pending_kitty: u64,
    pub completed_kitty: u64,
    pub sixel_body: u64,
    pub kitty_decoded: u64,
    pub sixel_decoded: u64,
}

fn requested(storage: Option<&OwnedImageStorage>) -> usize {
    storage.map_or(0, OwnedImageStorage::requested_bytes)
}

fn observed(storage: Option<&OwnedImageStorage>) -> usize {
    storage.map_or(0, OwnedImageStorage::observed_bytes)
}

fn decoded_requested(storage: Option<&DecodeBuffer>) -> usize {
    storage.map_or(0, DecodeBuffer::requested_bytes)
}

fn decoded_observed(storage: Option<&DecodeBuffer>) -> usize {
    storage.map_or(0, DecodeBuffer::observed_bytes)
}

fn storage_digest(storage: Option<&OwnedImageStorage>) -> u64 {
    storage.map_or(0, |storage| {
        storage.bytes.iter().fold(0, |digest, byte| {
            digest.wrapping_mul(1_099_511_628_211).wrapping_add(u64::from(*byte))
        })
    })
}

fn decoded_storage_digest(storage: Option<&DecodeBuffer>) -> u64 {
    storage.map_or(0, |storage| {
        storage.iter().fold(0, |digest, byte| {
            digest.wrapping_mul(1_099_511_628_211).wrapping_add(u64::from(*byte))
        })
    })
}

/// Immutable process policy shared by every session image seam.
#[derive(Debug)]
pub struct TerminalImageProcessPolicy {
    limits: ImageLimits,
    output_sequence_ceiling: u64,
    generation_ceiling: u64,
    process_storage: Arc<StorageProcess>,
    session_storage_limit: u64,
    observed_capacity_extra: usize,
    storage_validation_fault: Option<StorageLedgerValidationFault>,
    storage_validation_rejection: Option<StorageValidationRejection>,
    storage_validation_snapshot_fault: Option<StorageSnapshotValidationFault>,
    storage_validation_pause: Option<StorageValidationPause>,
    decode_scheduler: Arc<DecodeScheduler>,
}

/// Derive the frozen v1 decode ceilings from the frozen v1 image limits.
fn decode_ceilings(limits: ImageLimits) -> DecodeCeilings {
    DecodeCeilings {
        concurrent_decodes: limits.max_concurrent_decodes,
        queue_depth: limits.max_decode_queue_depth,
        queue_bytes: limits.max_decode_queue_bytes,
        queue_wait: Duration::from_millis(limits.max_queue_wait_ms),
    }
}

/// One process-wide scheduler shared by every v1 policy, so a second policy
/// cannot mint decode admissions that bypass the live process ceilings.
fn v1_decode_scheduler() -> Arc<DecodeScheduler> {
    static SCHEDULER: OnceLock<Arc<DecodeScheduler>> = OnceLock::new();
    Arc::clone(SCHEDULER.get_or_init(|| DecodeScheduler::new(decode_ceilings(ImageLimits::V1))))
}

impl TerminalImageProcessPolicy {
    /// Construct the frozen terminal-images-v1 process policy.
    #[must_use]
    pub fn v1() -> Arc<Self> {
        static POLICY: OnceLock<Arc<TerminalImageProcessPolicy>> = OnceLock::new();
        Arc::clone(POLICY.get_or_init(|| {
            Arc::new(Self {
                limits: ImageLimits::V1,
                output_sequence_ceiling: u64::MAX,
                generation_ceiling: u64::MAX,
                process_storage: StorageProcess::new(ImageLimits::V1.max_process_retained_bytes),
                session_storage_limit: ImageLimits::V1.max_session_retained_cpu_bytes,
                observed_capacity_extra: 0,
                storage_validation_fault: None,
                storage_validation_rejection: None,
                storage_validation_snapshot_fault: None,
                storage_validation_pause: None,
                decode_scheduler: v1_decode_scheduler(),
            })
        }))
    }

    /// Construct immutable v1 policy with a smaller sequence ceiling for
    /// deterministic exhaustion validation through the production seam.
    #[must_use]
    pub fn with_sequence_ceiling_for_validation(output_sequence_ceiling: u64) -> Arc<Self> {
        Arc::new(Self {
            limits: ImageLimits::V1,
            output_sequence_ceiling,
            generation_ceiling: u64::MAX,
            process_storage: StorageProcess::new(ImageLimits::V1.max_process_retained_bytes),
            session_storage_limit: ImageLimits::V1.max_session_retained_cpu_bytes,
            observed_capacity_extra: 0,
            storage_validation_fault: None,
            storage_validation_rejection: None,
            storage_validation_snapshot_fault: None,
            storage_validation_pause: None,
            decode_scheduler: v1_decode_scheduler(),
        })
    }

    /// Construct immutable v1 policy with a smaller generation ceiling so
    /// validation can reach generation exhaustion through the production seam.
    #[must_use]
    pub fn with_generation_ceiling_for_validation(generation_ceiling: u64) -> Arc<Self> {
        Arc::new(Self {
            limits: ImageLimits::V1,
            output_sequence_ceiling: u64::MAX,
            generation_ceiling,
            process_storage: StorageProcess::new(ImageLimits::V1.max_process_retained_bytes),
            session_storage_limit: ImageLimits::V1.max_session_retained_cpu_bytes,
            observed_capacity_extra: 0,
            storage_validation_fault: None,
            storage_validation_rejection: None,
            storage_validation_snapshot_fault: None,
            storage_validation_pause: None,
            decode_scheduler: v1_decode_scheduler(),
        })
    }

    /// Construct immutable v1 limits with a smaller per-command work ceiling so
    /// validation can prove that work admission precedes decoder allocation.
    #[must_use]
    pub fn with_work_ceiling_for_validation(max_work_units_per_command: u64) -> Arc<Self> {
        Arc::new(Self {
            limits: ImageLimits { max_work_units_per_command, ..ImageLimits::V1 },
            output_sequence_ceiling: u64::MAX,
            generation_ceiling: u64::MAX,
            process_storage: StorageProcess::new(ImageLimits::V1.max_process_retained_bytes),
            session_storage_limit: ImageLimits::V1.max_session_retained_cpu_bytes,
            observed_capacity_extra: 0,
            storage_validation_fault: None,
            storage_validation_rejection: None,
            storage_validation_snapshot_fault: None,
            storage_validation_pause: None,
            decode_scheduler: v1_decode_scheduler(),
        })
    }

    /// Construct a shared policy with small storage limits for exact boundary evidence.
    #[must_use]
    pub fn with_storage_limits_for_validation(
        session_storage_limit: u64,
        process_storage_limit: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            limits: ImageLimits::V1,
            output_sequence_ceiling: u64::MAX,
            generation_ceiling: u64::MAX,
            process_storage: StorageProcess::new(process_storage_limit),
            session_storage_limit,
            observed_capacity_extra: 0,
            storage_validation_fault: None,
            storage_validation_rejection: None,
            storage_validation_snapshot_fault: None,
            storage_validation_pause: None,
            decode_scheduler: v1_decode_scheduler(),
        })
    }

    /// Construct immutable storage limits with deterministic extra observed
    /// capacity for production-path reconciliation validation.
    #[must_use]
    pub fn with_storage_capacity_observer_for_validation(
        session_storage_limit: u64,
        process_storage_limit: u64,
        observed_capacity_extra: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            limits: ImageLimits::V1,
            output_sequence_ceiling: u64::MAX,
            generation_ceiling: u64::MAX,
            process_storage: StorageProcess::new(process_storage_limit),
            session_storage_limit,
            observed_capacity_extra,
            storage_validation_fault: None,
            storage_validation_rejection: None,
            storage_validation_snapshot_fault: None,
            storage_validation_pause: None,
            decode_scheduler: v1_decode_scheduler(),
        })
    }

    /// Construct immutable limits and one safe deterministic ledger fault.
    #[must_use]
    pub fn with_storage_fault_for_validation(
        session_storage_limit: u64,
        process_storage_limit: u64,
        observed_capacity_extra: usize,
        fault: StorageLedgerValidationFault,
    ) -> Arc<Self> {
        Arc::new(Self {
            limits: ImageLimits::V1,
            output_sequence_ceiling: u64::MAX,
            generation_ceiling: u64::MAX,
            process_storage: StorageProcess::new(process_storage_limit),
            session_storage_limit,
            observed_capacity_extra,
            storage_validation_fault: Some(fault),
            storage_validation_rejection: None,
            storage_validation_snapshot_fault: None,
            storage_validation_pause: None,
            decode_scheduler: v1_decode_scheduler(),
        })
    }

    /// Construct immutable limits with one typed reservation rejection at an
    /// exact production budget call for ingress transaction validation.
    #[must_use]
    pub fn with_storage_rejection_for_validation(
        session_storage_limit: u64,
        process_storage_limit: u64,
        class: StorageAllocationClass,
        matching_ordinal: u64,
        rejection: GraphicsStorageRejection,
    ) -> Arc<Self> {
        assert!(matching_ordinal > 0, "validation rejection ordinal must be nonzero");
        Arc::new(Self {
            limits: ImageLimits::V1,
            output_sequence_ceiling: u64::MAX,
            generation_ceiling: u64::MAX,
            process_storage: StorageProcess::new(process_storage_limit),
            session_storage_limit,
            observed_capacity_extra: 0,
            storage_validation_fault: None,
            storage_validation_rejection: Some(StorageValidationRejection {
                class,
                matching_ordinal,
                rejection,
            }),
            storage_validation_snapshot_fault: None,
            storage_validation_pause: None,
            decode_scheduler: v1_decode_scheduler(),
        })
    }

    /// Construct one paused production allocation rejection so validation can
    /// release an unrelated detached owner while the transaction gate is held.
    #[doc(hidden)]
    #[must_use]
    pub fn with_paused_storage_rejection_for_validation(
        class: StorageAllocationClass,
        matching_ordinal: u64,
        rejection: GraphicsStorageRejection,
        reached: Arc<Barrier>,
        resume: Arc<Barrier>,
    ) -> Arc<Self> {
        assert!(matching_ordinal > 0, "validation rejection ordinal must be nonzero");
        Arc::new(Self {
            limits: ImageLimits::V1,
            output_sequence_ceiling: u64::MAX,
            generation_ceiling: u64::MAX,
            process_storage: StorageProcess::new(u64::MAX),
            session_storage_limit: u64::MAX,
            observed_capacity_extra: 0,
            storage_validation_fault: None,
            storage_validation_rejection: Some(StorageValidationRejection {
                class,
                matching_ordinal,
                rejection,
            }),
            storage_validation_snapshot_fault: None,
            storage_validation_pause: Some(StorageValidationPause {
                class,
                matching_ordinal,
                reached,
                resume,
            }),
            decode_scheduler: v1_decode_scheduler(),
        })
    }

    /// Construct immutable limits with one ledger-side snapshot rejection.
    #[must_use]
    pub fn with_storage_snapshot_fault_for_validation(
        session_storage_limit: u64,
        process_storage_limit: u64,
        observed_capacity_extra: usize,
        fault: StorageSnapshotValidationFault,
    ) -> Arc<Self> {
        Arc::new(Self {
            limits: ImageLimits::V1,
            output_sequence_ceiling: u64::MAX,
            generation_ceiling: u64::MAX,
            process_storage: StorageProcess::new(process_storage_limit),
            session_storage_limit,
            observed_capacity_extra,
            storage_validation_fault: None,
            storage_validation_rejection: None,
            storage_validation_snapshot_fault: Some(fault),
            storage_validation_pause: None,
            decode_scheduler: v1_decode_scheduler(),
        })
    }

    /// Construct a policy owning a private decode scheduler with smaller
    /// ceilings so admission ordering is observable in bounded wall time.
    #[must_use]
    pub fn with_decode_ceilings_for_validation(ceilings: DecodeCeilings) -> Arc<Self> {
        Arc::new(Self {
            limits: ImageLimits::V1,
            output_sequence_ceiling: u64::MAX,
            generation_ceiling: u64::MAX,
            process_storage: StorageProcess::new(ImageLimits::V1.max_process_retained_bytes),
            session_storage_limit: ImageLimits::V1.max_session_retained_cpu_bytes,
            observed_capacity_extra: 0,
            storage_validation_fault: None,
            storage_validation_rejection: None,
            storage_validation_snapshot_fault: None,
            storage_validation_pause: None,
            decode_scheduler: DecodeScheduler::new(ceilings),
        })
    }

    /// Return a copy of the immutable process limits.
    #[must_use]
    pub fn limits(&self) -> ImageLimits {
        self.limits
    }

    /// The mandatory decode scheduler every session of this policy admits
    /// through.
    #[must_use]
    pub fn decode_scheduler(&self) -> &Arc<DecodeScheduler> {
        &self.decode_scheduler
    }
}

/// Image-side meaning of one ordered graphics boundary.
#[derive(Debug, PartialEq, Eq)]
pub enum TerminalImageBoundary {
    /// A Kitty command; `decoded` carries the canonical facts of a completed
    /// transfer and stays `None` for continuations, queries, and pass-through.
    Kitty {
        command: KittyCommand,
        decoded: Option<DecodedImageMeta>,
    },
    /// A decoded Sixel image and its canonical facts.
    Sixel {
        command: SixelCommand,
        decoded: DecodedImageMeta,
    },
    SixelMode {
        mode: SixelMode,
        enabled: bool,
    },
    Failure(GraphicsFailure),
}

/// One output from production framing, in original PTY byte order.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionTerminalOutput {
    /// Bytes to feed to the ordinary terminal exactly once.
    Raw(RawBytes),
    /// An image boundary assigned the session's monotonic output sequence.
    Image { sequence: TerminalOutputSequence, range: RawByteRange, boundary: TerminalImageBoundary },
}

/// Caller-owned result of one PTY read; no output is fanned out by the seam.
#[derive(Debug, PartialEq, Eq)]
pub struct SessionTerminalCommit {
    pub generation: TerminalImageGeneration,
    pub through_sequence: TerminalOutputSequence,
    pub outputs: GraphicsStorageVec<SessionTerminalOutput>,
    pub input_range: RawByteRange,
    grid_observations: Option<GraphicsStorageVec<TerminalGridSpanObservation>>,
    /// Typed paired-ledger rejection that truncated this commit's observations.
    pub grid_observation_rejection: Option<GraphicsStorageRejection>,
}

impl SessionTerminalCommit {
    /// Borrow the ordered grid spans while their storage ownership lives.
    #[must_use]
    pub fn grid_observations(&self) -> &[TerminalGridSpanObservation] {
        self.grid_observations.as_ref().map_or(&[], GraphicsStorageVec::as_slice)
    }
}

/// Failure at the session ordering seam before any image state is committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTerminalError {
    SequenceExhausted,
    GenerationExhausted,
    Storage(GraphicsStorageRejection),
    /// A handoff payload failed bounded validation before anything was
    /// installed, so the successor keeps an empty session instead of a
    /// half-restored one.
    HandoffRejected(ImageBoundError),
}

impl fmt::Display for SessionTerminalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SequenceExhausted => {
                formatter.write_str("terminal image output sequence exhausted")
            }
            Self::GenerationExhausted => formatter.write_str("terminal image generation exhausted"),
            Self::Storage(rejection) => {
                write!(formatter, "terminal image storage failure: {rejection:?}")
            }
            Self::HandoffRejected(error) => {
                write!(formatter, "terminal image handoff payload rejected: {error}")
            }
        }
    }
}

impl std::error::Error for SessionTerminalError {}

/// Why an incomplete graphics transfer is being retired.
// @lat: [[terminal-images#Terminal Images#Incomplete Transfer Retirement]]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferRetirement {
    /// PTY EOF: unterminated candidate bytes are still ordinary text, and an
    /// unterminated image string becomes a truncated-sequence failure.
    StreamEnd,
    /// Parser or session reset: framing state is discarded without output
    /// because the terminal context those bytes belonged to is gone.
    Reset,
    /// Session close: reset, plus cancellation of every outstanding decode
    /// admission and release of all retained image storage.
    Close,
}

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
    canonical: CanonicalImageState,
    /// Active screen the last published burst left the client on. Canonical
    /// state can adopt an observed screen before a read commits, so the
    /// publication boundary keeps its own view of what the client knows.
    published_screen: TerminalScreenKind,
    pending_transfer: Option<PendingGraphicsTransfer>,
    framing_work: SessionTerminalFramingWork,
    grid_observer: TerminalGridObserverHandle,
    storage_budget: Arc<DecodeStorage>,
    pending_kitty_storage: Option<OwnedImageStorage>,
    completed_kitty_storage: Option<OwnedImageStorage>,
    sixel_body_storage: Option<OwnedImageStorage>,
    kitty_decoded_storage: Option<Arc<DecodeBuffer>>,
    sixel_decoded_storage: Option<Arc<DecodeBuffer>>,
    pending_kitty_decode: Option<PendingKittyDecode>,
    /// Canonical RGBA for every committed definition, keyed by image id.
    ///
    /// The decode buffer a transfer produced is single-slot and gone by the
    /// next image, so without this the server could state what it has but
    /// never re-send it. Retention is what lets a late attacher, a shed sink,
    /// and an upgrade successor be given a scene rather than a promise. The
    /// buffer is *moved* here rather than copied: the bytes keep the lease
    /// they were already charged under, so retention costs the session
    /// nothing it had not already paid for.
    canonical_rgba: BTreeMap<TerminalImageId, Arc<DecodeBuffer>>,
    decode_session: DecodeSessionId,
}

impl SessionTerminal {
    /// Construct a session from its process owner's shared immutable policy.
    #[must_use]
    pub fn new(policy: Arc<TerminalImageProcessPolicy>) -> Self {
        let max_control_string_bytes =
            usize::try_from(policy.limits.max_control_string_bytes).unwrap_or(usize::MAX);
        let policy_limits = policy.limits;
        let storage_budget = DecodeStorage::new(
            Arc::clone(&policy.process_storage),
            policy.session_storage_limit,
            policy.observed_capacity_extra,
            StorageValidation {
                ledger_fault: policy.storage_validation_fault,
                rejection: policy.storage_validation_rejection,
                snapshot_fault: policy.storage_validation_snapshot_fault,
                pause: policy.storage_validation_pause.clone(),
            },
        );
        let decode_session = policy.decode_scheduler.new_session();
        Self {
            policy,
            framer: GraphicsFramer::with_storage_budget(
                max_control_string_bytes,
                Arc::clone(&storage_budget),
            ),
            generation: TerminalImageGeneration(1),
            sequence: TerminalOutputSequence(0),
            canonical: CanonicalImageState::new(policy_limits),
            published_screen: TerminalScreenKind::Primary,
            pending_transfer: None,
            framing_work: SessionTerminalFramingWork::default(),
            grid_observer: TerminalGridObserverHandle::new(Arc::clone(&storage_budget)),
            storage_budget,
            pending_kitty_storage: None,
            completed_kitty_storage: None,
            sixel_body_storage: None,
            kitty_decoded_storage: None,
            sixel_decoded_storage: None,
            pending_kitty_decode: None,
            canonical_rgba: BTreeMap::new(),
            decode_session,
        }
    }

    /// Session identity every decode admission for this seam is bound to.
    #[must_use]
    pub const fn decode_session(&self) -> DecodeSessionId {
        self.decode_session
    }

    /// Describe the exact capability one decode entry point needs, so the
    /// ticket, the permit, and the authorization check all name one request.
    fn decode_request(&self, target: DecodeTarget, requested_bytes: u64) -> DecodeRequest {
        DecodeRequest {
            session: self.decode_session,
            generation: self.generation.0,
            target,
            requested_bytes,
            storage: Arc::clone(&self.storage_budget),
        }
    }

    /// Take one scheduler ticket and turn it into the permit every decode
    /// entry point requires, rejecting foreign capabilities before any work.
    fn admit_decode(&self, request: &DecodeRequest) -> Result<DecodePermit, DecodeAdmissionError> {
        let scheduler = &self.policy.decode_scheduler;
        let ticket = scheduler.issue(request.clone())?;
        let permit = scheduler.admit(ticket)?;
        permit.authorize(scheduler, request)?;
        Ok(permit)
    }

    /// Consume one PTY read and return raw/image boundaries without fanout.
    pub fn process_bytes(
        &mut self,
        bytes: &[u8],
    ) -> Result<SessionTerminalCommit, SessionTerminalError> {
        let storage_budget = Arc::clone(&self.storage_budget);
        let _transaction =
            storage_budget.lock_transaction().map_err(SessionTerminalError::Storage)?;
        let checkpoint = storage_budget.checkpoint().map_err(SessionTerminalError::Storage)?;
        let kitty_existed = self.pending_kitty_decode.is_some();
        if let Some(pending) = self.pending_kitty_decode.as_mut() {
            pending.transfer.begin_transaction().map_err(|_| {
                SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant)
            })?;
        }
        let mut result = self.process_bytes_transaction(bytes);
        if result.is_ok() {
            if let Err(error) = self.commit_staged_transactions() {
                result = Err(error);
            }
        } else {
            self.rollback_staged_transactions(kitty_existed)?;
        }
        if result.is_err() {
            storage_budget.rollback(&checkpoint).map_err(SessionTerminalError::Storage)?;
        }
        result
    }

    fn commit_staged_transactions(&mut self) -> Result<(), SessionTerminalError> {
        self.framer.commit_staged();
        let Some(pending) = self.pending_kitty_decode.as_mut() else { return Ok(()) };
        if !pending.transfer.transaction_active() {
            return Ok(());
        }
        pending.transfer.commit_transaction().map_err(|_| internal_storage_error())
    }

    fn rollback_staged_transactions(
        &mut self,
        kitty_existed: bool,
    ) -> Result<(), SessionTerminalError> {
        self.framer.rollback_staged().map_err(SessionTerminalError::Storage)?;
        self.pending_transfer = self.framer.pending_transfer();
        if !kitty_existed {
            self.pending_kitty_decode = None;
            return Ok(());
        }
        let pending = self.pending_kitty_decode.as_mut().ok_or_else(internal_storage_error)?;
        pending.transfer.rollback_transaction().map_err(|_| internal_storage_error())
    }

    fn process_bytes_transaction(
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
        let outputs = GraphicsStorageVec::new(
            Arc::clone(&self.storage_budget),
            GraphicsStorageClass::TerminalOutputs,
        )
        .map_err(SessionTerminalError::Storage)?;
        let direct_through = self
            .sequence
            .0
            .checked_add(boundary_upper_bound)
            .filter(|sequence| *sequence <= self.policy.output_sequence_ceiling);

        if direct_through.is_some() {
            let events = match self.framer.push_staged(bytes) {
                Ok(events) => events,
                Err(rejection) => {
                    self.pending_transfer = self.framer.pending_transfer();
                    return Err(SessionTerminalError::Storage(rejection));
                }
            };
            self.record_direct_read();
            self.pending_transfer = self.framer.pending_transfer();
            return self.commit_events(events, outputs, None, input_range);
        }

        // Only reads close enough to sequence exhaustion to fail the safe
        // upper bound need rollback parsing. The original framer and all
        // canonical state remain untouched when actual emitted events exceed
        // the remaining sequence capacity.
        let mut candidate_framer =
            self.framer.try_clone().map_err(SessionTerminalError::Storage)?;
        self.record_speculative_clone();
        let events = match candidate_framer.push(bytes) {
            Ok(events) => events,
            Err(rejection) => return Err(SessionTerminalError::Storage(rejection)),
        };
        let through_sequence = self.preflight_sequence(&events)?;
        let commit = self.commit_events(events, outputs, Some(through_sequence), input_range)?;
        self.framer = candidate_framer;
        self.pending_transfer = self.framer.pending_transfer();
        Ok(commit)
    }

    fn commit_events(
        &mut self,
        events: GraphicsStorageVec<GraphicsEvent>,
        outputs: GraphicsStorageVec<SessionTerminalOutput>,
        admitted_sequence: Option<TerminalOutputSequence>,
        input_range: RawByteRange,
    ) -> Result<SessionTerminalCommit, SessionTerminalError> {
        let mut staged = StagedRead {
            sequence: self.sequence,
            outputs,
            sixel_body: StagedStorage::Unchanged,
            kitty_decoded: StagedDecodeStorage::Unchanged,
            sixel_decoded: StagedDecodeStorage::Unchanged,
            completed_kitty_transfer: None,
        };
        if let Err(error) = self.stage_events(events, &mut staged) {
            self.restore_completed_kitty_transfer(&mut staged);
            return Err(error);
        }
        if admitted_sequence.is_some_and(|admitted| staged.sequence != admitted) {
            return Err(SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant));
        }
        staged.sixel_body.apply(&mut self.sixel_body_storage);
        staged.kitty_decoded.apply(&mut self.kitty_decoded_storage);
        staged.sixel_decoded.apply(&mut self.sixel_decoded_storage);
        self.sequence = staged.sequence;
        Ok(SessionTerminalCommit {
            generation: self.generation,
            through_sequence: self.sequence,
            outputs: staged.outputs,
            input_range,
            grid_observations: None,
            grid_observation_rejection: None,
        })
    }

    fn stage_events(
        &mut self,
        events: GraphicsStorageVec<GraphicsEvent>,
        staged: &mut StagedRead,
    ) -> Result<(), SessionTerminalError> {
        for event in events {
            self.stage_event(event, staged)?;
        }
        Ok(())
    }

    fn restore_completed_kitty_transfer(&mut self, staged: &mut StagedRead) {
        if let Some(pending) = staged.completed_kitty_transfer.take() {
            self.pending_kitty_decode = Some(pending);
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
        self.canonical.set_active_screen(screen);
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
            active_screen: self.canonical.active_screen(),
            definition_count: self.canonical.definition_count(),
            placement_count: self.canonical.placement_count(),
            pending_transfer: self.pending_transfer,
        }
    }

    /// Return exact work counters without exposing retained image payloads.
    #[must_use]
    pub fn framing_work(&self) -> SessionTerminalFramingWork {
        self.framing_work
    }

    /// Current and peak session/process storage counters.
    pub fn storage_counters(
        &self,
    ) -> Result<(ImageStorageCounters, ImageStorageCounters), SessionTerminalError> {
        self.storage_budget.counters().map_err(SessionTerminalError::Storage)
    }

    pub fn storage_class_counters(
        &self,
        class: StorageAllocationClass,
    ) -> Result<(ImageStorageClassCounters, ImageStorageClassCounters), SessionTerminalError> {
        self.storage_budget.class_counters(class).map_err(SessionTerminalError::Storage)
    }

    /// Retire every incomplete transfer this session still holds.
    ///
    /// Partial APC/DCS framing, buffered Kitty chunks, and any in-flight decode
    /// admission are released exactly once, and the discarded work is reported
    /// as typed boundaries in the same sequence stream as ordinary reads, so a
    /// pending query reply keeps its FIFO position ahead of the retirement.
    /// Nothing incomplete is ever published as a definition or placement.
    // @lat: [[terminal-images#Terminal Images#Incomplete Transfer Retirement]]
    pub fn retire_transfers(
        &mut self,
        retirement: TransferRetirement,
    ) -> Result<SessionTerminalCommit, SessionTerminalError> {
        let storage_budget = Arc::clone(&self.storage_budget);
        let _transaction =
            storage_budget.lock_transaction().map_err(SessionTerminalError::Storage)?;
        let checkpoint = storage_budget.checkpoint().map_err(SessionTerminalError::Storage)?;
        let result = self.retire_transaction(retirement);
        if result.is_err() {
            storage_budget.rollback(&checkpoint).map_err(SessionTerminalError::Storage)?;
        }
        result
    }

    fn retire_transaction(
        &mut self,
        retirement: TransferRetirement,
    ) -> Result<SessionTerminalCommit, SessionTerminalError> {
        if retirement == TransferRetirement::Close {
            self.policy
                .decode_scheduler
                .cancel_session(self.decode_session)
                .map_err(|_| internal_storage_error())?;
        }
        let offset = self.framer.offset();
        let mut events = match retirement {
            TransferRetirement::StreamEnd => {
                self.framer.finish().map_err(SessionTerminalError::Storage)?
            }
            TransferRetirement::Reset | TransferRetirement::Close => {
                self.framer.discard();
                GraphicsStorageVec::new(
                    Arc::clone(&self.storage_budget),
                    GraphicsStorageClass::FramingEvents,
                )
                .map_err(SessionTerminalError::Storage)?
            }
        };
        // Taking the transfer first releases its retained chunk storage even if
        // the boundary below cannot be recorded.
        if let Some(pending) = self.pending_kitty_decode.take() {
            events
                .push(GraphicsEvent::Failure(GraphicsFailure {
                    range: pending.range,
                    protocol: GraphicsProtocol::Kitty,
                    category: GraphicsFailureCategory::TruncatedSequence,
                    limit: None,
                }))
                .map_err(SessionTerminalError::Storage)?;
        }
        self.pending_transfer = self.framer.pending_transfer();
        if retirement == TransferRetirement::Close {
            self.release_retained_storage();
        }
        let outputs = GraphicsStorageVec::new(
            Arc::clone(&self.storage_budget),
            GraphicsStorageClass::TerminalOutputs,
        )
        .map_err(SessionTerminalError::Storage)?;
        let input_range = RawByteRange { start: offset, end: self.framer.offset() };
        self.commit_events(events, outputs, None, input_range)
    }

    /// Release every retained image buffer; framer/event owners release independently.
    pub fn release_retained_storage(&mut self) {
        self.pending_kitty_storage = None;
        self.completed_kitty_storage = None;
        self.pending_kitty_decode = None;
        self.sixel_body_storage = None;
        self.kitty_decoded_storage = None;
        self.sixel_decoded_storage = None;
        self.canonical_rgba.clear();
    }

    /// Release everything this session holds because the master switch went
    /// off, and report the retirement boundary the caller must still sequence.
    ///
    /// The kill switch owes the same guarantees a session close does plus one
    /// more: committed canonical state has to go too, because a disabled
    /// Scribe must not be able to replay a scene to a later viewer. Retirement
    /// is therefore the existing [`TransferRetirement::Close`] path — decode
    /// admissions cancelled, partial framing discarded, retained buffers
    /// dropped — followed by the same canonical reset a hard terminal reset
    /// performs, so definitions, placements, and the active screen return to
    /// empty under a fresh generation.
    ///
    /// Text is untouched: the raw outputs in the returned commit are the same
    /// bytes the terminal would have shown anyway.
    ///
    /// Returns `None` when the session owns nothing to release. That guard is
    /// here rather than in a caller because releasing is not free: it opens a
    /// retirement boundary and a fresh generation, and a config reload reaches
    /// every session whether or not it ever showed an image.
    // @lat: [[terminal-images#Terminal Images#Image Master Switch]]
    pub fn release_for_policy_disable(
        &mut self,
    ) -> Result<Option<SessionTerminalCommit>, SessionTerminalError> {
        if !self.holds_image_resources() {
            return Ok(None);
        }
        let commit = self.retire_transfers(TransferRetirement::Close)?;
        self.in_storage_transaction(|terminal, log| {
            let mut next = terminal.canonical.clone();
            next.set_generation(terminal.generation);
            next.reset(log).map_err(SessionTerminalError::Storage)?;
            terminal.generation = next.generation();
            terminal.canonical = next;
            Ok(())
        })?;
        Ok(Some(commit))
    }

    /// Whether this session still owns image resources a disable must free.
    ///
    /// Covers committed state, partial framing, and every retained decode
    /// buffer, including a transfer that finished framing and is waiting on
    /// decode — that one owns bytes while owning no framer state.
    #[must_use]
    pub fn holds_image_resources(&self) -> bool {
        self.canonical.definition_count() != 0
            || self.canonical.placement_count() != 0
            || self.pending_transfer.is_some()
            || self.pending_kitty_decode.is_some()
            || self.pending_kitty_storage.is_some()
            || self.completed_kitty_storage.is_some()
            || self.sixel_body_storage.is_some()
            || self.kitty_decoded_storage.is_some()
            || self.sixel_decoded_storage.is_some()
            || !self.canonical_rgba.is_empty()
    }

    /// Payload-free requested/observed ownership by allocation path.
    #[must_use]
    pub fn storage_ownership(&self) -> ImageStorageOwnership {
        ImageStorageOwnership {
            pending_kitty_requested: self
                .pending_kitty_decode
                .as_ref()
                .map_or(0, |pending| pending.transfer.retained_requested_bytes()),
            pending_kitty_observed: self
                .pending_kitty_decode
                .as_ref()
                .map_or(0, |pending| pending.transfer.retained_observed_bytes()),
            completed_kitty_requested: requested(self.completed_kitty_storage.as_ref()),
            completed_kitty_observed: observed(self.completed_kitty_storage.as_ref()),
            sixel_body_requested: requested(self.sixel_body_storage.as_ref()),
            sixel_body_observed: observed(self.sixel_body_storage.as_ref()),
            kitty_decoded_requested: decoded_requested(self.kitty_decoded_storage.as_deref()),
            kitty_decoded_observed: decoded_observed(self.kitty_decoded_storage.as_deref()),
            sixel_decoded_requested: decoded_requested(self.sixel_decoded_storage.as_deref()),
            sixel_decoded_observed: decoded_observed(self.sixel_decoded_storage.as_deref()),
        }
    }

    /// Return stable payload-free canonical digests for validation evidence.
    #[doc(hidden)]
    #[must_use]
    pub fn validation_storage_digests(&self) -> ImageStorageDigests {
        ImageStorageDigests {
            pending_kitty: self
                .pending_kitty_decode
                .as_ref()
                .map_or(0, |pending| pending.transfer.validation_digest()),
            completed_kitty: storage_digest(self.completed_kitty_storage.as_ref()),
            sixel_body: storage_digest(self.sixel_body_storage.as_ref()),
            kitty_decoded: decoded_storage_digest(self.kitty_decoded_storage.as_deref()),
            sixel_decoded: decoded_storage_digest(self.sixel_decoded_storage.as_deref()),
        }
    }

    #[doc(hidden)]
    #[must_use]
    pub fn validation_pending_kitty_decode_state(&self) -> Option<(u32, usize, bool)> {
        self.pending_kitty_decode.as_ref().map(|pending| pending.transfer.validation_state())
    }

    /// Confirm that two sessions use the exact same process policy object.
    #[must_use]
    pub fn shares_process_policy_with(&self, other: &Self) -> bool {
        self.storage_budget.shares_process_with(&other.storage_budget)
    }

    /// Commit every canonical mutation implied by one observed read.
    ///
    /// Grid effects and image boundaries are replayed in original byte order
    /// against a clone of canonical state. The clone is swapped in only after
    /// the whole read succeeds, so a storage rejection anywhere leaves the
    /// prior definitions, placements, screen, and counters untouched.
    // @lat: [[terminal-images#Terminal Images#Transactional Image Mutations]]
    pub fn commit_mutations(
        &mut self,
        commit: &SessionTerminalCommit,
    ) -> Result<MutationLog, SessionTerminalError> {
        self.in_storage_transaction(|terminal, log| {
            let mut next = terminal.canonical.clone();
            next.set_generation(terminal.generation);
            let mut observation = terminal.grid_observer.observation();
            let mut images = commit
                .outputs
                .iter()
                .filter_map(|output| match output {
                    SessionTerminalOutput::Image { range, boundary, .. } => {
                        Some((*range, boundary))
                    }
                    SessionTerminalOutput::Raw(_) => None,
                })
                .peekable();
            for span in commit.grid_observations() {
                observation = span.observation;
                replay_observed_span(&mut next, span, &mut images, log)?;
            }
            for (_, boundary) in images {
                apply_image_boundary(&mut next, boundary, observation, log)?;
            }
            terminal.generation = next.generation();
            terminal.canonical = next;
            terminal.retain_committed_rgba(last_decoded_protocol(commit), log);
            Ok(())
        })
    }

    /// Commit the canonical mutations implied by one out-of-band grid span.
    ///
    /// Resize and synchronized-update flush spans carry real Alacritty effects
    /// without consuming source bytes, so they commit through the same
    /// all-or-nothing boundary as an ordinary read.
    pub fn commit_span_mutations(
        &mut self,
        span: &ObservedTerminalGridSpan,
    ) -> Result<MutationLog, SessionTerminalError> {
        self.in_storage_transaction(|terminal, log| {
            let mut next = terminal.canonical.clone();
            next.set_generation(terminal.generation);
            for effect in span.effects() {
                apply_observed_effect(&mut next, effect, log)?;
            }
            terminal.generation = next.generation();
            terminal.canonical = next;
            // A span decodes nothing but can erase, scroll, or reset images
            // away, so it only ever releases retained pixels.
            terminal.retain_committed_rgba(None, log);
            Ok(())
        })
    }

    /// Commit one observed read and publish the client records it implies.
    ///
    /// Generation and sequence headroom are checked before anything mutates,
    /// so an exhausted counter returns a typed rejection while the last
    /// committed canonical state, cursor, and publication history stand.
    // @lat: [[terminal-images#Terminal Images#Client Convergence and Counter Safety]]
    pub fn commit_and_publish(
        &mut self,
        commit: &SessionTerminalCommit,
        payload: DefinitionPayload<'_>,
    ) -> Result<Vec<TerminalImageLiveMessage>, SessionTerminalError> {
        let resets = commit
            .grid_observations()
            .iter()
            .flat_map(TerminalGridSpanObservation::effects)
            .filter(|effect| matches!(effect, ObservedTerminalGridEffect::HardReset))
            .count();
        self.publish_committed(resets, |terminal| terminal.commit_mutations(commit), payload)
    }

    /// Commit one out-of-band grid span and publish its client records.
    pub fn commit_span_and_publish(
        &mut self,
        span: &ObservedTerminalGridSpan,
        payload: DefinitionPayload<'_>,
    ) -> Result<Vec<TerminalImageLiveMessage>, SessionTerminalError> {
        let resets = span
            .effects()
            .iter()
            .filter(|effect| matches!(effect, ObservedTerminalGridEffect::HardReset))
            .count();
        self.publish_committed(resets, |terminal| terminal.commit_span_mutations(span), payload)
    }

    fn publish_committed(
        &mut self,
        resets: usize,
        commit: impl FnOnce(&mut Self) -> Result<MutationLog, SessionTerminalError>,
        payload: DefinitionPayload<'_>,
    ) -> Result<Vec<TerminalImageLiveMessage>, SessionTerminalError> {
        let resets =
            u64::try_from(resets).map_err(|_| SessionTerminalError::GenerationExhausted)?;
        self.generation
            .0
            .checked_add(resets)
            .filter(|generation| *generation <= self.policy.generation_ceiling)
            .ok_or(SessionTerminalError::GenerationExhausted)?;
        // One burst per generation this read can open, plus one for the
        // records committed under the generation already in force.
        self.sequence
            .0
            .checked_add(resets.saturating_add(1))
            .filter(|sequence| *sequence <= self.policy.output_sequence_ceiling)
            .ok_or(SessionTerminalError::SequenceExhausted)?;

        let start_generation = self.generation;
        let start_screen = self.published_screen;
        let log = commit(self)?;
        let placements = self.canonical.placements();
        let inputs = PublicationInputs {
            start_generation,
            end_generation: self.generation,
            start_screen,
            end_screen: self.canonical.active_screen(),
            mutations: log.as_slice(),
            placements: &placements,
        };
        let first = TerminalOutputSequence(self.sequence.0.saturating_add(1));
        let end_screen = inputs.end_screen;
        let (messages, consumed) = publish_burst(&inputs, first, payload);
        self.sequence = TerminalOutputSequence(self.sequence.0.saturating_add(consumed));
        self.published_screen = end_screen;
        Ok(messages)
    }

    /// Run one mutation phase under a rolled-back storage transaction.
    fn in_storage_transaction(
        &mut self,
        apply: impl FnOnce(&mut Self, &mut MutationLog) -> Result<(), SessionTerminalError>,
    ) -> Result<MutationLog, SessionTerminalError> {
        let storage_budget = Arc::clone(&self.storage_budget);
        let _transaction =
            storage_budget.lock_transaction().map_err(SessionTerminalError::Storage)?;
        let checkpoint = storage_budget.checkpoint().map_err(SessionTerminalError::Storage)?;
        let mut log =
            MutationLog::new(Arc::clone(&storage_budget)).map_err(SessionTerminalError::Storage)?;
        match apply(self, &mut log) {
            Ok(()) => Ok(log),
            Err(error) => {
                drop(log);
                storage_budget.rollback(&checkpoint).map_err(SessionTerminalError::Storage)?;
                Err(error)
            }
        }
    }

    /// Capture this session's committed images and paused framing for upgrade.
    ///
    /// Reads are already paused when this runs, so nothing is in flight: the
    /// scene is whatever the last read committed, and the framer holds
    /// whatever prefix that read ended in the middle of. The scene travels as
    /// the same bounded burst a late attacher receives, so the successor has
    /// exactly one way to stage a scene rather than a second handoff-only one.
    // @lat: [[terminal-images#Terminal Images#Image State Across Handoff]]
    #[must_use]
    pub fn export_handoff(&self, payload: DefinitionPayload<'_>) -> ExportedSessionImages {
        let definitions = self.canonical.definitions();
        let placements = self.canonical.placements();
        let plan = plan_replay(
            &ReplayInputs {
                generation: self.generation,
                through_sequence: self.sequence,
                active_screen: self.canonical.active_screen(),
                definitions: &definitions,
                placements: &placements,
            },
            payload,
        );
        let pending_kitty = self.pending_kitty_decode.as_ref().map(|pending| PendingKittyHandoff {
            controls: pending.controls,
            presence: pending.presence,
            range: pending.range,
            transfer: pending.transfer.export(),
        });
        ExportedSessionImages {
            definitions: plan.counters.definitions,
            placements: plan.counters.placements,
            chunks: plan.counters.chunks,
            scene_bytes: plan.counters.total_rgba_bytes,
            max_chunk_bytes: plan.counters.max_chunk_bytes,
            state: SessionImageHandoff {
                generation: self.generation,
                sequence: self.sequence,
                active_screen: self.canonical.active_screen(),
                published_screen: self.published_screen,
                next_assigned_image_id: self.canonical.next_assigned_image_id(),
                records: plan.records,
                framing: self.framer.export_partial(),
                pending_kitty,
            },
        }
    }

    /// Install an exported payload on a fresh session before reads resume.
    ///
    /// Every record is validated and the whole burst reassembled before any
    /// field on this session moves, so a truncated or inconsistent payload
    /// leaves an empty session rather than a partial scene. Reassembled pixels
    /// go to `install`, which owns canonical bytes exactly as the live path's
    /// payload seam does.
    // @lat: [[terminal-images#Terminal Images#Image State Across Handoff]]
    pub fn restore_handoff(
        &mut self,
        state: &SessionImageHandoff,
        install: &mut dyn FnMut(&TerminalImageDefinition, Vec<u8>),
    ) -> Result<(), SessionTerminalError> {
        if self.generation != TerminalImageGeneration(1)
            || self.sequence != TerminalOutputSequence(0)
            || self.canonical.definition_count() != 0
            || self.canonical.placement_count() != 0
            || !self.canonical_rgba.is_empty()
        {
            return Err(SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant));
        }
        let restored = stage_handoff_records(state)?;
        // Charged before anything moves, for the same reason the burst is
        // reassembled before anything moves: a session that cannot pay to
        // retain the scene must stay empty rather than hold a scene it can
        // never re-state.
        let mut retained = BTreeMap::new();
        for (definition, rgba) in &restored.pixels {
            // The wire carries canonical pixels without their source protocol,
            // so a restored scene is charged to the Kitty decode class whatever
            // decoded it. The session and process ceilings — the numbers that
            // actually bound a session — are exact either way.
            let mut stored = DecodeBuffer::allocate(
                &self.storage_budget,
                DecodeAllocationClass::KittyRgba,
                rgba.len(),
            )
            .map_err(|_| {
                SessionTerminalError::Storage(GraphicsStorageRejection::AllocationFailed)
            })?;
            stored.extend_from_slice(rgba).map_err(|_| {
                SessionTerminalError::Storage(GraphicsStorageRejection::AllocationFailed)
            })?;
            retained.insert(definition.id, Arc::new(stored));
        }
        let pending = match &state.pending_kitty {
            Some(pending) => Some(self.restore_pending_kitty(pending)?),
            None => None,
        };
        self.framer.restore_partial(&state.framing).map_err(SessionTerminalError::Storage)?;
        self.canonical = CanonicalImageState::restore(
            self.policy.limits,
            CanonicalRestoreCursor {
                generation: state.generation,
                active_screen: state.active_screen,
                next_assigned_image_id: state.next_assigned_image_id,
            },
            &restored.definitions,
            &restored.placements,
        );
        self.generation = state.generation;
        self.sequence = state.sequence;
        self.published_screen = state.published_screen;
        self.pending_transfer = pending
            .as_ref()
            .map(|pending| PendingGraphicsTransfer {
                range: pending.range,
                protocol: GraphicsProtocol::Kitty,
                retained_payload_bytes: pending.transfer.retained_requested_bytes(),
                discarding: false,
            })
            .or(self.pending_transfer);
        self.pending_kitty_decode = pending;
        self.canonical_rgba = retained;
        for (definition, rgba) in restored.pixels {
            install(&definition, rgba);
        }
        Ok(())
    }

    fn restore_pending_kitty(
        &self,
        pending: &PendingKittyHandoff,
    ) -> Result<PendingKittyDecode, SessionTerminalError> {
        let accumulated =
            pending.transfer.decoded.as_ref().map_or(0, |decoded| decoded.len() as u64);
        let request = self.decode_request(
            DecodeTarget::kitty(u64::from(pending.controls.image_id.unwrap_or(0))),
            accumulated,
        );
        let permit = self.admit_decode(&request).map_err(|_| {
            SessionTerminalError::Storage(GraphicsStorageRejection::AllocationFailed)
        })?;
        let mut budget = DecodeBudget::new(decode_limits(self.policy.limits), &NoopHooks, &permit)
            .map_err(|_| {
                SessionTerminalError::Storage(GraphicsStorageRejection::AllocationFailed)
            })?;
        let transfer = KittyTransfer::restore(&pending.transfer, self.policy.limits, &mut budget)
            .map_err(|_| {
            SessionTerminalError::Storage(GraphicsStorageRejection::AllocationFailed)
        })?;
        Ok(PendingKittyDecode {
            transfer,
            controls: pending.controls,
            presence: pending.presence,
            range: pending.range,
        })
    }

    /// Payload-free canonical definitions for inspection and evidence.
    #[must_use]
    pub fn canonical_definitions(&self) -> Vec<TerminalImageDefinition> {
        self.canonical.definitions()
    }

    /// Payload-free canonical placements for inspection and evidence.
    #[must_use]
    pub fn canonical_placements(&self) -> Vec<(TerminalScreenKind, TerminalImagePlacement)> {
        self.canonical.placements()
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

    fn stage_event(
        &mut self,
        event: GraphicsEvent,
        staged: &mut StagedRead,
    ) -> Result<(), SessionTerminalError> {
        match event {
            GraphicsEvent::Raw(raw) => staged
                .outputs
                .push(SessionTerminalOutput::Raw(raw))
                .map_err(SessionTerminalError::Storage)?,
            GraphicsEvent::Kitty { range, command } => {
                self.stage_kitty_event(range, command, staged)?;
            }
            GraphicsEvent::Sixel { range, command } => {
                self.stage_sixel_event(range, command, staged)?;
            }
            GraphicsEvent::SixelMode(change) => {
                let range = change.raw.range;
                staged
                    .outputs
                    .push(SessionTerminalOutput::Raw(change.raw))
                    .map_err(SessionTerminalError::Storage)?;
                self.append_image(
                    range,
                    TerminalImageBoundary::SixelMode { mode: change.mode, enabled: change.enabled },
                    &mut staged.sequence,
                    &mut staged.outputs,
                )?;
            }
            GraphicsEvent::Failure(failure) => {
                let range = failure.range;
                self.append_image(
                    range,
                    TerminalImageBoundary::Failure(failure),
                    &mut staged.sequence,
                    &mut staged.outputs,
                )?;
            }
        }
        Ok(())
    }

    fn stage_kitty_event(
        &mut self,
        range: RawByteRange,
        command: KittyCommand,
        staged: &mut StagedRead,
    ) -> Result<(), SessionTerminalError> {
        match self.prepare_kitty_transfer(range, &command, staged)? {
            KittyTransferPreparation::Ready => {}
            KittyTransferPreparation::Passthrough => {
                self.append_image(
                    range,
                    TerminalImageBoundary::Kitty { command, decoded: None },
                    &mut staged.sequence,
                    &mut staged.outputs,
                )?;
                return Ok(());
            }
            KittyTransferPreparation::HandledFailure => return Ok(()),
        }

        let more = command.chunk_state == KittyChunkState::More;
        let limits = decode_limits(self.policy.limits);
        let target = DecodeTarget::kitty(u64::from(
            self.pending_kitty_decode
                .as_ref()
                .and_then(|pending| pending.controls.image_id)
                .unwrap_or(0),
        ));
        let requested_bytes = u64::try_from(command.payload().len()).unwrap_or(u64::MAX);
        let request = self.decode_request(target, requested_bytes);
        let permit = match self.admit_decode(&request) {
            Ok(permit) => permit,
            Err(error) => {
                return self.reject_decode_admission(range, GraphicsProtocol::Kitty, error, staged);
            }
        };
        let mut budget = match DecodeBudget::new(limits, &NoopHooks, &permit).map_err(|error| {
            kitty_boundary_error(scribe_common::kitty_decode::KittyDecodeError::from(error))
        }) {
            Ok(budget) => budget,
            Err(error) => {
                self.reject_kitty_decode(range, error, staged)?;
                return Ok(());
            }
        };
        let push_result = self
            .pending_kitty_decode
            .as_mut()
            .ok_or(SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant))?
            .transfer
            .push_chunk(command.payload(), more, &mut budget)
            .map_err(kitty_boundary_error);
        if let Err(error) = push_result {
            self.reject_kitty_decode(range, error, staged)?;
            return Ok(());
        }
        if let Some(pending) = self.pending_kitty_decode.as_mut() {
            pending.range.end = range.end;
        }

        if more {
            self.append_image(
                range,
                TerminalImageBoundary::Kitty { command, decoded: None },
                &mut staged.sequence,
                &mut staged.outputs,
            )?;
            return Ok(());
        }

        self.finish_kitty_transfer(range, command, &mut budget, staged)
    }

    /// Publish the final chunk of one Kitty transfer with its canonical facts.
    fn finish_kitty_transfer(
        &mut self,
        range: RawByteRange,
        mut command: KittyCommand,
        budget: &mut DecodeBudget<'_>,
        staged: &mut StagedRead,
    ) -> Result<(), SessionTerminalError> {
        let (controls, presence) = self
            .pending_kitty_decode
            .as_ref()
            .map(|pending| (pending.controls, pending.presence))
            .ok_or(SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant))?;
        // Continuation chunks may omit every control, so the published final
        // boundary carries the transfer's first-command controls and presence.
        command.adopt_transfer_controls(controls, presence);
        let decoded = self
            .pending_kitty_decode
            .as_ref()
            .ok_or(SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant))?
            .transfer
            .finish_preserving(budget)
            .map_err(kitty_boundary_error);
        let canonical = match decoded {
            Ok(_decoded) if controls.action == KittyAction::Query => None,
            Ok(decoded) => {
                let canonical = DecodedImageMeta {
                    width: decoded.width,
                    height: decoded.height,
                    has_alpha: decoded.has_alpha,
                };
                staged.kitty_decoded = StagedDecodeStorage::Replace(decoded.rgba);
                Some(canonical)
            }
            Err(DecodeBoundaryError::Storage(error)) => {
                return Err(SessionTerminalError::Storage(error));
            }
            Err(DecodeBoundaryError::Protocol(category)) => {
                staged.completed_kitty_transfer = self.pending_kitty_decode.take();
                self.append_decode_failure(range, GraphicsProtocol::Kitty, category, staged)?;
                return Ok(());
            }
        };
        staged.completed_kitty_transfer = self.pending_kitty_decode.take();
        self.append_image(
            range,
            TerminalImageBoundary::Kitty { command, decoded: canonical },
            &mut staged.sequence,
            &mut staged.outputs,
        )?;
        Ok(())
    }

    /// Turn a refused admission into a typed boundary. A foreign capability or
    /// poisoned queue is an internal invariant — the seam issued the ticket it
    /// is presenting — while quota, cancellation, and deadline refusals are
    /// ordinary hostile-stream outcomes.
    fn reject_decode_admission(
        &mut self,
        range: RawByteRange,
        protocol: GraphicsProtocol,
        error: DecodeAdmissionError,
        staged: &mut StagedRead,
    ) -> Result<(), SessionTerminalError> {
        match error {
            DecodeAdmissionError::ForeignIssuer
            | DecodeAdmissionError::ForeignSession
            | DecodeAdmissionError::ForeignGeneration
            | DecodeAdmissionError::ForeignTarget
            | DecodeAdmissionError::ForeignBudget
            | DecodeAdmissionError::Poisoned => {
                Err(SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant))
            }
            _ => {
                if protocol == GraphicsProtocol::Kitty {
                    staged.completed_kitty_transfer = self.pending_kitty_decode.take();
                }
                self.append_decode_failure(
                    range,
                    protocol,
                    GraphicsFailureCategory::QuotaExceeded,
                    staged,
                )
            }
        }
    }

    fn reject_kitty_decode(
        &mut self,
        range: RawByteRange,
        error: DecodeBoundaryError,
        staged: &mut StagedRead,
    ) -> Result<(), SessionTerminalError> {
        match error {
            DecodeBoundaryError::Storage(error) => Err(SessionTerminalError::Storage(error)),
            DecodeBoundaryError::Protocol(category) => {
                staged.completed_kitty_transfer = self.pending_kitty_decode.take();
                self.append_decode_failure(range, GraphicsProtocol::Kitty, category, staged)
            }
        }
    }

    fn prepare_kitty_transfer(
        &mut self,
        range: RawByteRange,
        command: &KittyCommand,
        staged: &mut StagedRead,
    ) -> Result<KittyTransferPreparation, SessionTerminalError> {
        if let Some(pending) = self.pending_kitty_decode.as_ref() {
            if pending.controls.accepts_continuation(command) {
                return Ok(KittyTransferPreparation::Ready);
            }
            staged.completed_kitty_transfer = self.pending_kitty_decode.take();
            self.append_decode_failure(
                range,
                GraphicsProtocol::Kitty,
                GraphicsFailureCategory::MalformedControl,
                staged,
            )?;
            return Ok(KittyTransferPreparation::HandledFailure);
        }
        if !matches!(
            command.action,
            KittyAction::Transmit | KittyAction::TransmitDisplay | KittyAction::Query
        ) {
            return Ok(KittyTransferPreparation::Passthrough);
        }
        let Some(params) = kitty_decode_params(command) else {
            self.append_decode_failure(
                range,
                GraphicsProtocol::Kitty,
                GraphicsFailureCategory::MalformedControl,
                staged,
            )?;
            return Ok(KittyTransferPreparation::HandledFailure);
        };
        let transfer =
            match KittyTransfer::new(params, self.policy.limits).map_err(kitty_boundary_error) {
                Ok(transfer) => transfer,
                Err(DecodeBoundaryError::Storage(error)) => {
                    return Err(SessionTerminalError::Storage(error));
                }
                Err(DecodeBoundaryError::Protocol(category)) => {
                    self.append_decode_failure(range, GraphicsProtocol::Kitty, category, staged)?;
                    return Ok(KittyTransferPreparation::HandledFailure);
                }
            };
        self.pending_kitty_decode = Some(PendingKittyDecode {
            transfer,
            controls: command.controls(),
            presence: command.control_presence(),
            range,
        });
        Ok(KittyTransferPreparation::Ready)
    }

    fn stage_sixel_event(
        &mut self,
        range: RawByteRange,
        command: SixelCommand,
        staged: &mut StagedRead,
    ) -> Result<(), SessionTerminalError> {
        let body = OwnedImageStorage::from_slices(
            &self.storage_budget,
            StorageAllocationClass::CanonicalSixel,
            &[command.payload()],
        )
        .map_err(SessionTerminalError::Storage)?;
        staged.sixel_body = StagedStorage::Replace(body);
        self.storage_budget.record_validation_stage(StorageAllocationClass::CanonicalSixel);
        let target = DecodeTarget::sixel(staged.sequence.0.saturating_add(1));
        let requested_bytes = u64::try_from(command.payload().len()).unwrap_or(u64::MAX);
        let request = self.decode_request(target, requested_bytes);
        let permit = match self.admit_decode(&request) {
            Ok(permit) => permit,
            Err(error) => {
                staged.sixel_body = StagedStorage::Clear;
                return self.reject_decode_admission(range, GraphicsProtocol::Sixel, error, staged);
            }
        };
        let canonical = match self.decode_sixel(&command, &permit) {
            Ok(decoded) => {
                let canonical = DecodedImageMeta {
                    width: u32::try_from(decoded.width).unwrap_or(u32::MAX),
                    height: u32::try_from(decoded.height).unwrap_or(u32::MAX),
                    has_alpha: false,
                };
                staged.sixel_decoded = StagedDecodeStorage::Replace(decoded.rgba);
                canonical
            }
            Err(DecodeBoundaryError::Storage(error)) => {
                return Err(SessionTerminalError::Storage(error));
            }
            Err(DecodeBoundaryError::Protocol(category)) => {
                staged.sixel_body = StagedStorage::Clear;
                self.append_decode_failure(range, GraphicsProtocol::Sixel, category, staged)?;
                return Ok(());
            }
        };
        self.append_image(
            range,
            TerminalImageBoundary::Sixel { command, decoded: canonical },
            &mut staged.sequence,
            &mut staged.outputs,
        )?;
        Ok(())
    }

    /// Move the pixels this read decoded onto the definition it committed,
    /// then drop the pixels of every image canonical state no longer holds.
    ///
    /// The decode slots are single-slot by construction, so a read that
    /// decoded several images still holds only the last one's bytes — the
    /// same buffer the last committed definition describes. Pairing is
    /// therefore last-to-last, guarded by an exact canonical-length check so a
    /// definition is left unbacked rather than backed by another image's
    /// bytes: unbacked is withdrawn wherever a scene is stated, which is
    /// recoverable; mismatched would be a silently wrong picture, which is not.
    ///
    /// The slot keeps its own handle on the bytes, so a read that decodes and
    /// then fails to commit leaves ownership exactly where it found it.
    ///
    /// ponytail: one image retained per committed read. Upgrade path: keep the
    /// decoded buffers of a whole read in framing order and pair them with
    /// that read's definitions positionally — only worth it once an
    /// application is observed transmitting several images inside one read.
    // @lat: [[terminal-images#Terminal Images#Retained Canonical Pixels]]
    fn retain_committed_rgba(&mut self, decoded: Option<GraphicsProtocol>, log: &MutationLog) {
        if let Some(protocol) = decoded {
            let defined = log
                .as_slice()
                .iter()
                .rev()
                .find_map(|mutation| match mutation {
                    CanonicalImageMutation::Define { definition } => Some(definition),
                    _ => None,
                })
                .cloned();
            let slot = match protocol {
                GraphicsProtocol::Kitty => &mut self.kitty_decoded_storage,
                GraphicsProtocol::Sixel => &mut self.sixel_decoded_storage,
            };
            if let Some(definition) = defined
                && let Some(rgba) = slot.as_ref()
                && rgba.len() as u64 == definition.rgba_bytes
            {
                let rgba = Arc::clone(rgba);
                self.canonical_rgba.insert(definition.id, rgba);
            }
        }
        let live = self.canonical.definition_ids();
        self.canonical_rgba.retain(|id, _| live.contains(id));
    }

    /// Canonical RGBA for one definition, or `None` when the session cannot
    /// pay for it. Backs the definition-payload seam every scene is stated
    /// through.
    // @lat: [[terminal-images#Terminal Images#Retained Canonical Pixels]]
    #[must_use]
    pub fn canonical_rgba(&self, definition: &TerminalImageDefinition) -> Option<Vec<u8>> {
        self.canonical_rgba
            .get(&definition.id)
            .filter(|retained| retained.len() as u64 == definition.rgba_bytes)
            .map(|retained| retained.to_vec())
    }

    fn append_image(
        &self,
        range: RawByteRange,
        boundary: TerminalImageBoundary,
        sequence: &mut TerminalOutputSequence,
        outputs: &mut GraphicsStorageVec<SessionTerminalOutput>,
    ) -> Result<(), SessionTerminalError> {
        let next = sequence
            .0
            .checked_add(1)
            .filter(|next| *next <= self.policy.output_sequence_ceiling)
            .ok_or(SessionTerminalError::SequenceExhausted)?;
        *sequence = TerminalOutputSequence(next);
        outputs
            .push(SessionTerminalOutput::Image { sequence: *sequence, range, boundary })
            .map_err(SessionTerminalError::Storage)
    }

    fn append_decode_failure(
        &self,
        range: RawByteRange,
        protocol: GraphicsProtocol,
        category: GraphicsFailureCategory,
        staged: &mut StagedRead,
    ) -> Result<(), SessionTerminalError> {
        self.append_image(
            range,
            TerminalImageBoundary::Failure(GraphicsFailure {
                range,
                protocol,
                category,
                limit: None,
            }),
            &mut staged.sequence,
            &mut staged.outputs,
        )
    }

    fn decode_sixel(
        &self,
        command: &SixelCommand,
        permit: &DecodePermit,
    ) -> Result<icy_sixel_decoder::DecodedSixel, DecodeBoundaryError> {
        let settings = DcsSettings {
            aspect_ratio: command.parameters.aspect,
            background_mode: command.parameters.background,
            grid_size: command.parameters.horizontal_grid,
        };
        decode_sixel_payload(
            command.payload(),
            settings,
            decode_limits(self.policy.limits),
            &NoopHooks,
            permit,
        )
        .map_err(|error| sixel_boundary_error(&error))
    }
}

/// Apply one span's grid effects, then every image boundary that ended inside
/// it, preserving original PTY byte order across both kinds of mutation.
fn replay_observed_span<'a, Images>(
    next: &mut CanonicalImageState,
    span: &TerminalGridSpanObservation,
    images: &mut std::iter::Peekable<Images>,
    log: &mut MutationLog,
) -> Result<(), SessionTerminalError>
where
    Images: Iterator<Item = (RawByteRange, &'a TerminalImageBoundary)>,
{
    for effect in span.effects() {
        apply_observed_effect(next, effect, log)?;
    }
    while images.peek().is_some_and(|(range, _)| range.end <= span.range.end) {
        let Some((_, boundary)) = images.next() else { break };
        apply_image_boundary(next, boundary, span.observation, log)?;
    }
    Ok(())
}

/// Apply one Alacritty-observed effect to canonical image state.
///
/// Kitty graphics survive ordinary text erases; only ED2, a hard reset, and
/// alternate-screen creation follow the Kitty visibility lifecycle. All row and
/// column bounds stay half-open exactly as the observer produced them.
fn apply_observed_effect(
    next: &mut CanonicalImageState,
    effect: &ObservedTerminalGridEffect,
    log: &mut MutationLog,
) -> Result<(), SessionTerminalError> {
    let result = match *effect {
        ObservedTerminalGridEffect::Scroll { screen, top, bottom, rows } => next.scroll(
            screen,
            TerminalImageCellClip {
                top: i32::from(top),
                left: 0,
                bottom: i32::from(bottom),
                right: TerminalImageCellClip::MAX_EXCLUSIVE_CELL,
            },
            rows,
            log,
        ),
        ObservedTerminalGridEffect::EraseCells { screen, top, left, bottom, right } => next
            .erase_cells(
                screen,
                TerminalImageCellClip {
                    top: i32::from(top),
                    left: i32::from(left),
                    bottom: i32::from(bottom),
                    right: i32::from(right),
                },
                log,
            ),
        ObservedTerminalGridEffect::EraseDisplay { screen } => next.clear_screen(screen, log),
        ObservedTerminalGridEffect::Resize { primary, alternate } => {
            let mut clip = |screen, size: TerminalGridSizeObservation| {
                next.clip_to_viewport(
                    screen,
                    TerminalImageCellClip {
                        top: 0,
                        left: 0,
                        bottom: i32::from(size.rows),
                        right: i32::from(size.columns),
                    },
                    log,
                )
            };
            clip(TerminalScreenKind::Primary, primary)
                .and_then(|()| clip(TerminalScreenKind::Alternate, alternate))
        }
        ObservedTerminalGridEffect::SwitchScreen { to, .. } => {
            next.set_active_screen(to);
            // Entering the alternate screen creates a fresh grid, so no image
            // placed on a previous alternate screen may survive it.
            if to == TerminalScreenKind::Alternate { next.clear_screen(to, log) } else { Ok(()) }
        }
        // DECSTR leaves Kitty and Sixel graphics alone.
        ObservedTerminalGridEffect::SoftReset => Ok(()),
        ObservedTerminalGridEffect::HardReset => next.reset(log),
    };
    result.map_err(SessionTerminalError::Storage)
}

/// Apply one ordered image boundary using the terminal state it observed.
fn apply_image_boundary(
    next: &mut CanonicalImageState,
    boundary: &TerminalImageBoundary,
    observation: TerminalGridObservation,
    log: &mut MutationLog,
) -> Result<(), SessionTerminalError> {
    let screen = observation.active_screen;
    let cursor = match screen {
        TerminalScreenKind::Primary => observation.primary.cursor,
        TerminalScreenKind::Alternate => observation.alternate.cursor,
    }
    .unwrap_or_default();
    let context = MutationContext {
        screen,
        cursor_row: cursor.row,
        cursor_column: cursor.column,
        cell_width_pixels: observation.cell_width_pixels,
        cell_height_pixels: observation.cell_height_pixels,
    };
    let result = match boundary {
        TerminalImageBoundary::Kitty { command, decoded } => {
            next.apply_kitty(command, *decoded, context, log)
        }
        TerminalImageBoundary::Sixel { decoded, .. } => next.apply_sixel(*decoded, context, log),
        TerminalImageBoundary::SixelMode { .. } | TerminalImageBoundary::Failure(_) => Ok(()),
    };
    result.map_err(SessionTerminalError::Storage)
}

#[derive(Clone, Copy)]
enum DecodeBoundaryError {
    Storage(GraphicsStorageRejection),
    Protocol(GraphicsFailureCategory),
}

/// A handoff burst reassembled off to the side of any live session state.
struct StagedHandoff {
    definitions: Vec<TerminalImageDefinition>,
    placements: Vec<(TerminalScreenKind, TerminalImagePlacement)>,
    pixels: Vec<(TerminalImageDefinition, Vec<u8>)>,
}

/// Validate and reassemble a handoff burst without touching a live session.
///
/// The rules are the receiver's, not the sender's: every record validates on
/// its own terms, chunks arrive contiguously and complete their definition, and
/// every record shares the burst's generation. A `Begin` whose counts disagree
/// with what actually arrived is a truncated payload, which is exactly the case
/// a partial scene would come from.
fn stage_handoff_records(
    state: &SessionImageHandoff,
) -> Result<StagedHandoff, SessionTerminalError> {
    let reject = SessionTerminalError::HandoffRejected;
    let mut staged =
        StagedHandoff { definitions: Vec::new(), placements: Vec::new(), pixels: Vec::new() };
    let mut partial: Vec<(TerminalImageDefinition, Vec<u8>)> = Vec::new();
    let mut committed = false;
    let mut declared: Option<(u32, u32)> = None;
    for record in &state.records {
        record.validate().map_err(reject)?;
        if record.generation() != state.generation {
            return Err(reject(ImageBoundError::InconsistentGeneration));
        }
        match record {
            TerminalImageReplayMessage::Begin { definition_count, placement_count, .. } => {
                if declared.is_some() {
                    return Err(reject(ImageBoundError::InconsistentGeneration));
                }
                declared = Some((*definition_count, *placement_count));
            }
            TerminalImageReplayMessage::Definition { definition, .. } => {
                partial.push((definition.clone(), Vec::new()));
            }
            TerminalImageReplayMessage::DefinitionChunk { chunk, .. } => {
                let Some((definition, rgba)) =
                    partial.iter_mut().find(|(definition, _)| definition.id == chunk.id)
                else {
                    return Err(reject(ImageBoundError::InconsistentCanonicalLength));
                };
                chunk.validate(definition).map_err(reject)?;
                if chunk.offset != rgba.len() as u64 {
                    return Err(reject(ImageBoundError::InconsistentCanonicalLength));
                }
                rgba.extend_from_slice(chunk.data.as_slice());
            }
            TerminalImageReplayMessage::Placement { placement, screen, .. } => {
                staged.placements.push((screen.unwrap_or(state.active_screen), placement.clone()));
            }
            TerminalImageReplayMessage::Commit { .. } => committed = true,
        }
    }
    if !committed || declared.is_none() {
        return Err(reject(ImageBoundError::InconsistentCanonicalLength));
    }
    for (definition, rgba) in partial {
        if rgba.len() as u64 != definition.rgba_bytes {
            return Err(reject(ImageBoundError::InconsistentCanonicalLength));
        }
        staged.definitions.push(definition.clone());
        staged.pixels.push((definition, rgba));
    }
    let counted = (
        u32::try_from(staged.definitions.len()).unwrap_or(u32::MAX),
        u32::try_from(staged.placements.len()).unwrap_or(u32::MAX),
    );
    if declared != Some(counted) {
        return Err(reject(ImageBoundError::InconsistentCanonicalLength));
    }
    // A placement whose definition never arrived would leave the successor
    // painting a hole, so the whole payload is refused instead.
    if staged.placements.iter().any(|(_, placement)| {
        !staged.definitions.iter().any(|definition| definition.id == placement.image_id)
    }) {
        return Err(reject(ImageBoundError::InconsistentCanonicalLength));
    }
    Ok(staged)
}

/// Protocol of the last boundary in one read that produced canonical pixels.
///
/// `None` when the read decoded nothing, which is what keeps a stale decode
/// buffer from an earlier read out of a later read's definition.
// @lat: [[terminal-images#Terminal Images#Retained Canonical Pixels]]
fn last_decoded_protocol(commit: &SessionTerminalCommit) -> Option<GraphicsProtocol> {
    commit.outputs.iter().rev().find_map(|output| {
        let SessionTerminalOutput::Image { boundary, .. } = output else { return None };
        match boundary {
            TerminalImageBoundary::Kitty { decoded: Some(_), .. } => Some(GraphicsProtocol::Kitty),
            TerminalImageBoundary::Sixel { .. } => Some(GraphicsProtocol::Sixel),
            _ => None,
        }
    })
}

fn decode_limits(limits: ImageLimits) -> DecodeLimits {
    DecodeLimits {
        max_width_pixels: limits.max_width_pixels as usize,
        max_height_pixels: limits.max_height_pixels as usize,
        max_pixels: usize::try_from(limits.max_pixels).unwrap_or(usize::MAX),
        max_rgba_bytes: usize::try_from(limits.max_canonical_rgba_bytes).unwrap_or(usize::MAX),
        max_work_units: limits.max_work_units_per_command,
        deadline: Instant::now() + Duration::from_millis(limits.max_decode_ms),
        check_interval_work_units: limits.deadline_check_interval_work_units,
    }
}

fn kitty_decode_params(command: &KittyCommand) -> Option<KittyDataParams> {
    let format = match command.format? {
        KittyFormat::Rgb => DecodeKittyFormat::Rgb,
        KittyFormat::Rgba => DecodeKittyFormat::Rgba,
        KittyFormat::Png => DecodeKittyFormat::Png,
    };
    let compression = match command.compression {
        KittyCompression::None => DecodeKittyCompression::None,
        KittyCompression::Zlib => DecodeKittyCompression::Rfc1950Zlib,
    };
    Some(KittyDataParams {
        format,
        transport: KittyTransport::Direct,
        compression,
        width: command.width,
        height: command.height,
    })
}

fn kitty_boundary_error(
    error: scribe_common::kitty_decode::KittyDecodeError,
) -> DecodeBoundaryError {
    if let Some(storage) = error.storage {
        return DecodeBoundaryError::Storage(storage);
    }
    DecodeBoundaryError::Protocol(rejection_category(error.reason))
}

fn internal_storage_error() -> SessionTerminalError {
    SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant)
}

fn sixel_boundary_error(error: &SixelDecodeError) -> DecodeBoundaryError {
    let category = match error {
        SixelDecodeError::InvalidLimit { .. } | SixelDecodeError::InvalidDimensions { .. } => {
            GraphicsFailureCategory::MalformedControl
        }
        SixelDecodeError::QuotaExceeded { .. } | SixelDecodeError::AllocationFailed { .. } => {
            GraphicsFailureCategory::QuotaExceeded
        }
        SixelDecodeError::Malformed { .. } => GraphicsFailureCategory::MalformedPayload,
        SixelDecodeError::WorkBudgetExceeded { .. }
        | SixelDecodeError::DecodeDeadlineExceeded { .. }
        | SixelDecodeError::DecodeCancelled { .. } => GraphicsFailureCategory::QuotaExceeded,
        SixelDecodeError::Storage(storage) => {
            return DecodeBoundaryError::Storage(*storage);
        }
    };
    DecodeBoundaryError::Protocol(category)
}

fn rejection_category(reason: TerminalImageRejectionReason) -> GraphicsFailureCategory {
    match reason {
        TerminalImageRejectionReason::PolicyDisabled
        | TerminalImageRejectionReason::UnsupportedProtocol => {
            GraphicsFailureCategory::UnsupportedProtocol
        }
        TerminalImageRejectionReason::UnsupportedAction => {
            GraphicsFailureCategory::UnsupportedAction
        }
        TerminalImageRejectionReason::UnsupportedTransport => {
            GraphicsFailureCategory::UnsupportedTransport
        }
        TerminalImageRejectionReason::MalformedFraming
        | TerminalImageRejectionReason::TruncatedSequence => {
            GraphicsFailureCategory::MalformedFraming
        }
        TerminalImageRejectionReason::MalformedControl => GraphicsFailureCategory::MalformedControl,
        TerminalImageRejectionReason::MalformedPayload
        | TerminalImageRejectionReason::ChunkMismatch
        | TerminalImageRejectionReason::DecodeFailed => GraphicsFailureCategory::MalformedPayload,
        TerminalImageRejectionReason::InvalidDimensions => {
            GraphicsFailureCategory::MalformedControl
        }
        TerminalImageRejectionReason::QuotaExceeded
        | TerminalImageRejectionReason::WorkBudgetExceeded
        | TerminalImageRejectionReason::DecodeDeadlineExceeded
        | TerminalImageRejectionReason::DecodeCancelled => GraphicsFailureCategory::QuotaExceeded,
        TerminalImageRejectionReason::ImageNotFound
        | TerminalImageRejectionReason::CapabilityMismatch
        | TerminalImageRejectionReason::RendererUnavailable
        | TerminalImageRejectionReason::Evicted => GraphicsFailureCategory::UnsupportedAction,
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

    /// Session identity every decode admission for this reader is bound to.
    #[must_use]
    pub const fn decode_session(&self) -> DecodeSessionId {
        self.terminal.decode_session()
    }

    /// Frame one effective PTY read through the integrated session seam.
    pub fn process_bytes(
        &mut self,
        bytes: &[u8],
    ) -> Result<SessionTerminalCommit, SessionTerminalError> {
        self.terminal.process_bytes(bytes)
    }

    /// Retire incomplete transfers on EOF, reset, or close.
    pub fn retire_transfers(
        &mut self,
        retirement: TransferRetirement,
    ) -> Result<SessionTerminalCommit, SessionTerminalError> {
        self.terminal.retire_transfers(retirement)
    }

    /// Release every retained and committed image resource after the master
    /// switch went off. `None` means the session held nothing.
    pub fn release_for_policy_disable(
        &mut self,
    ) -> Result<Option<SessionTerminalCommit>, SessionTerminalError> {
        self.terminal.release_for_policy_disable()
    }

    /// Whether this session still owns image resources a disable must free.
    #[must_use]
    pub fn holds_image_resources(&self) -> bool {
        self.terminal.holds_image_resources()
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

    /// Commit the canonical mutations implied by one observed read.
    pub fn commit_mutations(
        &mut self,
        commit: &SessionTerminalCommit,
    ) -> Result<MutationLog, SessionTerminalError> {
        self.terminal.commit_mutations(commit)
    }

    /// Commit the canonical mutations implied by one out-of-band grid span.
    pub fn commit_span_mutations(
        &mut self,
        span: &ObservedTerminalGridSpan,
    ) -> Result<MutationLog, SessionTerminalError> {
        self.terminal.commit_span_mutations(span)
    }

    /// Commit one observed read and publish the client records it implies.
    pub fn commit_and_publish(
        &mut self,
        commit: &SessionTerminalCommit,
        payload: DefinitionPayload<'_>,
    ) -> Result<Vec<TerminalImageLiveMessage>, SessionTerminalError> {
        self.terminal.commit_and_publish(commit, payload)
    }

    /// Commit one out-of-band grid span and publish its client records.
    pub fn commit_span_and_publish(
        &mut self,
        span: &ObservedTerminalGridSpan,
        payload: DefinitionPayload<'_>,
    ) -> Result<Vec<TerminalImageLiveMessage>, SessionTerminalError> {
        self.terminal.commit_span_and_publish(span, payload)
    }

    /// Capture committed images and paused framing for a server upgrade.
    #[must_use]
    pub fn export_handoff(&self, payload: DefinitionPayload<'_>) -> ExportedSessionImages {
        self.terminal.export_handoff(payload)
    }

    /// Install an exported payload on this session before reads resume.
    pub fn restore_handoff(
        &mut self,
        state: &SessionImageHandoff,
        install: &mut dyn FnMut(&TerminalImageDefinition, Vec<u8>),
    ) -> Result<(), SessionTerminalError> {
        self.terminal.restore_handoff(state, install)
    }

    /// Borrow the session seam a bounded handoff export reads from.
    #[must_use]
    pub const fn session(&self) -> &SessionTerminal {
        &self.terminal
    }

    /// Canonical RGBA this session retained for one committed definition.
    // @lat: [[terminal-images#Terminal Images#Retained Canonical Pixels]]
    #[must_use]
    pub fn canonical_rgba(&self, definition: &TerminalImageDefinition) -> Option<Vec<u8>> {
        self.terminal.canonical_rgba(definition)
    }

    /// Payload-free canonical definitions for inspection and evidence.
    #[must_use]
    pub fn canonical_definitions(&self) -> Vec<TerminalImageDefinition> {
        self.terminal.canonical_definitions()
    }

    /// Payload-free canonical placements for inspection and evidence.
    #[must_use]
    pub fn canonical_placements(&self) -> Vec<(TerminalScreenKind, TerminalImagePlacement)> {
        self.terminal.canonical_placements()
    }

    /// Current and peak session/process storage counters.
    pub fn storage_counters(
        &self,
    ) -> Result<(ImageStorageCounters, ImageStorageCounters), SessionTerminalError> {
        self.terminal.storage_counters()
    }

    /// Read immutable ledger snapshots for deterministic fault validation.
    #[doc(hidden)]
    #[must_use]
    pub fn validation_storage_counters(&self) -> (ImageStorageCounters, ImageStorageCounters) {
        self.terminal.storage_budget.validation_counters()
    }

    #[doc(hidden)]
    pub fn validation_storage_class_counters(
        &self,
        class: StorageAllocationClass,
    ) -> Result<(ImageStorageClassCounters, ImageStorageClassCounters), SessionTerminalError> {
        self.terminal.storage_class_counters(class)
    }

    /// Return matching allocation attempts and fired rejections for the
    /// immutable allocation-class validation target.
    #[doc(hidden)]
    #[must_use]
    pub fn validation_rejection_observation(&self) -> (u64, u64, u64) {
        self.terminal.storage_budget.validation_rejection_observation()
    }

    /// Return the allocation class and class-local occurrence whose observed
    /// capacity reconciliation most recently hit a storage ceiling.
    #[doc(hidden)]
    #[must_use]
    pub fn validation_reconcile_rejection(&self) -> Option<(StorageAllocationClass, u64)> {
        self.terminal.storage_budget.validation_reconcile_rejection()
    }

    /// Release all canonical/pending/decoded image storage.
    pub fn release_retained_storage(&mut self) {
        self.terminal.release_retained_storage();
    }

    /// Payload-free ownership facts for accounting evidence.
    #[must_use]
    pub fn storage_ownership(&self) -> ImageStorageOwnership {
        self.terminal.storage_ownership()
    }

    /// Return stable payload-free canonical digests for validation evidence.
    #[doc(hidden)]
    #[must_use]
    pub fn validation_storage_digests(&self) -> ImageStorageDigests {
        self.terminal.validation_storage_digests()
    }

    #[doc(hidden)]
    #[must_use]
    pub fn validation_pending_kitty_decode_state(&self) -> Option<(u32, usize, bool)> {
        self.terminal.validation_pending_kitty_decode_state()
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
    let cuts: Vec<RawByteRange> = commit
        .outputs
        .iter()
        .filter_map(|output| match output {
            SessionTerminalOutput::Image { range, .. } => Some(*range),
            SessionTerminalOutput::Raw(_) => None,
        })
        .collect();
    let collected = feed_terminal_observed_at_cuts(
        observer,
        term,
        ansi_processor,
        bytes,
        (commit.input_range, cuts),
    );
    commit.grid_observations = collected.spans;
    commit.grid_observation_rejection = collected.storage_rejection;
}

/// Ordered span observations plus the typed paired-ledger rejection, if any,
/// that truncated them under storage pressure.
#[derive(Debug, Default)]
pub struct ObservedTerminalGridSpans {
    spans: Option<GraphicsStorageVec<TerminalGridSpanObservation>>,
    pub storage_rejection: Option<GraphicsStorageRejection>,
}

impl ObservedTerminalGridSpans {
    /// Borrow the ordered spans while their storage ownership lives.
    #[must_use]
    pub fn as_slice(&self) -> &[TerminalGridSpanObservation] {
        self.spans.as_ref().map_or(&[], GraphicsStorageVec::as_slice)
    }
}

/// Collect ordered span observations, reserving every retained byte of span
/// and effect metadata from the session/process ledger pair before it is
/// allocated. Storage pressure truncates the payload-free list and returns the
/// typed rejection; the already-fed terminal is never rewound.
fn feed_terminal_observed_at_cuts<T, Cuts>(
    observer: &TerminalGridObserverHandle,
    term: &mut Term<T>,
    ansi_processor: &mut AnsiProcessor,
    bytes: &[u8],
    boundaries: (RawByteRange, Cuts),
) -> ObservedTerminalGridSpans
where
    T: EventListener,
    Cuts: IntoIterator<Item = RawByteRange>,
{
    let (input_range, cuts) = boundaries;
    let mut collected = ObservedTerminalGridSpans::default();
    let mut state = observer.lock();
    let mut absolute_start = input_range.start;
    for range in cuts {
        if range.end <= input_range.start || range.end > input_range.end {
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
            usize::try_from(absolute_start - input_range.start).unwrap_or(bytes.len());
        let relative_end = usize::try_from(absolute_end - input_range.start).unwrap_or(bytes.len());
        let Some(span) = bytes.get(relative_start..relative_end) else {
            break;
        };
        let mut handler = ObservedTermHandler::new(term, &mut state, &observer.budget);
        ansi_processor.advance(&mut handler, span);
        let finished = handler.finish();
        push_span(
            &observer.budget,
            &mut collected,
            RawByteRange { start: absolute_start, end: absolute_end },
            finished,
        );
        absolute_start = absolute_end;
    }
    if absolute_start < input_range.end {
        let relative_start =
            usize::try_from(absolute_start - input_range.start).unwrap_or(bytes.len());
        if let Some(span) = bytes.get(relative_start..) {
            let mut handler = ObservedTermHandler::new(term, &mut state, &observer.budget);
            ansi_processor.advance(&mut handler, span);
            let finished = handler.finish();
            push_span(
                &observer.budget,
                &mut collected,
                RawByteRange { start: absolute_start, end: input_range.end },
                finished,
            );
        }
    }
    collected
}

/// Reserve, then retain, one span observation and its effect ownership.
fn push_span(
    budget: &Arc<GraphicsStorageBudget>,
    collected: &mut ObservedTerminalGridSpans,
    range: RawByteRange,
    finished: ObservedTerminalGridSpan,
) {
    if let Some(error) = finished.storage_rejection {
        collected.storage_rejection = Some(error);
    }
    let span = TerminalGridSpanObservation {
        range,
        observation: finished.observation,
        effects: finished.effects,
    };
    let spans = match collected.spans.as_mut() {
        Some(spans) => spans,
        None => match GraphicsStorageVec::new(
            Arc::clone(budget),
            GraphicsStorageClass::GridObservations,
        ) {
            Ok(spans) => collected.spans.insert(spans),
            Err(error) => {
                collected.storage_rejection = Some(error);
                return;
            }
        },
    };
    if let Err(error) = spans.push(span) {
        collected.storage_rejection = Some(error);
    }
}

/// Exercise ordered boundary-cut deduplication using payload-free metadata.
#[doc(hidden)]
pub fn feed_terminal_observed_with_validation_cuts<T: EventListener>(
    observer: &TerminalGridObserverHandle,
    term: &mut Term<T>,
    ansi_processor: &mut AnsiProcessor,
    bytes: &[u8],
    boundaries: (RawByteRange, &[RawByteRange]),
) -> ObservedTerminalGridSpans {
    let (input_range, cuts) = boundaries;
    feed_terminal_observed_at_cuts(
        observer,
        term,
        ansi_processor,
        bytes,
        (input_range, cuts.iter().copied()),
    )
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
) -> ObservedTerminalGridSpan {
    let mut state = observer.lock();
    let mut handler = ObservedTermHandler::new(term, &mut state, &observer.budget);
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
) -> ObservedTerminalGridSpan {
    let observer = terminal_images.grid_observer();
    let span = feed_terminal_image_result_with_observer(
        &observer,
        term,
        ansi_processor,
        bytes,
        image_result,
    );
    terminal_images.record_grid_observation(&span.observation);
    span
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
) -> ObservedTerminalGridSpan {
    match image_result {
        Ok(commit) => {
            feed_terminal_observed(observer, term, ansi_processor, bytes, commit);
            // Committed effect ownership stays on the commit's own spans; this
            // summary never duplicates the accounted effect storage.
            ObservedTerminalGridSpan {
                observation: commit
                    .grid_observations()
                    .last()
                    .map_or_else(|| observer.observation(), |span| span.observation),
                effects: None,
                storage_rejection: commit.grid_observation_rejection,
            }
        }
        Err(_) => feed_terminal_observed_full_span(observer, term, ansi_processor, bytes),
    }
}

/// Exact production lock/parser/observer orchestration for one PTY ingress result.
pub struct ProductionTerminalFeed<'a, T: EventListener> {
    observer: &'a TerminalGridObserverHandle,
    term: &'a Arc<tokio::sync::Mutex<Term<T>>>,
    ansi_processor: &'a mut AnsiProcessor,
}

impl<'a, T: EventListener> ProductionTerminalFeed<'a, T> {
    pub fn new(
        observer: &'a TerminalGridObserverHandle,
        term: &'a Arc<tokio::sync::Mutex<Term<T>>>,
        ansi_processor: &'a mut AnsiProcessor,
    ) -> Self {
        Self { observer, term, ansi_processor }
    }
}

/// Feed one result through the production terminal context under its lock.
pub async fn feed_terminal_image_result_production<T, AfterFeed>(
    context: ProductionTerminalFeed<'_, T>,
    bytes: &[u8],
    mut image_result: Result<SessionTerminalCommit, SessionTerminalError>,
    after_feed: AfterFeed,
) -> (Result<SessionTerminalCommit, SessionTerminalError>, Option<ObservedTerminalGridSpan>)
where
    T: EventListener,
    AfterFeed: FnOnce(),
{
    let mut term_guard = context.term.lock().await;
    let span = feed_terminal_image_result_with_observer(
        context.observer,
        &mut *term_guard,
        context.ansi_processor,
        bytes,
        &mut image_result,
    );
    after_feed();
    (image_result, Some(span))
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
) -> ObservedTerminalGridSpan {
    let mut state = observer.lock();
    let mut handler = ObservedTermHandler::new(term, &mut state, &observer.budget);
    ansi_processor.stop_sync(&mut handler);
    handler.finish()
}

/// Exact production lock/orchestrator path for synchronized-update timeout flush.
pub async fn flush_terminal_observed_production<T: EventListener>(
    observer: &TerminalGridObserverHandle,
    term: &Arc<tokio::sync::Mutex<Term<T>>>,
    ansi_processor: &mut AnsiProcessor,
) -> ObservedTerminalGridSpan {
    let mut term_guard = term.lock().await;
    flush_terminal_observed(observer, &mut *term_guard, ansi_processor)
}

/// Synchronize the session observer after production `Term::resize`. Alacritty
/// resizes active and inactive grids in the same call; both dimensions are
/// published in one typed effect.
pub fn observe_terminal_resize<T>(
    observer: &TerminalGridObserverHandle,
    term: &Term<T>,
    changed: bool,
) -> ObservedTerminalGridSpan {
    let mut state = observer.lock();
    let resized = state.observe_resize(term, changed);
    let observation = state.observation;
    drop(state);
    let mut span = ObservedTerminalGridSpan { observation, effects: None, storage_rejection: None };
    if !resized {
        return span;
    }
    let effect = ObservedTerminalGridEffect::Resize {
        primary: observation.primary.size,
        alternate: observation.alternate.size,
    };
    match GraphicsStorageVec::new(
        Arc::clone(&observer.budget),
        GraphicsStorageClass::GridObservations,
    ) {
        Ok(mut effects) => match effects.push(effect) {
            Ok(()) => span.effects = Some(effects),
            Err(error) => span.storage_rejection = Some(error),
        },
        Err(error) => span.storage_rejection = Some(error),
    }
    span
}

/// Apply an image-derived cursor movement to the real terminal and observer
/// once. Alacritty clears deferred wrap in `goto`; raw text is never replayed.
pub fn apply_observed_cursor_move<T: EventListener>(
    observer: &TerminalGridObserverHandle,
    term: &mut Term<T>,
    row: i32,
    column: u16,
) -> ObservedTerminalGridSpan {
    let mut state = observer.lock();
    let mut handler = ObservedTermHandler::new(term, &mut state, &observer.budget);
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
            Option<ObservedTerminalGridSpan>,
        ),
    >,
    Reject: FnOnce(PtyReaderIngressRejection),
{
    let image_result = terminal_images.terminal.process_bytes(bytes.as_ref());
    let observer = terminal_images.grid_observer();
    deliver(bytes.as_ref());
    let (image_result, span) = feed(observer, bytes, image_result).await;
    if let Some(span) = span {
        terminal_images.record_grid_observation(&span.observation);
    }
    if let Err(error) = image_result {
        reject(PtyReaderIngressRejection {
            error,
            image_sequence: terminal_images.state().sequence,
        });
    }
    image_result
}
