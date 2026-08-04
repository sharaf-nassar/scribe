//! Bounded streaming framing for terminal-image control strings.
//!
//! PTY reads have arbitrary boundaries. This module recognizes only the
//! frozen terminal-images-v1 subset, consumes image strings, and returns all
//! unrelated bytes exactly once with absolute half-open stream ranges.

use std::collections::HashSet;

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

/// Bytes that must continue to the ordinary terminal parser exactly once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBytes {
    pub range: RawByteRange,
    pub bytes: Vec<u8>,
}

/// Supported image protocols at the framing boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsProtocol {
    Kitty,
    Sixel,
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
    ControlStringBytes,
    KittyChunkPayloadBytes,
}

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
#[derive(Clone, Debug, PartialEq, Eq)]
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
    pub payload: Vec<u8>,
}

/// Parsed Sixel introducer parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SixelParameters {
    pub aspect: Option<u16>,
    pub background: Option<u16>,
    pub horizontal_grid: Option<u16>,
}

/// Narrow validated Sixel command. Payload remains encoded for the decoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SixelCommand {
    pub parameters: SixelParameters,
    pub payload: Vec<u8>,
}

/// Xterm private modes relevant to Sixel chronology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SixelMode {
    Display,
    CursorRight,
}

/// Parsed private mode transition. Its `raw` bytes still go to Alacritty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SixelModeChange {
    pub raw: RawBytes,
    pub mode: SixelMode,
    pub enabled: bool,
}

/// One ordered result from [`GraphicsFramer`].
#[derive(Clone, Debug, PartialEq, Eq)]
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
            Self::Raw(raw) => Some(&raw.bytes),
            Self::SixelMode(change) => Some(&change.raw.bytes),
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

#[derive(Debug)]
struct Candidate {
    start: u64,
    bytes: Vec<u8>,
    kind: CandidateKind,
}

#[derive(Debug)]
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
    body: Vec<u8>,
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

/// Incremental bounded APC/DCS framer over arbitrary PTY byte chunks.
// @lat: [[terminal-images#Terminal Images#Bounded Framing and Parsing]]
pub struct GraphicsFramer {
    state: FramerState,
    offset: u64,
    max_control_string_bytes: usize,
}

impl GraphicsFramer {
    /// Construct the production v1 framer.
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_control_string_bytes(MAX_CONTROL_STRING_BYTES)
    }

    /// Construct with a smaller ceiling for deterministic boundary validation.
    #[must_use]
    pub fn with_max_control_string_bytes(max_control_string_bytes: usize) -> Self {
        Self { state: FramerState::Ground, offset: 0, max_control_string_bytes }
    }

    /// Feed one arbitrary PTY read and return ordered complete events.
    #[must_use]
    pub fn push(&mut self, input: &[u8]) -> Vec<GraphicsEvent> {
        let mut output = EventOutput::default();
        for &byte in input {
            let position = self.offset;
            self.offset = self.offset.saturating_add(1);
            self.process_byte(position, byte, &mut output);
        }
        output.finish()
    }

    /// End the stream, rejecting an incomplete image string without payload.
    #[must_use]
    pub fn finish(&mut self) -> Vec<GraphicsEvent> {
        let state = std::mem::replace(&mut self.state, FramerState::Ground);
        match state {
            FramerState::Ground => Vec::new(),
            FramerState::Candidate(candidate) => vec![GraphicsEvent::Raw(RawBytes {
                range: RawByteRange::new(candidate.start, self.offset),
                bytes: candidate.bytes,
            })],
            FramerState::Active(active) => {
                let failure = Self::active_failure(
                    &active,
                    self.offset,
                    GraphicsFailureCategory::TruncatedSequence,
                );
                vec![GraphicsEvent::Failure(failure)]
            }
        }
    }

    /// Current absolute raw-stream offset.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    fn process_byte(&mut self, position: u64, byte: u8, output: &mut EventOutput) {
        let state = std::mem::replace(&mut self.state, FramerState::Ground);
        self.state = match state {
            FramerState::Ground => Self::process_ground(position, byte, output),
            FramerState::Candidate(candidate) => self.process_candidate(candidate, byte, output),
            FramerState::Active(active) => self.process_active(active, position, byte, output),
        };
    }

    fn process_ground(position: u64, byte: u8, output: &mut EventOutput) -> FramerState {
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
            FramerState::Candidate(Candidate { start: position, bytes: vec![byte], kind })
        } else {
            output.raw_byte(position, byte);
            FramerState::Ground
        }
    }

    fn process_candidate(
        &self,
        mut candidate: Candidate,
        byte: u8,
        output: &mut EventOutput,
    ) -> FramerState {
        let position = candidate.start.saturating_add(candidate.bytes.len() as u64);
        match candidate.kind {
            CandidateKind::Escape => match byte {
                b'_' => {
                    candidate.bytes.push(byte);
                    candidate.kind = CandidateKind::ApcPrefix;
                    FramerState::Candidate(candidate)
                }
                b'P' => {
                    candidate.bytes.push(byte);
                    candidate.kind = CandidateKind::DcsHeader {
                        form: StringForm::SevenBit,
                        scanner: SixelHeaderScanner::default(),
                    };
                    FramerState::Candidate(candidate)
                }
                b'[' => {
                    candidate.bytes.push(byte);
                    candidate.kind = CandidateKind::Csi { form: StringForm::SevenBit };
                    FramerState::Candidate(candidate)
                }
                _ => Self::abandon_candidate(candidate, position, byte, output),
            },
            CandidateKind::ApcPrefix => {
                if byte == b'G' {
                    self.start_active(candidate.start, ActiveKind::Kitty, 1, None)
                } else {
                    Self::abandon_candidate(candidate, position, byte, output)
                }
            }
            CandidateKind::C1ApcPrefix => {
                if byte == b'G' {
                    self.start_active(candidate.start, ActiveKind::UnsupportedKittyC1, 1, None)
                } else {
                    Self::abandon_candidate(candidate, position, byte, output)
                }
            }
            CandidateKind::DcsHeader { .. } => self.process_dcs_candidate(candidate, byte, output),
            CandidateKind::Csi { form } => {
                Self::process_csi(candidate, form, position, byte, output)
            }
        }
    }

    fn abandon_candidate(
        candidate: Candidate,
        position: u64,
        byte: u8,
        output: &mut EventOutput,
    ) -> FramerState {
        output.raw(candidate.start, candidate.bytes);
        Self::process_ground(position, byte, output)
    }

    fn process_dcs_candidate(
        &self,
        mut candidate: Candidate,
        byte: u8,
        output: &mut EventOutput,
    ) -> FramerState {
        let CandidateKind::DcsHeader { form, mut scanner } = candidate.kind else {
            return FramerState::Candidate(candidate);
        };
        let position = candidate.start.saturating_add(candidate.bytes.len() as u64);
        let introducer_len = if form == StringForm::SevenBit { 2 } else { 1 };
        let held = candidate.bytes.len().saturating_sub(introducer_len);
        if byte.is_ascii_digit() || byte == b';' {
            if held >= self.max_control_string_bytes {
                scanner.scan(byte);
                return self.start_active(
                    candidate.start,
                    ActiveKind::Sixel { form, parameters: scanner.finish() },
                    held.saturating_add(1),
                    Some((
                        GraphicsFailureCategory::QuotaExceeded,
                        Some(GraphicsLimit::ControlStringBytes),
                    )),
                );
            }
            scanner.scan(byte);
            candidate.bytes.push(byte);
            candidate.kind = CandidateKind::DcsHeader { form, scanner };
            return FramerState::Candidate(candidate);
        }
        if byte != b'q' {
            return Self::abandon_candidate(candidate, position, byte, output);
        }

        let parameters = scanner.finish();
        let initial_failure = parameters.as_ref().err().copied().map(|category| (category, None));
        self.start_active(
            candidate.start,
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
    ) -> FramerState {
        let failure = initial_failure.or_else(|| {
            (control_bytes > self.max_control_string_bytes).then_some((
                GraphicsFailureCategory::QuotaExceeded,
                Some(GraphicsLimit::ControlStringBytes),
            ))
        });
        FramerState::Active(ActiveString {
            start,
            kind,
            body: Vec::new(),
            control_bytes,
            pending_escape: false,
            kitty_payload_started: false,
            kitty_payload_bytes: 0,
            failure,
        })
    }

    fn process_csi(
        mut candidate: Candidate,
        form: StringForm,
        position: u64,
        byte: u8,
        output: &mut EventOutput,
    ) -> FramerState {
        candidate.bytes.push(byte);
        let introducer_len = if form == StringForm::SevenBit { 2 } else { 1 };
        let sequence = candidate.bytes.get(introducer_len..).unwrap_or_default();
        if !(0x40..=0x7e).contains(&byte) && is_sixel_mode_prefix(sequence) {
            return FramerState::Candidate(candidate);
        }
        if !(0x40..=0x7e).contains(&byte) && is_ground_control(byte) {
            let current = candidate.bytes.pop();
            debug_assert_eq!(current, Some(byte));
            return Self::abandon_candidate(candidate, position, byte, output);
        }
        let raw = RawBytes {
            range: RawByteRange::new(
                candidate.start,
                candidate.start.saturating_add(candidate.bytes.len() as u64),
            ),
            bytes: candidate.bytes,
        };
        if let Some((mode, enabled)) =
            parse_sixel_mode_bytes(raw.bytes.get(introducer_len..).unwrap_or_default())
        {
            output.event(GraphicsEvent::SixelMode(SixelModeChange { raw, mode, enabled }));
        } else {
            output.raw(raw.range.start, raw.bytes);
        }
        FramerState::Ground
    }

    fn process_active(
        &self,
        mut active: ActiveString,
        position: u64,
        byte: u8,
        output: &mut EventOutput,
    ) -> FramerState {
        if byte == CAN || byte == SUB {
            let failure = Self::active_failure(
                &active,
                position.saturating_add(1),
                GraphicsFailureCategory::MalformedFraming,
            );
            output.event(GraphicsEvent::Failure(failure));
            return FramerState::Ground;
        }

        if active.pending_escape {
            active.pending_escape = false;
            if byte == b'\\' {
                return Self::finish_string(
                    active,
                    position.saturating_add(1),
                    StringForm::SevenBit,
                    output,
                );
            }
            self.charge_and_append(&mut active, ESC);
            if active.failure.is_none() {
                active.failure = Some((GraphicsFailureCategory::MalformedFraming, None));
            }
            if byte == ESC {
                active.pending_escape = true;
                return FramerState::Active(active);
            }
            if byte == C1_ST {
                return Self::finish_string(
                    active,
                    position.saturating_add(1),
                    StringForm::C1,
                    output,
                );
            }
            self.charge_and_append(&mut active, byte);
            return FramerState::Active(active);
        }

        if byte == ESC {
            active.pending_escape = true;
            return FramerState::Active(active);
        }
        if byte == C1_ST {
            return Self::finish_string(active, position.saturating_add(1), StringForm::C1, output);
        }

        self.charge_and_append(&mut active, byte);
        FramerState::Active(active)
    }

    fn charge_and_append(&self, active: &mut ActiveString, byte: u8) {
        active.control_bytes = active.control_bytes.saturating_add(1);
        if active.control_bytes > self.max_control_string_bytes {
            if active.failure.is_none() {
                active.failure = Some((
                    GraphicsFailureCategory::QuotaExceeded,
                    Some(GraphicsLimit::ControlStringBytes),
                ));
            }
            return;
        }
        if matches!(&active.kind, ActiveKind::Kitty)
            && active.failure.is_none()
            && charge_kitty_payload(active, byte)
        {
            active.failure = Some((
                GraphicsFailureCategory::QuotaExceeded,
                Some(GraphicsLimit::KittyChunkPayloadBytes),
            ));
            return;
        }
        if active.failure.is_none() {
            active.body.push(byte);
        }
    }

    fn finish_string(
        active: ActiveString,
        end: u64,
        terminator_form: StringForm,
        output: &mut EventOutput,
    ) -> FramerState {
        let range = RawByteRange::new(active.start, end);
        let protocol = active.kind.protocol();
        if active.failure.is_some() {
            output.event(GraphicsEvent::Failure(Self::active_failure(
                &active,
                end,
                GraphicsFailureCategory::MalformedFraming,
            )));
            return FramerState::Ground;
        }
        if active.kind.form() != terminator_form {
            output.event(GraphicsEvent::Failure(GraphicsFailure::new(
                range,
                protocol,
                GraphicsFailureCategory::MalformedFraming,
            )));
            return FramerState::Ground;
        }

        match active.kind {
            ActiveKind::Kitty => match parse_kitty(&active.body) {
                Ok(command) => output.event(GraphicsEvent::Kitty { range, command }),
                Err((category, limit)) => output.event(GraphicsEvent::Failure(GraphicsFailure {
                    range,
                    protocol,
                    category,
                    limit,
                })),
            },
            ActiveKind::UnsupportedKittyC1 => {
                output.event(GraphicsEvent::Failure(GraphicsFailure::new(
                    range,
                    protocol,
                    GraphicsFailureCategory::UnsupportedProtocol,
                )));
            }
            ActiveKind::Sixel { parameters, .. } => match parameters {
                Err(category) => output
                    .event(GraphicsEvent::Failure(GraphicsFailure::new(range, protocol, category))),
                Ok(parameters) => match validate_sixel_payload(&active.body) {
                    Ok(()) => output.event(GraphicsEvent::Sixel {
                        range,
                        command: SixelCommand { parameters, payload: active.body },
                    }),
                    Err(category) => output.event(GraphicsEvent::Failure(GraphicsFailure::new(
                        range, protocol, category,
                    ))),
                },
            },
        }
        FramerState::Ground
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

impl Default for GraphicsFramer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct EventOutput {
    events: Vec<GraphicsEvent>,
    raw_start: Option<u64>,
    raw: Vec<u8>,
}

impl EventOutput {
    fn raw_byte(&mut self, position: u64, byte: u8) {
        if self.raw_start.is_none() {
            self.raw_start = Some(position);
        }
        self.raw.push(byte);
    }

    fn raw(&mut self, start: u64, bytes: Vec<u8>) {
        self.flush_raw();
        let end = start.saturating_add(bytes.len() as u64);
        self.events
            .push(GraphicsEvent::Raw(RawBytes { range: RawByteRange::new(start, end), bytes }));
    }

    fn event(&mut self, event: GraphicsEvent) {
        self.flush_raw();
        self.events.push(event);
    }

    fn flush_raw(&mut self) {
        let Some(start) = self.raw_start.take() else { return };
        let bytes = std::mem::take(&mut self.raw);
        let end = start.saturating_add(bytes.len() as u64);
        self.events
            .push(GraphicsEvent::Raw(RawBytes { range: RawByteRange::new(start, end), bytes }));
    }

    fn finish(mut self) -> Vec<GraphicsEvent> {
        self.flush_raw();
        self.events
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
    bytes: &[u8],
) -> Result<KittyCommand, (GraphicsFailureCategory, Option<GraphicsLimit>)> {
    let (controls, payload) = split_at_byte(bytes, b';').map_or((bytes, &[][..]), |parts| parts);
    if controls.is_empty() {
        return Err((GraphicsFailureCategory::MalformedControl, None));
    }
    if payload.len() > MAX_KITTY_CHUNK_PAYLOAD_BYTES {
        return Err((
            GraphicsFailureCategory::QuotaExceeded,
            Some(GraphicsLimit::KittyChunkPayloadBytes),
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
        payload: payload.to_vec(),
    };
    let mut seen = HashSet::new();

    for pair in controls.split(|byte| *byte == b',') {
        let Some((key_bytes, value)) = split_at_byte(pair, b'=') else {
            return Err((GraphicsFailureCategory::MalformedControl, None));
        };
        let Some(&key) = key_bytes.first().filter(|_| key_bytes.len() == 1) else {
            return Err((GraphicsFailureCategory::MalformedControl, None));
        };
        if !seen.insert(key) || value.is_empty() {
            return Err((GraphicsFailureCategory::MalformedControl, None));
        }
        apply_kitty_control(&mut command, key, value)?;
    }

    if command.chunk_state == KittyChunkState::More && !command.payload.len().is_multiple_of(4) {
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
                let (_, next) = parse_numeric_fields(bytes, cursor.saturating_add(1), 4)?;
                cursor = next;
            }
            b'#' => {
                let (palette, next) = parse_decimal_at(bytes, cursor.saturating_add(1))?;
                if palette > 255 {
                    return Err(GraphicsFailureCategory::MalformedPayload);
                }
                if bytes.get(next) == Some(&b';') {
                    let (fields, after) = parse_numeric_fields(bytes, next.saturating_add(1), 4)?;
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

fn parse_numeric_fields(
    bytes: &[u8],
    start: usize,
    count: usize,
) -> Result<(Vec<u32>, usize), GraphicsFailureCategory> {
    let mut values = Vec::with_capacity(count);
    let mut cursor = start;
    for field in 0..count {
        let (value, next) = parse_decimal_at(bytes, cursor)?;
        values.push(value);
        cursor = next;
        if field.saturating_add(1) < count {
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
        let mut framer = GraphicsFramer::with_max_control_string_bytes(16);
        assert!(framer.push(b"\x1bP0;0;0;0q").is_empty());
        assert!(framer.push(b"~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~").is_empty());

        let FramerState::Active(active) = &framer.state else {
            panic!("malformed Sixel must remain active until its terminator");
        };
        assert!(matches!(active.failure, Some((GraphicsFailureCategory::MalformedControl, None))));
        assert!(active.body.is_empty());
        assert_eq!(active.control_bytes, 40);

        let events = framer.push(b"\x1b\\");
        assert!(matches!(
            events.as_slice(),
            [GraphicsEvent::Failure(failure)]
                if failure.category == GraphicsFailureCategory::MalformedControl
                    && failure.limit.is_none()
        ));
    }
}
