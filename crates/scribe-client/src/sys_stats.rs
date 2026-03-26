//! System resource statistics collection (CPU, memory, network, GPU).
//!
//! [`SystemStatsCollector`] wraps `sysinfo` to provide cached, rate-limited
//! readings refreshed at most every 2 seconds, with rolling history buffers
//! for sparkline rendering.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use sysinfo::{Networks, System};

/// Maximum number of CPU/GPU history entries.
const CPU_HISTORY_CAP: usize = 8;

/// Maximum number of network history entries.
const NET_HISTORY_CAP: usize = 4;

/// Minimum elapsed time between refreshes.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Bytes per gigabyte as f64 for precision during conversion.
const BYTES_PER_GB: f64 = 1_073_741_824.0;

/// Cached snapshot of system resource usage.
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

/// Collects and caches system resource statistics at a 2-second interval.
pub struct SystemStatsCollector {
    sys: System,
    networks: Networks,
    stats: SystemStats,
    last_refresh: Instant,
}

impl SystemStatsCollector {
    /// Create a new collector and perform an initial refresh.
    pub fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        let mut networks = Networks::new_with_refreshed_list();
        networks.refresh(false);

        let mut collector =
            Self { sys, networks, stats: SystemStats::new(), last_refresh: Instant::now() };
        collector.do_refresh();
        collector
    }

    /// Return a reference to cached stats, refreshing if 2 s have elapsed.
    pub fn maybe_refresh(&mut self) -> &SystemStats {
        if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.do_refresh();
        }
        &self.stats
    }

    /// Return cached stats without triggering a refresh.
    pub fn stats(&self) -> &SystemStats {
        &self.stats
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    fn do_refresh(&mut self) {
        let elapsed_secs = self.last_refresh.elapsed().as_secs_f64().max(f64::EPSILON);
        self.last_refresh = Instant::now();

        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        self.networks.refresh(false);

        let cpu = self.sys.global_cpu_usage();
        push_capped(&mut self.stats.cpu_history, cpu, CPU_HISTORY_CAP);
        self.stats.cpu_percent = cpu;

        self.stats.mem_used_gb = bytes_to_gb(self.sys.used_memory());
        self.stats.mem_total_gb = bytes_to_gb(self.sys.total_memory());

        let (up_bytes, down_bytes) = net_delta(&self.networks);
        let up_rate = rate(up_bytes, elapsed_secs);
        let down_rate = rate(down_bytes, elapsed_secs);
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

impl Default for SystemStatsCollector {
    fn default() -> Self {
        Self::new()
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

/// Convert a byte count (u64) to gigabytes as f32.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "memory sizes fit comfortably in f32 for display"
)]
fn bytes_to_gb(bytes: u64) -> f32 {
    (bytes as f64 / BYTES_PER_GB) as f32
}

/// Sum received and transmitted bytes across all non-loopback interfaces.
fn net_delta(networks: &Networks) -> (u64, u64) {
    let mut up: u64 = 0;
    let mut down: u64 = 0;
    for (name, data) in networks {
        if name == "lo" {
            continue;
        }
        up = up.saturating_add(data.transmitted());
        down = down.saturating_add(data.received());
    }
    (up, down)
}

/// Convert a byte delta to a per-second rate, rounding to u64.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "byte rates are non-negative and fit in u64"
)]
fn rate(bytes: u64, elapsed_secs: f64) -> u64 {
    #[allow(
        clippy::cast_precision_loss,
        reason = "bytes as f64 is precise enough for network rate display"
    )]
    let bps = bytes as f64 / elapsed_secs;
    bps as u64
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
fn read_amd_gpu() -> Option<f32> {
    let raw = std::fs::read_to_string("/sys/class/drm/card0/device/gpu_busy_percent").ok()?;
    parse_percent(raw.trim())
}

#[cfg(target_os = "linux")]
fn read_nvidia_gpu() -> Option<f32> {
    let sysfs = std::fs::read_to_string("/sys/class/drm/card0/device/nvidia/gpuutil");
    if let Ok(raw) = sysfs {
        return parse_percent(raw.trim());
    }
    read_nvidia_smi()
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
    parse_percent(stdout.trim())
}

/// Parse a decimal string into a clamped 0–100 f32 percentage.
#[cfg(target_os = "linux")]
fn parse_percent(s: &str) -> Option<f32> {
    let v: f32 = s.parse().ok()?;
    Some(v.clamp(0.0, 100.0))
}
