//! Bounded decoder-only fork of `icy_sixel` 0.5.0.
//!
//! Upstream decoder and palette concepts are retained under MIT OR
//! Apache-2.0. Scribe replaced the API, allocation, growth, work, deadline,
//! cancellation, and error boundaries. See the adjacent `README.md`.

use std::error::Error;
use std::fmt;
use std::ops::Range;

pub use scribe_image_decode::{
    AllocationDenied, DecodeHooks, DecodeLimits, DecodeStats, NoopHooks,
};
use scribe_image_decode::{
    BudgetError, DecodeAllocationClass, DecodeBudget as Budget, DecodeBuffer, DecodePermit,
    DecodeStorageError,
};

const SIXEL_CELL_HEIGHT: usize = 6;
const PALETTE_SIZE: usize = 256;

/// Stable limit identifiers safe for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitName {
    Dimensions,
    Pixels,
    RgbaBytes,
    WorkUnits,
    CheckInterval,
    PaletteRegisters,
}

impl LimitName {
    /// Stable contract spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dimensions => "dimensions",
            Self::Pixels => "pixels",
            Self::RgbaBytes => "canonical_rgba_bytes",
            Self::WorkUnits => "work_units",
            Self::CheckInterval => "deadline_check_interval_work_units",
            Self::PaletteRegisters => "palette_registers",
        }
    }
}

/// Stable malformed-input reasons without source bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MalformedReason {
    MissingDcsFinal,
    MissingTerminator,
    UnexpectedEscape,
    InvalidParameter,
    NumericOverflow,
    EmptyRaster,
}

/// Typed failures. No variant owns or formats payload data.
#[derive(Debug)]
pub enum DecodeError {
    InvalidLimit { limit: LimitName },
    InvalidDimensions { width: usize, height: usize, limit: LimitName },
    QuotaExceeded { limit: LimitName, requested: u64, maximum: u64 },
    WorkBudgetExceeded { requested: u64, maximum: u64 },
    DecodeDeadlineExceeded { work_units: u64 },
    DecodeCancelled { work_units: u64 },
    AllocationFailed { requested_bytes: usize },
    Storage(DecodeStorageError),
    Malformed { offset: usize, reason: MalformedReason },
}

impl DecodeError {
    /// Stable terminal-image failure category.
    pub const fn category(&self) -> &'static str {
        match self {
            Self::InvalidLimit { .. } | Self::InvalidDimensions { .. } => "invalid_dimensions",
            Self::QuotaExceeded { .. } => "quota_exceeded",
            Self::WorkBudgetExceeded { .. } => "work_budget_exceeded",
            Self::DecodeDeadlineExceeded { .. } => "decode_deadline_exceeded",
            Self::DecodeCancelled { .. } => "decode_cancelled",
            Self::AllocationFailed { .. } => "allocation_failed",
            Self::Storage(_) => "storage",
            Self::Malformed { .. } => "malformed_payload",
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit { limit } => write!(formatter, "invalid {} limit", limit.as_str()),
            Self::InvalidDimensions { width, height, limit } => {
                write!(formatter, "invalid dimensions {width}x{height} for {}", limit.as_str())
            }
            Self::QuotaExceeded { limit, requested, maximum } => {
                write!(formatter, "{} quota exceeded: {requested} > {maximum}", limit.as_str())
            }
            Self::WorkBudgetExceeded { requested, maximum } => {
                write!(formatter, "work budget exceeded: {requested} > {maximum}")
            }
            Self::DecodeDeadlineExceeded { work_units } => {
                write!(formatter, "decode deadline exceeded at {work_units} work units")
            }
            Self::DecodeCancelled { work_units } => {
                write!(formatter, "decode cancelled at {work_units} work units")
            }
            Self::AllocationFailed { requested_bytes } => {
                write!(formatter, "allocation failed for {requested_bytes} bytes")
            }
            Self::Storage(error) => write!(formatter, "Sixel storage failure: {error:?}"),
            Self::Malformed { offset, reason } => {
                write!(formatter, "malformed Sixel at offset {offset}: {reason:?}")
            }
        }
    }
}

impl Error for DecodeError {}

/// DCS parameters captured by the protocol framer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DcsSettings {
    pub aspect_ratio: Option<u16>,
    pub background_mode: Option<u16>,
    pub grid_size: Option<u16>,
}

/// Historical pixel aspect-ratio metadata retained from the DCS header.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PixelAspectRatio {
    Ratio5To1,
    Ratio3To1,
    Ratio2To1,
    #[default]
    Square,
}

impl PixelAspectRatio {
    const fn from_p1(value: u16) -> Self {
        match value {
            0 | 1 => Self::Ratio5To1,
            2 => Self::Ratio3To1,
            3..=6 => Self::Ratio2To1,
            _ => Self::Square,
        }
    }
}

/// Sixel background mode from DCS P2.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackgroundMode {
    #[default]
    Opaque,
    Transparent,
}

impl BackgroundMode {
    const fn from_p2(value: Option<u16>) -> Self {
        if matches!(value, Some(1)) { Self::Transparent } else { Self::Opaque }
    }
}

/// Completed canonical RGBA Sixel image.
#[derive(Debug)]
pub struct DecodedSixel {
    pub rgba: DecodeBuffer,
    pub width: usize,
    pub height: usize,
    pub aspect_ratio: PixelAspectRatio,
    pub background_mode: BackgroundMode,
    pub stats: DecodeStats,
}

/// Decode one complete 7-bit or C1 Sixel DCS sequence.
pub fn decode_sixel(
    data: &[u8],
    limits: DecodeLimits,
    hooks: &impl DecodeHooks,
    permit: &DecodePermit,
) -> Result<DecodedSixel, DecodeError> {
    limits.validate()?;
    let mut budget = Budget::new(limits, hooks, permit)?;
    let parsed = parse_sequence(data, &mut budget)?;
    decode_payload_with_budget(parsed.payload, parsed.settings, budget)
}

impl From<BudgetError> for DecodeError {
    fn from(error: BudgetError) -> Self {
        match error {
            BudgetError::InvalidLimits => Self::InvalidLimit { limit: LimitName::WorkUnits },
            BudgetError::WorkBudgetExceeded { requested, maximum } => {
                Self::WorkBudgetExceeded { requested, maximum }
            }
            BudgetError::DecodeDeadlineExceeded { work_units } => {
                Self::DecodeDeadlineExceeded { work_units }
            }
            BudgetError::DecodeCancelled { work_units } => Self::DecodeCancelled { work_units },
            BudgetError::AllocationFailed { requested_bytes } => {
                Self::AllocationFailed { requested_bytes }
            }
            BudgetError::Storage(error) => Self::Storage(error),
        }
    }
}

/// Decode a Sixel payload after an external framer has removed DCS/ST.
pub fn decode_sixel_payload(
    payload: &[u8],
    settings: DcsSettings,
    limits: DecodeLimits,
    hooks: &impl DecodeHooks,
    permit: &DecodePermit,
) -> Result<DecodedSixel, DecodeError> {
    limits.validate()?;
    let budget = Budget::new(limits, hooks, permit)?;
    decode_payload_with_budget(payload, settings, budget)
}

fn decode_payload_with_budget(
    payload: &[u8],
    settings: DcsSettings,
    mut budget: Budget<'_>,
) -> Result<DecodedSixel, DecodeError> {
    let aspect_ratio = settings.aspect_ratio.map(PixelAspectRatio::from_p1).unwrap_or_default();
    let background_mode = BackgroundMode::from_p2(settings.background_mode);
    let mut decoder = Decoder::new(settings, background_mode);
    decoder.process(payload, &mut budget)?;
    let (rgba, width, height) = decoder.finish(&mut budget)?;
    budget.check_now()?;
    Ok(DecodedSixel { rgba, width, height, aspect_ratio, background_mode, stats: budget.stats() })
}

struct ParsedSequence<'a> {
    payload: &'a [u8],
    settings: DcsSettings,
}

fn parse_sequence<'a>(
    data: &'a [u8],
    budget: &mut Budget<'_>,
) -> Result<ParsedSequence<'a>, DecodeError> {
    let (mut cursor, c1) = if data.starts_with(&[0x90]) {
        budget.charge(1)?;
        (1, true)
    } else if data.starts_with(b"\x1bP") {
        budget.charge(2)?;
        (2, false)
    } else {
        return Err(DecodeError::Malformed { offset: 0, reason: MalformedReason::MissingDcsFinal });
    };

    let header_start = cursor;
    while cursor < data.len() {
        budget.charge(1)?;
        let byte = data[cursor];
        if byte == b'q' {
            let settings = parse_dcs_settings(&data[header_start..cursor], header_start)?;
            cursor = checked_add(cursor, 1)?;
            let payload_start = cursor;
            let payload_end = find_terminator(data, payload_start, c1, budget)?;
            return Ok(ParsedSequence { payload: &data[payload_start..payload_end], settings });
        }
        if byte == 0x9c || byte == 0x1b {
            return Err(DecodeError::Malformed {
                offset: cursor,
                reason: MalformedReason::MissingDcsFinal,
            });
        }
        cursor = checked_add(cursor, 1)?;
    }
    Err(DecodeError::Malformed { offset: data.len(), reason: MalformedReason::MissingDcsFinal })
}

fn parse_dcs_settings(header: &[u8], base: usize) -> Result<DcsSettings, DecodeError> {
    if header.is_empty() {
        return Ok(DcsSettings::default());
    }
    let values = parse_fixed_u16_params::<3>(header, base)?;
    Ok(DcsSettings { aspect_ratio: values[0], background_mode: values[1], grid_size: values[2] })
}

fn find_terminator(
    data: &[u8],
    start: usize,
    c1: bool,
    budget: &mut Budget<'_>,
) -> Result<usize, DecodeError> {
    let mut cursor = start;
    while cursor < data.len() {
        budget.charge(1)?;
        match data[cursor] {
            0x9c if c1 => return Ok(cursor),
            0x9c => {
                return Err(DecodeError::Malformed {
                    offset: cursor,
                    reason: MalformedReason::UnexpectedEscape,
                });
            }
            0x1b if !c1 => {
                let following = checked_add(cursor, 1)?;
                budget.charge(1)?;
                if data.get(following) == Some(&b'\\') {
                    return Ok(cursor);
                }
                return Err(DecodeError::Malformed {
                    offset: cursor,
                    reason: MalformedReason::UnexpectedEscape,
                });
            }
            0x1b => {
                return Err(DecodeError::Malformed {
                    offset: cursor,
                    reason: MalformedReason::UnexpectedEscape,
                });
            }
            _ => cursor = checked_add(cursor, 1)?,
        }
    }
    Err(DecodeError::Malformed { offset: data.len(), reason: MalformedReason::MissingTerminator })
}

fn parse_fixed_u16_params<const N: usize>(
    bytes: &[u8],
    base: usize,
) -> Result<[Option<u16>; N], DecodeError> {
    let mut output = [None; N];
    let mut parameter = 0usize;
    let mut value = 0u16;
    let mut has_digit = false;
    for (offset, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'0'..=b'9' => {
                value = value
                    .checked_mul(10)
                    .and_then(|current| current.checked_add(u16::from(byte - b'0')))
                    .ok_or(DecodeError::Malformed {
                        offset: base.saturating_add(offset),
                        reason: MalformedReason::NumericOverflow,
                    })?;
                has_digit = true;
            }
            b';' => {
                if parameter >= N {
                    return Err(DecodeError::Malformed {
                        offset: base.saturating_add(offset),
                        reason: MalformedReason::InvalidParameter,
                    });
                }
                output[parameter] = has_digit.then_some(value);
                parameter = checked_add(parameter, 1)?;
                value = 0;
                has_digit = false;
            }
            _ => {
                return Err(DecodeError::Malformed {
                    offset: base.saturating_add(offset),
                    reason: MalformedReason::InvalidParameter,
                });
            }
        }
    }
    if parameter >= N {
        return Err(DecodeError::Malformed {
            offset: base.saturating_add(bytes.len()),
            reason: MalformedReason::InvalidParameter,
        });
    }
    output[parameter] = has_digit.then_some(value);
    Ok(output)
}

struct Decoder {
    canvas: Canvas,
    palette: Palette,
    color_index: usize,
    current_color: [u8; 4],
    repeat: usize,
    pos_x: usize,
    pos_y: usize,
    extent_width: usize,
    extent_height: usize,
    target_width: usize,
    target_height: usize,
    background_mode: BackgroundMode,
    saw_raster: bool,
}

impl Decoder {
    fn new(_settings: DcsSettings, background_mode: BackgroundMode) -> Self {
        let palette = Palette::new();
        let current_color = palette.rgba(0);
        Self {
            canvas: Canvas::new(),
            palette,
            color_index: 0,
            current_color,
            repeat: 1,
            pos_x: 0,
            pos_y: 0,
            extent_width: 0,
            extent_height: 0,
            target_width: 0,
            target_height: 0,
            background_mode,
            saw_raster: false,
        }
    }

    fn process(&mut self, data: &[u8], budget: &mut Budget<'_>) -> Result<(), DecodeError> {
        let mut cursor = 0usize;
        while cursor < data.len() {
            budget.charge(1)?;
            match data[cursor] {
                b'\n' | b'\r' | b'\t' | 0x0c => cursor = checked_add(cursor, 1)?,
                b'$' => {
                    self.pos_x = 0;
                    cursor = checked_add(cursor, 1)?;
                }
                b'-' => {
                    self.pos_x = 0;
                    self.pos_y = checked_add(self.pos_y, SIXEL_CELL_HEIGHT)?;
                    cursor = checked_add(cursor, 1)?;
                }
                b'!' => {
                    cursor = self.repeat_command(data, cursor, budget)?;
                }
                b'#' => {
                    let start = checked_add(cursor, 1)?;
                    let consumed = self.color_command(data, start, budget)?;
                    cursor = checked_add(start, consumed)?;
                }
                b'"' => {
                    let start = checked_add(cursor, 1)?;
                    let consumed = self.raster_command(data, start, budget)?;
                    cursor = checked_add(start, consumed)?;
                }
                b'?'..=b'~' => {
                    self.sixel(data[cursor], budget)?;
                    cursor = checked_add(cursor, 1)?;
                }
                0x1b | 0x9c => {
                    return Err(DecodeError::Malformed {
                        offset: cursor,
                        reason: MalformedReason::UnexpectedEscape,
                    });
                }
                _ => cursor = checked_add(cursor, 1)?,
            }
        }
        Ok(())
    }

    fn repeat_command(
        &mut self,
        data: &[u8],
        command_offset: usize,
        budget: &mut Budget<'_>,
    ) -> Result<usize, DecodeError> {
        let start = checked_add(command_offset, 1)?;
        let (value, consumed) = read_usize(data, start, budget)?;
        if consumed == 0 {
            return Err(DecodeError::Malformed {
                offset: command_offset,
                reason: MalformedReason::InvalidParameter,
            });
        }
        self.repeat = value.max(1);
        checked_add(start, consumed)
    }

    fn color_command(
        &mut self,
        data: &[u8],
        start: usize,
        budget: &mut Budget<'_>,
    ) -> Result<usize, DecodeError> {
        let (values, consumed) = command_params::<5>(data, start, budget)?;
        let Some(index) = values[0] else {
            return Err(DecodeError::Malformed {
                offset: start,
                reason: MalformedReason::InvalidParameter,
            });
        };
        if index >= PALETTE_SIZE {
            return Err(DecodeError::QuotaExceeded {
                limit: LimitName::PaletteRegisters,
                requested: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                maximum: PALETTE_SIZE as u64,
            });
        }
        self.color_index = index;
        if let (Some(space), Some(a), Some(b), Some(c)) =
            (values[1], values[2], values[3], values[4])
        {
            match space {
                1 => self.palette.set_hls(index, a, b, c),
                2 => self.palette.set_rgb_percent(index, a, b, c),
                _ => {}
            }
        }
        self.current_color = self.palette.rgba(index);
        Ok(consumed)
    }

    fn raster_command(
        &mut self,
        data: &[u8],
        start: usize,
        budget: &mut Budget<'_>,
    ) -> Result<usize, DecodeError> {
        let (values, consumed) = command_params::<4>(data, start, budget)?;
        if let Some(width) = values[2].filter(|value| *value > 0) {
            self.target_width = width;
        }
        if let Some(height) = values[3].filter(|value| *value > 0) {
            self.target_height = height;
        }
        if self.target_width > 0 || self.target_height > 0 {
            let width = self.target_width.max(1);
            let height = self.target_height.max(1);
            guard_dimensions(width, height, budget.limits())?;
            self.canvas.ensure_visible(width, height, self.background(), budget)?;
            self.extent_width = self.extent_width.max(width);
            self.extent_height = self.extent_height.max(height);
            self.saw_raster = true;
        }
        Ok(consumed)
    }

    fn sixel(&mut self, byte: u8, budget: &mut Budget<'_>) -> Result<(), DecodeError> {
        let bits = byte - b'?';
        let span = self.repeat;
        self.repeat = 1;
        let width = checked_add(self.pos_x, span)?;
        let height = checked_add(self.pos_y, SIXEL_CELL_HEIGHT)?;
        guard_dimensions(width, height, budget.limits())?;
        self.canvas.ensure_visible(width, height, self.background(), budget)?;
        for bit in 0..SIXEL_CELL_HEIGHT {
            if bits & (1 << bit) != 0 {
                self.canvas.paint_span(
                    checked_add(self.pos_y, bit)?,
                    self.pos_x..width,
                    self.current_color,
                    budget,
                )?;
            }
        }
        self.pos_x = width;
        self.extent_width = self.extent_width.max(width);
        self.extent_height = self.extent_height.max(height);
        self.saw_raster = true;
        Ok(())
    }

    fn background(&self) -> [u8; 4] {
        match self.background_mode {
            BackgroundMode::Opaque => self.palette.rgba(0),
            BackgroundMode::Transparent => [0, 0, 0, 0],
        }
    }

    fn finish(
        mut self,
        budget: &mut Budget<'_>,
    ) -> Result<(DecodeBuffer, usize, usize), DecodeError> {
        if !self.saw_raster {
            return Err(DecodeError::Malformed { offset: 0, reason: MalformedReason::EmptyRaster });
        }
        let width = self.extent_width.max(self.target_width).max(1);
        let height = self.extent_height.max(self.target_height).max(1);
        guard_dimensions(width, height, budget.limits())?;
        self.canvas.ensure_visible(width, height, self.background(), budget)?;
        if self.canvas.width != width || self.canvas.height != height {
            return Err(DecodeError::InvalidDimensions {
                width: self.canvas.width,
                height: self.canvas.height,
                limit: LimitName::Dimensions,
            });
        }
        let rgba = self.canvas.into_compact(width, height, budget)?;
        Ok((rgba, width, height))
    }
}

struct Canvas {
    data: Option<DecodeBuffer>,
    width: usize,
    height: usize,
    stride_width: usize,
    allocated_height: usize,
}

impl Canvas {
    const fn new() -> Self {
        Self { data: None, width: 0, height: 0, stride_width: 0, allocated_height: 0 }
    }

    fn ensure_visible(
        &mut self,
        width: usize,
        height: usize,
        background: [u8; 4],
        budget: &mut Budget<'_>,
    ) -> Result<(), DecodeError> {
        if width <= self.width && height <= self.height {
            return Ok(());
        }
        let need_growth = width > self.stride_width || height > self.allocated_height;
        if need_growth {
            let new_width =
                geometric_dimension(self.stride_width, width, budget.limits().max_width_pixels);
            let new_height = geometric_dimension(
                self.allocated_height,
                height,
                budget.limits().max_height_pixels,
            );
            guard_dimensions(new_width, new_height, budget.limits())?;
            let new_bytes = rgba_bytes(new_width, new_height, budget.limits())?;
            let copied = self
                .width
                .checked_mul(self.height)
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or(DecodeError::InvalidDimensions {
                    width: self.width,
                    height: self.height,
                    limit: LimitName::RgbaBytes,
                })?;
            let initialized_work = new_width
                .checked_mul(new_height)
                .and_then(|pixels| pixels.checked_mul(5))
                .ok_or(DecodeError::InvalidDimensions {
                    width: new_width,
                    height: new_height,
                    limit: LimitName::Pixels,
                })?;
            // Admission gates the work: every byte this growth initializes or
            // copies is charged before any of it is performed.
            budget.charge(u64::try_from(initialized_work).unwrap_or(u64::MAX))?;
            budget.charge(u64::try_from(copied).unwrap_or(u64::MAX))?;
            let mut next = budget.allocate(DecodeAllocationClass::SixelRgba, new_bytes)?;
            next.resize(new_bytes, 0).map_err(DecodeError::Storage)?;
            for pixel in next.chunks_exact_mut(4) {
                pixel.copy_from_slice(&background);
            }

            if let Some(current) = &self.data {
                for row in 0..self.height {
                    let source = pixel_offset(self.stride_width, 0, row)?;
                    let target = pixel_offset(new_width, 0, row)?;
                    let row_bytes =
                        self.width.checked_mul(4).ok_or(DecodeError::InvalidDimensions {
                            width: self.width,
                            height: self.height,
                            limit: LimitName::RgbaBytes,
                        })?;
                    let source_end = checked_add(source, row_bytes)?;
                    let target_end = checked_add(target, row_bytes)?;
                    next[target..target_end].copy_from_slice(&current[source..source_end]);
                }
            }

            let old_bytes = self.data.as_ref().map_or(0, DecodeBuffer::requested_bytes);
            self.data = Some(next);
            self.stride_width = new_width;
            self.allocated_height = new_height;
            budget.end_allocation(old_bytes);
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn into_compact(
        mut self,
        width: usize,
        height: usize,
        budget: &mut Budget<'_>,
    ) -> Result<DecodeBuffer, DecodeError> {
        let data = self
            .data
            .take()
            .ok_or(DecodeError::Malformed { offset: 0, reason: MalformedReason::EmptyRaster })?;
        if self.stride_width == width && self.allocated_height == height {
            return Ok(data);
        }
        let exact_bytes = rgba_bytes(width, height, budget.limits())?;
        budget.charge(u64::try_from(exact_bytes).unwrap_or(u64::MAX))?;
        let mut compact = budget.allocate(DecodeAllocationClass::SixelRgba, exact_bytes)?;
        for row in 0..height {
            let source = pixel_offset(self.stride_width, 0, row)?;
            let row_bytes = width.checked_mul(4).ok_or(DecodeError::InvalidDimensions {
                width,
                height,
                limit: LimitName::RgbaBytes,
            })?;
            let source_end = checked_add(source, row_bytes)?;
            compact.extend_from_slice(&data[source..source_end]).map_err(DecodeError::Storage)?;
        }
        budget.end_allocation(data.requested_bytes());
        Ok(compact)
    }

    fn paint_span(
        &mut self,
        y: usize,
        columns: Range<usize>,
        color: [u8; 4],
        budget: &mut Budget<'_>,
    ) -> Result<(), DecodeError> {
        if y >= self.height || columns.end > self.width {
            return Err(DecodeError::InvalidDimensions {
                width: columns.end,
                height: y.saturating_add(1),
                limit: LimitName::Dimensions,
            });
        }
        for column in columns {
            budget.charge(5)?;
            let start = pixel_offset(self.stride_width, column, y)?;
            let end = checked_add(start, 4)?;
            let data = self.data.as_mut().ok_or(DecodeError::Malformed {
                offset: 0,
                reason: MalformedReason::EmptyRaster,
            })?;
            data[start..end].copy_from_slice(&color);
        }
        Ok(())
    }
}

fn geometric_dimension(current: usize, needed: usize, maximum: usize) -> usize {
    current.max(1).checked_mul(2).unwrap_or(needed).min(maximum).max(needed)
}

struct Palette {
    colors: [u32; PALETTE_SIZE],
}

impl Palette {
    fn new() -> Self {
        let mut colors = [0u32; PALETTE_SIZE];
        let base = [
            (0, 0, 0),
            (20, 20, 80),
            (80, 13, 13),
            (20, 80, 20),
            (80, 20, 80),
            (20, 80, 80),
            (80, 80, 20),
            (53, 53, 53),
            (26, 26, 26),
            (33, 33, 60),
            (60, 26, 26),
            (33, 60, 33),
            (60, 33, 60),
            (33, 60, 60),
            (60, 60, 33),
            (80, 80, 80),
        ];
        let mut cursor = 0usize;
        for (red, green, blue) in base {
            colors[cursor] =
                pack_rgb(percent_to_byte(red), percent_to_byte(green), percent_to_byte(blue));
            cursor += 1;
        }
        for cube_index in 0..216 {
            let red = cube_index / 36;
            let green = (cube_index / 6) % 6;
            let blue = cube_index % 6;
            colors[cursor] = pack_rgb(
                percent_to_byte(red * 20),
                percent_to_byte(green * 20),
                percent_to_byte(blue * 20),
            );
            cursor += 1;
        }
        for level in 0..24 {
            let value = percent_to_byte(level * 100 / 23);
            colors[cursor] = pack_rgb(value, value, value);
            cursor += 1;
        }
        while cursor < PALETTE_SIZE {
            colors[cursor] = 0x00ff_ffff;
            cursor += 1;
        }
        Self { colors }
    }

    fn rgba(&self, index: usize) -> [u8; 4] {
        let value = self.colors[index];
        [((value >> 16) & 0xff) as u8, ((value >> 8) & 0xff) as u8, (value & 0xff) as u8, 0xff]
    }

    fn set_rgb_percent(&mut self, index: usize, red: usize, green: usize, blue: usize) {
        self.colors[index] =
            pack_rgb(percent_to_byte(red), percent_to_byte(green), percent_to_byte(blue));
    }

    fn set_hls(&mut self, index: usize, hue: usize, lightness: usize, saturation: usize) {
        let [red, green, blue] = hls_to_rgb(hue, lightness, saturation);
        self.colors[index] = pack_rgb(red, green, blue);
    }
}

fn command_params<const N: usize>(
    data: &[u8],
    start: usize,
    budget: &mut Budget<'_>,
) -> Result<([Option<usize>; N], usize), DecodeError> {
    let mut values = [None; N];
    let mut parameter = 0usize;
    let mut value = 0usize;
    let mut has_digit = false;
    let mut cursor = start;
    while cursor < data.len() {
        let byte = data[cursor];
        if !byte.is_ascii_digit() && byte != b';' {
            break;
        }
        budget.charge(1)?;
        match byte {
            b'0'..=b'9' => {
                value = value
                    .checked_mul(10)
                    .and_then(|current| current.checked_add(usize::from(byte - b'0')))
                    .ok_or(DecodeError::Malformed {
                        offset: cursor,
                        reason: MalformedReason::NumericOverflow,
                    })?;
                has_digit = true;
            }
            b';' => {
                if parameter >= N {
                    return Err(DecodeError::Malformed {
                        offset: cursor,
                        reason: MalformedReason::InvalidParameter,
                    });
                }
                values[parameter] = has_digit.then_some(value);
                parameter = checked_add(parameter, 1)?;
                value = 0;
                has_digit = false;
            }
            _ => {}
        }
        cursor = checked_add(cursor, 1)?;
    }
    if has_digit {
        if parameter >= N {
            return Err(DecodeError::Malformed {
                offset: cursor,
                reason: MalformedReason::InvalidParameter,
            });
        }
        values[parameter] = Some(value);
    } else if parameter >= N {
        return Err(DecodeError::Malformed {
            offset: cursor,
            reason: MalformedReason::InvalidParameter,
        });
    }
    Ok((values, cursor.saturating_sub(start)))
}

fn read_usize(
    data: &[u8],
    start: usize,
    budget: &mut Budget<'_>,
) -> Result<(usize, usize), DecodeError> {
    let mut value = 0usize;
    let mut cursor = start;
    while let Some(byte @ b'0'..=b'9') = data.get(cursor).copied() {
        budget.charge(1)?;
        value = value
            .checked_mul(10)
            .and_then(|current| current.checked_add(usize::from(byte - b'0')))
            .ok_or(DecodeError::Malformed {
                offset: cursor,
                reason: MalformedReason::NumericOverflow,
            })?;
        cursor = checked_add(cursor, 1)?;
    }
    Ok((value, cursor.saturating_sub(start)))
}

fn guard_dimensions(width: usize, height: usize, limits: DecodeLimits) -> Result<(), DecodeError> {
    if width == 0
        || height == 0
        || width > limits.max_width_pixels
        || height > limits.max_height_pixels
    {
        return Err(DecodeError::InvalidDimensions { width, height, limit: LimitName::Dimensions });
    }
    let pixels = width.checked_mul(height).ok_or(DecodeError::InvalidDimensions {
        width,
        height,
        limit: LimitName::Pixels,
    })?;
    if pixels > limits.max_pixels {
        return Err(DecodeError::QuotaExceeded {
            limit: LimitName::Pixels,
            requested: u64::try_from(pixels).unwrap_or(u64::MAX),
            maximum: u64::try_from(limits.max_pixels).unwrap_or(u64::MAX),
        });
    }
    let _ = rgba_bytes(width, height, limits)?;
    Ok(())
}

fn rgba_bytes(width: usize, height: usize, limits: DecodeLimits) -> Result<usize, DecodeError> {
    let bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(DecodeError::InvalidDimensions { width, height, limit: LimitName::RgbaBytes })?;
    if bytes > limits.max_rgba_bytes {
        return Err(DecodeError::QuotaExceeded {
            limit: LimitName::RgbaBytes,
            requested: u64::try_from(bytes).unwrap_or(u64::MAX),
            maximum: u64::try_from(limits.max_rgba_bytes).unwrap_or(u64::MAX),
        });
    }
    Ok(bytes)
}

fn pixel_offset(width: usize, x: usize, y: usize) -> Result<usize, DecodeError> {
    y.checked_mul(width)
        .and_then(|row| row.checked_add(x))
        .and_then(|pixel| pixel.checked_mul(4))
        .ok_or(DecodeError::InvalidDimensions {
            width,
            height: y.saturating_add(1),
            limit: LimitName::RgbaBytes,
        })
}

fn checked_add(left: usize, right: usize) -> Result<usize, DecodeError> {
    left.checked_add(right)
        .ok_or(DecodeError::Malformed { offset: left, reason: MalformedReason::NumericOverflow })
}

fn percent_to_byte(value: usize) -> u8 {
    let clamped = value.min(100);
    ((clamped * 255 + 50) / 100) as u8
}

const fn pack_rgb(red: u8, green: u8, blue: u8) -> u32 {
    ((red as u32) << 16) | ((green as u32) << 8) | blue as u32
}

fn hls_to_rgb(hue: usize, lightness: usize, saturation: usize) -> [u8; 3] {
    if saturation == 0 {
        let gray = percent_to_byte(lightness);
        return [gray, gray, gray];
    }
    let hue = ((hue + 240) % 360) as f64 / 360.0;
    let lightness = lightness.min(100) as f64 / 100.0;
    let saturation = saturation.min(100) as f64 / 100.0;
    let q = if lightness < 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let p = 2.0 * lightness - q;
    [
        float_to_byte(hue_to_rgb(p, q, hue + 1.0 / 3.0)),
        float_to_byte(hue_to_rgb(p, q, hue)),
        float_to_byte(hue_to_rgb(p, q, hue - 1.0 / 3.0)),
    ]
}

fn hue_to_rgb(p: f64, q: f64, mut value: f64) -> f64 {
    if value < 0.0 {
        value += 1.0;
    }
    if value > 1.0 {
        value -= 1.0;
    }
    if value < 1.0 / 6.0 {
        p + (q - p) * 6.0 * value
    } else if value < 0.5 {
        q
    } else if value < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - value) * 6.0
    } else {
        p
    }
}

fn float_to_byte(value: f64) -> u8 {
    (value * 255.0 + 0.5).floor().clamp(0.0, 255.0) as u8
}
