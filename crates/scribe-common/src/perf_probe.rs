//! Client-agnostic runtime performance probe for the launch perf gate.
//!
//! The perf A/B rig (`tools/perf-ab-rig/run-perf-ab.sh`) has to compare the new
//! GPUI client against the old client on five metrics, four of which are only
//! observable from inside a running client: input echo latency, sustained
//! `cat`-firehose drain rate, memory at ten tabs, and scroll frame pacing. This
//! module is the single instrumentation implementation both clients call, so the
//! two halves of the A/B are measured by the *same* code and the comparison is
//! meaningful.
//!
//! The probe is entirely opt-in: it activates only when [`PERF_PROBE_ENV`] names
//! an output file. When unset, every entry point is an atomic load plus a
//! `None` check, so normal runs pay nothing and write nothing.
//!
//! The report is a flat `key=value` text file, rewritten in place at most every
//! [`FLUSH_INTERVAL`]. Counters are cumulative since process start and carry an
//! `uptime_ms` stamp, so the rig computes a per-workload number by reading the
//! file before and after a workload and dividing the deltas — no window
//! bookkeeping is needed in the client.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::ids::SessionId;

/// Env var that opts a client into the probe. Its value is the report path.
pub const PERF_PROBE_ENV: &str = "SCRIBE_PERF_PROBE";

/// Minimum spacing between report rewrites. Short enough that the rig sees a
/// fresh file a fraction of a second after a workload ends, long enough that a
/// firehose burst does not turn the probe itself into the bottleneck.
const FLUSH_INTERVAL: Duration = Duration::from_millis(200);

/// One frame's budget in microseconds at the Clarification Q3 pacing target of
/// sustained 60 fps. Kept as an integer so drop accounting needs no float cast.
const FRAME_BUDGET_US: u128 = 16_666;

/// Gaps longer than this are treated as the client sitting idle with nothing to
/// draw, not as dropped frames. Without it every pause between workloads would
/// be scored as thousands of drops.
const IDLE_GAP: Duration = Duration::from_millis(250);

/// Upper bound on retained latency samples so a long run cannot grow unbounded.
const MAX_LATENCY_SAMPLES: usize = 8192;

/// Process-wide probe handle. `None` means the env var was unset at init.
static PROBE: OnceLock<Option<PerfProbe>> = OnceLock::new();

/// Mutable probe state that cannot live in an atomic.
struct ProbeState {
    /// Paint time of the previous frame, for gap-based drop accounting.
    last_frame: Option<Instant>,
    /// When the last report rewrite happened.
    last_flush: Instant,
    /// Send time of a keystroke still awaiting its echo, if any.
    pending_input: Option<Instant>,
    /// Round-trip samples in milliseconds, oldest first.
    latencies: Vec<f64>,
    /// Sessions the client currently renders as tabs.
    sessions: Vec<SessionId>,
    /// The session keystrokes are routed to.
    focused: Option<SessionId>,
}

/// Sentinel held by [`PerfProbe::first_frame_ms_bits`] until the first frame is
/// painted. It is a NaN bit pattern, so it can never collide with the
/// [`f64::to_bits`] encoding of a real elapsed-millisecond value.
const FIRST_FRAME_UNSET: u64 = u64::MAX;

/// Cumulative counters plus the derived values written to the report.
struct Snapshot {
    pid: u32,
    uptime_ms: f64,
    startup_first_frame_ms: Option<f64>,
    frames: u64,
    dropped_frames: u64,
    pty_bytes: u64,
    latency_samples: usize,
    latency_p50_ms: Option<f64>,
    latency_mean_ms: Option<f64>,
    sessions: Vec<SessionId>,
    focused: Option<SessionId>,
}

/// The live probe: cumulative counters plus the report path.
pub struct PerfProbe {
    path: PathBuf,
    start: Instant,
    /// Milliseconds from [`Self::start`] to the first painted frame as
    /// [`f64::to_bits`], or [`FIRST_FRAME_UNSET`] while nothing has been painted
    /// yet. This is the startup-to-first-frame metric, and both clients arm the
    /// probe as the first statement of `main`, so the two halves of the A/B
    /// measure the same span with the same code.
    first_frame_ms_bits: AtomicU64,
    frames: AtomicU64,
    dropped_frames: AtomicU64,
    pty_bytes: AtomicU64,
    state: Mutex<ProbeState>,
}

impl PerfProbe {
    /// Build a probe that writes its report to `path`.
    fn new(path: PathBuf) -> Self {
        let now = Instant::now();
        Self {
            path,
            start: now,
            first_frame_ms_bits: AtomicU64::new(FIRST_FRAME_UNSET),
            frames: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            pty_bytes: AtomicU64::new(0),
            state: Mutex::new(ProbeState {
                last_frame: None,
                last_flush: now,
                pending_input: None,
                latencies: Vec::new(),
                sessions: Vec::new(),
                focused: None,
            }),
        }
    }

    /// Count one painted frame and the frames its gap implies were missed.
    fn frame(&self, now: Instant) {
        self.latch_first_frame(now);
        self.frames.fetch_add(1, Ordering::Relaxed);
        let missed = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let missed = state.last_frame.map_or(0, |prev| missed_frames(now - prev));
            state.last_frame = Some(now);
            missed
        };
        if missed > 0 {
            self.dropped_frames.fetch_add(missed, Ordering::Relaxed);
        }
        self.maybe_flush(now);
    }

    /// Record the startup-to-first-frame span, once, on the first painted frame.
    ///
    /// Later frames leave the latch alone, so the reported value is always the
    /// initial paint even though the rig reads the report long afterwards.
    fn latch_first_frame(&self, now: Instant) {
        let elapsed_ms = now.saturating_duration_since(self.start).as_secs_f64() * 1000.0;
        // An `Err` just means a frame already latched the span, which is the
        // whole point of the compare-exchange.
        let latched = self
            .first_frame_ms_bits
            .compare_exchange(
                FIRST_FRAME_UNSET,
                elapsed_ms.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok();
        if latched {
            tracing::debug!(elapsed_ms, "perf probe latched startup-to-first-frame");
        }
    }

    /// The startup-to-first-frame span in milliseconds, or `None` before the
    /// first paint.
    fn first_frame_ms(&self) -> Option<f64> {
        match self.first_frame_ms_bits.load(Ordering::Relaxed) {
            FIRST_FRAME_UNSET => None,
            bits => Some(f64::from_bits(bits)),
        }
    }

    /// Stamp a keystroke as awaiting its echo. A keystroke sent while another is
    /// still unmatched is ignored so the pairing can never drift.
    fn input_sent(&self, now: Instant) {
        if let Ok(mut state) = self.state.lock()
            && state.pending_input.is_none()
        {
            state.pending_input = Some(now);
        }
    }

    /// Account `len` bytes of PTY output and close an open echo measurement.
    fn pty_output(&self, len: usize, now: Instant) {
        self.pty_bytes.fetch_add(u64::try_from(len).unwrap_or(u64::MAX), Ordering::Relaxed);
        if let Ok(mut state) = self.state.lock()
            && let Some(sent) = state.pending_input.take()
        {
            let elapsed_ms = (now - sent).as_secs_f64() * 1000.0;
            if state.latencies.len() < MAX_LATENCY_SAMPLES {
                state.latencies.push(elapsed_ms);
            }
        }
        self.maybe_flush(now);
    }

    /// Publish which sessions the client renders and which one has focus.
    ///
    /// The rig uses this as a safety interlock: it only types into a session it
    /// watched appear after the client launched, so a workload can never land
    /// keystrokes in a pane that was already open.
    fn sessions(&self, sessions: Vec<SessionId>, focused: Option<SessionId>, now: Instant) {
        let changed = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let changed = state.sessions != sessions || state.focused != focused;
            if changed {
                state.sessions = sessions;
                state.focused = focused;
            }
            changed
        };
        if changed {
            self.write_report(now);
        }
    }

    /// Rewrite the report when the flush interval has elapsed.
    fn maybe_flush(&self, now: Instant) {
        let due = self
            .state
            .lock()
            .is_ok_and(|state| now.saturating_duration_since(state.last_flush) >= FLUSH_INTERVAL);
        if due {
            self.write_report(now);
        }
    }

    /// Render and write the report unconditionally.
    fn write_report(&self, now: Instant) {
        let snapshot = self.snapshot(now);
        if let Ok(mut state) = self.state.lock() {
            state.last_flush = now;
        }
        if let Err(error) = std::fs::write(&self.path, render_report(&snapshot)) {
            tracing::warn!(%error, "failed to write perf probe report");
        }
    }

    /// Collect the cumulative counters and derived latency statistics.
    fn snapshot(&self, now: Instant) -> Snapshot {
        let (latency_samples, latency_p50_ms, latency_mean_ms, sessions, focused) =
            self.state.lock().map_or_else(
                |_| (0, None, None, Vec::new(), None),
                |state| {
                    (
                        state.latencies.len(),
                        median(&state.latencies),
                        mean(&state.latencies),
                        state.sessions.clone(),
                        state.focused,
                    )
                },
            );
        Snapshot {
            pid: std::process::id(),
            uptime_ms: now.saturating_duration_since(self.start).as_secs_f64() * 1000.0,
            startup_first_frame_ms: self.first_frame_ms(),
            frames: self.frames.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            pty_bytes: self.pty_bytes.load(Ordering::Relaxed),
            latency_samples,
            latency_p50_ms,
            latency_mean_ms,
            sessions,
            focused,
        }
    }
}

/// How many 60 fps slots a gap between two painted frames skipped.
///
/// A gap at or under one frame budget missed nothing; a gap past [`IDLE_GAP`] is
/// an idle client rather than a stall and is likewise scored as zero, so only
/// genuine mid-workload hitches count against the drop budget.
fn missed_frames(gap: Duration) -> u64 {
    if gap > IDLE_GAP {
        return 0;
    }
    // Integer arithmetic keeps the slot count exact and avoids a float cast:
    // the gap in microseconds divided by one 60 fps budget in microseconds.
    let slots = gap.as_micros() / FRAME_BUDGET_US;
    u64::try_from(slots.saturating_sub(1)).unwrap_or(u64::MAX)
}

/// Median of the samples, or `None` when there are none.
fn median(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted.get(mid).copied()
    } else {
        match (sorted.get(mid - 1), sorted.get(mid)) {
            (Some(low), Some(high)) => Some((low + high) / 2.0),
            _ => None,
        }
    }
}

/// Arithmetic mean of the samples, or `None` when there are none.
fn mean(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let count = u32::try_from(samples.len()).ok()?;
    Some(samples.iter().sum::<f64>() / f64::from(count))
}

/// Serialize a snapshot into the flat `key=value` report the rig parses.
fn render_report(snapshot: &Snapshot) -> String {
    let mut lines = vec![
        format!("pid={}", snapshot.pid),
        format!("uptime_ms={:.3}", snapshot.uptime_ms),
        format!("frames={}", snapshot.frames),
        format!("dropped_frames={}", snapshot.dropped_frames),
        format!("pty_bytes={}", snapshot.pty_bytes),
        format!("input_samples={}", snapshot.latency_samples),
    ];
    if let Some(first_frame) = snapshot.startup_first_frame_ms {
        lines.push(format!("startup_first_frame_ms={first_frame:.3}"));
    }
    if let Some(p50) = snapshot.latency_p50_ms {
        lines.push(format!("input_latency_p50_ms={p50:.3}"));
    }
    if let Some(mean_ms) = snapshot.latency_mean_ms {
        lines.push(format!("input_latency_mean_ms={mean_ms:.3}"));
    }
    lines.push(format!("sessions={}", snapshot.sessions.len()));
    let ids: Vec<String> =
        snapshot.sessions.iter().map(|session| session.to_full_string()).collect();
    lines.push(format!("session_ids={}", ids.join(",")));
    let focused = snapshot.focused.map_or_else(|| String::from("-"), SessionId::to_full_string);
    lines.push(format!("focused_session={focused}"));
    format!("{}\n", lines.join("\n"))
}

/// Arm the probe from the environment. Safe to call more than once; only the
/// first call decides whether the probe is active for this process.
pub fn init_from_env() {
    let _ = PROBE.get_or_init(|| {
        std::env::var_os(PERF_PROBE_ENV)
            .filter(|value| !value.is_empty())
            .map(|value| PerfProbe::new(PathBuf::from(value)))
    });
}

/// The live probe, or `None` when the client was not launched under the rig.
fn probe() -> Option<&'static PerfProbe> {
    PROBE.get().and_then(Option::as_ref)
}

/// Whether this process is running under the perf rig.
///
/// Call sites that would have to build a value just to report it (the session
/// list, for instance) gate on this so a normal run allocates nothing.
pub fn is_active() -> bool {
    probe().is_some()
}

/// Record one painted frame. Call from the client's per-frame render entry.
pub fn record_frame() {
    if let Some(probe) = probe() {
        probe.frame(Instant::now());
    }
}

/// Record that a keystroke was handed to the outbound IPC path.
pub fn record_input_sent() {
    if let Some(probe) = probe() {
        probe.input_sent(Instant::now());
    }
}

/// Record `len` bytes of PTY output arriving from the server.
pub fn record_pty_output(len: usize) {
    if let Some(probe) = probe() {
        probe.pty_output(len, Instant::now());
    }
}

/// Record the client's current tab sessions and focused session.
pub fn record_sessions(sessions: Vec<SessionId>, focused: Option<SessionId>) {
    if let Some(probe) = probe() {
        probe.sessions(sessions, focused, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#Test Harness#GPUI Perf A/B Gate#Runtime probe instrumentation#Frame gaps score missed 60 fps slots]]
    #[test]
    fn frame_gaps_score_missed_slots() {
        assert_eq!(missed_frames(Duration::from_millis(16)), 0);
        assert_eq!(missed_frames(Duration::from_millis(34)), 1);
        assert_eq!(missed_frames(Duration::from_millis(50)), 2);
    }

    // @lat: [[test#Test Harness#GPUI Perf A/B Gate#Runtime probe instrumentation#Idle gaps are not dropped frames]]
    #[test]
    fn idle_gaps_are_not_dropped_frames() {
        assert_eq!(missed_frames(IDLE_GAP + Duration::from_millis(1)), 0);
        assert_eq!(missed_frames(Duration::from_secs(30)), 0);
    }

    // @lat: [[test#Test Harness#GPUI Perf A/B Gate#Runtime probe instrumentation#Latency statistics summarise samples]]
    #[test]
    fn latency_statistics_summarise_samples() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[5.0]), Some(5.0));
        assert_eq!(median(&[9.0, 1.0, 5.0]), Some(5.0));
        assert_eq!(median(&[4.0, 1.0, 9.0, 5.0]), Some(4.5));
        assert_eq!(mean(&[]), None);
        assert_eq!(mean(&[1.0, 3.0]), Some(2.0));
    }

    // @lat: [[test#Test Harness#GPUI Perf A/B Gate#Runtime probe instrumentation#Report renders every rig key]]
    #[test]
    fn report_renders_every_rig_key() {
        let session = SessionId::new();
        let snapshot = Snapshot {
            pid: 42,
            uptime_ms: 1000.0,
            startup_first_frame_ms: Some(612.5),
            frames: 120,
            dropped_frames: 3,
            pty_bytes: 4096,
            latency_samples: 2,
            latency_p50_ms: Some(7.5),
            latency_mean_ms: Some(8.0),
            sessions: vec![session],
            focused: Some(session),
        };
        let report = render_report(&snapshot);
        for key in [
            "pid=42",
            "uptime_ms=1000.000",
            "frames=120",
            "dropped_frames=3",
            "pty_bytes=4096",
            "input_samples=2",
            "startup_first_frame_ms=612.500",
            "input_latency_p50_ms=7.500",
            "input_latency_mean_ms=8.000",
            "sessions=1",
        ] {
            assert!(report.contains(key), "missing {key} in {report}");
        }
        assert!(report.contains(&format!("focused_session={}", session.to_full_string())));
        assert!(report.contains(&format!("session_ids={}", session.to_full_string())));
    }

    // @lat: [[test#Test Harness#GPUI Perf A/B Gate#Runtime probe instrumentation#Probe stays inert without the env var]]
    #[test]
    fn probe_stays_inert_without_the_env_var() {
        // The env var is unset in the test harness, so the shared entry points
        // must be no-ops rather than writing anywhere or panicking.
        init_from_env();
        record_frame();
        record_input_sent();
        record_pty_output(128);
        record_sessions(vec![SessionId::new()], None);
        assert!(probe().is_none());
    }

    // @lat: [[test#Test Harness#GPUI Perf A/B Gate#Runtime probe instrumentation#Counters pair input with its echo]]
    #[test]
    fn counters_pair_input_with_its_echo() {
        let dir = std::env::temp_dir().join(format!("scribe-perf-probe-{}", SessionId::new()));
        let probe = PerfProbe::new(dir.clone());
        let start = Instant::now();
        probe.input_sent(start);
        // A second keystroke while one is outstanding must not restart the clock.
        probe.input_sent(start + Duration::from_millis(5));
        probe.pty_output(64, start + Duration::from_millis(10));
        probe.pty_output(64, start + Duration::from_millis(20));
        let snapshot = probe.snapshot(start + Duration::from_millis(30));
        assert_eq!(snapshot.pty_bytes, 128);
        assert_eq!(snapshot.latency_samples, 1);
        assert_eq!(snapshot.latency_p50_ms, Some(10.0));
        drop(std::fs::remove_file(&dir));
    }

    // @lat: [[test#Test Harness#GPUI Perf A/B Gate#Runtime probe instrumentation#First frame latches the startup span]]
    #[test]
    fn first_frame_latches_the_startup_span() {
        let path = std::env::temp_dir().join(format!("scribe-perf-startup-{}", SessionId::new()));
        let probe = PerfProbe::new(path.clone());
        let start = probe.start;
        assert_eq!(probe.snapshot(start).startup_first_frame_ms, None);
        probe.frame(start + Duration::from_millis(640));
        // Every later frame leaves the latch alone, so the report keeps naming
        // the initial paint no matter when the rig reads it.
        probe.frame(start + Duration::from_secs(5));
        let snapshot = probe.snapshot(start + Duration::from_secs(6));
        assert_eq!(snapshot.startup_first_frame_ms, Some(640.0));
        assert_eq!(snapshot.frames, 2);
        drop(std::fs::remove_file(&path));
    }
}
