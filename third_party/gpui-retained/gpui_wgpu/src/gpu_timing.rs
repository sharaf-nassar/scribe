//! Opt-in dev diagnostics. Bounded readback pools never wait for the GPU.

use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

static SAMPLES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static NANOSECONDS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static SKIPPED: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("SCRIBE_DEV_GPU_TIMESTAMPS").is_ok_and(|value| value == "1")
            && std::env::current_exe()
                .is_ok_and(|path| path.file_stem().is_some_and(|name| name == "scribe-dev"))
    })
}

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Main = 0,
    Capture = 1,
}

struct Slot {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    // 0 free, 1 reserved/pending, 2 mapped, 3 mapping failed.
    state: Arc<AtomicU8>,
}

// Process-global totals mix windows of very different sizes and frame shares, so
// they cannot show whether one window's frames became cheaper. These counters
// cost no GPU work: they only split samples that are already collected.
const RENDERERS: usize = 4;
static NEXT_RENDERER: AtomicUsize = AtomicUsize::new(0);
static MAIN_SAMPLES_BY_RENDERER: [AtomicU64; RENDERERS] =
    [const { AtomicU64::new(0) }; RENDERERS];
static MAIN_NS_BY_RENDERER: [AtomicU64; RENDERERS] = [const { AtomicU64::new(0) }; RENDERERS];

pub(crate) struct GpuTimings {
    // Separate pools prevent capture-heavy frames from starving main samples.
    pools: [Vec<Slot>; 2],
    period_ns: f64,
    // Renderers past the split share the last bucket, so totals stay exact.
    index: usize,
}

impl GpuTimings {
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Self> {
        if !enabled() || !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let period_ns = f64::from(queue.get_timestamp_period());
        if !period_ns.is_finite() || period_ns <= 0.0 {
            log::warn!("dev GPU timestamps unavailable: invalid period {period_ns}");
            return None;
        }
        let pools = [4, 32].map(|count| {
            (0..count)
                .map(|_| Slot {
                    queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                        label: Some("dev_gpu_timestamps"),
                        ty: wgpu::QueryType::Timestamp,
                        count: 2,
                    }),
                    resolve: device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("dev_gpu_timestamp_resolve"),
                        size: 256,
                        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                        mapped_at_creation: false,
                    }),
                    readback: device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("dev_gpu_timestamp_readback"),
                        size: 16,
                        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                        mapped_at_creation: false,
                    }),
                    state: Arc::new(AtomicU8::new(0)),
                })
                .collect()
        });
        log::info!(
            "dev GPU timestamps enabled: period_ns={period_ns}, main_slots=4, capture_slots=32"
        );
        let index = NEXT_RENDERER
            .fetch_add(1, Ordering::Relaxed)
            .min(RENDERERS - 1);
        Some(Self {
            pools,
            period_ns,
            index,
        })
    }

    pub(crate) fn collect(&self, device: &wgpu::Device) {
        if let Err(error) = device.poll(wgpu::PollType::Poll) {
            ERRORS.fetch_add(1, Ordering::Relaxed);
            log::warn!("dev GPU timestamp poll failed: {error}");
            return;
        }
        for (kind, pool) in self.pools.iter().enumerate() {
            for slot in pool {
                match slot.state.load(Ordering::Acquire) {
                    2 => {
                        let mapped = slot.readback.slice(..).get_mapped_range();
                        let start = u64::from_ne_bytes(mapped[..8].try_into().unwrap());
                        let end = u64::from_ne_bytes(mapped[8..16].try_into().unwrap());
                        if let Some(ticks) = end.checked_sub(start) {
                            let ns = (ticks as f64 * self.period_ns).round() as u64;
                            NANOSECONDS[kind].fetch_add(ns, Ordering::Relaxed);
                            SAMPLES[kind].fetch_add(1, Ordering::Relaxed);
                            if kind == Kind::Main as usize {
                                MAIN_NS_BY_RENDERER[self.index].fetch_add(ns, Ordering::Relaxed);
                                MAIN_SAMPLES_BY_RENDERER[self.index]
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        } else {
                            ERRORS.fetch_add(1, Ordering::Relaxed);
                        }
                        drop(mapped);
                        slot.readback.unmap();
                        slot.state.store(0, Ordering::Release);
                    }
                    3 => {
                        ERRORS.fetch_add(1, Ordering::Relaxed);
                        slot.readback.unmap();
                        slot.state.store(0, Ordering::Release);
                    }
                    _ => {}
                }
            }
        }
    }

    pub(crate) fn begin(&self, kind: Kind) -> Option<Ticket<'_>> {
        for slot in &self.pools[kind as usize] {
            if slot
                .state
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(Ticket {
                    slot,
                    submitted: false,
                });
            }
        }
        SKIPPED.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub(crate) fn index(&self) -> usize {
        self.index
    }
}

pub(crate) struct Ticket<'a> {
    slot: &'a Slot,
    submitted: bool,
}

impl Ticket<'_> {
    pub(crate) fn writes(
        &self,
        begin: bool,
        end: bool,
    ) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
        (begin || end).then_some(wgpu::RenderPassTimestampWrites {
            query_set: &self.slot.queries,
            beginning_of_pass_write_index: begin.then_some(0),
            end_of_pass_write_index: end.then_some(1),
        })
    }

    pub(crate) fn resolve(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.resolve_query_set(&self.slot.queries, 0..2, &self.slot.resolve, 0);
        encoder.copy_buffer_to_buffer(&self.slot.resolve, 0, &self.slot.readback, 0, 16);
    }

    pub(crate) fn submitted(mut self) {
        self.submitted = true;
        let state = Arc::clone(&self.slot.state);
        self.slot
            .readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                state.store(if result.is_ok() { 2 } else { 3 }, Ordering::Release);
            });
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if !self.submitted {
            // Aborted encoders never map their readback buffer.
            self.slot.state.store(0, Ordering::Release);
        }
    }
}

pub(crate) fn report() -> String {
    let mut per_renderer = String::new();
    for index in 0..RENDERERS {
        let samples = MAIN_SAMPLES_BY_RENDERER[index].load(Ordering::Relaxed);
        let ns = MAIN_NS_BY_RENDERER[index].load(Ordering::Relaxed);
        per_renderer.push_str(&format!(
            "gpu_main_samples_r{index}={samples}\ngpu_main_ns_r{index}={ns}\n"
        ));
    }
    per_renderer
        + &format!(
        "gpu_main_samples={}\ngpu_main_ns={}\ngpu_capture_samples={}\ngpu_capture_ns={}\ngpu_timestamp_skips={}\ngpu_timestamp_errors={}\n",
        SAMPLES[0].load(Ordering::Relaxed),
        NANOSECONDS[0].load(Ordering::Relaxed),
        SAMPLES[1].load(Ordering::Relaxed),
        NANOSECONDS[1].load(Ordering::Relaxed),
        SKIPPED.load(Ordering::Relaxed),
        ERRORS.load(Ordering::Relaxed),
    )
}
