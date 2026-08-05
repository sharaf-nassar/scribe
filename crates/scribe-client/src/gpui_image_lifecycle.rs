//! Bounded GPUI resources for canonical terminal images.
//!
//! The pinned GPUI revision keys its atlas by `RenderImage` identity and frame,
//! so every source definition owns one image regardless of placement count or
//! crop. Cropping translates and scales that full image under a destination
//! content mask; it does not allocate per-placement variants.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use gpui::{Bounds, ContentMask, Corners, Pixels, RenderImage, Window, point, px, size};
use image::{Frame, ImageBuffer, Rgba};
use scribe_common::{
    ids::SessionId,
    terminal_images::{
        ImageBoundError, ImageLimitName, ImageLimits, PixelRect, TerminalImageDefinition,
        TerminalImageGeneration, TerminalImageId, TerminalImageRejectionReason,
    },
};

/// Stable identity of one decoded source generation in a window-local cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GpuiImageKey {
    pub session_id: Option<SessionId>,
    pub image_id: TerminalImageId,
    pub generation: TerminalImageGeneration,
}

impl From<&TerminalImageDefinition> for GpuiImageKey {
    fn from(definition: &TerminalImageDefinition) -> Self {
        Self { session_id: None, image_id: definition.id, generation: definition.generation }
    }
}

impl GpuiImageKey {
    /// Window-local identity for a source owned by one terminal session.
    #[must_use]
    pub fn for_session(session_id: SessionId, definition: &TerminalImageDefinition) -> Self {
        Self {
            session_id: Some(session_id),
            image_id: definition.id,
            generation: definition.generation,
        }
    }
}

/// Observable cache counters used by the isolated lifecycle spike.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GpuiImageCacheStats {
    pub render_images_created: u64,
    pub cache_reuses: u64,
    pub pressure_rejections: u64,
    pub atlas_drops: u64,
    pub final_reference_drops: u64,
}

/// Typed failures at the canonical-RGBA to GPUI boundary.
#[derive(Debug, thiserror::Error)]
pub enum GpuiImageError {
    #[error(transparent)]
    Bound(#[from] ImageBoundError),
    #[error("canonical RGBA bytes do not match image metadata")]
    CanonicalLength,
    #[error("an image key was reused with different metadata")]
    ConflictingDefinition,
    #[error("canonical RGBA allocation failed")]
    AllocationFailed,
    #[error("source crop is outside the canonical image")]
    InvalidSourceCrop,
    #[error("destination image bounds must be positive and finite")]
    InvalidDestination,
    #[error("GPUI image atlas cleanup failed")]
    DropImage(#[source] anyhow::Error),
    #[error("GPUI image paint failed")]
    PaintImage(#[source] anyhow::Error),
}

impl GpuiImageError {
    /// Whether this failure means the renderer itself could not be used, as
    /// opposed to a bounded rejection of one particular image.
    ///
    /// Only the two window operations qualify. A limit, a bad crop, or an
    /// inconsistent definition is Scribe refusing specific data and says
    /// nothing about whether the GPU path works.
    #[must_use]
    pub const fn is_renderer_failure(&self) -> bool {
        matches!(self, Self::DropImage(_) | Self::PaintImage(_))
    }

    /// The payload-free diagnostic category the user is shown for this
    /// failure. Carries no message text of its own, so nothing derived from
    /// image bytes can reach the UI through this path.
    // @lat: [[terminal-images#Terminal Images#Localized Image Diagnostics]]
    #[must_use]
    pub const fn rejection_reason(&self) -> TerminalImageRejectionReason {
        match self {
            Self::DropImage(_) | Self::PaintImage(_) => {
                TerminalImageRejectionReason::RendererUnavailable
            }
            Self::Bound(ImageBoundError::LimitExceeded(_)) => {
                TerminalImageRejectionReason::QuotaExceeded
            }
            Self::Bound(_) | Self::InvalidSourceCrop | Self::InvalidDestination => {
                TerminalImageRejectionReason::InvalidDimensions
            }
            Self::CanonicalLength | Self::ConflictingDefinition | Self::AllocationFailed => {
                TerminalImageRejectionReason::DecodeFailed
            }
        }
    }
}

struct CacheEntry {
    definition: TerminalImageDefinition,
    image: Arc<RenderImage>,
    projected_gpu_bytes: u64,
}

/// Window-local source cache bounded by frozen projected-GPU bytes.
pub struct GpuiImageCache {
    entries: HashMap<GpuiImageKey, CacheEntry>,
    insertion_order: VecDeque<GpuiImageKey>,
    projected_gpu_bytes: u64,
    max_projected_gpu_bytes: u64,
    stats: GpuiImageCacheStats,
    renderer_unavailable: bool,
}

impl Default for GpuiImageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuiImageCache {
    /// Construct an empty cache using the frozen v1 per-view ceiling.
    #[must_use]
    pub fn new() -> Self {
        Self::with_projected_gpu_limit(ImageLimits::V1.max_view_projected_gpu_bytes)
    }

    /// Construct a cache with a smaller view-local ceiling for isolated
    /// renderer pressure probes. The frozen production ceiling remains the
    /// upper bound even when callers request more.
    #[must_use]
    pub fn with_projected_gpu_limit(max_projected_gpu_bytes: u64) -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            projected_gpu_bytes: 0,
            max_projected_gpu_bytes: max_projected_gpu_bytes
                .min(ImageLimits::V1.max_view_projected_gpu_bytes),
            stats: GpuiImageCacheStats::default(),
            renderer_unavailable: false,
        }
    }

    /// Whether this view's renderer is currently considered unusable.
    ///
    /// Latched by [`Self::note_renderer_failure`] and cleared by the first
    /// later success, so the pane shows the localized notice for exactly as
    /// long as the GPU path is actually broken.
    #[must_use]
    pub const fn renderer_unavailable(&self) -> bool {
        self.renderer_unavailable
    }

    /// Record a renderer failure for one session and release everything that
    /// session still holds on the GPU.
    ///
    /// Cleanup is the point: a window operation that failed leaves textures
    /// this cache can no longer account for, so the session's sources are
    /// dropped rather than retried every frame. Text is untouched — the pane
    /// keeps painting glyphs and the application's own textual fallback.
    // @lat: [[terminal-images#Terminal Images#Renderer Failure Cleanup]]
    pub fn note_renderer_failure(
        &mut self,
        session_id: SessionId,
        window: &mut Window,
    ) -> Result<(), GpuiImageError> {
        self.renderer_unavailable = true;
        self.clear_session(session_id, window)
    }

    /// Current projected upload-plus-texture charge.
    #[must_use]
    pub fn projected_gpu_bytes(&self) -> u64 {
        self.projected_gpu_bytes
    }

    /// Cache counters accumulated since construction.
    #[must_use]
    pub fn stats(&self) -> GpuiImageCacheStats {
        self.stats
    }

    /// Return an existing source or allocate one after all frozen checks pass.
    pub fn get_or_insert(
        &mut self,
        definition: &TerminalImageDefinition,
        canonical_rgba: &[u8],
        window: &mut Window,
    ) -> Result<Arc<RenderImage>, GpuiImageError> {
        self.get_or_insert_key(GpuiImageKey::from(definition), definition, canonical_rgba, window)
    }

    /// Return or create the source scoped to one session in this window.
    pub fn get_or_insert_for_session(
        &mut self,
        session_id: SessionId,
        definition: &TerminalImageDefinition,
        canonical_rgba: &[u8],
        window: &mut Window,
    ) -> Result<Arc<RenderImage>, GpuiImageError> {
        self.get_or_insert_key(
            GpuiImageKey::for_session(session_id, definition),
            definition,
            canonical_rgba,
            window,
        )
    }

    fn get_or_insert_key(
        &mut self,
        key: GpuiImageKey,
        definition: &TerminalImageDefinition,
        canonical_rgba: &[u8],
        _window: &mut Window,
    ) -> Result<Arc<RenderImage>, GpuiImageError> {
        validate_canonical(definition, canonical_rgba)?;
        if let Some(entry) = self.entries.get(&key) {
            if entry.definition != *definition {
                return Err(GpuiImageError::ConflictingDefinition);
            }
            self.stats.cache_reuses = self.stats.cache_reuses.saturating_add(1);
            return Ok(Arc::clone(&entry.image));
        }

        let projected_gpu_bytes =
            definition.rgba_bytes.checked_mul(2).ok_or(ImageBoundError::ArithmeticOverflow)?;
        if projected_gpu_bytes > self.max_projected_gpu_bytes {
            self.stats.pressure_rejections = self.stats.pressure_rejections.saturating_add(1);
            return Err(
                ImageBoundError::LimitExceeded(ImageLimitName::ViewProjectedGpuBytes).into()
            );
        }
        if self
            .projected_gpu_bytes
            .checked_add(projected_gpu_bytes)
            .ok_or(ImageBoundError::ArithmeticOverflow)?
            > self.max_projected_gpu_bytes
        {
            // Existing entries may already be referenced by primitives queued
            // earlier in this frame. Never drop or reuse their atlas tiles
            // during admission; reject this source and retry on a later frame
            // after stale/unplaced cleanup has run before painting.
            self.stats.pressure_rejections = self.stats.pressure_rejections.saturating_add(1);
            return Err(
                ImageBoundError::LimitExceeded(ImageLimitName::ViewProjectedGpuBytes).into()
            );
        }

        // No GPUI object or backing buffer is created before the checks above.
        let image = build_render_image(definition, canonical_rgba)?;
        self.projected_gpu_bytes = self
            .projected_gpu_bytes
            .checked_add(projected_gpu_bytes)
            .ok_or(ImageBoundError::ArithmeticOverflow)?;
        self.entries.insert(
            key,
            CacheEntry {
                definition: definition.clone(),
                image: Arc::clone(&image),
                projected_gpu_bytes,
            },
        );
        self.insertion_order.push_back(key);
        self.stats.render_images_created = self.stats.render_images_created.saturating_add(1);
        // The only place a view's projected GPU charge changes upward, and the
        // only observable moment of a first upload. Resource review reads its
        // GPU numbers here rather than re-deriving them from definitions.
        tracing::info!(
            image_id = definition.id.0,
            generation = definition.generation.0,
            width = definition.width,
            height = definition.height,
            entry_projected_gpu_bytes = projected_gpu_bytes,
            projected_gpu_bytes = self.projected_gpu_bytes,
            sources = self.entries.len(),
            "terminal image source uploaded"
        );
        // A source built successfully, so whatever broke the renderer earlier
        // is over and the pane stops showing the unavailable notice.
        self.renderer_unavailable = false;
        Ok(image)
    }

    /// Remove sources from one session that no longer exist in its CPU scene.
    pub fn retain_session_definitions(
        &mut self,
        session_id: SessionId,
        live: &std::collections::HashSet<GpuiImageKey>,
        window: &mut Window,
    ) -> Result<(), GpuiImageError> {
        let stale = self
            .entries
            .keys()
            .copied()
            .filter(|key| key.session_id == Some(session_id) && !live.contains(key))
            .collect::<Vec<_>>();
        for key in stale {
            self.evict(key, window)?;
        }
        Ok(())
    }

    /// Remove every source belonging to a closed pane session.
    pub fn clear_session(
        &mut self,
        session_id: SessionId,
        window: &mut Window,
    ) -> Result<(), GpuiImageError> {
        let keys = self
            .entries
            .keys()
            .copied()
            .filter(|key| key.session_id == Some(session_id))
            .collect::<Vec<_>>();
        for key in keys {
            self.evict(key, window)?;
        }
        Ok(())
    }

    /// Drop cache entries whose owning pane session is no longer in the view.
    pub fn retain_sessions(
        &mut self,
        live_sessions: &std::collections::HashSet<SessionId>,
        window: &mut Window,
    ) -> Result<(), GpuiImageError> {
        let stale_sessions = self
            .entries
            .keys()
            .filter_map(|key| key.session_id)
            .filter(|session_id| !live_sessions.contains(session_id))
            .collect::<std::collections::HashSet<_>>();
        for session_id in stale_sessions {
            self.clear_session(session_id, window)?;
        }
        Ok(())
    }

    /// Look up a source without changing deterministic insertion order.
    #[must_use]
    pub fn get(&self, key: GpuiImageKey) -> Option<Arc<RenderImage>> {
        self.entries.get(&key).map(|entry| Arc::clone(&entry.image))
    }

    /// Remove one source and its frame keys from the current window atlas.
    pub fn evict(
        &mut self,
        key: GpuiImageKey,
        window: &mut Window,
    ) -> Result<bool, GpuiImageError> {
        self.insertion_order.retain(|candidate| *candidate != key);
        let Some(entry) = self.entries.remove(&key) else {
            return Ok(false);
        };
        self.drop_entry(entry, window)?;
        Ok(true)
    }

    /// Drop every atlas key while preserving CPU `RenderImage` identities.
    ///
    /// This is the Scribe-visible half of device recovery: GPUI clears its WGPU
    /// atlas, then the next paint lazily uploads these same CPU-backed images.
    pub fn invalidate_atlas(&mut self, window: &mut Window) -> Result<(), GpuiImageError> {
        for key in &self.insertion_order {
            let Some(entry) = self.entries.get(key) else {
                continue;
            };
            window.drop_image(Arc::clone(&entry.image)).map_err(GpuiImageError::DropImage)?;
            self.stats.atlas_drops = self.stats.atlas_drops.saturating_add(1);
        }
        Ok(())
    }

    /// Evict every source in deterministic insertion order.
    pub fn clear(&mut self, window: &mut Window) -> Result<(), GpuiImageError> {
        while self.evict_oldest(window)? {}
        Ok(())
    }

    fn evict_oldest(&mut self, window: &mut Window) -> Result<bool, GpuiImageError> {
        while let Some(key) = self.insertion_order.pop_front() {
            if let Some(entry) = self.entries.remove(&key) {
                self.drop_entry(entry, window)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn drop_entry(&mut self, entry: CacheEntry, window: &mut Window) -> Result<(), GpuiImageError> {
        self.projected_gpu_bytes =
            self.projected_gpu_bytes.saturating_sub(entry.projected_gpu_bytes);
        if Arc::strong_count(&entry.image) == 1 {
            self.stats.final_reference_drops = self.stats.final_reference_drops.saturating_add(1);
        }
        window.drop_image(entry.image).map_err(GpuiImageError::DropImage)?;
        self.stats.atlas_drops = self.stats.atlas_drops.saturating_add(1);
        Ok(())
    }
}

fn validate_canonical(
    definition: &TerminalImageDefinition,
    canonical_rgba: &[u8],
) -> Result<(), GpuiImageError> {
    definition.validate()?;
    let expected =
        usize::try_from(definition.rgba_bytes).map_err(|_| ImageBoundError::ArithmeticOverflow)?;
    if canonical_rgba.len() != expected {
        return Err(GpuiImageError::CanonicalLength);
    }
    Ok(())
}

fn build_render_image(
    definition: &TerminalImageDefinition,
    canonical_rgba: &[u8],
) -> Result<Arc<RenderImage>, GpuiImageError> {
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(canonical_rgba.len()).map_err(|_| GpuiImageError::AllocationFailed)?;
    for rgba in canonical_rgba.chunks_exact(4) {
        let [red, green, blue, alpha] = rgba else {
            return Err(GpuiImageError::CanonicalLength);
        };
        bgra.extend_from_slice(&[*blue, *green, *red, *alpha]);
    }
    let buffer = ImageBuffer::<Rgba<u8>, _>::from_raw(definition.width, definition.height, bgra)
        .ok_or(GpuiImageError::CanonicalLength)?;
    Ok(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
}

/// Paint one source rect through GPUI's existing bounds and content-mask APIs.
///
/// `paint_image` maps the whole atlas tile across `full_bounds`. Translating
/// those bounds by the source offset and clipping to `destination` selects the
/// requested source pixels without another upload or cached crop variant.
pub fn paint_cropped_image(
    window: &mut Window,
    image: Arc<RenderImage>,
    source_size: (u32, u32),
    source: PixelRect,
    destination: Bounds<Pixels>,
) -> Result<(), GpuiImageError> {
    paint_cropped_image_clipped(
        window,
        image,
        CroppedImageGeometry { source_size, source, destination, clip: destination },
    )
}

/// Source and destination geometry for one clipped image placement.
#[derive(Clone, Copy)]
pub struct CroppedImageGeometry {
    pub source_size: (u32, u32),
    pub source: PixelRect,
    pub destination: Bounds<Pixels>,
    pub clip: Bounds<Pixels>,
}

/// Paint a source rect with placement geometry distinct from its viewport clip.
///
/// Keeping `destination` unchanged while narrowing `clip` preserves scaling
/// across partially visible cells, negative rows, pixel offsets, and resizes.
pub fn paint_cropped_image_clipped(
    window: &mut Window,
    image: Arc<RenderImage>,
    geometry: CroppedImageGeometry,
) -> Result<(), GpuiImageError> {
    let CroppedImageGeometry { source_size, source, destination, clip } = geometry;
    ImageLimits::V1.canonical_rgba_len(source_size.0, source_size.1)?;
    let source_right =
        source.x.checked_add(source.width).ok_or(GpuiImageError::InvalidSourceCrop)?;
    let source_bottom =
        source.y.checked_add(source.height).ok_or(GpuiImageError::InvalidSourceCrop)?;
    if source.width == 0
        || source.height == 0
        || source_right > source_size.0
        || source_bottom > source_size.1
    {
        return Err(GpuiImageError::InvalidSourceCrop);
    }
    let destination_width = f32::from(destination.size.width);
    let destination_height = f32::from(destination.size.height);
    if !destination_width.is_finite()
        || !destination_height.is_finite()
        || destination_width <= 0.0
        || destination_height <= 0.0
    {
        return Err(GpuiImageError::InvalidDestination);
    }

    // Frozen dimensions are at most 4096, so u16 -> f32 is exact.
    let source_x =
        f32::from(u16::try_from(source.x).map_err(|_| GpuiImageError::InvalidSourceCrop)?);
    let source_y =
        f32::from(u16::try_from(source.y).map_err(|_| GpuiImageError::InvalidSourceCrop)?);
    let source_width =
        f32::from(u16::try_from(source.width).map_err(|_| GpuiImageError::InvalidSourceCrop)?);
    let source_height =
        f32::from(u16::try_from(source.height).map_err(|_| GpuiImageError::InvalidSourceCrop)?);
    let image_width =
        f32::from(u16::try_from(source_size.0).map_err(|_| GpuiImageError::InvalidSourceCrop)?);
    let image_height =
        f32::from(u16::try_from(source_size.1).map_err(|_| GpuiImageError::InvalidSourceCrop)?);
    let scale_x = destination_width / source_width;
    let scale_y = destination_height / source_height;
    let full_bounds = Bounds {
        origin: point(
            destination.origin.x - px(source_x * scale_x),
            destination.origin.y - px(source_y * scale_y),
        ),
        size: size(px(image_width * scale_x), px(image_height * scale_y)),
    };
    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        window
            .paint_image(full_bounds, Corners::default(), image, 0, false)
            .map_err(GpuiImageError::PaintImage)
    })
}
