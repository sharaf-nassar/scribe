//! Bounded streaming framing for terminal-image control strings.
//!
//! PTY reads have arbitrary boundaries. This module recognizes only the
//! frozen terminal-images-v1 subset, consumes image strings, and returns all
//! unrelated bytes exactly once with absolute half-open stream ranges.

use std::fmt;
use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

pub use scribe_image_decode::{
    DecodeStorage as GraphicsStorageBudget, DecodeStorageError as GraphicsStorageRejection,
    DecodeStorageLease as GraphicsStorageLease, StorageClass as GraphicsStorageClass,
};

/// Frozen terminal-images-v1 control-string ceiling.
pub const MAX_CONTROL_STRING_BYTES: usize = 16_777_216;

/// Frozen direct Kitty chunk ceiling.
pub const MAX_KITTY_CHUNK_PAYLOAD_BYTES: usize = 4_096;

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const C1_DCS: u8 = 0x90;
const C1_CSI: u8 = 0x9b;
const C1_ST: u8 = 0x9c;
const C1_APC: u8 = 0x9f;
const INLINE_RAW_BYTES: usize = 32;

/// Absolute half-open byte boundary in the raw PTY stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawByteRange {
    pub start: u64,
    pub end: u64,
}

impl RawByteRange {
    fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }
}

/// Move-only bytes whose storage ownership cannot detach from their lease.
pub struct GraphicsPayload {
    bytes: Vec<u8>,
    retention: GraphicsRetention,
}

impl GraphicsPayload {
    /// Borrow the retained bytes without transferring their accounting lease.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Number of logical bytes in this payload.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether this payload has no logical bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Requested live storage held with these bytes.
    #[must_use]
    pub fn requested_bytes(&self) -> usize {
        self.retention.requested_bytes()
    }

    /// Allocator-observed live storage held with these bytes.
    #[must_use]
    pub fn observed_bytes(&self) -> usize {
        self.retention.observed_bytes()
    }
}

impl fmt::Debug for GraphicsPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GraphicsPayload")
            .field("len", &self.bytes.len())
            .field("requested_bytes", &self.requested_bytes())
            .field("observed_bytes", &self.observed_bytes())
            .finish_non_exhaustive()
    }
}

impl PartialEq for GraphicsPayload {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for GraphicsPayload {}

/// Bytes that must continue to the ordinary terminal parser exactly once.
#[derive(Debug, PartialEq, Eq)]
pub struct RawBytes {
    pub range: RawByteRange,
    payload: RawPayload,
}

#[derive(Debug, PartialEq, Eq)]
enum RawPayload {
    Inline { bytes: [u8; INLINE_RAW_BYTES], len: u8 },
    Retained(GraphicsPayload),
}

impl RawBytes {
    /// Borrow raw terminal bytes while their transient/retained owner lives.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        match &self.payload {
            RawPayload::Inline { bytes, len } => bytes.get(..usize::from(*len)).unwrap_or_default(),
            RawPayload::Retained(payload) => payload.as_slice(),
        }
    }

    /// Number of raw terminal bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    /// Whether this raw span is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
}

/// Supported image protocols at the framing boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsProtocol {
    Kitty,
    Sixel,
}

/// Payload-free metadata for the recognized graphics transfer currently held
/// across PTY reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingGraphicsTransfer {
    pub range: RawByteRange,
    pub protocol: GraphicsProtocol,
    pub retained_payload_bytes: usize,
    pub discarding: bool,
}

/// Stable terminal-images-v1 rejection categories used by framing/parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsFailureCategory {
    UnsupportedProtocol,
    UnsupportedAction,
    UnsupportedTransport,
    MalformedFraming,
    MalformedControl,
    MalformedPayload,
    TruncatedSequence,
    QuotaExceeded,
}

/// Safe limit identifier for a rejection; no payload bytes are retained here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsLimit {
    ControlString,
    KittyChunkPayload,
}

/// Non-forgeable storage ownership that travels with a retained image event.
pub struct GraphicsRetention {
    lease: GraphicsStorageLease,
}

impl GraphicsRetention {
    /// Requested live storage represented by this event.
    #[must_use]
    pub fn requested_bytes(&self) -> usize {
        self.lease.requested_bytes()
    }

    /// Allocator-observed retained capacity represented by this event.
    #[must_use]
    pub fn observed_bytes(&self) -> usize {
        self.lease.observed_bytes()
    }
}

impl fmt::Debug for GraphicsRetention {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GraphicsRetention")
            .field("requested_bytes", &self.requested_bytes())
            .field("observed_bytes", &self.observed_bytes())
            .finish()
    }
}

/// Move-only metadata whose vector capacity remains paired with a storage lease.
pub struct GraphicsStorageVec<T> {
    items: Vec<T>,
    retention: GraphicsRetention,
    budget: Arc<GraphicsStorageBudget>,
    class: GraphicsStorageClass,
}

impl<T> GraphicsStorageVec<T> {
    pub fn new(
        budget: Arc<GraphicsStorageBudget>,
        class: GraphicsStorageClass,
    ) -> Result<Self, GraphicsStorageRejection> {
        let retention = GraphicsRetention { lease: budget.reserve(class, 0)? };
        Ok(Self { items: Vec::new(), retention, budget, class })
    }

    pub fn push(&mut self, item: T) -> Result<(), GraphicsStorageRejection> {
        if self.items.len() < self.items.capacity() {
            self.items.push(item);
            return Ok(());
        }
        let needed =
            self.items.len().checked_add(1).ok_or(GraphicsStorageRejection::CounterOverflow)?;
        let capacity = self.items.capacity().max(1).checked_mul(2).unwrap_or(needed).max(needed);
        let requested = capacity
            .checked_mul(size_of::<T>())
            .ok_or(GraphicsStorageRejection::CounterOverflow)?;
        let mut lease = self.budget.reserve(self.class, requested)?;
        if requested != 0 {
            lease.record_allocation_attempt()?;
        }
        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(capacity)
            .map_err(|_| GraphicsStorageRejection::AllocationFailed)?;
        let observed = replacement
            .capacity()
            .checked_mul(size_of::<T>())
            .ok_or(GraphicsStorageRejection::CounterOverflow)?;
        let observed = self.budget.observe_allocation_capacity(observed)?;
        lease.reconcile_observed(observed)?;
        replacement.append(&mut self.items);
        replacement.push(item);
        self.items = replacement;
        self.retention = GraphicsRetention { lease };
        Ok(())
    }

    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    pub fn try_extend(
        &mut self,
        items: impl IntoIterator<Item = T>,
    ) -> Result<(), GraphicsStorageRejection> {
        for item in items {
            self.push(item)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn requested_bytes(&self) -> usize {
        self.retention.requested_bytes()
    }

    #[must_use]
    pub fn observed_bytes(&self) -> usize {
        self.retention.observed_bytes()
    }
}

impl<T> Deref for GraphicsStorageVec<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

impl<T> DerefMut for GraphicsStorageVec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.items
    }
}

/// Owning iterator that keeps the storage lease alive until the backing
/// allocation is actually freed. Field order is load-bearing: the items are
/// dropped, and only then is their ownership released from the ledger.
pub struct GraphicsStorageIntoIter<T> {
    items: std::vec::IntoIter<T>,
    _retention: GraphicsRetention,
}

impl<T> Iterator for GraphicsStorageIntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.items.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.items.size_hint()
    }
}

impl<T> ExactSizeIterator for GraphicsStorageIntoIter<T> {
    fn len(&self) -> usize {
        self.items.len()
    }
}

impl<T> IntoIterator for GraphicsStorageVec<T> {
    type Item = T;
    type IntoIter = GraphicsStorageIntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        GraphicsStorageIntoIter { items: self.items.into_iter(), _retention: self.retention }
    }
}

impl<'a, T> IntoIterator for &'a GraphicsStorageVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<T: fmt::Debug> fmt::Debug for GraphicsStorageVec<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GraphicsStorageVec")
            .field("items", &self.items)
            .field("requested_bytes", &self.requested_bytes())
            .field("observed_bytes", &self.observed_bytes())
            .finish_non_exhaustive()
    }
}

impl<T: PartialEq> PartialEq for GraphicsStorageVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.items == other.items
    }
}

impl<T: Eq> Eq for GraphicsStorageVec<T> {}

impl PartialEq for GraphicsRetention {
    fn eq(&self, other: &Self) -> bool {
        self.requested_bytes() == other.requested_bytes()
            && self.observed_bytes() == other.observed_bytes()
    }
}

impl Eq for GraphicsRetention {}

/// Typed, payload-free failure annotation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphicsFailure {
    pub range: RawByteRange,
    pub protocol: GraphicsProtocol,
    pub category: GraphicsFailureCategory,
    pub limit: Option<GraphicsLimit>,
}

impl GraphicsFailure {
    fn new(
        range: RawByteRange,
        protocol: GraphicsProtocol,
        category: GraphicsFailureCategory,
    ) -> Self {
        Self { range, protocol, category, limit: None }
    }
}

/// Kitty v1 actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyAction {
    Transmit,
    TransmitDisplay,
    Put,
    Query,
    Delete,
}

/// Direct Kitty pixel formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyFormat {
    Rgb,
    Rgba,
    Png,
}

/// Supported Kitty delete selector, preserving soft/lowercase polarity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyDelete {
    pub selector: char,
    pub free_data: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyCompression {
    None,
    Zlib,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyChunkState {
    Final,
    More,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyPlacementMode {
    Classic,
    UnicodePlaceholder,
}

/// Narrow parsed Kitty control data plus still-encoded direct payload.
#[derive(Debug, PartialEq, Eq)]
pub struct KittyCommand {
    pub action: KittyAction,
    pub format: Option<KittyFormat>,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub source_x: Option<u32>,
    pub source_y: Option<u32>,
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,
    pub columns: Option<u32>,
    pub rows: Option<u32>,
    pub pixel_x: Option<u32>,
    pub pixel_y: Option<u32>,
    pub z_index: Option<i32>,
    pub move_cursor: Option<bool>,
    pub placement_mode: KittyPlacementMode,
    pub chunk_state: KittyChunkState,
    pub quiet: u8,
    pub compression: KittyCompression,
    pub delete: Option<KittyDelete>,
    control_mask: [u64; 4],
    payload: Option<GraphicsPayload>,
}

/// Payload-free immutable controls captured from a Kitty transfer's first chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyCommandControls {
    pub action: KittyAction,
    pub format: Option<KittyFormat>,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub source_x: Option<u32>,
    pub source_y: Option<u32>,
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,
    pub columns: Option<u32>,
    pub rows: Option<u32>,
    pub pixel_x: Option<u32>,
    pub pixel_y: Option<u32>,
    pub z_index: Option<i32>,
    pub move_cursor: Option<bool>,
    pub placement_mode: KittyPlacementMode,
    pub quiet: u8,
    pub compression: KittyCompression,
    pub delete: Option<KittyDelete>,
}

impl KittyCommand {
    /// Borrow the still-encoded direct payload with its lease attached.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        self.payload.as_ref().map_or(&[], GraphicsPayload::as_slice)
    }

    /// Requested bytes owned by this parsed command event.
    #[must_use]
    pub fn retained_requested_bytes(&self) -> usize {
        self.payload.as_ref().map_or(0, GraphicsPayload::requested_bytes)
    }

    /// Observed bytes owned by this parsed command event.
    #[must_use]
    pub fn retained_observed_bytes(&self) -> usize {
        self.payload.as_ref().map_or(0, GraphicsPayload::observed_bytes)
    }

    #[must_use]
    pub fn controls(&self) -> KittyCommandControls {
        KittyCommandControls {
            action: self.action,
            format: self.format,
            image_id: self.image_id,
            placement_id: self.placement_id,
            width: self.width,
            height: self.height,
            source_x: self.source_x,
            source_y: self.source_y,
            source_width: self.source_width,
            source_height: self.source_height,
            columns: self.columns,
            rows: self.rows,
            pixel_x: self.pixel_x,
            pixel_y: self.pixel_y,
            z_index: self.z_index,
            move_cursor: self.move_cursor,
            placement_mode: self.placement_mode,
            quiet: self.quiet,
            compression: self.compression,
            delete: self.delete,
        }
    }

    #[must_use]
    pub fn control_present(&self, key: u8) -> bool {
        let word = usize::from(key) / 64;
        let bit = u32::from(key % 64);
        self.control_mask.get(word).is_some_and(|mask| mask & (1_u64 << bit) != 0)
    }

    /// Payload-free record of which controls this command carried explicitly.
    #[must_use]
    pub fn control_presence(&self) -> KittyControlPresence {
        KittyControlPresence(self.control_mask)
    }

    /// Republish a split transfer's saved first-command controls on its final
    /// boundary. Continuation chunks legally omit every control, so the last
    /// chunk's defaults must never reach consumers; only the payload, chunk
    /// state, and range stay local to this chunk.
    pub fn adopt_transfer_controls(
        &mut self,
        controls: KittyCommandControls,
        presence: KittyControlPresence,
    ) {
        self.action = controls.action;
        self.format = controls.format;
        self.image_id = controls.image_id;
        self.placement_id = controls.placement_id;
        self.width = controls.width;
        self.height = controls.height;
        self.source_x = controls.source_x;
        self.source_y = controls.source_y;
        self.source_width = controls.source_width;
        self.source_height = controls.source_height;
        self.columns = controls.columns;
        self.rows = controls.rows;
        self.pixel_x = controls.pixel_x;
        self.pixel_y = controls.pixel_y;
        self.z_index = controls.z_index;
        self.move_cursor = controls.move_cursor;
        self.placement_mode = controls.placement_mode;
        self.quiet = controls.quiet;
        self.compression = controls.compression;
        self.delete = controls.delete;
        self.control_mask = presence.0;
    }
}

/// Payload-free presence bitmap of the controls one Kitty command carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyControlPresence([u64; 4]);

impl KittyCommandControls {
    /// Validate every explicitly repeated continuation control against chunk one.
    #[must_use]
    pub fn accepts_continuation(self, command: &KittyCommand) -> bool {
        (!command.control_present(b'a') || command.action == self.action)
            && (!command.control_present(b'f') || command.format == self.format)
            && (!command.control_present(b'i') || command.image_id == self.image_id)
            && (!command.control_present(b'p') || command.placement_id == self.placement_id)
            && (!command.control_present(b's') || command.width == self.width)
            && (!command.control_present(b'v') || command.height == self.height)
            && (!command.control_present(b'x') || command.source_x == self.source_x)
            && (!command.control_present(b'y') || command.source_y == self.source_y)
            && (!command.control_present(b'w') || command.source_width == self.source_width)
            && (!command.control_present(b'h') || command.source_height == self.source_height)
            && (!command.control_present(b'c') || command.columns == self.columns)
            && (!command.control_present(b'r') || command.rows == self.rows)
            && (!command.control_present(b'X') || command.pixel_x == self.pixel_x)
            && (!command.control_present(b'Y') || command.pixel_y == self.pixel_y)
            && (!command.control_present(b'z') || command.z_index == self.z_index)
            && (!command.control_present(b'C') || command.move_cursor == self.move_cursor)
            && (!command.control_present(b'U') || command.placement_mode == self.placement_mode)
            && (!command.control_present(b'q') || command.quiet == self.quiet)
            && (!command.control_present(b'o') || command.compression == self.compression)
            && (!command.control_present(b'd') || command.delete == self.delete)
    }
}

/// Parsed Sixel introducer parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SixelParameters {
    pub aspect: Option<u16>,
    pub background: Option<u16>,
    pub horizontal_grid: Option<u16>,
}

/// Narrow validated Sixel command. Payload remains encoded for the decoder.
#[derive(Debug, PartialEq, Eq)]
pub struct SixelCommand {
    pub parameters: SixelParameters,
    payload: GraphicsPayload,
}

impl SixelCommand {
    /// Borrow the encoded Sixel body with its lease attached.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        self.payload.as_slice()
    }

    /// Requested bytes owned by this parsed command event.
    #[must_use]
    pub fn retained_requested_bytes(&self) -> usize {
        self.payload.requested_bytes()
    }

    /// Observed bytes owned by this parsed command event.
    #[must_use]
    pub fn retained_observed_bytes(&self) -> usize {
        self.payload.observed_bytes()
    }
}

/// Xterm private modes relevant to Sixel chronology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SixelMode {
    Display,
    CursorRight,
}

/// Parsed private mode transition. Its `raw` bytes still go to Alacritty.
#[derive(Debug, PartialEq, Eq)]
pub struct SixelModeChange {
    pub raw: RawBytes,
    pub mode: SixelMode,
    pub enabled: bool,
}

/// One ordered result from [`GraphicsFramer`].
#[derive(Debug, PartialEq, Eq)]
pub enum GraphicsEvent {
    Raw(RawBytes),
    Kitty { range: RawByteRange, command: KittyCommand },
    Sixel { range: RawByteRange, command: SixelCommand },
    SixelMode(SixelModeChange),
    Failure(GraphicsFailure),
}

impl GraphicsEvent {
    /// Bytes this event forwards to the existing terminal path.
    #[must_use]
    pub fn terminal_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Raw(raw) => Some(raw.as_slice()),
            Self::SixelMode(change) => Some(change.raw.as_slice()),
            Self::Kitty { .. } | Self::Sixel { .. } | Self::Failure(_) => None,
        }
    }

    /// Absolute source range for ordering and later commit annotations.
    #[must_use]
    pub fn range(&self) -> RawByteRange {
        match self {
            Self::Raw(raw) => raw.range,
            Self::Kitty { range, .. } | Self::Sixel { range, .. } => *range,
            Self::SixelMode(change) => change.raw.range,
            Self::Failure(failure) => failure.range,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StringForm {
    SevenBit,
    C1,
}

#[derive(Clone, Copy, Debug)]
enum CandidateKind {
    Escape,
    ApcPrefix,
    DcsHeader { form: StringForm, scanner: SixelHeaderScanner },
    Csi { form: StringForm },
    C1ApcPrefix,
}

/// Fixed-size speculative state for the three Sixel DCS parameters.
///
/// The candidate still retains the raw DCS prefix, bounded by the control
/// string ceiling, because a later non-`q` final byte must be forwarded
/// exactly. This scanner is separate constant-size metadata: it detects a
/// fourth field or `u16` overflow on the byte that makes the header malformed.
#[derive(Clone, Copy, Debug, Default)]
struct SixelHeaderScanner {
    values: [Option<u16>; 3],
    current: Option<u16>,
    field_index: usize,
    malformed: bool,
}

impl SixelHeaderScanner {
    fn scan(&mut self, byte: u8) {
        if self.malformed {
            return;
        }
        if byte == b';' {
            let Some(slot) = self.values.get_mut(self.field_index) else {
                self.malformed = true;
                return;
            };
            *slot = self.current;
            self.current = None;
            if self.field_index == self.values.len().saturating_sub(1) {
                self.malformed = true;
            } else {
                self.field_index = self.field_index.saturating_add(1);
            }
            return;
        }

        let digit = u16::from(byte.saturating_sub(b'0'));
        match self.current.unwrap_or(0).checked_mul(10).and_then(|value| value.checked_add(digit)) {
            Some(value) => self.current = Some(value),
            None => self.malformed = true,
        }
    }

    fn finish(mut self) -> Result<SixelParameters, GraphicsFailureCategory> {
        if self.malformed {
            return Err(GraphicsFailureCategory::MalformedControl);
        }
        let Some(slot) = self.values.get_mut(self.field_index) else {
            return Err(GraphicsFailureCategory::MalformedControl);
        };
        *slot = self.current;
        let [aspect, background, horizontal_grid] = self.values;
        if background.is_some_and(|value| value > 2) {
            return Err(GraphicsFailureCategory::MalformedControl);
        }
        Ok(SixelParameters { aspect, background, horizontal_grid })
    }
}

/// Fallibly allocated bytes whose observed capacity is covered before retain.
// @lat: [[terminal-images#Terminal Images#Exact Requested Storage Accounting]]
struct RetainedVec {
    bytes: Vec<u8>,
    retention: GraphicsRetention,
    rollback: Option<(Vec<u8>, GraphicsRetention)>,
    transactional: bool,
    budget: Arc<GraphicsStorageBudget>,
    class: GraphicsStorageClass,
}

impl RetainedVec {
    fn empty(
        budget: Arc<GraphicsStorageBudget>,
        class: GraphicsStorageClass,
    ) -> Result<Self, GraphicsStorageRejection> {
        Self::with_requested_capacity(budget, class, 0)
    }

    fn with_requested_capacity(
        budget: Arc<GraphicsStorageBudget>,
        class: GraphicsStorageClass,
        requested: usize,
    ) -> Result<Self, GraphicsStorageRejection> {
        let mut retention = GraphicsRetention { lease: budget.reserve(class, requested)? };
        if requested > 0 {
            retention.lease.record_allocation_attempt()?;
        }
        let mut bytes = Vec::new();
        if requested > 0 {
            bytes
                .try_reserve_exact(requested)
                .map_err(|_| GraphicsStorageRejection::AllocationFailed)?;
            let observed = budget.observe_allocation_capacity(bytes.capacity())?;
            retention.lease.reconcile_observed(observed)?;
        }
        Ok(Self { bytes, retention, rollback: None, transactional: false, budget, class })
    }

    fn from_slice(
        budget: Arc<GraphicsStorageBudget>,
        class: GraphicsStorageClass,
        source: &[u8],
    ) -> Result<Self, GraphicsStorageRejection> {
        let mut retained = Self::with_requested_capacity(budget, class, source.len())?;
        retained.bytes.extend_from_slice(source);
        Ok(retained)
    }

    fn push(&mut self, byte: u8) -> Result<(), GraphicsStorageRejection> {
        if self.bytes.len() < self.bytes.capacity() {
            self.bytes.push(byte);
            return Ok(());
        }
        let needed =
            self.bytes.len().checked_add(1).ok_or(GraphicsStorageRejection::CounterOverflow)?;
        let requested = self.bytes.capacity().max(1).checked_mul(2).unwrap_or(needed).max(needed);
        let mut replacement =
            Self::with_requested_capacity(Arc::clone(&self.budget), self.class, requested)?;
        replacement.bytes.extend_from_slice(&self.bytes);
        replacement.bytes.push(byte);
        let previous = std::mem::replace(self, replacement);
        self.transactional = previous.transactional;
        self.rollback = if previous.transactional {
            previous.rollback.or(Some((previous.bytes, previous.retention)))
        } else {
            None
        };
        Ok(())
    }

    fn pop(&mut self) -> Option<u8> {
        self.bytes.pop()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    fn into_payload(self) -> GraphicsPayload {
        GraphicsPayload { bytes: self.bytes, retention: self.retention }
    }

    fn begin_transaction(&mut self) {
        self.transactional = true;
    }

    fn commit_transaction(&mut self) {
        self.rollback = None;
        self.transactional = false;
    }

    fn rollback_transaction(&mut self, old_len: usize) -> Result<(), GraphicsStorageRejection> {
        if let Some((bytes, retention)) = self.rollback.take() {
            self.bytes = bytes;
            self.retention = retention;
        }
        if old_len > self.bytes.len() {
            return Err(GraphicsStorageRejection::InternalInvariant);
        }
        self.bytes.truncate(old_len);
        self.transactional = false;
        Ok(())
    }

    fn try_clone(&self) -> Result<Self, GraphicsStorageRejection> {
        Self::from_slice(Arc::clone(&self.budget), self.class, &self.bytes)
    }
}

impl fmt::Debug for RetainedVec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedVec")
            .field("len", &self.bytes.len())
            .field("capacity", &self.bytes.capacity())
            .field("retention", &self.retention)
            .finish_non_exhaustive()
    }
}

impl Deref for RetainedVec {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

#[derive(Debug)]
struct Candidate {
    start: u64,
    bytes: RetainedVec,
    kind: CandidateKind,
}

#[derive(Clone, Debug)]
enum ActiveKind {
    Kitty,
    UnsupportedKittyC1,
    Sixel { form: StringForm, parameters: Result<SixelParameters, GraphicsFailureCategory> },
}

impl ActiveKind {
    fn protocol(&self) -> GraphicsProtocol {
        match self {
            Self::Kitty | Self::UnsupportedKittyC1 => GraphicsProtocol::Kitty,
            Self::Sixel { .. } => GraphicsProtocol::Sixel,
        }
    }

    fn form(&self) -> StringForm {
        match self {
            Self::Kitty => StringForm::SevenBit,
            Self::UnsupportedKittyC1 => StringForm::C1,
            Self::Sixel { form, .. } => *form,
        }
    }
}

#[derive(Debug)]
struct ActiveString {
    start: u64,
    kind: ActiveKind,
    body: RetainedVec,
    control_bytes: usize,
    pending_escape: bool,
    kitty_payload_started: bool,
    kitty_payload_bytes: usize,
    failure: Option<(GraphicsFailureCategory, Option<GraphicsLimit>)>,
}

#[derive(Debug)]
enum FramerState {
    Ground,
    Candidate(Candidate),
    Active(ActiveString),
}

#[derive(Clone)]
enum FramerStateSnapshot {
    Ground,
    Candidate {
        start: u64,
        kind: CandidateKind,
        len: usize,
    },
    Active {
        start: u64,
        kind: ActiveKind,
        len: usize,
        control_bytes: usize,
        pending_escape: bool,
        kitty_payload_started: bool,
        kitty_payload_bytes: usize,
        failure: Option<(GraphicsFailureCategory, Option<GraphicsLimit>)>,
    },
}

struct FramerTransaction {
    offset: u64,
    snapshot: FramerStateSnapshot,
    owned_original: Option<FramerState>,
}

fn rollback_state_buffer(
    state: &mut FramerState,
    snapshot: &FramerStateSnapshot,
) -> Result<(), GraphicsStorageRejection> {
    match (snapshot, state) {
        (FramerStateSnapshot::Ground, _) => Ok(()),
        (FramerStateSnapshot::Candidate { len, .. }, FramerState::Candidate(candidate)) => {
            candidate.bytes.rollback_transaction(*len)
        }
        (FramerStateSnapshot::Active { len, .. }, FramerState::Active(active)) => {
            active.body.rollback_transaction(*len)
        }
        _ => Err(GraphicsStorageRejection::InternalInvariant),
    }
}

/// Incremental bounded APC/DCS framer over arbitrary PTY byte chunks.
// @lat: [[terminal-images#Terminal Images#Bounded Framing and Parsing]]
pub struct GraphicsFramer {
    state: FramerState,
    offset: u64,
    max_control_string_bytes: usize,
    storage_budget: Arc<GraphicsStorageBudget>,
    transaction: Option<FramerTransaction>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsFramerValidationState {
    Ground,
    Candidate,
    Active,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphicsFramerValidationSnapshot {
    pub state: GraphicsFramerValidationState,
    pub offset: u64,
    pub start: Option<u64>,
    pub len: usize,
    pub capacity: usize,
    pub digest: u64,
    pub requested: usize,
    pub observed: usize,
    pub transaction_active: bool,
}

impl GraphicsFramer {
    /// Construct a production framer bound to one session/process budget pair.
    #[must_use]
    pub fn with_storage_budget(
        max_control_string_bytes: usize,
        storage_budget: Arc<GraphicsStorageBudget>,
    ) -> Self {
        Self {
            state: FramerState::Ground,
            offset: 0,
            max_control_string_bytes,
            storage_budget,
            transaction: None,
        }
    }

    /// Fallibly clone retained state after reserving simultaneous copy storage.
    pub fn try_clone(&self) -> Result<Self, GraphicsStorageRejection> {
        let state = match &self.state {
            FramerState::Ground => FramerState::Ground,
            FramerState::Candidate(candidate) => FramerState::Candidate(Candidate {
                start: candidate.start,
                bytes: candidate.bytes.try_clone()?,
                kind: candidate.kind,
            }),
            FramerState::Active(active) => FramerState::Active(ActiveString {
                start: active.start,
                kind: active.kind.clone(),
                body: active.body.try_clone()?,
                control_bytes: active.control_bytes,
                pending_escape: active.pending_escape,
                kitty_payload_started: active.kitty_payload_started,
                kitty_payload_bytes: active.kitty_payload_bytes,
                failure: active.failure,
            }),
        };
        Ok(Self {
            state,
            offset: self.offset,
            max_control_string_bytes: self.max_control_string_bytes,
            storage_budget: Arc::clone(&self.storage_budget),
            transaction: None,
        })
    }

    /// Feed one arbitrary PTY read and return ordered complete events.
    pub fn push(
        &mut self,
        input: &[u8],
    ) -> Result<GraphicsStorageVec<GraphicsEvent>, GraphicsStorageRejection> {
        let events = self.push_staged(input)?;
        self.commit_staged();
        Ok(events)
    }

    /// Stage one read while retaining enough journal state for caller rollback.
    #[doc(hidden)]
    pub fn push_staged(
        &mut self,
        input: &[u8],
    ) -> Result<GraphicsStorageVec<GraphicsEvent>, GraphicsStorageRejection> {
        let input_len =
            u64::try_from(input.len()).map_err(|_| GraphicsStorageRejection::CounterOverflow)?;
        let _end_offset =
            self.offset.checked_add(input_len).ok_or(GraphicsStorageRejection::CounterOverflow)?;
        self.begin_transaction()?;
        let mut output = match EventOutput::new(Arc::clone(&self.storage_budget)) {
            Ok(output) => output,
            Err(rejection) => {
                self.rollback_staged()?;
                return Err(rejection);
            }
        };
        for &byte in input {
            let position = self.offset;
            self.offset =
                self.offset.checked_add(1).ok_or(GraphicsStorageRejection::CounterOverflow)?;
            if let Err(rejection) = self.process_byte(position, byte, &mut output) {
                self.rollback_staged()?;
                return Err(rejection);
            }
        }
        match output.finish() {
            Ok(events) => Ok(events),
            Err(rejection) => {
                self.rollback_staged()?;
                Err(rejection)
            }
        }
    }

    #[doc(hidden)]
    pub fn commit_staged(&mut self) {
        let Some(_transaction) = self.transaction.take() else { return };
        match &mut self.state {
            FramerState::Candidate(candidate) => candidate.bytes.commit_transaction(),
            FramerState::Active(active) => active.body.commit_transaction(),
            FramerState::Ground => {}
        }
    }

    #[doc(hidden)]
    pub fn rollback_staged(&mut self) -> Result<(), GraphicsStorageRejection> {
        let Some(mut transaction) = self.transaction.take() else { return Ok(()) };
        self.offset = transaction.offset;
        if let Some(mut original) = transaction.owned_original.take() {
            rollback_state_buffer(&mut original, &transaction.snapshot)?;
            self.state = original;
            return Ok(());
        }
        let mut current = std::mem::replace(&mut self.state, FramerState::Ground);
        rollback_state_buffer(&mut current, &transaction.snapshot)?;
        self.state = match (transaction.snapshot, current) {
            (FramerStateSnapshot::Ground, _) => FramerState::Ground,
            (
                FramerStateSnapshot::Candidate { start, kind, .. },
                FramerState::Candidate(candidate),
            ) => FramerState::Candidate(Candidate { start, kind, bytes: candidate.bytes }),
            (
                FramerStateSnapshot::Active {
                    start,
                    kind,
                    control_bytes,
                    pending_escape,
                    kitty_payload_started,
                    kitty_payload_bytes,
                    failure,
                    ..
                },
                FramerState::Active(active),
            ) => FramerState::Active(ActiveString {
                start,
                kind,
                body: active.body,
                control_bytes,
                pending_escape,
                kitty_payload_started,
                kitty_payload_bytes,
                failure,
            }),
            _ => return Err(GraphicsStorageRejection::InternalInvariant),
        };
        Ok(())
    }

    fn begin_transaction(&mut self) -> Result<(), GraphicsStorageRejection> {
        if self.transaction.is_some() {
            return Err(GraphicsStorageRejection::InternalInvariant);
        }
        let snapshot = match &mut self.state {
            FramerState::Ground => FramerStateSnapshot::Ground,
            FramerState::Candidate(candidate) => {
                candidate.bytes.begin_transaction();
                FramerStateSnapshot::Candidate {
                    start: candidate.start,
                    kind: candidate.kind,
                    len: candidate.bytes.len(),
                }
            }
            FramerState::Active(active) => {
                active.body.begin_transaction();
                FramerStateSnapshot::Active {
                    start: active.start,
                    kind: active.kind.clone(),
                    len: active.body.len(),
                    control_bytes: active.control_bytes,
                    pending_escape: active.pending_escape,
                    kitty_payload_started: active.kitty_payload_started,
                    kitty_payload_bytes: active.kitty_payload_bytes,
                    failure: active.failure,
                }
            }
        };
        self.transaction =
            Some(FramerTransaction { offset: self.offset, snapshot, owned_original: None });
        Ok(())
    }

    fn preserve_candidate(
        &mut self,
        candidate: Candidate,
    ) -> Result<Candidate, GraphicsStorageRejection> {
        let must_preserve = self.transaction.as_ref().is_some_and(|transaction| {
            transaction.owned_original.is_none()
                && matches!(transaction.snapshot, FramerStateSnapshot::Candidate { .. })
        });
        if !must_preserve {
            return Ok(candidate);
        }
        let working = Candidate {
            start: candidate.start,
            bytes: candidate.bytes.try_clone()?,
            kind: candidate.kind,
        };
        if let Some(transaction) = self.transaction.as_mut() {
            transaction.owned_original = Some(FramerState::Candidate(candidate));
        }
        Ok(working)
    }

    fn preserve_active(
        &mut self,
        active: ActiveString,
    ) -> Result<ActiveString, GraphicsStorageRejection> {
        let must_preserve = self.transaction.as_ref().is_some_and(|transaction| {
            transaction.owned_original.is_none()
                && matches!(transaction.snapshot, FramerStateSnapshot::Active { .. })
        });
        if !must_preserve {
            return Ok(active);
        }
        let working = ActiveString {
            start: active.start,
            kind: active.kind.clone(),
            body: active.body.try_clone()?,
            control_bytes: active.control_bytes,
            pending_escape: active.pending_escape,
            kitty_payload_started: active.kitty_payload_started,
            kitty_payload_bytes: active.kitty_payload_bytes,
            failure: active.failure,
        };
        if let Some(transaction) = self.transaction.as_mut() {
            transaction.owned_original = Some(FramerState::Active(active));
        }
        Ok(working)
    }

    /// Abandon any incomplete graphics string without emitting its bytes.
    ///
    /// Reset and close destroy the terminal context an unterminated candidate
    /// or active string belonged to, so no raw text and no failure boundary is
    /// owed; dropping the retained state releases its storage exactly once.
    // @lat: [[terminal-images#Terminal Images#Incomplete Transfer Retirement]]
    pub fn discard(&mut self) {
        self.transaction = None;
        self.state = FramerState::Ground;
    }

    /// End the stream, rejecting an incomplete image string without payload.
    pub fn finish(
        &mut self,
    ) -> Result<GraphicsStorageVec<GraphicsEvent>, GraphicsStorageRejection> {
        self.begin_transaction()?;
        let mut events = match GraphicsStorageVec::new(
            Arc::clone(&self.storage_budget),
            GraphicsStorageClass::FramingEvents,
        ) {
            Ok(events) => events,
            Err(rejection) => {
                self.rollback_staged()?;
                return Err(rejection);
            }
        };
        let state = std::mem::replace(&mut self.state, FramerState::Ground);
        let result = (|| {
            match state {
                FramerState::Ground => {}
                FramerState::Candidate(candidate) => {
                    let candidate = self.preserve_candidate(candidate)?;
                    events.push(GraphicsEvent::Raw(RawBytes {
                        range: RawByteRange::new(candidate.start, self.offset),
                        payload: RawPayload::Retained(candidate.bytes.into_payload()),
                    }))?;
                }
                FramerState::Active(active) => {
                    let active = self.preserve_active(active)?;
                    let failure = Self::active_failure(
                        &active,
                        self.offset,
                        GraphicsFailureCategory::TruncatedSequence,
                    );
                    events.push(GraphicsEvent::Failure(failure))?;
                }
            }
            Ok::<(), GraphicsStorageRejection>(())
        })();
        if let Err(rejection) = result {
            self.rollback_staged()?;
            return Err(rejection);
        }
        self.commit_staged();
        Ok(events)
    }

    /// Current absolute raw-stream offset.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    #[doc(hidden)]
    #[must_use]
    pub fn validation_snapshot(&self) -> GraphicsFramerValidationSnapshot {
        let (state, start, bytes) = match &self.state {
            FramerState::Ground => (GraphicsFramerValidationState::Ground, None, None),
            FramerState::Candidate(candidate) => (
                GraphicsFramerValidationState::Candidate,
                Some(candidate.start),
                Some(&candidate.bytes),
            ),
            FramerState::Active(active) => {
                (GraphicsFramerValidationState::Active, Some(active.start), Some(&active.body))
            }
        };
        GraphicsFramerValidationSnapshot {
            state,
            offset: self.offset,
            start,
            len: bytes.map_or(0, |bytes| bytes.bytes.len()),
            capacity: bytes.map_or(0, |bytes| bytes.bytes.capacity()),
            digest: bytes.map_or(0, |bytes| {
                bytes.bytes.iter().fold(0, |digest, byte| {
                    digest.wrapping_mul(1_099_511_628_211).wrapping_add(u64::from(*byte))
                })
            }),
            requested: bytes.map_or(0, |bytes| bytes.retention.requested_bytes()),
            observed: bytes.map_or(0, |bytes| bytes.retention.observed_bytes()),
            transaction_active: self.transaction.is_some(),
        }
    }

    /// Describe an active graphics string without exposing its payload.
    #[must_use]
    pub fn pending_transfer(&self) -> Option<PendingGraphicsTransfer> {
        let FramerState::Active(active) = &self.state else {
            return None;
        };
        let retained_payload_bytes = match active.kind {
            ActiveKind::Kitty | ActiveKind::UnsupportedKittyC1 => active.kitty_payload_bytes,
            ActiveKind::Sixel { .. } => active.body.len(),
        };
        Some(PendingGraphicsTransfer {
            range: RawByteRange::new(active.start, self.offset),
            protocol: active.kind.protocol(),
            retained_payload_bytes,
            discarding: active.failure.is_some(),
        })
    }

    fn process_byte(
        &mut self,
        position: u64,
        byte: u8,
        output: &mut EventOutput,
    ) -> Result<(), GraphicsStorageRejection> {
        let state = std::mem::replace(&mut self.state, FramerState::Ground);
        self.state = match state {
            FramerState::Ground => self.process_ground(position, byte, output)?,
            FramerState::Candidate(candidate) => self.process_candidate(candidate, byte, output)?,
            FramerState::Active(active) => self.process_active(active, position, byte, output)?,
        };
        Ok(())
    }

    fn process_ground(
        &self,
        position: u64,
        byte: u8,
        output: &mut EventOutput,
    ) -> Result<FramerState, GraphicsStorageRejection> {
        let (kind, special) = match byte {
            ESC => (CandidateKind::Escape, true),
            C1_DCS => (
                CandidateKind::DcsHeader {
                    form: StringForm::C1,
                    scanner: SixelHeaderScanner::default(),
                },
                true,
            ),
            C1_CSI => (CandidateKind::Csi { form: StringForm::C1 }, true),
            C1_APC => (CandidateKind::C1ApcPrefix, true),
            _ => (CandidateKind::Escape, false),
        };
        if special {
            let bytes = RetainedVec::from_slice(
                Arc::clone(&self.storage_budget),
                GraphicsStorageClass::FramingCandidate,
                &[byte],
            )?;
            Ok(FramerState::Candidate(Candidate { start: position, bytes, kind }))
        } else {
            output.raw_byte(position, byte)?;
            Ok(FramerState::Ground)
        }
    }

    fn process_candidate(
        &mut self,
        mut candidate: Candidate,
        byte: u8,
        output: &mut EventOutput,
    ) -> Result<FramerState, GraphicsStorageRejection> {
        let position = candidate.start.saturating_add(candidate.bytes.len() as u64);
        match candidate.kind {
            CandidateKind::Escape => match byte {
                b'_' => {
                    if let Err(rejection) = candidate.bytes.push(byte) {
                        self.state = FramerState::Candidate(candidate);
                        return Err(rejection);
                    }
                    candidate.kind = CandidateKind::ApcPrefix;
                    Ok(FramerState::Candidate(candidate))
                }
                b'P' => {
                    if let Err(rejection) = candidate.bytes.push(byte) {
                        self.state = FramerState::Candidate(candidate);
                        return Err(rejection);
                    }
                    candidate.kind = CandidateKind::DcsHeader {
                        form: StringForm::SevenBit,
                        scanner: SixelHeaderScanner::default(),
                    };
                    Ok(FramerState::Candidate(candidate))
                }
                b'[' => {
                    if let Err(rejection) = candidate.bytes.push(byte) {
                        self.state = FramerState::Candidate(candidate);
                        return Err(rejection);
                    }
                    candidate.kind = CandidateKind::Csi { form: StringForm::SevenBit };
                    Ok(FramerState::Candidate(candidate))
                }
                _ => self.abandon_candidate(candidate, position, byte, output),
            },
            CandidateKind::ApcPrefix => {
                if byte == b'G' {
                    let start = candidate.start;
                    let _working = self.preserve_candidate(candidate)?;
                    self.start_active(start, ActiveKind::Kitty, 1, None)
                } else {
                    self.abandon_candidate(candidate, position, byte, output)
                }
            }
            CandidateKind::C1ApcPrefix => {
                if byte == b'G' {
                    let start = candidate.start;
                    let _working = self.preserve_candidate(candidate)?;
                    self.start_active(start, ActiveKind::UnsupportedKittyC1, 1, None)
                } else {
                    self.abandon_candidate(candidate, position, byte, output)
                }
            }
            CandidateKind::DcsHeader { .. } => self.process_dcs_candidate(candidate, byte, output),
            CandidateKind::Csi { .. } => self.process_csi(candidate, position, byte, output),
        }
    }

    fn abandon_candidate(
        &mut self,
        candidate: Candidate,
        position: u64,
        byte: u8,
        output: &mut EventOutput,
    ) -> Result<FramerState, GraphicsStorageRejection> {
        let candidate = self.preserve_candidate(candidate)?;
        output.raw(candidate.start, candidate.bytes.into_payload())?;
        self.process_ground(position, byte, output)
    }

    fn process_dcs_candidate(
        &mut self,
        mut candidate: Candidate,
        byte: u8,
        output: &mut EventOutput,
    ) -> Result<FramerState, GraphicsStorageRejection> {
        let CandidateKind::DcsHeader { form, mut scanner } = candidate.kind else {
            return Ok(FramerState::Candidate(candidate));
        };
        let position = candidate.start.saturating_add(candidate.bytes.len() as u64);
        let introducer_len = if form == StringForm::SevenBit { 2 } else { 1 };
        let held = candidate.bytes.len().saturating_sub(introducer_len);
        if byte.is_ascii_digit() || byte == b';' {
            if held >= self.max_control_string_bytes {
                scanner.scan(byte);
                let start = candidate.start;
                let _working = self.preserve_candidate(candidate)?;
                return self.start_active(
                    start,
                    ActiveKind::Sixel { form, parameters: scanner.finish() },
                    held.saturating_add(1),
                    Some((
                        GraphicsFailureCategory::QuotaExceeded,
                        Some(GraphicsLimit::ControlString),
                    )),
                );
            }
            scanner.scan(byte);
            if let Err(rejection) = candidate.bytes.push(byte) {
                self.state = FramerState::Candidate(candidate);
                return Err(rejection);
            }
            candidate.kind = CandidateKind::DcsHeader { form, scanner };
            return Ok(FramerState::Candidate(candidate));
        }
        if byte != b'q' {
            return self.abandon_candidate(candidate, position, byte, output);
        }

        let parameters = scanner.finish();
        let initial_failure = parameters.as_ref().err().copied().map(|category| (category, None));
        let start = candidate.start;
        let _working = self.preserve_candidate(candidate)?;
        self.start_active(
            start,
            ActiveKind::Sixel { form, parameters },
            held.saturating_add(1),
            initial_failure,
        )
    }

    fn start_active(
        &self,
        start: u64,
        kind: ActiveKind,
        control_bytes: usize,
        initial_failure: Option<(GraphicsFailureCategory, Option<GraphicsLimit>)>,
    ) -> Result<FramerState, GraphicsStorageRejection> {
        let failure = initial_failure.or_else(|| {
            (control_bytes > self.max_control_string_bytes).then_some((
                GraphicsFailureCategory::QuotaExceeded,
                Some(GraphicsLimit::ControlString),
            ))
        });
        Ok(FramerState::Active(ActiveString {
            start,
            kind,
            body: RetainedVec::empty(
                Arc::clone(&self.storage_budget),
                GraphicsStorageClass::FramingActive,
            )?,
            control_bytes,
            pending_escape: false,
            kitty_payload_started: false,
            kitty_payload_bytes: 0,
            failure,
        }))
    }

    fn process_csi(
        &mut self,
        mut candidate: Candidate,
        position: u64,
        byte: u8,
        output: &mut EventOutput,
    ) -> Result<FramerState, GraphicsStorageRejection> {
        let CandidateKind::Csi { form } = candidate.kind else {
            return Ok(FramerState::Candidate(candidate));
        };
        if let Err(rejection) = candidate.bytes.push(byte) {
            self.state = FramerState::Candidate(candidate);
            return Err(rejection);
        }
        let introducer_len = if form == StringForm::SevenBit { 2 } else { 1 };
        let sequence = candidate.bytes.get(introducer_len..).unwrap_or_default();
        if !(0x40..=0x7e).contains(&byte) && is_sixel_mode_prefix(sequence) {
            return Ok(FramerState::Candidate(candidate));
        }
        if !(0x40..=0x7e).contains(&byte) && is_ground_control(byte) {
            let current = candidate.bytes.pop();
            debug_assert_eq!(current, Some(byte));
            return self.abandon_candidate(candidate, position, byte, output);
        }
        let candidate = self.preserve_candidate(candidate)?;
        let range = RawByteRange::new(
            candidate.start,
            candidate.start.saturating_add(candidate.bytes.len() as u64),
        );
        let payload = candidate.bytes.into_payload();
        if let Some((mode, enabled)) =
            parse_sixel_mode_bytes(payload.as_slice().get(introducer_len..).unwrap_or_default())
        {
            let raw = RawBytes { range, payload: RawPayload::Retained(payload) };
            output.event(GraphicsEvent::SixelMode(SixelModeChange { raw, mode, enabled }))?;
        } else {
            output.raw(range.start, payload)?;
        }
        Ok(FramerState::Ground)
    }

    fn process_active(
        &mut self,
        mut active: ActiveString,
        position: u64,
        byte: u8,
        output: &mut EventOutput,
    ) -> Result<FramerState, GraphicsStorageRejection> {
        if byte == CAN || byte == SUB {
            let active = self.preserve_active(active)?;
            let failure = Self::active_failure(
                &active,
                position.saturating_add(1),
                GraphicsFailureCategory::MalformedFraming,
            );
            output.event(GraphicsEvent::Failure(failure))?;
            return Ok(FramerState::Ground);
        }

        if active.pending_escape {
            active.pending_escape = false;
            if byte == b'\\' {
                return self.finish_string(
                    active,
                    position.saturating_add(1),
                    StringForm::SevenBit,
                    output,
                );
            }
            if let Err(rejection) = self.charge_and_append(&mut active, ESC) {
                self.state = FramerState::Active(active);
                return Err(rejection);
            }
            if active.failure.is_none() {
                active.failure = Some((GraphicsFailureCategory::MalformedFraming, None));
            }
            if byte == ESC {
                active.pending_escape = true;
                return Ok(FramerState::Active(active));
            }
            if byte == C1_ST {
                return self.finish_string(
                    active,
                    position.saturating_add(1),
                    StringForm::C1,
                    output,
                );
            }
            if let Err(rejection) = self.charge_and_append(&mut active, byte) {
                self.state = FramerState::Active(active);
                return Err(rejection);
            }
            return Ok(FramerState::Active(active));
        }

        if byte == ESC {
            active.pending_escape = true;
            return Ok(FramerState::Active(active));
        }
        if byte == C1_ST {
            return self.finish_string(active, position.saturating_add(1), StringForm::C1, output);
        }

        if let Err(rejection) = self.charge_and_append(&mut active, byte) {
            self.state = FramerState::Active(active);
            return Err(rejection);
        }
        Ok(FramerState::Active(active))
    }

    fn charge_and_append(
        &self,
        active: &mut ActiveString,
        byte: u8,
    ) -> Result<(), GraphicsStorageRejection> {
        active.control_bytes = active.control_bytes.saturating_add(1);
        if active.control_bytes > self.max_control_string_bytes {
            if active.failure.is_none() {
                active.failure = Some((
                    GraphicsFailureCategory::QuotaExceeded,
                    Some(GraphicsLimit::ControlString),
                ));
            }
            return Ok(());
        }
        if matches!(&active.kind, ActiveKind::Kitty)
            && active.failure.is_none()
            && charge_kitty_payload(active, byte)
        {
            active.failure = Some((
                GraphicsFailureCategory::QuotaExceeded,
                Some(GraphicsLimit::KittyChunkPayload),
            ));
            return Ok(());
        }
        if active.failure.is_none() {
            active.body.push(byte)?;
        }
        Ok(())
    }

    fn finish_string(
        &mut self,
        active: ActiveString,
        end: u64,
        terminator_form: StringForm,
        output: &mut EventOutput,
    ) -> Result<FramerState, GraphicsStorageRejection> {
        let active = self.preserve_active(active)?;
        let range = RawByteRange::new(active.start, end);
        let protocol = active.kind.protocol();
        if active.failure.is_some() {
            output.event(GraphicsEvent::Failure(Self::active_failure(
                &active,
                end,
                GraphicsFailureCategory::MalformedFraming,
            )))?;
            return Ok(FramerState::Ground);
        }
        if active.kind.form() != terminator_form {
            output.event(GraphicsEvent::Failure(GraphicsFailure::new(
                range,
                protocol,
                GraphicsFailureCategory::MalformedFraming,
            )))?;
            return Ok(FramerState::Ground);
        }

        match active.kind {
            ActiveKind::Kitty => {
                let body = active.body.into_payload();
                match parse_kitty(body) {
                    Ok(command) => output.event(GraphicsEvent::Kitty { range, command })?,
                    Err((category, limit)) => {
                        output.event(GraphicsEvent::Failure(GraphicsFailure {
                            range,
                            protocol,
                            category,
                            limit,
                        }))?;
                    }
                }
            }
            ActiveKind::UnsupportedKittyC1 => {
                output.event(GraphicsEvent::Failure(GraphicsFailure::new(
                    range,
                    protocol,
                    GraphicsFailureCategory::UnsupportedProtocol,
                )))?;
            }
            ActiveKind::Sixel { parameters, .. } => match parameters {
                Err(category) => output.event(GraphicsEvent::Failure(GraphicsFailure::new(
                    range, protocol, category,
                )))?,
                Ok(parameters) => match validate_sixel_payload(active.body.as_slice()) {
                    Ok(()) => {
                        let payload = active.body.into_payload();
                        output.event(GraphicsEvent::Sixel {
                            range,
                            command: SixelCommand { parameters, payload },
                        })?;
                    }
                    Err(category) => output.event(GraphicsEvent::Failure(GraphicsFailure::new(
                        range, protocol, category,
                    )))?,
                },
            },
        }
        Ok(FramerState::Ground)
    }

    fn active_failure(
        active: &ActiveString,
        end: u64,
        fallback: GraphicsFailureCategory,
    ) -> GraphicsFailure {
        let (category, limit) = active.failure.unwrap_or((fallback, None));
        GraphicsFailure {
            range: RawByteRange::new(active.start, end),
            protocol: active.kind.protocol(),
            category,
            limit,
        }
    }
}

fn charge_kitty_payload(active: &mut ActiveString, byte: u8) -> bool {
    if !active.kitty_payload_started {
        active.kitty_payload_started = byte == b';';
        return false;
    }
    active.kitty_payload_bytes = active.kitty_payload_bytes.saturating_add(1);
    active.kitty_payload_bytes > MAX_KITTY_CHUNK_PAYLOAD_BYTES
}

struct EventOutput {
    events: GraphicsStorageVec<GraphicsEvent>,
    raw_start: Option<u64>,
    raw: [u8; INLINE_RAW_BYTES],
    raw_len: u8,
}

impl EventOutput {
    fn new(budget: Arc<GraphicsStorageBudget>) -> Result<Self, GraphicsStorageRejection> {
        let events = GraphicsStorageVec::new(budget, GraphicsStorageClass::FramingEvents)?;
        Ok(Self { events, raw_start: None, raw: [0; INLINE_RAW_BYTES], raw_len: 0 })
    }

    fn raw_byte(&mut self, position: u64, byte: u8) -> Result<(), GraphicsStorageRejection> {
        if usize::from(self.raw_len) == self.raw.len() {
            self.flush_raw()?;
        }
        if self.raw_start.is_none() {
            self.raw_start = Some(position);
        }
        if let Some(slot) = self.raw.get_mut(usize::from(self.raw_len)) {
            *slot = byte;
            self.raw_len = self.raw_len.saturating_add(1);
        }
        Ok(())
    }

    fn raw(
        &mut self,
        start: u64,
        payload: GraphicsPayload,
    ) -> Result<(), GraphicsStorageRejection> {
        self.flush_raw()?;
        let end = start.saturating_add(payload.len() as u64);
        self.events.push(GraphicsEvent::Raw(RawBytes {
            range: RawByteRange::new(start, end),
            payload: RawPayload::Retained(payload),
        }))
    }

    fn event(&mut self, event: GraphicsEvent) -> Result<(), GraphicsStorageRejection> {
        self.flush_raw()?;
        self.events.push(event)
    }

    fn flush_raw(&mut self) -> Result<(), GraphicsStorageRejection> {
        let Some(start) = self.raw_start.take() else { return Ok(()) };
        let len = self.raw_len;
        self.raw_len = 0;
        let bytes = std::mem::replace(&mut self.raw, [0; INLINE_RAW_BYTES]);
        self.events.push(GraphicsEvent::Raw(RawBytes {
            range: RawByteRange::new(start, start.saturating_add(u64::from(len))),
            payload: RawPayload::Inline { bytes, len },
        }))
    }

    fn finish(mut self) -> Result<GraphicsStorageVec<GraphicsEvent>, GraphicsStorageRejection> {
        self.flush_raw()?;
        Ok(self.events)
    }
}

fn is_sixel_mode_prefix(bytes: &[u8]) -> bool {
    [b"?80h".as_slice(), b"?80l", b"?8452h", b"?8452l"]
        .iter()
        .any(|candidate| candidate.starts_with(bytes))
}

fn is_ground_control(byte: u8) -> bool {
    matches!(byte, ESC | C1_DCS | C1_CSI | C1_APC)
}

fn parse_sixel_mode_bytes(bytes: &[u8]) -> Option<(SixelMode, bool)> {
    match bytes {
        b"?80h" => Some((SixelMode::Display, true)),
        b"?80l" => Some((SixelMode::Display, false)),
        b"?8452h" => Some((SixelMode::CursorRight, true)),
        b"?8452l" => Some((SixelMode::CursorRight, false)),
        _ => None,
    }
}

fn parse_kitty(
    mut payload_owner: GraphicsPayload,
) -> Result<KittyCommand, (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    let bytes = payload_owner.as_slice();
    let separator = bytes.iter().position(|byte| *byte == b';');
    let payload_start = separator.map_or(bytes.len(), |index| index.saturating_add(1));
    let controls_end = separator.unwrap_or(bytes.len());
    let controls = bytes.get(..controls_end).unwrap_or_default();
    let payload = bytes.get(payload_start..).unwrap_or_default();
    if controls.is_empty() {
        return Err((GraphicsFailureCategory::MalformedControl, None));
    }
    if payload.len() > MAX_KITTY_CHUNK_PAYLOAD_BYTES {
        return Err((
            GraphicsFailureCategory::QuotaExceeded,
            Some(GraphicsLimit::KittyChunkPayload),
        ));
    }
    if !payload
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'+' | b'/' | b'='))
    {
        return Err((GraphicsFailureCategory::MalformedPayload, None));
    }

    let mut command = KittyCommand {
        action: KittyAction::Transmit,
        format: Some(KittyFormat::Rgba),
        image_id: None,
        placement_id: None,
        width: None,
        height: None,
        source_x: None,
        source_y: None,
        source_width: None,
        source_height: None,
        columns: None,
        rows: None,
        pixel_x: None,
        pixel_y: None,
        z_index: None,
        move_cursor: None,
        placement_mode: KittyPlacementMode::Classic,
        chunk_state: KittyChunkState::Final,
        quiet: 0,
        compression: KittyCompression::None,
        delete: None,
        control_mask: [0; 4],
        payload: None,
    };
    let mut seen = [false; 256];

    for pair in controls.split(|byte| *byte == b',') {
        let Some((key_bytes, value)) = split_at_byte(pair, b'=') else {
            return Err((GraphicsFailureCategory::MalformedControl, None));
        };
        let Some(&key) = key_bytes.first().filter(|_| key_bytes.len() == 1) else {
            return Err((GraphicsFailureCategory::MalformedControl, None));
        };
        let Some(was_seen) = seen.get_mut(usize::from(key)) else {
            return Err((GraphicsFailureCategory::MalformedControl, None));
        };
        if *was_seen || value.is_empty() {
            return Err((GraphicsFailureCategory::MalformedControl, None));
        }
        *was_seen = true;
        let word = usize::from(key) / 64;
        let bit = u32::from(key % 64);
        if let Some(mask) = command.control_mask.get_mut(word) {
            *mask |= 1_u64 << bit;
        }
        apply_kitty_control(&mut command, key, value)?;
    }

    if payload_start > 0 && payload_start <= payload_owner.bytes.len() {
        payload_owner.bytes.drain(..payload_start);
    } else {
        payload_owner.bytes.clear();
    }
    command.payload = Some(payload_owner);

    if command.chunk_state == KittyChunkState::More && !command.payload().len().is_multiple_of(4) {
        return Err((GraphicsFailureCategory::MalformedPayload, None));
    }
    Ok(command)
}

fn apply_kitty_control(
    command: &mut KittyCommand,
    key: u8,
    value: &[u8],
) -> Result<(), (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    match key {
        b'a' => command.action = parse_kitty_action(value)?,
        b't' if value == b"d" => {}
        b't' => return Err((GraphicsFailureCategory::UnsupportedTransport, None)),
        b'f' => command.format = Some(parse_kitty_format(value)?),
        b'o' if value == b"z" => command.compression = KittyCompression::Zlib,
        b'o' => return Err((GraphicsFailureCategory::UnsupportedAction, None)),
        b'i' => command.image_id = Some(parse_nonzero_u32(value)?),
        b'p' => command.placement_id = Some(parse_nonzero_u32(value)?),
        b's' => command.width = Some(parse_u32(value)?),
        b'v' => command.height = Some(parse_u32(value)?),
        b'x' => command.source_x = Some(parse_u32(value)?),
        b'y' => command.source_y = Some(parse_u32(value)?),
        b'w' => command.source_width = Some(parse_u32(value)?),
        b'h' => command.source_height = Some(parse_u32(value)?),
        b'c' => command.columns = Some(parse_u32(value)?),
        b'r' => command.rows = Some(parse_u32(value)?),
        b'X' => command.pixel_x = Some(parse_u32(value)?),
        b'Y' => command.pixel_y = Some(parse_u32(value)?),
        b'z' => command.z_index = Some(parse_i32(value)?),
        b'C' => command.move_cursor = Some(parse_bool(value)?),
        b'U' => {
            command.placement_mode = if parse_bool(value)? {
                KittyPlacementMode::UnicodePlaceholder
            } else {
                KittyPlacementMode::Classic
            };
        }
        b'm' => {
            command.chunk_state =
                if parse_bool(value)? { KittyChunkState::More } else { KittyChunkState::Final };
        }
        b'q' => command.quiet = parse_quiet(value)?,
        b'd' => command.delete = Some(parse_delete(value)?),
        b'I' | b'N' | b'P' | b'Q' => {
            return Err((GraphicsFailureCategory::UnsupportedAction, None));
        }
        _ => return Err((GraphicsFailureCategory::MalformedControl, None)),
    }
    Ok(())
}

fn parse_kitty_action(
    value: &[u8],
) -> Result<KittyAction, (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    match value {
        b"t" => Ok(KittyAction::Transmit),
        b"T" => Ok(KittyAction::TransmitDisplay),
        b"p" => Ok(KittyAction::Put),
        b"q" => Ok(KittyAction::Query),
        b"d" => Ok(KittyAction::Delete),
        _ => Err((GraphicsFailureCategory::UnsupportedAction, None)),
    }
}

fn parse_kitty_format(
    value: &[u8],
) -> Result<KittyFormat, (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    match value {
        b"24" => Ok(KittyFormat::Rgb),
        b"32" => Ok(KittyFormat::Rgba),
        b"100" => Ok(KittyFormat::Png),
        _ => Err((GraphicsFailureCategory::UnsupportedAction, None)),
    }
}

fn parse_delete(
    value: &[u8],
) -> Result<KittyDelete, (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    let Some(&byte) = value.first().filter(|_| value.len() == 1) else {
        return Err((GraphicsFailureCategory::MalformedControl, None));
    };
    let selector = char::from(byte);
    if !matches!(
        byte,
        b'a' | b'A' | b'i' | b'I' | b'p' | b'P' | b'x' | b'X' | b'y' | b'Y' | b'z' | b'Z'
    ) {
        return Err((GraphicsFailureCategory::UnsupportedAction, None));
    }
    Ok(KittyDelete { selector, free_data: byte.is_ascii_uppercase() })
}

fn parse_bool(value: &[u8]) -> Result<bool, (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    match value {
        b"0" => Ok(false),
        b"1" => Ok(true),
        _ => Err((GraphicsFailureCategory::MalformedControl, None)),
    }
}

fn parse_quiet(value: &[u8]) -> Result<u8, (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    match value {
        b"0" => Ok(0),
        b"1" => Ok(1),
        b"2" => Ok(2),
        _ => Err((GraphicsFailureCategory::MalformedControl, None)),
    }
}

fn parse_nonzero_u32(
    value: &[u8],
) -> Result<u32, (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    let parsed = parse_u32(value)?;
    if parsed == 0 {
        return Err((GraphicsFailureCategory::MalformedControl, None));
    }
    Ok(parsed)
}

fn parse_u32(value: &[u8]) -> Result<u32, (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    parse_ascii_unsigned(value)
        .and_then(|number| u32::try_from(number).ok())
        .ok_or((GraphicsFailureCategory::MalformedControl, None))
}

fn parse_i32(bytes: &[u8]) -> Result<i32, (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    let (negative, magnitude) =
        bytes.strip_prefix(b"-").map_or((false, bytes), |remaining| (true, remaining));
    let number =
        parse_ascii_unsigned(magnitude).ok_or((GraphicsFailureCategory::MalformedControl, None))?;
    let signed = i64::try_from(number)
        .ok()
        .and_then(|signed| if negative { signed.checked_neg() } else { Some(signed) })
        .and_then(|signed| i32::try_from(signed).ok())
        .ok_or((GraphicsFailureCategory::MalformedControl, None))?;
    Ok(signed)
}

fn parse_ascii_unsigned(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    value.iter().try_fold(0_u64, |number, byte| {
        number.checked_mul(10)?.checked_add(u64::from(byte.saturating_sub(b'0')))
    })
}

fn split_at_byte(bytes: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    let index = bytes.iter().position(|byte| *byte == separator)?;
    let before = bytes.get(..index)?;
    let after = bytes.get(index.saturating_add(1)..)?;
    Some((before, after))
}

fn validate_sixel_payload(bytes: &[u8]) -> Result<(), GraphicsFailureCategory> {
    let mut cursor = 0;
    while let Some(&byte) = bytes.get(cursor) {
        match byte {
            b'?'..=b'~' | b'$' | b'-' => cursor = cursor.saturating_add(1),
            b'!' => {
                let (repeat, next) = parse_decimal_at(bytes, cursor.saturating_add(1))?;
                if repeat == 0
                    || !bytes.get(next).is_some_and(|next_byte| (b'?'..=b'~').contains(next_byte))
                {
                    return Err(GraphicsFailureCategory::MalformedPayload);
                }
                cursor = next.saturating_add(1);
            }
            b'"' => {
                let (_, next) = parse_four_numeric_fields(bytes, cursor.saturating_add(1))?;
                cursor = next;
            }
            b'#' => {
                let (palette, next) = parse_decimal_at(bytes, cursor.saturating_add(1))?;
                if palette > 255 {
                    return Err(GraphicsFailureCategory::MalformedPayload);
                }
                if bytes.get(next) == Some(&b';') {
                    let (fields, after) = parse_four_numeric_fields(bytes, next.saturating_add(1))?;
                    validate_palette_definition(&fields)?;
                    cursor = after;
                } else {
                    cursor = next;
                }
            }
            _ => return Err(GraphicsFailureCategory::MalformedPayload),
        }
    }
    Ok(())
}

fn parse_decimal_at(bytes: &[u8], start: usize) -> Result<(u32, usize), GraphicsFailureCategory> {
    let mut cursor = start;
    let mut value = 0_u32;
    let mut found = false;
    while let Some(&byte) = bytes.get(cursor).filter(|byte| byte.is_ascii_digit()) {
        found = true;
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte.saturating_sub(b'0'))))
            .ok_or(GraphicsFailureCategory::MalformedPayload)?;
        cursor = cursor.saturating_add(1);
    }
    if !found {
        return Err(GraphicsFailureCategory::MalformedPayload);
    }
    Ok((value, cursor))
}

fn parse_four_numeric_fields(
    bytes: &[u8],
    start: usize,
) -> Result<([u32; 4], usize), GraphicsFailureCategory> {
    let mut values = [0_u32; 4];
    let mut cursor = start;
    for (field, slot) in values.iter_mut().enumerate() {
        let (value, next) = parse_decimal_at(bytes, cursor)?;
        *slot = value;
        cursor = next;
        if field < 3 {
            if bytes.get(cursor) != Some(&b';') {
                return Err(GraphicsFailureCategory::MalformedPayload);
            }
            cursor = cursor.saturating_add(1);
        }
    }
    Ok((values, cursor))
}

fn validate_palette_definition(values: &[u32]) -> Result<(), GraphicsFailureCategory> {
    let Some((&mode, components)) = values.split_first() else {
        return Err(GraphicsFailureCategory::MalformedPayload);
    };
    let valid = match mode {
        1 => {
            components.first().is_some_and(|hue| *hue <= 360)
                && components.get(1..).is_some_and(|tail| tail.iter().all(|value| *value <= 100))
        }
        2 => components.iter().all(|value| *value <= 100),
        _ => false,
    };
    if valid { Ok(()) } else { Err(GraphicsFailureCategory::MalformedPayload) }
}

#[cfg(test)]
mod tests {
    use super::*;

    use scribe_image_decode::{StorageProcess, StorageValidation};

    #[test]
    fn sixel_scanner_marks_fourth_field_at_separator() {
        let mut scanner = SixelHeaderScanner::default();
        for &byte in b"0;0;0" {
            scanner.scan(byte);
            assert!(!scanner.malformed);
        }

        scanner.scan(b';');

        assert!(scanner.malformed);
        assert_eq!(scanner.values, [Some(0), Some(0), Some(0)]);
    }

    #[test]
    fn sixel_scanner_marks_u16_overflow_at_digit() {
        let mut scanner = SixelHeaderScanner::default();
        for &byte in b"6553" {
            scanner.scan(byte);
            assert!(!scanner.malformed);
        }

        scanner.scan(b'6');

        assert!(scanner.malformed);
        assert_eq!(scanner.current, Some(6_553));
    }

    #[test]
    fn malformed_sixel_header_discards_body_while_recovering() {
        let budget = GraphicsStorageBudget::new(
            StorageProcess::new(u64::MAX),
            u64::MAX,
            0,
            StorageValidation::default(),
        );
        let mut framer = GraphicsFramer::with_storage_budget(16, budget);
        assert!(framer.push(b"\x1bP0;0;0;0q").expect("header output allocation").is_empty());
        assert!(
            framer
                .push(b"~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~")
                .expect("payload output allocation")
                .is_empty()
        );

        let FramerState::Active(active) = &framer.state else {
            panic!("malformed Sixel must remain active until its terminator");
        };
        assert!(matches!(active.failure, Some((GraphicsFailureCategory::MalformedControl, None))));
        assert!(active.body.is_empty());
        assert_eq!(active.control_bytes, 40);

        let events = framer.push(b"\x1b\\").expect("terminator output allocation");
        assert!(matches!(
            events.as_slice(),
            [GraphicsEvent::Failure(failure)]
                if failure.category == GraphicsFailureCategory::MalformedControl
                    && failure.limit.is_none()
        ));
    }
}
