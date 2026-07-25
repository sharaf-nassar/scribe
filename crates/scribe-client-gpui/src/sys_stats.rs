//! System resource statistics collection (CPU, memory, network, GPU).
//!
//! [`SystemStatsCollector`] hands the status bar a cached snapshot that a
//! background thread refreshes every 2 seconds, with rolling history buffers
//! for sparkline rendering. Sampling never runs on the UI thread: the readings
//! feed decorative sparklines, but the underlying `sysinfo` and GPU probes are
//! slow enough to blow the startup-to-first-frame budget if the window waits
//! on them.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};

/// Maximum number of CPU/GPU history entries.
const CPU_HISTORY_CAP: usize = 8;

/// Maximum number of network history entries.
const NET_HISTORY_CAP: usize = 4;

/// Minimum elapsed time between refreshes.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// How long the sampler thread sleeps between checks of its stop flag while
/// idling out [`REFRESH_INTERVAL`]. Short slices let a dropped collector tear
/// the thread down promptly instead of waiting out the full interval.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bytes per gibibyte.
const BYTES_PER_GIB: u64 = 1_073_741_824;
/// Bytes per mebibyte.
const BYTES_PER_MIB: u64 = 1_048_576;
/// Mebibytes per gibibyte.
const MIB_PER_GIB: u16 = 1024;

/// Cached snapshot of system resource usage.
#[derive(Clone)]
pub struct SystemStats {
    /// Total CPU utilisation, 0–100.
    pub cpu_percent: f32,
    /// Used memory in gigabytes.
    pub mem_used_gb: f32,
    /// Total installed memory in gigabytes.
    pub mem_total_gb: f32,
    /// GPU utilisation 0–100, or `None` if unavailable.
    pub gpu_percent: Option<f32>,
    /// Network upload in bytes per second.
    pub net_up_bytes_sec: u64,
    /// Network download in bytes per second.
    pub net_down_bytes_sec: u64,
    /// Rolling buffer of the last 8 CPU readings.
    pub cpu_history: VecDeque<f32>,
    /// Rolling buffer of the last 8 GPU readings (empty when no GPU).
    pub gpu_history: VecDeque<f32>,
    /// Rolling buffer of the last 4 upload byte-rate readings.
    pub net_up_history: VecDeque<u64>,
    /// Rolling buffer of the last 4 download byte-rate readings.
    pub net_down_history: VecDeque<u64>,
}

impl SystemStats {
    fn new() -> Self {
        Self {
            cpu_percent: 0.0,
            mem_used_gb: 0.0,
            mem_total_gb: 0.0,
            gpu_percent: None,
            net_up_bytes_sec: 0,
            net_down_bytes_sec: 0,
            cpu_history: VecDeque::new(),
            gpu_history: VecDeque::new(),
            net_up_history: VecDeque::new(),
            net_down_history: VecDeque::new(),
        }
    }
}

/// Publishes system resource statistics sampled off the UI thread.
///
/// Construction is non-blocking: it spawns a sampler thread and returns an
/// all-zero snapshot immediately, so opening a window never waits on `sysinfo`
/// or on a GPU probe. The status bar simply shows zeroed segments until the
/// first background sample lands a few milliseconds later.
pub struct SystemStatsCollector {
    /// Newest sample published by the sampler thread.
    shared: Arc<Mutex<SystemStats>>,
    /// UI-thread copy of the last sample read out of `shared`, so callers keep
    /// borrowing a plain `&SystemStats` and never hold the lock across a frame.
    snapshot: SystemStats,
    /// Cleared on drop to stop the sampler thread.
    running: Arc<AtomicBool>,
}

impl SystemStatsCollector {
    /// Create a collector and start its background sampler.
    pub fn new() -> Self {
        let shared = Arc::new(Mutex::new(SystemStats::new()));
        let running = Arc::new(AtomicBool::new(true));
        spawn_sampler(Arc::clone(&shared), Arc::clone(&running));
        Self { shared, snapshot: SystemStats::new(), running }
    }

    /// Adopt the newest background sample and return it.
    ///
    /// Cheap enough for every frame: it takes an uncontended lock and clones a
    /// handful of scalars plus two short history buffers. All the expensive
    /// probing already happened on the sampler thread.
    pub fn maybe_refresh(&mut self) -> &SystemStats {
        if let Ok(latest) = self.shared.lock() {
            self.snapshot.clone_from(&latest);
        }
        &self.snapshot
    }

    /// Return the last adopted sample without touching the shared slot.
    pub fn stats(&self) -> &SystemStats {
        &self.snapshot
    }
}

impl Drop for SystemStatsCollector {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
    }
}

impl Default for SystemStatsCollector {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Background sampler
// ---------------------------------------------------------------------------

/// Start the sampler thread, degrading to permanently-zero stats if the thread
/// cannot be spawned. A missing sparkline is never worth failing a window over.
fn spawn_sampler(shared: Arc<Mutex<SystemStats>>, running: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("scribe-sys-stats".to_owned())
        .spawn(move || run_sampler(&shared, &running));
    if let Err(error) = spawned {
        tracing::warn!(%error, "system-stats sampler unavailable; status-bar stats stay at zero");
    }
}

/// Sample on [`REFRESH_INTERVAL`] until the owning collector is dropped.
fn run_sampler(shared: &Mutex<SystemStats>, running: &AtomicBool) {
    let mut sampler = Sampler::new();
    while running.load(Ordering::Acquire) {
        sampler.refresh();
        if let Ok(mut slot) = shared.lock() {
            slot.clone_from(&sampler.stats);
        }
        if !sleep_until_next_sample(running) {
            return;
        }
    }
}

/// Idle out one refresh interval, waking every [`STOP_POLL_INTERVAL`] to notice
/// a stop request. Returns `false` when the collector was dropped mid-sleep.
fn sleep_until_next_sample(running: &AtomicBool) -> bool {
    let deadline = Instant::now() + REFRESH_INTERVAL;
    loop {
        if !running.load(Ordering::Acquire) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        std::thread::sleep(remaining.min(STOP_POLL_INTERVAL));
    }
}

/// Refresh selectors that keep `sysinfo` off the process table.
///
/// `System::new_all` walks every entry in `/proc` and cost ~1.7 s on a busy
/// host, which by itself blew the 500 ms startup-to-first-frame budget. The
/// status bar only ever reads global CPU usage and RAM totals, so the sampler
/// asks for exactly those and never enumerates processes.
fn stats_refresh_kind() -> RefreshKind {
    RefreshKind::nothing()
        .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
        .with_memory(MemoryRefreshKind::nothing().with_ram())
}

/// The sampler thread's private state: the `sysinfo` handles plus the stats it
/// accumulates between publishes.
struct Sampler {
    sys: System,
    networks: Networks,
    stats: SystemStats,
    last_refresh: Instant,
}

impl Sampler {
    fn new() -> Self {
        let sys = System::new_with_specifics(stats_refresh_kind());
        let mut networks = Networks::new_with_refreshed_list();
        networks.refresh(false);
        Self { sys, networks, stats: SystemStats::new(), last_refresh: Instant::now() }
    }

    fn refresh(&mut self) {
        let elapsed = self.last_refresh.elapsed().max(Duration::from_nanos(1));
        self.last_refresh = Instant::now();

        self.sys.refresh_cpu_specifics(CpuRefreshKind::nothing().with_cpu_usage());
        self.sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        self.networks.refresh(false);

        let cpu = self.sys.global_cpu_usage();
        push_capped(&mut self.stats.cpu_history, cpu, CPU_HISTORY_CAP);
        self.stats.cpu_percent = cpu;

        self.stats.mem_used_gb = bytes_to_gb(self.sys.used_memory());
        self.stats.mem_total_gb = bytes_to_gb(self.sys.total_memory());

        let (up_bytes, down_bytes) = net_delta(&self.networks);
        let up_rate = rate(up_bytes, elapsed);
        let down_rate = rate(down_bytes, elapsed);
        push_capped(&mut self.stats.net_up_history, up_rate, NET_HISTORY_CAP);
        push_capped(&mut self.stats.net_down_history, down_rate, NET_HISTORY_CAP);
        self.stats.net_up_bytes_sec = up_rate;
        self.stats.net_down_bytes_sec = down_rate;

        let gpu = read_gpu_percent();
        self.stats.gpu_percent = gpu;
        if let Some(g) = gpu {
            push_capped(&mut self.stats.gpu_history, g, CPU_HISTORY_CAP);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Push `value` onto `buf`, evicting the oldest entry once `cap` is exceeded.
fn push_capped<T>(buf: &mut VecDeque<T>, value: T, cap: usize) {
    if buf.len() >= cap {
        buf.pop_front();
    }
    buf.push_back(value);
}

fn bytes_to_gb(bytes: u64) -> f32 {
    let whole_gib = u16::try_from(bytes / BYTES_PER_GIB).unwrap_or(u16::MAX);
    let remainder_mib = u16::try_from(bytes % BYTES_PER_GIB / BYTES_PER_MIB).unwrap_or(u16::MAX);
    f32::from(whole_gib) + f32::from(remainder_mib) / f32::from(MIB_PER_GIB)
}

/// Sum received and transmitted bytes for routed network interfaces.
///
/// Linux prefers default-route interfaces to avoid double-counting bridge/veth
/// traffic; other platforms and missing route data fall back to non-loopback.
fn net_delta(networks: &Networks) -> (u64, u64) {
    let preferred_interfaces = preferred_network_interfaces();
    if !preferred_interfaces.is_empty() {
        let (up, down, matched) = net_delta_for_interfaces(networks, Some(&preferred_interfaces));
        if matched > 0 {
            return (up, down);
        }
    }

    let (up, down, _) = net_delta_for_interfaces(networks, None);
    (up, down)
}

fn net_delta_for_interfaces(
    networks: &Networks,
    allowed_interfaces: Option<&HashSet<String>>,
) -> (u64, u64, usize) {
    let mut up: u64 = 0;
    let mut down: u64 = 0;
    let mut matched = 0;
    for (name, data) in networks {
        if name == "lo" {
            continue;
        }
        if let Some(allowed) = allowed_interfaces
            && !allowed.contains(name)
        {
            continue;
        }
        up = up.saturating_add(data.transmitted());
        down = down.saturating_add(data.received());
        matched += 1;
    }
    (up, down, matched)
}

#[cfg(target_os = "linux")]
fn preferred_network_interfaces() -> HashSet<String> {
    linux_default_route_interfaces()
}

#[cfg(not(target_os = "linux"))]
fn preferred_network_interfaces() -> HashSet<String> {
    HashSet::new()
}

#[cfg(target_os = "linux")]
fn linux_default_route_interfaces() -> HashSet<String> {
    const RTF_UP: u16 = 0x1;

    let mut interfaces = HashSet::new();
    let Ok(route_table) = std::fs::read_to_string("/proc/net/route") else {
        return interfaces;
    };

    for line in route_table.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        let Some(destination) = fields.next() else { continue };
        let _gateway = fields.next();
        let Some(flags) = fields.next().and_then(|value| u16::from_str_radix(value, 16).ok())
        else {
            continue;
        };

        if destination == "00000000" && flags & RTF_UP != 0 && name != "lo" {
            interfaces.insert(name.to_owned());
        }
    }

    interfaces
}

/// Convert a byte delta to a per-second rate, rounding to u64.
fn rate(bytes: u64, elapsed: Duration) -> u64 {
    let nanos = elapsed.as_nanos().max(1);
    let scaled = u128::from(bytes).saturating_mul(1_000_000_000);
    let rounded = scaled.saturating_add(nanos / 2) / nanos;
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// GPU detection (Linux-only, best-effort)
// ---------------------------------------------------------------------------

/// Attempt to read GPU utilisation percentage from platform-specific sources.
///
/// Returns `None` on non-Linux platforms or when no GPU is detected.
fn read_gpu_percent() -> Option<f32> {
    #[cfg(target_os = "linux")]
    {
        read_gpu_percent_linux()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn read_gpu_percent_linux() -> Option<f32> {
    read_amd_gpu().or_else(read_nvidia_gpu)
}

#[cfg(target_os = "linux")]
fn drm_cards() -> impl Iterator<Item = std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return vec![].into_iter();
    };
    entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name())
        .filter(|name| {
            let s = name.to_string_lossy();
            // Match bare `card0`, `card1`, … but not `card0-DP-1` connector entries.
            s.starts_with("card") && s.len() > 4 && s.chars().skip(4).all(|c| c.is_ascii_digit())
        })
        .map(|name| std::path::Path::new("/sys/class/drm").join(name))
        .collect::<Vec<_>>()
        .into_iter()
}

#[cfg(target_os = "linux")]
fn read_amd_gpu() -> Option<f32> {
    drm_cards().find_map(|card| {
        let path = card.join("device/gpu_busy_percent");
        let raw = std::fs::read_to_string(path).ok()?;
        parse_percent(raw.trim())
    })
}

#[cfg(target_os = "linux")]
fn read_nvidia_gpu() -> Option<f32> {
    let sysfs_result = drm_cards().find_map(|card| {
        let path = card.join("device/nvidia/gpuutil");
        let raw = std::fs::read_to_string(path).ok()?;
        parse_percent(raw.trim())
    });
    sysfs_result.or_else(read_nvidia_smi)
}

#[cfg(target_os = "linux")]
fn read_nvidia_smi() -> Option<f32> {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let first_line = stdout.lines().next()?;
    parse_percent(first_line.trim())
}

/// Parse a decimal string into a clamped 0–100 f32 percentage.
#[cfg(target_os = "linux")]
fn parse_percent(s: &str) -> Option<f32> {
    let v: f32 = s.parse().ok()?;
    Some(v.clamp(0.0, 100.0))
}
