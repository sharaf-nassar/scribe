//! Linux-only paint captures. Normal scenes and unsupported targets stay direct.

use super::{GlobalParams, SurfaceParams, WgpuRenderer};
use gpui::{EntityId, PrimitiveBatch, RetainedLayer, Scene};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const LAYER_BUDGET: u64 = 64 * 1024 * 1024;
static CAPTURES: AtomicU64 = AtomicU64::new(0);
static DEFERRED: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
static COMPOSITES: AtomicU64 = AtomicU64::new(0);
static FALLBACKS: AtomicU64 = AtomicU64::new(0);
static UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static CAPTURE_OPERATIONS: AtomicU64 = AtomicU64::new(0);

pub(super) struct CachedLayer {
    revision: u64,
    width: u32,
    height: u32,
    bytes: u64,
    /// Whether this texture was ever composited. A texture that is replaced
    /// having never been reused cost a capture and bought nothing.
    reused: bool,
    // Keep the render attachment alive alongside its sampled view.
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// How many consecutive repeats of one revision earn a capture, and how that
/// grows for an owner whose captures keep going to waste.
///
/// A pane that changes every frame can still pause for a single frame, which is
/// all the base rule asks for, so it is captured and invalidated immediately
/// and forever: the concurrent-output workload recorded captures with zero
/// hits. Doubling the requirement after each wasted texture makes that cost
/// self-limiting, while a pane that goes quiet still gets retained within a few
/// frames. Reuse resets it, so a pane is never permanently punished for a busy
/// period.
#[derive(Clone, Copy)]
pub(super) struct Admission {
    revision: u64,
    /// Consecutive frames this owner has presented `revision`.
    streak: u32,
    /// Repeats currently required before capturing.
    required: u32,
}

impl Admission {
    /// A revision has to survive into a second frame before it is worth a
    /// texture. Capturing on first sight would pay for every transient frame.
    const BASE_REQUIRED: u32 = 2;

    /// Never wait longer than this, so a pane that settles is always retained.
    const MAX_REQUIRED: u32 = 16;

    const fn first_seen(revision: u64) -> Self {
        Self {
            revision,
            streak: 1,
            required: Self::BASE_REQUIRED,
        }
    }

    /// Count another frame of the same revision, or restart on a new one.
    fn observe(&mut self, revision: u64) {
        if self.revision == revision {
            self.streak = self.streak.saturating_add(1);
        } else {
            self.revision = revision;
            self.streak = 1;
        }
    }

    const fn earned(self) -> bool {
        self.streak >= self.required
    }

    /// A capture went unused: ask for twice as much stability next time.
    fn penalise(&mut self) {
        self.required = self
            .required
            .saturating_mul(2)
            .clamp(Self::BASE_REQUIRED, Self::MAX_REQUIRED);
    }

    /// A texture was composited, so this owner's content really does rest.
    fn reward(&mut self) {
        self.required = Self::BASE_REQUIRED;
    }
}

impl Drop for CachedLayer {
    fn drop(&mut self) {
        LIVE_BYTES.fetch_sub(self.bytes, Relaxed);
    }
}

pub(super) type LayerCache = HashMap<EntityId, CachedLayer>;
pub(super) type RevisionHistory = HashMap<EntityId, Admission>;

impl WgpuRenderer {
    // Captures submit independently before main-frame writes. No shared globals
    // or instance range can be overwritten ahead of an unsubmitted capture.
    pub(super) fn prepare_retained_layers(&mut self, scene: &Scene, globals: GlobalParams) -> bool {
        let present = |owner: &EntityId| scene.retained_layers.iter().any(|l| l.owner == *owner);
        self.resources_mut()
            .retained
            .retain(|owner, _| present(owner));
        self.resources_mut()
            .retained_revisions
            .retain(|owner, _| present(owner));
        // Both existing pipeline modes store premultiplied attachment pixels:
        // PreMultiplied multiplies in blend_color; other modes multiply through
        // ALPHA_BLENDING's SrcAlpha factor. Surface presentation mode does not
        // change that offscreen representation. Composite it with factor One.
        if !matches!(
            self.surface_config.format,
            wgpu::TextureFormat::Bgra8Unorm
                | wgpu::TextureFormat::Bgra8UnormSrgb
                | wgpu::TextureFormat::Rgba8Unorm
                | wgpu::TextureFormat::Rgba8UnormSrgb
        ) {
            self.resources_mut().retained.clear();
            self.resources_mut().retained_revisions.clear();
            return false;
        }
        let mut wrote_globals = false;
        for layer in &scene.retained_layers {
            let admission = self
                .resources_mut()
                .retained_revisions
                .entry(layer.owner)
                .and_modify(|admission| admission.observe(layer.revision))
                .or_insert_with(|| Admission::first_seen(layer.revision));
            let earned = admission.earned();
            let width = layer.bounds.size.width.0 as u32;
            let height = layer.bounds.size.height.0 as u32;
            if let Some(entry) = self.resources_mut().retained.get_mut(&layer.owner)
                && entry.revision == layer.revision
                && entry.width == width
                && entry.height == height
            {
                entry.reused = true;
                HITS.fetch_add(1, Relaxed);
                if let Some(admission) = self
                    .resources_mut()
                    .retained_revisions
                    .get_mut(&layer.owner)
                {
                    admission.reward();
                }
                continue;
            }
            // Paint changing bases directly. Pay for a texture only after the
            // same revision has survived long enough to be worth one; never
            // delay its display.
            if !earned {
                DEFERRED.fetch_add(1, Relaxed);
                continue;
            }
            if width == 0
                || height == 0
                || width > self.max_texture_size
                || height > self.max_texture_size
            {
                self.resources_mut().retained.remove(&layer.owner);
                continue;
            }
            let bytes = u64::from(width) * u64::from(height) * 4;
            let previous_bytes = self
                .resources()
                .retained
                .get(&layer.owner)
                .map_or(0, |entry| entry.bytes);
            let used: u64 = self
                .resources()
                .retained
                .values()
                .map(|entry| entry.bytes)
                .sum();
            if bytes > LAYER_BUDGET || used - previous_bytes + bytes > LAYER_BUDGET {
                self.resources_mut().retained.remove(&layer.owner);
                continue;
            }
            let previous = self.resources_mut().retained.remove(&layer.owner);
            // Replacing a texture nothing ever composited means the last
            // capture was spent on content that did not rest. Charge for it
            // before spending another.
            if previous.as_ref().is_some_and(|entry| !entry.reused)
                && let Some(admission) = self
                    .resources_mut()
                    .retained_revisions
                    .get_mut(&layer.owner)
            {
                admission.penalise();
            }
            let mut entry = match previous {
                Some(mut entry) if entry.width == width && entry.height == height => {
                    // The texture survives, but it is about to hold different
                    // content. Carrying the old flag forward would let an
                    // owner that never reuses anything keep escaping the
                    // backoff by reusing the same allocation each time.
                    entry.reused = false;
                    entry
                }
                previous => {
                    drop(previous);
                    let texture =
                        self.resources()
                            .device
                            .create_texture(&wgpu::TextureDescriptor {
                                label: Some("retained_terminal_base"),
                                size: wgpu::Extent3d {
                                    width,
                                    height,
                                    depth_or_array_layers: 1,
                                },
                                mip_level_count: 1,
                                sample_count: 1,
                                dimension: wgpu::TextureDimension::D2,
                                format: self.surface_config.format,
                                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                                    | wgpu::TextureUsages::TEXTURE_BINDING,
                                view_formats: &[],
                            });
                    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                    LIVE_BYTES.fetch_add(bytes, Relaxed);
                    CachedLayer {
                        revision: 0,
                        reused: false,
                        width,
                        height,
                        bytes,
                        _texture: texture,
                        view,
                    }
                }
            };
            let capture_globals = GlobalParams {
                viewport_size: [width as f32, height as f32],
                viewport_origin: [layer.bounds.origin.x.0, layer.bounds.origin.y.0],
                ..globals
            };
            wrote_globals = true;
            self.resources().queue.write_buffer(
                &self.resources().globals_buffer,
                0,
                bytemuck::bytes_of(&capture_globals),
            );
            loop {
                let mut encoder = self.resources().device.create_command_encoder(
                    &wgpu::CommandEncoderDescriptor {
                        label: Some("retained_terminal_capture"),
                    },
                );
                let mut timing = self
                    .resources()
                    .gpu_timings
                    .as_ref()
                    .and_then(|timings| timings.begin(crate::gpu_timing::Kind::Capture));
                let mut offset = 0;
                let ok = {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("retained_terminal_capture"),
                        timestamp_writes: timing
                            .as_ref()
                            .and_then(|timing| timing.writes(true, true)),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &entry.view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                store: wgpu::StoreOp::Store,
                            },
                            depth_slice: None,
                        })],
                        depth_stencil_attachment: None,
                        ..Default::default()
                    });
                    self.draw_retained_contents(&layer.scene, &mut offset, &mut pass)
                };
                // Includes aligned spans written during an overflow attempt.
                UPLOAD_BYTES.fetch_add(offset, Relaxed);
                if ok {
                    if let Some(timing) = &timing {
                        timing.resolve(&mut encoder);
                    }
                    self.resources()
                        .queue
                        .submit(std::iter::once(encoder.finish()));
                    if let Some(ticket) = timing.take() {
                        ticket.submitted();
                    }
                    // Ends the borrow of `self` the ticket holds before resources_mut.
                    drop(timing);
                    entry.revision = layer.revision;
                    CAPTURES.fetch_add(1, Relaxed);
                    CAPTURE_OPERATIONS.fetch_add(layer.scene.len() as u64, Relaxed);
                    self.resources_mut().retained.insert(layer.owner, entry);
                    break;
                }
                drop(encoder);
                drop(timing);
                if self.instance_buffer_capacity >= self.max_buffer_size {
                    // The capture never landed. Keep the allocation for the
                    // next attempt, but with no revision so it is never
                    // composited until a capture succeeds.
                    entry.revision = 0;
                    self.resources_mut().retained.insert(layer.owner, entry);
                    break;
                }
                self.grow_instance_buffer();
            }
        }
        wrote_globals
    }

    fn draw_retained_contents(
        &self,
        scene: &Scene,
        offset: &mut u64,
        pass: &mut wgpu::RenderPass<'_>,
    ) -> bool {
        scene.batches().all(|batch| match batch {
            PrimitiveBatch::Quads(range) => self.draw_quads(&scene.quads[range], offset, pass),
            PrimitiveBatch::Underlines(range) => {
                self.draw_underlines(&scene.underlines[range], offset, pass)
            }
            PrimitiveBatch::MonochromeSprites { texture_id, range } => self
                .draw_monochrome_sprites(
                    &scene.monochrome_sprites[range],
                    texture_id,
                    offset,
                    pass,
                ),
            PrimitiveBatch::PolychromeSprites { texture_id, range } => self
                .draw_polychrome_sprites(
                    &scene.polychrome_sprites[range],
                    texture_id,
                    offset,
                    pass,
                ),
            // Window::paint_retained_layer rejects these before emitting a layer.
            _ => false,
        })
    }

    pub(super) fn draw_retained_layer(
        &self,
        layer: &RetainedLayer,
        offset: &mut u64,
        pass: &mut wgpu::RenderPass<'_>,
    ) -> bool {
        if let Some(entry) = self
            .resources()
            .retained
            .get(&layer.owner)
            .filter(|entry| entry.revision == layer.revision)
        {
            let params = SurfaceParams {
                bounds: layer.bounds.into(),
                content_mask: layer.content_mask.bounds.into(),
            };
            let ok = self.draw_instances_with_texture(
                bytemuck::bytes_of(&params),
                1,
                &entry.view,
                &self.resources().pipelines.retained_layers,
                offset,
                pass,
            );
            if ok {
                COMPOSITES.fetch_add(1, Relaxed);
            }
            ok
        } else {
            FALLBACKS.fetch_add(1, Relaxed);
            self.draw_retained_contents(&layer.scene, offset, pass)
        }
    }
}

// Opt-in diagnostic only. Logical live bytes exclude driver-held in-flight
// references after a cache entry is dropped, not an absolute VRAM ceiling.
pub(super) fn flush_probe() {
    struct Probe {
        path: std::path::PathBuf,
        last: Mutex<Instant>,
    }
    static PROBE: OnceLock<Option<Probe>> = OnceLock::new();
    let Some(probe) = PROBE.get_or_init(|| {
        std::env::var_os("GPUI_RETAINED_LAYER_PROBE").map(|path| Probe {
            path: path.into(),
            last: Mutex::new(Instant::now() - Duration::from_secs(1)),
        })
    }) else {
        return;
    };
    let mut last = probe.last.lock().unwrap();
    if last.elapsed() < Duration::from_millis(200) {
        return;
    }
    *last = Instant::now();
    let mut report = format!(
        "captures={}\nhits={}\ncomposites={}\nfallbacks={}\nupload_span_bytes={}\nlive_bytes={}\ncapture_operations={}\n",
        CAPTURES.load(Relaxed),
        HITS.load(Relaxed),
        COMPOSITES.load(Relaxed),
        FALLBACKS.load(Relaxed),
        UPLOAD_BYTES.load(Relaxed),
        LIVE_BYTES.load(Relaxed),
        CAPTURE_OPERATIONS.load(Relaxed),
    );
    report.push_str(&format!("capture_deferrals={}\n", DEFERRED.load(Relaxed)));
    report.push_str(&crate::gpu_timing::report());
    if let Err(error) = std::fs::write(&probe.path, report) {
        log::warn!("retained layer probe: {error}");
    }
}
