//! Isolated running-window evidence for GPUI terminal-image lifecycle choices.

use std::sync::Arc;

use anyhow::{Context as _, ensure};
use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, FocusHandle, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, RenderImage, Styled as _,
    TitlebarOptions, Window, WindowBounds, WindowOptions, canvas, div, px, rgb, size,
};
use gpui_platform::application;
use scribe_common::terminal_images::{
    ImageBoundError, ImageLimitName, ImageLimits, PixelRect, TerminalImageDefinition,
    TerminalImageGeneration, TerminalImageId,
};

use crate::gpui_image_lifecycle::{GpuiImageCache, GpuiImageKey, paint_cropped_image};

const WINDOW_WIDTH: f32 = 640.0;
const WINDOW_HEIGHT: f32 = 320.0;

struct Fixture {
    definition: TerminalImageDefinition,
    rgba: Vec<u8>,
}

impl Fixture {
    fn solid(id: u64, width: u32, height: u32, rgba: [u8; 4]) -> anyhow::Result<Self> {
        let definition = TerminalImageDefinition::new(
            TerminalImageId(id),
            TerminalImageGeneration(1),
            width,
            height,
            rgba[3] != u8::MAX,
        )?;
        let byte_len = usize::try_from(definition.rgba_bytes)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(byte_len)?;
        for _ in 0..u64::from(width) * u64::from(height) {
            bytes.extend_from_slice(&rgba);
        }
        Ok(Self { definition, rgba: bytes })
    }

    fn quadrants(id: u64, width: u32, height: u32) -> anyhow::Result<Self> {
        let definition = TerminalImageDefinition::new(
            TerminalImageId(id),
            TerminalImageGeneration(1),
            width,
            height,
            false,
        )?;
        let byte_len = usize::try_from(definition.rgba_bytes)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(byte_len)?;
        for y in 0..height {
            for x in 0..width {
                let pixel = match (x < width / 2, y < height / 2) {
                    (true, true) => [255, 0, 0, 255],
                    (false, true) => [0, 255, 0, 255],
                    (true, false) => [0, 0, 255, 255],
                    (false, false) => [255, 255, 0, 255],
                };
                bytes.extend_from_slice(&pixel);
            }
        }
        Ok(Self { definition, rgba: bytes })
    }

    fn key(&self) -> GpuiImageKey {
        GpuiImageKey::from(&self.definition)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpikeStage {
    Initial,
    AtlasInvalidated,
    Recovered,
    Evicted,
    Recreated,
}

struct SpikeImages {
    source: Arc<RenderImage>,
    max_axis: Arc<RenderImage>,
    one_pixel: Arc<RenderImage>,
}

struct SpikeState {
    cache: GpuiImageCache,
    source: Fixture,
    max_axis: Fixture,
    one_pixel: Fixture,
    stage: SpikeStage,
    ready_logged: bool,
    ids_before_invalidation: Option<[usize; 3]>,
    ids_before_eviction: Option<[usize; 3]>,
}

struct GpuiImageSpike {
    focus: FocusHandle,
    state: Option<SpikeState>,
}

impl GpuiImageSpike {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let state = match SpikeState::new(window) {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::error!(%error, "GPUI image spike initialization failed");
                None
            }
        };
        Self { focus, state }
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if let Err(error) = state.handle_key(event, window, cx) {
            tracing::error!(%error, "GPUI image spike lifecycle action failed");
            self.state = None;
            cx.notify();
        }
    }
}

impl SpikeState {
    fn new(window: &mut Window) -> anyhow::Result<Self> {
        let source = Fixture::quadrants(1, 128, 128)?;
        let max_axis = Fixture::solid(2, ImageLimits::V1.max_width_pixels, 1, [0, 160, 255, 255])?;
        let one_pixel = Fixture::solid(3, 1, 1, [255, 0, 255, 255])?;

        let mut cache = GpuiImageCache::new();
        let created_before = cache.stats().render_images_created;
        let over_limit = TerminalImageDefinition {
            id: TerminalImageId(4),
            generation: TerminalImageGeneration(1),
            width: ImageLimits::V1.max_width_pixels + 1,
            height: 1,
            rgba_bytes: 0,
            has_alpha: false,
        };
        match cache.get_or_insert(&over_limit, &[], window) {
            Err(crate::gpui_image_lifecycle::GpuiImageError::Bound(
                ImageBoundError::LimitExceeded(ImageLimitName::Dimensions),
            )) => {}
            Err(error) => return Err(error.into()),
            Ok(_) => anyhow::bail!("max-plus-one reached GPUI allocation"),
        }
        ensure!(
            cache.stats().render_images_created == created_before,
            "max-plus-one changed RenderImage creation count"
        );
        tracing::info!(
            max_width = ImageLimits::V1.max_width_pixels,
            rejected_width = over_limit.width,
            render_images_created_before = created_before,
            render_images_created_after = cache.stats().render_images_created,
            "GPUI image max-plus-one rejected before allocation"
        );

        Ok(Self {
            cache,
            source,
            max_axis,
            one_pixel,
            stage: SpikeStage::Initial,
            ready_logged: false,
            ids_before_invalidation: None,
            ids_before_eviction: None,
        })
    }

    fn ensure_images(&mut self, window: &mut Window) -> anyhow::Result<SpikeImages> {
        let source =
            self.cache.get_or_insert(&self.source.definition, &self.source.rgba, window)?;
        let shared_placement =
            self.cache.get_or_insert(&self.source.definition, &self.source.rgba, window)?;
        ensure!(Arc::ptr_eq(&source, &shared_placement), "shared placement source diverged");
        let max_axis =
            self.cache.get_or_insert(&self.max_axis.definition, &self.max_axis.rgba, window)?;
        let one_pixel =
            self.cache.get_or_insert(&self.one_pixel.definition, &self.one_pixel.rgba, window)?;

        let ids = [source.id.0, max_axis.id.0, one_pixel.id.0];
        if self.stage == SpikeStage::AtlasInvalidated {
            let prior = self
                .ids_before_invalidation
                .context("atlas invalidation did not record prior IDs")?;
            ensure!(ids == prior, "atlas invalidation changed source identities");
            self.stage = SpikeStage::Recovered;
            tracing::info!(
                source_id = ids[0],
                max_axis_id = ids[1],
                one_pixel_id = ids[2],
                "GPUI image cache reused after atlas invalidation"
            );
        }
        if self.stage == SpikeStage::Evicted {
            let prior = self.ids_before_eviction.context("eviction did not record prior IDs")?;
            ensure!(
                ids.iter().zip(prior).all(|(new, old)| *new != old),
                "eviction retained a stale RenderImage identity"
            );
            self.stage = SpikeStage::Recreated;
            tracing::info!(
                old_source_id = prior[0],
                new_source_id = ids[0],
                old_max_id = prior[1],
                new_max_id = ids[1],
                old_one_pixel_id = prior[2],
                new_one_pixel_id = ids[2],
                "GPUI image cache recreated after final-reference eviction"
            );
        }
        if !self.ready_logged {
            self.ready_logged = true;
            tracing::info!(
                source_id = source.id.0,
                full_placement_id = source.id.0,
                crop_placement_id = shared_placement.id.0,
                max_axis_id = max_axis.id.0,
                one_pixel_id = one_pixel.id.0,
                projected_gpu_bytes = self.cache.projected_gpu_bytes(),
                render_images_created = self.cache.stats().render_images_created,
                cache_reuses = self.cache.stats().cache_reuses,
                "GPUI image spike ready"
            );
        }
        Ok(SpikeImages { source, max_axis, one_pixel })
    }

    fn cached_ids(&self) -> anyhow::Result<[usize; 3]> {
        Ok([
            self.cache.get(self.source.key()).context("source fixture is not cached")?.id.0,
            self.cache.get(self.max_axis.key()).context("max-axis fixture is not cached")?.id.0,
            self.cache.get(self.one_pixel.key()).context("one-pixel fixture is not cached")?.id.0,
        ])
    }

    fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<GpuiImageSpike>,
    ) -> anyhow::Result<()> {
        match event.keystroke.key.as_ref() {
            "d" => {
                let ids = self.cached_ids()?;
                self.cache.invalidate_atlas(window)?;
                self.ids_before_invalidation = Some(ids);
                self.stage = SpikeStage::AtlasInvalidated;
                tracing::info!(
                    source_id = ids[0],
                    max_axis_id = ids[1],
                    one_pixel_id = ids[2],
                    atlas_drops = self.cache.stats().atlas_drops,
                    "GPUI image atlas invalidated for recovery"
                );
                cx.notify();
            }
            "e" => {
                let ids = self.cached_ids()?;
                let final_before = self.cache.stats().final_reference_drops;
                self.cache.clear(window)?;
                let final_drops = self.cache.stats().final_reference_drops - final_before;
                ensure!(final_drops == 3, "not every cache entry reached its final reference");
                self.ids_before_eviction = Some(ids);
                self.stage = SpikeStage::Evicted;
                tracing::info!(
                    final_reference_drops = final_drops,
                    projected_gpu_bytes = self.cache.projected_gpu_bytes(),
                    "GPUI image cache evicted at final reference"
                );
                cx.notify();
            }
            _ => {}
        }
        Ok(())
    }
}

impl Render for GpuiImageSpike {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let images_result = self.state.as_mut().map(|state| state.ensure_images(window));
        let images = match images_result {
            Some(Ok(images)) => images,
            Some(Err(error)) => {
                tracing::error!(%error, "GPUI image spike render preparation failed");
                self.state = None;
                return failed_element(&self.focus);
            }
            None => return failed_element(&self.focus),
        };

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key))
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(24.0))
            .p(px(24.0))
            .bg(rgb(0x0010_1010))
            .child(image_canvas(
                Arc::clone(&images.source),
                (128, 128),
                PixelRect { x: 0, y: 0, width: 128, height: 128 },
                192.0,
                192.0,
            ))
            .child(image_canvas(
                images.source,
                (128, 128),
                PixelRect { x: 64, y: 0, width: 64, height: 64 },
                192.0,
                192.0,
            ))
            .child(image_canvas(
                images.max_axis,
                (ImageLimits::V1.max_width_pixels, 1),
                PixelRect { x: 0, y: 0, width: ImageLimits::V1.max_width_pixels, height: 1 },
                64.0,
                192.0,
            ))
            .child(image_canvas(
                images.one_pixel,
                (1, 1),
                PixelRect { x: 0, y: 0, width: 1, height: 1 },
                64.0,
                192.0,
            ))
            .into_any_element()
    }
}

fn failed_element(focus: &FocusHandle) -> AnyElement {
    div().track_focus(focus).size_full().bg(rgb(0x0030_0000)).into_any_element()
}

fn image_canvas(
    image: Arc<RenderImage>,
    source_size: (u32, u32),
    source: PixelRect,
    width: f32,
    height: f32,
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            if let Err(error) = paint_cropped_image(window, image, source_size, source, bounds) {
                tracing::error!(%error, "GPUI image spike paint failed");
            }
        },
    )
    .flex_none()
    .w(px(width))
    .h(px(height))
}

/// Run the isolated GPUI paint and lifecycle window.
pub fn run() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Scribe GPUI Image Spike".into()),
                    ..Default::default()
                }),
                app_id: Some("scribe-gpui-image-spike".to_owned()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| GpuiImageSpike::new(window, cx)),
        ) {
            Ok(_) => cx.activate(true),
            Err(error) => tracing::error!(%error, "failed to open GPUI image spike window"),
        }
    });
}
