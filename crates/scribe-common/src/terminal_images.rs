//! Bounded terminal-image model and IPC records.
//!
//! These types are the only image-data contract shared by server, client, and
//! test tooling. They carry canonical RGBA chunks and placement metadata; they
//! cannot name a path, URL, shared-memory object, or other indirect loader.

use std::fmt;

use serde::de::{Error as _, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Frozen v1 limits for untrusted terminal image input and retained state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageLimits {
    pub max_control_string_bytes: u64,
    pub max_kitty_chunk_payload_bytes: u64,
    pub max_chunks_per_transfer: u32,
    pub max_accumulated_encoded_bytes: u64,
    pub max_base64_decoded_bytes: u64,
    pub max_inflated_bytes: u64,
    pub max_width_pixels: u32,
    pub max_height_pixels: u32,
    pub max_pixels: u64,
    pub max_canonical_rgba_bytes: u64,
    pub max_images_per_session: u32,
    pub max_placements_per_session: u32,
    pub max_session_retained_cpu_bytes: u64,
    pub max_view_projected_gpu_bytes: u64,
    pub max_process_retained_bytes: u64,
    pub max_concurrent_decodes: u32,
    pub max_decode_queue_depth: u32,
    pub max_decode_queue_bytes: u64,
    pub max_work_units_per_command: u64,
    pub max_queue_wait_ms: u64,
    pub max_decode_ms: u64,
    pub max_replay_chunk_bytes: u64,
    pub deadline_check_interval_work_units: u64,
}

impl ImageLimits {
    /// Exact security ceilings frozen by terminal-images contract v1.
    pub const V1: Self = Self {
        max_control_string_bytes: 16_777_216,
        max_kitty_chunk_payload_bytes: 4_096,
        max_chunks_per_transfer: 32_768,
        max_accumulated_encoded_bytes: 89_478_488,
        max_base64_decoded_bytes: 67_108_864,
        max_inflated_bytes: 67_108_864,
        max_width_pixels: 4_096,
        max_height_pixels: 4_096,
        max_pixels: 16_777_216,
        max_canonical_rgba_bytes: 67_108_864,
        max_images_per_session: 128,
        max_placements_per_session: 1_024,
        max_session_retained_cpu_bytes: 134_217_728,
        max_view_projected_gpu_bytes: 268_435_456,
        max_process_retained_bytes: 536_870_912,
        max_concurrent_decodes: 2,
        max_decode_queue_depth: 8,
        max_decode_queue_bytes: 134_217_728,
        max_work_units_per_command: 134_217_728,
        max_queue_wait_ms: 1_000,
        max_decode_ms: 2_000,
        max_replay_chunk_bytes: 1_048_576,
        deadline_check_interval_work_units: 4_096,
    };

    /// Validate canonical RGBA dimensions and return their exact byte length.
    pub fn canonical_rgba_len(self, width: u32, height: u32) -> Result<u64, ImageBoundError> {
        if width == 0 || height == 0 {
            return Err(ImageBoundError::InvalidDimensions);
        }
        if width > self.max_width_pixels || height > self.max_height_pixels {
            return Err(ImageBoundError::LimitExceeded(ImageLimitName::Dimensions));
        }
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or(ImageBoundError::ArithmeticOverflow)?;
        if pixels > self.max_pixels {
            return Err(ImageBoundError::LimitExceeded(ImageLimitName::Pixels));
        }
        let bytes = pixels.checked_mul(4).ok_or(ImageBoundError::ArithmeticOverflow)?;
        if bytes > self.max_canonical_rgba_bytes {
            return Err(ImageBoundError::LimitExceeded(ImageLimitName::CanonicalRgbaBytes));
        }
        Ok(bytes)
    }
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self::V1
    }
}

/// Stable names for limits included in payload-free rejection metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageLimitName {
    ControlStringBytes,
    KittyChunkPayloadBytes,
    ChunksPerTransfer,
    AccumulatedEncodedBytes,
    Base64DecodedBytes,
    InflatedBytes,
    Dimensions,
    Pixels,
    CanonicalRgbaBytes,
    ImagesPerSession,
    PlacementsPerSession,
    SessionRetainedCpuBytes,
    ViewProjectedGpuBytes,
    ProcessRetainedBytes,
    ConcurrentDecodes,
    DecodeQueueDepth,
    DecodeQueueBytes,
    WorkUnitsPerCommand,
    QueueWaitMs,
    DecodeMs,
    ReplayChunkBytes,
}

/// Failure returned while constructing bounded common model values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageBoundError {
    InvalidDimensions,
    InvalidPlacementGeometry,
    InvalidPlacementClip,
    InvalidPlacementKind,
    ArithmeticOverflow,
    LimitExceeded(ImageLimitName),
    InconsistentCanonicalLength,
    InconsistentGeneration,
}

impl fmt::Display for ImageBoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions => f.write_str("image dimensions must be non-zero"),
            Self::InvalidPlacementGeometry => {
                f.write_str("terminal image placement geometry is invalid")
            }
            Self::InvalidPlacementClip => f.write_str("terminal image placement clip is invalid"),
            Self::InvalidPlacementKind => {
                f.write_str("terminal image placement protocol and kind are inconsistent")
            }
            Self::ArithmeticOverflow => f.write_str("image size arithmetic overflowed"),
            Self::LimitExceeded(limit) => write!(f, "image limit exceeded: {limit:?}"),
            Self::InconsistentCanonicalLength => {
                f.write_str("canonical RGBA length does not match dimensions")
            }
            Self::InconsistentGeneration => f.write_str("image record generation mismatch"),
        }
    }
}

impl std::error::Error for ImageBoundError {}

/// Image protocols normalized into the common scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalImageProtocol {
    Kitty,
    Sixel,
}

/// Runtime image feature set advertised in local `Hello`/`Welcome` messages.
///
/// Every field defaults to false, so missing data from an older local peer is
/// safely interpreted as no image support.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalImageFeatures(u16);

impl TerminalImageFeatures {
    pub const KITTY_RGB: u16 = 1 << 0;
    pub const KITTY_RGBA: u16 = 1 << 1;
    pub const KITTY_PNG: u16 = 1 << 2;
    pub const KITTY_ZLIB: u16 = 1 << 3;
    pub const KITTY_CLASSIC_PLACEMENT: u16 = 1 << 4;
    pub const KITTY_UNICODE_PLACEHOLDERS: u16 = 1 << 5;
    pub const SIXEL: u16 = 1 << 6;
    pub const V1: Self = Self(
        Self::KITTY_RGB
            | Self::KITTY_RGBA
            | Self::KITTY_PNG
            | Self::KITTY_ZLIB
            | Self::KITTY_CLASSIC_PLACEMENT
            | Self::KITTY_UNICODE_PLACEHOLDERS
            | Self::SIXEL,
    );

    #[must_use]
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Build a feature set from its raw bits. Unknown bits are dropped, so a
    /// peer cannot claim a feature this build does not define.
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits & Self::V1.0)
    }

    /// Raw bits, for intersecting two advertised sets.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalImageCapabilities {
    #[serde(default)]
    pub runtime_enabled: bool,
    #[serde(default)]
    pub features: TerminalImageFeatures,
}

impl TerminalImageCapabilities {
    /// Complete compile-time v1 renderer capability before runtime policy is
    /// intersected by the server.
    pub const V1: Self = Self { runtime_enabled: true, features: TerminalImageFeatures::V1 };

    #[must_use]
    pub fn supports(self, required: Self) -> bool {
        (!required.runtime_enabled || self.runtime_enabled)
            && self.features.contains(required.features)
    }
}

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);
    };
}

typed_id!(TerminalImageId);
typed_id!(TerminalPlacementId);
typed_id!(TerminalImageGeneration);
typed_id!(TerminalOutputSequence);

/// Canonical RGBA definition metadata. Bytes travel separately in bounded
/// [`TerminalImageDataChunk`] values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalImageDefinition {
    pub id: TerminalImageId,
    pub generation: TerminalImageGeneration,
    pub width: u32,
    pub height: u32,
    pub rgba_bytes: u64,
    pub has_alpha: bool,
}

impl TerminalImageDefinition {
    pub fn new(
        id: TerminalImageId,
        generation: TerminalImageGeneration,
        width: u32,
        height: u32,
        has_alpha: bool,
    ) -> Result<Self, ImageBoundError> {
        let rgba_bytes = ImageLimits::V1.canonical_rgba_len(width, height)?;
        Ok(Self { id, generation, width, height, rgba_bytes, has_alpha })
    }

    pub fn validate(&self) -> Result<(), ImageBoundError> {
        let expected = ImageLimits::V1.canonical_rgba_len(self.width, self.height)?;
        if self.rgba_bytes != expected {
            return Err(ImageBoundError::InconsistentCanonicalLength);
        }
        Ok(())
    }
}

/// Byte storage whose serde decoder enforces the replay/live IPC chunk ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedImageBytes(Vec<u8>);

impl BoundedImageBytes {
    pub const MAX_LEN: usize = 1_048_576;

    pub fn new(bytes: Vec<u8>) -> Result<Self, ImageBoundError> {
        if bytes.len() > Self::MAX_LEN {
            return Err(ImageBoundError::LimitExceeded(ImageLimitName::ReplayChunkBytes));
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for BoundedImageBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

struct BoundedImageBytesVisitor;

impl<'de> Visitor<'de> for BoundedImageBytesVisitor {
    type Value = BoundedImageBytes;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at most {} image bytes", BoundedImageBytes::MAX_LEN)
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > BoundedImageBytes::MAX_LEN {
            return Err(E::custom("terminal image chunk exceeds v1 limit"));
        }
        Ok(BoundedImageBytes(value.to_vec()))
    }

    fn visit_borrowed_bytes<E>(self, value: &'de [u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_bytes(value)
    }

    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        BoundedImageBytes::new(value).map_err(E::custom)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if sequence.size_hint().is_some_and(|size| size > BoundedImageBytes::MAX_LEN) {
            return Err(A::Error::custom("terminal image chunk exceeds v1 limit"));
        }
        let mut bytes = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        while let Some(byte) = sequence.next_element()? {
            if bytes.len() == BoundedImageBytes::MAX_LEN {
                return Err(A::Error::custom("terminal image chunk exceeds v1 limit"));
            }
            bytes.push(byte);
        }
        Ok(BoundedImageBytes(bytes))
    }
}

impl<'de> Deserialize<'de> for BoundedImageBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_bytes(BoundedImageBytesVisitor)
    }
}

/// One bounded canonical RGBA segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalImageDataChunk {
    pub id: TerminalImageId,
    pub generation: TerminalImageGeneration,
    pub offset: u64,
    pub data: BoundedImageBytes,
    pub final_chunk: bool,
}

impl TerminalImageDataChunk {
    pub fn validate(&self, definition: &TerminalImageDefinition) -> Result<(), ImageBoundError> {
        if self.generation != definition.generation || self.id != definition.id {
            return Err(ImageBoundError::InconsistentGeneration);
        }
        let end = self
            .offset
            .checked_add(self.data.len() as u64)
            .ok_or(ImageBoundError::ArithmeticOverflow)?;
        if end > definition.rgba_bytes {
            return Err(ImageBoundError::LimitExceeded(ImageLimitName::CanonicalRgbaBytes));
        }
        if self.final_chunk != (end == definition.rgba_bytes) {
            return Err(ImageBoundError::InconsistentCanonicalLength);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCellAnchor {
    pub row: i32,
    pub column: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellExtent {
    pub columns: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalImagePlacementKind {
    KittyClassic,
    KittyUnicodePlaceholder,
    Sixel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceholderMetadata {
    pub image_identity_bits: u8,
    pub placement_id_in_underline: bool,
    /// Reserved wire-compatibility byte. Kitty has no separate placeholder
    /// background-opacity channel; renderers must not apply this as image alpha.
    pub background_alpha: u8,
}

/// Persistent exclusive cell bounds masking a classic or Sixel placement.
///
/// Source/destination geometry stays immutable while scroll and resize effects
/// move and intersect this mask. Renderers convert its edges with their own
/// current cell metrics, preserving pixel offsets and multi-view resizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalImageCellClip {
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
}

impl TerminalImageCellClip {
    pub const MAX_EXCLUSIVE_CELL: i32 = 65_536;

    pub fn validate(self) -> Result<(), ImageBoundError> {
        if self.top < 0
            || self.left < 0
            || self.top >= self.bottom
            || self.left >= self.right
            || self.bottom > Self::MAX_EXCLUSIVE_CELL
            || self.right > Self::MAX_EXCLUSIVE_CELL
        {
            return Err(ImageBoundError::InvalidPlacementClip);
        }
        Ok(())
    }

    #[must_use]
    pub fn intersection(self, other: Self) -> Option<Self> {
        let intersection = Self {
            top: self.top.max(other.top),
            left: self.left.max(other.left),
            bottom: self.bottom.min(other.bottom),
            right: self.right.min(other.right),
        };
        (intersection.top < intersection.bottom && intersection.left < intersection.right)
            .then_some(intersection)
    }

    /// Half-open row containment.
    #[must_use]
    pub fn contains_row(self, row: i32) -> bool {
        row >= self.top && row < self.bottom
    }

    /// Half-open column containment.
    #[must_use]
    pub fn contains_column(self, column: i32) -> bool {
        column >= self.left && column < self.right
    }

    /// Translate this mask upward by a scroll of `rows` lines.
    #[must_use]
    pub fn shifted_rows(self, rows: i32) -> Self {
        Self {
            top: self.top.saturating_sub(rows),
            bottom: self.bottom.saturating_sub(rows),
            ..self
        }
    }
}

/// One canonical terminal-cell placement. All geometry is scalar and bounded
/// by the referenced definition and terminal viewport at application time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalImagePlacement {
    pub id: TerminalPlacementId,
    pub image_id: TerminalImageId,
    pub generation: TerminalImageGeneration,
    pub protocol: TerminalImageProtocol,
    pub kind: TerminalImagePlacementKind,
    pub anchor: TerminalCellAnchor,
    pub source: PixelRect,
    pub destination: CellExtent,
    pub pixel_offset_x: u16,
    pub pixel_offset_y: u16,
    pub z_index: i32,
    pub scrolls_with_grid: bool,
    pub move_cursor: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell_clip: Option<TerminalImageCellClip>,
    #[serde(default)]
    pub placeholder: Option<PlaceholderMetadata>,
}

impl TerminalImagePlacement {
    /// Validate placement-only scalars before definition-bound source checks.
    pub fn validate_scalars(&self) -> Result<(), ImageBoundError> {
        if self.source.width == 0
            || self.source.height == 0
            || self.destination.columns == 0
            || self.destination.rows == 0
        {
            return Err(ImageBoundError::InvalidPlacementGeometry);
        }
        match self.kind {
            TerminalImagePlacementKind::KittyClassic
                if self.protocol == TerminalImageProtocol::Kitty && self.placeholder.is_none() => {}
            TerminalImagePlacementKind::KittyUnicodePlaceholder
                if self.protocol == TerminalImageProtocol::Kitty && self.placeholder.is_some() => {}
            TerminalImagePlacementKind::Sixel
                if self.protocol == TerminalImageProtocol::Sixel && self.placeholder.is_none() => {}
            _ => return Err(ImageBoundError::InvalidPlacementKind),
        }
        if self.kind == TerminalImagePlacementKind::KittyUnicodePlaceholder
            && self.cell_clip.is_some()
        {
            return Err(ImageBoundError::InvalidPlacementClip);
        }
        if let Some(clip) = self.cell_clip {
            clip.validate()?;
            let envelope = self.logical_cell_envelope()?;
            if envelope.intersection(clip) != Some(clip) {
                return Err(ImageBoundError::InvalidPlacementClip);
            }
        }
        Ok(())
    }

    /// Visible half-open cell mask, or `None` when nothing remains visible.
    ///
    /// Unicode placeholders occupy terminal cells the application owns, so
    /// they have no geometric envelope and never participate in area effects.
    #[must_use]
    pub fn effective_cell_clip(&self) -> Option<TerminalImageCellClip> {
        let envelope = self.logical_cell_envelope().ok()?;
        self.cell_clip.map_or(Some(envelope), |clip| envelope.intersection(clip))
    }

    /// Whether a half-open cell rectangle overlaps this placement's mask.
    #[must_use]
    pub fn intersects_cells(&self, area: TerminalImageCellClip) -> bool {
        self.effective_cell_clip().and_then(|effective| effective.intersection(area)).is_some()
    }

    /// Exact protocol-identity match for one Kitty delete command.
    ///
    /// Omitted operands stay `None` and match every value; an explicit `0`
    /// stays a literal comparison, so `i=0` can never behave as a wildcard.
    #[must_use]
    pub fn matches_delete(&self, delete: &TerminalImageDelete) -> bool {
        let image_matches = delete.image_id.is_none_or(|id| self.image_id == id);
        let placement_matches = delete.placement_id.is_none_or(|id| self.id == id);
        if self.kind == TerminalImagePlacementKind::KittyUnicodePlaceholder {
            return delete.scope == TerminalImageDeleteScope::Image
                && delete.image_id.is_some()
                && image_matches;
        }
        let effective = self.effective_cell_clip();
        match delete.scope {
            TerminalImageDeleteScope::AllPlacements => true,
            TerminalImageDeleteScope::Image => image_matches,
            TerminalImageDeleteScope::Placement => image_matches && placement_matches,
            TerminalImageDeleteScope::Cell => delete.coordinate.is_some_and(|coordinate| {
                effective.is_some_and(|clip| {
                    clip.contains_row(coordinate) || clip.contains_column(coordinate)
                })
            }),
            TerminalImageDeleteScope::Row => delete.coordinate.is_some_and(|coordinate| {
                effective.is_some_and(|clip| clip.contains_row(coordinate))
            }),
            TerminalImageDeleteScope::Column => delete.coordinate.is_some_and(|coordinate| {
                effective.is_some_and(|clip| clip.contains_column(coordinate))
            }),
            TerminalImageDeleteScope::ZIndex => delete.coordinate == Some(self.z_index),
        }
    }

    /// Move this placement through one half-open scroll margin.
    ///
    /// Returns `false` when nothing visible survives, so callers can `retain`
    /// on the result.
    pub fn apply_scroll(&mut self, margin: TerminalImageCellClip, rows: i32) -> bool {
        if rows == 0
            || !self.scrolls_with_grid
            || self.kind == TerminalImagePlacementKind::KittyUnicodePlaceholder
        {
            return true;
        }
        let Ok(old_envelope) = self.logical_cell_envelope() else { return false };
        let participates = match self.cell_clip {
            None => self.anchor.row >= margin.top && self.anchor.row < margin.bottom,
            Some(clip) => old_envelope
                .intersection(clip)
                .and_then(|effective| effective.intersection(margin))
                .is_some(),
        };
        if !participates {
            return true;
        }
        self.anchor.row = self.anchor.row.saturating_sub(rows);
        let Ok(new_envelope) = self.logical_cell_envelope() else { return false };
        let candidate = self.cell_clip.map_or(margin, |clip| clip.shifted_rows(rows));
        self.cell_clip = new_envelope
            .intersection(candidate)
            .and_then(|effective| effective.intersection(margin));
        self.cell_clip.is_some()
    }

    /// Intersect this placement's mask with a half-open viewport rectangle.
    ///
    /// Returns `false` when nothing visible survives, so callers can `retain`
    /// on the result.
    pub fn clip_to_viewport(&mut self, viewport: TerminalImageCellClip) -> bool {
        if self.kind == TerminalImagePlacementKind::KittyUnicodePlaceholder {
            return true;
        }
        let Ok(envelope) = self.logical_cell_envelope() else { return false };
        let candidate = self.cell_clip.unwrap_or(envelope);
        self.cell_clip =
            envelope.intersection(candidate).and_then(|effective| effective.intersection(viewport));
        self.cell_clip.is_some()
    }

    /// Checked conservative cell envelope for classic and Sixel pixels.
    pub fn logical_cell_envelope(&self) -> Result<TerminalImageCellClip, ImageBoundError> {
        if self.kind == TerminalImagePlacementKind::KittyUnicodePlaceholder {
            return Err(ImageBoundError::InvalidPlacementKind);
        }
        let top = self.anchor.row;
        let left = i32::from(self.anchor.column);
        let extra_row = i32::from(u8::from(self.pixel_offset_y > 0));
        let extra_column = i32::from(u8::from(self.pixel_offset_x > 0));
        let bottom = top
            .checked_add(i32::from(self.destination.rows))
            .and_then(|value| value.checked_add(extra_row))
            .ok_or(ImageBoundError::ArithmeticOverflow)?;
        let right = left
            .checked_add(i32::from(self.destination.columns))
            .and_then(|value| value.checked_add(extra_column))
            .ok_or(ImageBoundError::ArithmeticOverflow)?;
        if top >= bottom || left >= right {
            return Err(ImageBoundError::InvalidPlacementGeometry);
        }
        Ok(TerminalImageCellClip { top, left, bottom, right })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalScreenKind {
    Primary,
    Alternate,
}

/// Grid consequences committed in the same output sequence as image state.
///
/// Every row and column bound is half-open: the `top`/`left` edge is included
/// and the `bottom`/`right` edge is excluded, matching the server-side
/// Alacritty observation that produced it. An empty rectangle therefore has
/// `bottom <= top` or `right <= left` and affects nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalGridEffect {
    MoveCursor {
        row: i32,
        column: u16,
    },
    /// Half-open scroll region; `bottom` is the first row below the margin.
    Scroll {
        top: u16,
        bottom: u16,
        rows: i32,
    },
    /// Half-open erase rectangle; `bottom`/`right` are exclusive.
    EraseCells {
        top: u16,
        left: u16,
        bottom: u16,
        right: u16,
    },
    ResizeClip {
        columns: u16,
        rows: u16,
    },
    SwitchScreen {
        screen: TerminalScreenKind,
    },
    SoftReset,
    HardReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalImageDeleteScope {
    AllPlacements,
    Image,
    Placement,
    Cell,
    Row,
    Column,
    ZIndex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalImageDelete {
    pub scope: TerminalImageDeleteScope,
    pub image_id: Option<TerminalImageId>,
    pub placement_id: Option<TerminalPlacementId>,
    pub coordinate: Option<i32>,
    pub free_image_data: bool,
}

/// Exact payload-free rejection taxonomy frozen by terminal-images v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalImageRejectionReason {
    PolicyDisabled,
    UnsupportedProtocol,
    UnsupportedAction,
    UnsupportedTransport,
    MalformedFraming,
    MalformedControl,
    MalformedPayload,
    TruncatedSequence,
    ChunkMismatch,
    InvalidDimensions,
    QuotaExceeded,
    WorkBudgetExceeded,
    DecodeDeadlineExceeded,
    DecodeCancelled,
    DecodeFailed,
    ImageNotFound,
    CapabilityMismatch,
    RendererUnavailable,
    Evicted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalImageAction {
    Transmit,
    TransmitAndDisplay,
    Place,
    Query,
    Delete,
    Decode,
    Replay,
    Render,
}

/// Safe diagnostic metadata. It intentionally has no string or byte field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalImageRejection {
    pub reason: TerminalImageRejectionReason,
    pub protocol: Option<TerminalImageProtocol>,
    pub action: Option<TerminalImageAction>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub observed: Option<u64>,
    pub limit: Option<ImageLimitName>,
}

/// Canonical live scene operation. Definition bytes are always chunked.
///
/// `screen` names the grid a placement operation owns. It is omitted at its
/// legacy default, where the receiver uses whichever screen is active, so
/// existing encoded records keep their exact bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalImageUpdate {
    Define {
        definition: TerminalImageDefinition,
    },
    DefinitionChunk {
        chunk: TerminalImageDataChunk,
    },
    Place {
        placement: TerminalImagePlacement,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        screen: Option<TerminalScreenKind>,
    },
    Delete {
        delete: TerminalImageDelete,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        screen: Option<TerminalScreenKind>,
    },
    GridEffect {
        effect: TerminalGridEffect,
    },
    Rejected {
        rejection: TerminalImageRejection,
    },
}

/// Live updates use explicit begin/commit boundaries, binding raw PTY output
/// and canonical image effects to one monotonic sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalImageLiveMessage {
    Begin {
        generation: TerminalImageGeneration,
        sequence: TerminalOutputSequence,
    },
    Update {
        generation: TerminalImageGeneration,
        sequence: TerminalOutputSequence,
        update: TerminalImageUpdate,
    },
    Commit {
        generation: TerminalImageGeneration,
        sequence: TerminalOutputSequence,
    },
}

/// Generation-tagged snapshot records. Clients stage every record until the
/// matching commit and never expose a partial scene.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalImageReplayMessage {
    Begin {
        generation: TerminalImageGeneration,
        after_sequence: TerminalOutputSequence,
        definition_count: u32,
        placement_count: u32,
        total_rgba_bytes: u64,
    },
    Definition {
        generation: TerminalImageGeneration,
        definition: TerminalImageDefinition,
    },
    DefinitionChunk {
        generation: TerminalImageGeneration,
        chunk: TerminalImageDataChunk,
    },
    Placement {
        generation: TerminalImageGeneration,
        placement: TerminalImagePlacement,
    },
    Commit {
        generation: TerminalImageGeneration,
        through_sequence: TerminalOutputSequence,
    },
}

impl TerminalImageReplayMessage {
    /// Validate scalar replay metadata immediately after decode.
    pub fn validate(&self) -> Result<(), ImageBoundError> {
        match self {
            Self::Begin { definition_count, placement_count, total_rgba_bytes, .. } => {
                if *definition_count > ImageLimits::V1.max_images_per_session {
                    return Err(ImageBoundError::LimitExceeded(ImageLimitName::ImagesPerSession));
                }
                if *placement_count > ImageLimits::V1.max_placements_per_session {
                    return Err(ImageBoundError::LimitExceeded(
                        ImageLimitName::PlacementsPerSession,
                    ));
                }
                if *total_rgba_bytes > ImageLimits::V1.max_session_retained_cpu_bytes {
                    return Err(ImageBoundError::LimitExceeded(
                        ImageLimitName::SessionRetainedCpuBytes,
                    ));
                }
                Ok(())
            }
            Self::Definition { generation, definition } => {
                if *generation != definition.generation {
                    return Err(ImageBoundError::InconsistentGeneration);
                }
                definition.validate()
            }
            Self::DefinitionChunk { generation, chunk } => {
                if *generation != chunk.generation {
                    return Err(ImageBoundError::InconsistentGeneration);
                }
                Ok(())
            }
            Self::Placement { generation, placement } => {
                if *generation != placement.generation {
                    return Err(ImageBoundError::InconsistentGeneration);
                }
                placement.validate_scalars()
            }
            Self::Commit { .. } => Ok(()),
        }
    }
}

/// Typed local attach refusal when an image-enabled session cannot be rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalImageCapabilityMismatch {
    pub required: TerminalImageCapabilities,
    pub offered: TerminalImageCapabilities,
}

impl TerminalImageCapabilityMismatch {
    #[must_use]
    pub fn new(
        required: TerminalImageCapabilities,
        offered: TerminalImageCapabilities,
    ) -> Option<Self> {
        (!offered.supports(required)).then_some(Self { required, offered })
    }
}

/// Which remote endpoint must update after an exact-version mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProtocolUpdateTarget {
    Client,
    Server,
}

/// Typed exact-version mismatch carried in remote handshake replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteProtocolMismatch {
    pub client_version: u32,
    pub server_version: u32,
    pub update: RemoteProtocolUpdateTarget,
}

impl RemoteProtocolMismatch {
    #[must_use]
    pub fn between(client_version: u32, server_version: u32) -> Option<Self> {
        let update = match client_version.cmp(&server_version) {
            std::cmp::Ordering::Less => RemoteProtocolUpdateTarget::Client,
            std::cmp::Ordering::Greater => RemoteProtocolUpdateTarget::Server,
            std::cmp::Ordering::Equal => return None,
        };
        Some(Self { client_version, server_version, update })
    }
}
