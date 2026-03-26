//! GPU-rendered window-level status bar.
//!
//! Generates [`CellInstance`] quads for a single status bar spanning the full
//! window width at the bottom. The instances are collected into the same
//! buffer as the terminal grid and drawn in a single render pass.

use std::path::Path;

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;
use crate::sys_stats::SystemStats;

/// Height of the status bar in pixels.
pub const STATUS_BAR_HEIGHT: f32 = 24.0;

/// Clickable regions produced by [`build_status_bar`].
pub struct StatusBarHitTargets {
    /// Clickable rect for the equalize icon.
    pub equalize_rect: Option<Rect>,
    /// Clickable rect for the settings gear icon.
    pub gear_rect: Option<Rect>,
}

/// Data needed to render the window-level status bar.
pub struct StatusBarData<'a> {
    pub connected: bool,
    /// Show the equalize button (only when multiple workspaces exist).
    pub show_equalize: bool,
    /// Name of the focused workspace (shown when multiple workspaces exist).
    pub workspace_name: Option<&'a str>,
    /// CWD of the focused pane, displayed as a shortened path.
    pub cwd: Option<&'a Path>,
    /// Git branch of the focused pane.
    pub git_branch: Option<&'a str>,
    /// Total number of active sessions in this window.
    pub session_count: usize,
    /// System hostname.
    pub hostname: &'a str,
    /// Current time string (e.g. "14:32").
    pub time: &'a str,
    pub sys_stats: Option<&'a SystemStats>,
    pub stats_config: Option<&'a scribe_common::config::StatusBarStatsConfig>,
}

/// Fallback green when ANSI index 2 is unavailable.
const FALLBACK_GREEN: [f32; 4] = [0.4, 0.9, 0.5, 1.0];
/// Fallback red when ANSI index 1 is unavailable.
const FALLBACK_RED: [f32; 4] = [1.0, 0.2, 0.2, 1.0];
/// Fallback yellow when ANSI index 3 is unavailable.
const FALLBACK_YELLOW: [f32; 4] = [0.9, 0.8, 0.2, 1.0];

/// Number of sparkline chars for CPU and GPU displays.
const CPU_SPARK_WIDTH: usize = 8;
/// Number of sparkline chars for network displays.
const NET_SPARK_WIDTH: usize = 4;

/// Colors for the status bar, derived from the theme's [`ChromeColors`]
/// and ANSI palette.
pub struct StatusBarColors {
    pub bg: [f32; 4],
    pub text: [f32; 4],
    pub accent: [f32; 4],
    pub separator: [f32; 4],
    /// Color for the connection-status dot when connected (ANSI green).
    pub connected_dot: [f32; 4],
    /// Color for the connection-status dot when disconnected (ANSI red).
    pub disconnected_dot: [f32; 4],
    /// Color for moderate usage (60-85%) — ANSI yellow (index 3).
    pub warning: [f32; 4],
    /// Color for high usage (>85%) — ANSI red (index 1).
    pub critical: [f32; 4],
    /// Dimmed color for stat labels — text at reduced alpha.
    pub label: [f32; 4],
}

impl StatusBarColors {
    /// Build status bar colors from chrome colors and the theme's ANSI palette.
    pub fn from_theme(chrome: &ChromeColors, ansi_colors: &[[f32; 4]; 16]) -> Self {
        Self {
            bg: srgb_to_linear_rgba(chrome.status_bar_bg),
            text: srgb_to_linear_rgba(chrome.status_bar_text),
            accent: srgb_to_linear_rgba(chrome.accent),
            separator: srgb_to_linear_rgba(chrome.divider),
            connected_dot: srgb_to_linear_rgba(
                ansi_colors.get(2).copied().unwrap_or(FALLBACK_GREEN),
            ),
            disconnected_dot: srgb_to_linear_rgba(
                ansi_colors.get(1).copied().unwrap_or(FALLBACK_RED),
            ),
            warning: srgb_to_linear_rgba(ansi_colors.get(3).copied().unwrap_or(FALLBACK_YELLOW)),
            critical: srgb_to_linear_rgba(ansi_colors.get(1).copied().unwrap_or(FALLBACK_RED)),
            label: {
                let t = srgb_to_linear_rgba(chrome.status_bar_text);
                [
                    t.first().copied().unwrap_or(0.0),
                    t.get(1).copied().unwrap_or(0.0),
                    t.get(2).copied().unwrap_or(0.0),
                    t.get(3).copied().unwrap_or(1.0) * 0.55,
                ]
            },
        }
    }
}

/// Build cell instances for the window-level status bar.
///
/// The bar spans the full `window_rect` width and is anchored at
/// `window_rect.y + window_rect.height - STATUS_BAR_HEIGHT`.
#[allow(
    clippy::too_many_arguments,
    reason = "needs rect, cell size, colors, data, and glyph resolver for full status bar rendering"
)]
pub fn build_status_bar(
    out: &mut Vec<CellInstance>,
    window_rect: Rect,
    cell_size: (f32, f32),
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) -> StatusBarHitTargets {
    let (cell_w, _cell_h) = cell_size;
    if cell_w <= 0.0 {
        return StatusBarHitTargets { equalize_rect: None, gear_rect: None };
    }

    let bar_y = window_rect.y + window_rect.height - STATUS_BAR_HEIGHT;
    let max_cols = columns_in_width(window_rect.width, cell_w);
    let mut w = BarWriter { out, x_origin: window_rect.x, y: bar_y, cell_w, max_cols, col: 0 };

    build_background(w.out, w.x_origin, w.y, w.max_cols, w.cell_w, colors.bg);

    let col = render_left_side(&mut w, colors, data, resolve_glyph);
    w.col = col;

    let (equalize_rect, gear_rect) = render_right_side(&mut w, colors, data, resolve_glyph);

    StatusBarHitTargets { equalize_rect, gear_rect }
}

/// Mutable writer state for emitting status bar characters.
struct BarWriter<'a> {
    out: &'a mut Vec<CellInstance>,
    x_origin: f32,
    y: f32,
    cell_w: f32,
    max_cols: usize,
    col: usize,
}

impl BarWriter<'_> {
    /// Emit a single character at the current column with the given colors.
    #[allow(
        clippy::cast_precision_loss,
        reason = "column index is a small positive integer fitting in f32"
    )]
    fn put(
        &mut self,
        ch: char,
        fg: [f32; 4],
        bg: [f32; 4],
        resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    ) {
        if self.col >= self.max_cols {
            return;
        }
        let x = self.x_origin + self.col as f32 * self.cell_w;
        let (uv_min, uv_max) = resolve_glyph(ch);
        self.out.push(CellInstance {
            pos: [x, self.y],
            size: [0.0, 0.0],
            uv_min,
            uv_max,
            fg_color: fg,
            bg_color: bg,
        });
        self.col += 1;
    }

    /// Emit a string with the given colors.
    fn put_str(
        &mut self,
        s: &str,
        fg: [f32; 4],
        bg: [f32; 4],
        resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    ) {
        for ch in s.chars() {
            self.put(ch, fg, bg, resolve_glyph);
        }
    }

    /// Fill from current column to target with spaces.
    fn pad_to(
        &mut self,
        target: usize,
        fg: [f32; 4],
        bg: [f32; 4],
        resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    ) {
        while self.col < target {
            self.put(' ', fg, bg, resolve_glyph);
        }
    }
}

/// Render the left side: connection dot, workspace name (if multi), CWD.
fn render_left_side(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) -> usize {
    w.put(' ', colors.text, colors.bg, resolve_glyph);

    let dot_color = if data.connected { colors.connected_dot } else { colors.disconnected_dot };
    w.put('\u{25CF}', dot_color, colors.bg, resolve_glyph);
    w.put(' ', colors.text, colors.bg, resolve_glyph);

    if let Some(name) = data.workspace_name {
        w.put_str(name, colors.accent, colors.bg, resolve_glyph);
        w.put_str("  ", colors.text, colors.bg, resolve_glyph);
    }

    if let Some(cwd) = data.cwd {
        let short = shorten_cwd(cwd);
        w.put_str(&short, colors.text, colors.bg, resolve_glyph);
    }

    w.col
}

/// Render the right side: git branch | session count | hostname | time | equalize | gear.
///
/// Returns `(equalize_rect, gear_rect)` — clickable rects for the equalize and gear icons.
fn render_right_side(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) -> (Option<Rect>, Option<Rect>) {
    // +3 for the gear segment: " ⚙ " (space, gear, space)
    let gear_cols: usize = 3;
    // +2 for the equalize segment: " ⊞" (space, icon) — gear provides the trailing space
    let equalize_cols: usize = if data.show_equalize { 2 } else { 0 };
    let segments = build_right_segments(data, colors);
    let right_cols: usize =
        segments.iter().map(|s| s.text.chars().count()).sum::<usize>() + equalize_cols + gear_cols;
    let right_start = w.max_cols.saturating_sub(right_cols + 1);

    w.pad_to(right_start, colors.text, colors.bg, resolve_glyph);

    for seg in &segments {
        w.put_str(&seg.text, seg.color, colors.bg, resolve_glyph);
    }

    // Equalize icon: " ⊞" left of the gear, only when multiple workspaces exist.
    let equalize_rect = render_equalize(w, colors, data, resolve_glyph);

    // Gear icon: " ⚙ " at the far right.
    let gear_rect = render_gear(w, colors, resolve_glyph);

    w.pad_to(w.max_cols, colors.text, colors.bg, resolve_glyph);

    (equalize_rect, gear_rect)
}

/// Render the equalize icon and return its clickable rect, or `None` when hidden.
#[allow(
    clippy::cast_precision_loss,
    reason = "column index is a small positive integer fitting in f32"
)]
fn render_equalize(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) -> Option<Rect> {
    if !data.show_equalize || w.col >= w.max_cols {
        return None;
    }
    w.put(' ', colors.text, colors.bg, resolve_glyph);
    let eq_col = w.col;
    w.put('\u{229E}', colors.text, colors.bg, resolve_glyph);
    let eq_x = w.x_origin + eq_col as f32 * w.cell_w;
    let eq_width = (w.col - eq_col) as f32 * w.cell_w;
    Some(Rect { x: eq_x, y: w.y, width: eq_width, height: STATUS_BAR_HEIGHT })
}

/// Render the gear icon and return its clickable rect.
#[allow(
    clippy::cast_precision_loss,
    reason = "column index is a small positive integer fitting in f32"
)]
fn render_gear(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) -> Option<Rect> {
    if w.col >= w.max_cols {
        return None;
    }
    w.put(' ', colors.text, colors.bg, resolve_glyph);
    let gear_col = w.col;
    w.put('\u{2699}', colors.text, colors.bg, resolve_glyph);
    w.put(' ', colors.text, colors.bg, resolve_glyph);
    let gear_x = w.x_origin + gear_col as f32 * w.cell_w;
    let gear_width = (w.col - gear_col) as f32 * w.cell_w;
    Some(Rect { x: gear_x, y: w.y, width: gear_width, height: STATUS_BAR_HEIGHT })
}

/// Map a 0-100 percentage to a Unicode block element (▁▂▃▄▅▆▇█).
fn sparkline_char(pct: f32) -> char {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let idx = ((pct / 100.0) * 7.0).round().clamp(0.0, 7.0);
    // cast is safe: idx is clamped to 0..=7
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "idx clamped to 0..=7"
    )]
    BLOCKS.get(idx as usize).copied().unwrap_or('▁')
}

/// Pick green/yellow/red based on usage percentage.
fn usage_color(pct: f32, colors: &StatusBarColors) -> [f32; 4] {
    if pct >= 85.0 {
        colors.critical
    } else if pct >= 60.0 {
        colors.warning
    } else {
        colors.connected_dot
    }
}

/// Format bytes/sec as a human-readable string of ≤4 chars (e.g., "1.2M", "340K", "0B").
fn format_bytes_rate(bytes_per_sec: u64) -> String {
    if bytes_per_sec >= 1_000_000_000 {
        String::from(">1G")
    } else if bytes_per_sec >= 10_000_000 {
        #[allow(clippy::cast_precision_loss, reason = "bytes_per_sec is a reasonable network rate")]
        let mb = bytes_per_sec as f64 / 1_000_000.0;
        if mb >= 999.5 { String::from(">1G") } else { format!("{mb:.0}M") }
    } else if bytes_per_sec >= 1_000_000 {
        #[allow(clippy::cast_precision_loss, reason = "bytes_per_sec is a reasonable network rate")]
        let mb = bytes_per_sec as f64 / 1_000_000.0;
        format!("{mb:.1}M")
    } else if bytes_per_sec >= 1_000 {
        #[allow(clippy::cast_precision_loss, reason = "bytes_per_sec is a reasonable network rate")]
        let kb = bytes_per_sec as f64 / 1_000.0;
        if kb >= 999.5 { String::from("1.0M") } else { format!("{kb:.0}K") }
    } else {
        format!("{bytes_per_sec}B")
    }
}

/// Format bytes/sec right-aligned in exactly 4 characters.
fn format_bytes_rate_fixed(bytes_per_sec: u64) -> String {
    format!("{:>4}", format_bytes_rate(bytes_per_sec))
}

/// A styled text segment for the right side of the status bar.
struct RightSegment {
    text: String,
    color: [f32; 4],
}

/// Push a separator " | " segment if `segs` is non-empty.
fn push_sep(segs: &mut Vec<RightSegment>, color: [f32; 4]) {
    if !segs.is_empty() {
        segs.push(RightSegment { text: String::from(" \u{2502} "), color });
    }
}

/// Build system-stats segments (CPU, MEM, NET, GPU) for the right side.
fn build_stats_segments(
    stats: &SystemStats,
    config: &scribe_common::config::StatusBarStatsConfig,
    colors: &StatusBarColors,
) -> Vec<RightSegment> {
    let mut segs: Vec<RightSegment> = Vec::new();

    if config.cpu {
        push_sep(&mut segs, colors.separator);
        segs.extend(build_cpu_segments(stats, colors));
    }

    if config.memory {
        push_sep(&mut segs, colors.separator);
        segs.extend(build_mem_segments(stats, colors));
    }

    if config.network {
        push_sep(&mut segs, colors.separator);
        segs.extend(build_net_segments(stats, colors));
    }

    if config.gpu && stats.gpu_percent.is_some() {
        push_sep(&mut segs, colors.separator);
        segs.extend(build_gpu_segments(stats, colors));
    }

    segs
}

/// CPU stats: label + 8 sparkline bars (padded) + fixed-width percentage.
fn build_cpu_segments(stats: &SystemStats, colors: &StatusBarColors) -> Vec<RightSegment> {
    let mut segs = Vec::new();
    segs.push(RightSegment { text: String::from("CPU "), color: colors.label });

    let pad = CPU_SPARK_WIDTH.saturating_sub(stats.cpu_history.len());
    for _ in 0..pad {
        segs.push(RightSegment { text: String::from("\u{2581}"), color: colors.label });
    }
    for &v in &stats.cpu_history {
        segs.push(RightSegment {
            text: sparkline_char(v).to_string(),
            color: usage_color(v, colors),
        });
    }

    let pct = stats.cpu_percent;
    segs.push(RightSegment { text: format!(" {pct:>3.0}%"), color: usage_color(pct, colors) });
    segs
}

/// Memory stats: label + 1 sparkline bar + fixed-width percentage.
fn build_mem_segments(stats: &SystemStats, colors: &StatusBarColors) -> Vec<RightSegment> {
    let mut segs = Vec::new();
    let mem_pct =
        if stats.mem_total_gb > 0.0 { stats.mem_used_gb / stats.mem_total_gb * 100.0 } else { 0.0 };

    segs.push(RightSegment { text: String::from("MEM "), color: colors.label });
    segs.push(RightSegment {
        text: sparkline_char(mem_pct).to_string(),
        color: usage_color(mem_pct, colors),
    });
    segs.push(RightSegment {
        text: format!(" {mem_pct:>3.0}%"),
        color: usage_color(mem_pct, colors),
    });
    segs
}

/// Network stats: ↑ sparklines rate ↓ sparklines rate (all fixed-width).
fn build_net_segments(stats: &SystemStats, colors: &StatusBarColors) -> Vec<RightSegment> {
    let mut segs = Vec::new();

    segs.push(RightSegment { text: String::from("\u{2191}"), color: colors.label });
    let up_pad = NET_SPARK_WIDTH.saturating_sub(stats.net_up_history.len());
    for _ in 0..up_pad {
        segs.push(RightSegment { text: String::from("\u{2581}"), color: colors.label });
    }
    for &v in &stats.net_up_history {
        #[allow(clippy::cast_precision_loss, reason = "network rate fits in f32 for sparkline")]
        let pct = (v as f32 / 1_000_000.0).min(100.0);
        segs.push(RightSegment { text: sparkline_char(pct).to_string(), color: colors.accent });
    }
    segs.push(RightSegment {
        text: format!(" {}", format_bytes_rate_fixed(stats.net_up_bytes_sec)),
        color: colors.text,
    });

    segs.push(RightSegment { text: String::from(" \u{2193}"), color: colors.label });
    let down_pad = NET_SPARK_WIDTH.saturating_sub(stats.net_down_history.len());
    for _ in 0..down_pad {
        segs.push(RightSegment { text: String::from("\u{2581}"), color: colors.label });
    }
    for &v in &stats.net_down_history {
        #[allow(clippy::cast_precision_loss, reason = "network rate fits in f32 for sparkline")]
        let pct = (v as f32 / 1_000_000.0).min(100.0);
        segs.push(RightSegment { text: sparkline_char(pct).to_string(), color: colors.accent });
    }
    segs.push(RightSegment {
        text: format!(" {}", format_bytes_rate_fixed(stats.net_down_bytes_sec)),
        color: colors.text,
    });

    segs
}

/// GPU stats: label + 8 sparkline bars (padded) + fixed-width percentage.
fn build_gpu_segments(stats: &SystemStats, colors: &StatusBarColors) -> Vec<RightSegment> {
    let mut segs = Vec::new();

    if let Some(gpu_pct) = stats.gpu_percent {
        segs.push(RightSegment { text: String::from("GPU "), color: colors.label });

        let cap = CPU_SPARK_WIDTH;
        let pad = cap.saturating_sub(stats.gpu_history.len());
        for _ in 0..pad {
            segs.push(RightSegment { text: String::from("\u{2581}"), color: colors.label });
        }
        for &v in &stats.gpu_history {
            segs.push(RightSegment {
                text: sparkline_char(v).to_string(),
                color: usage_color(v, colors),
            });
        }

        segs.push(RightSegment {
            text: format!(" {gpu_pct:>3.0}%"),
            color: usage_color(gpu_pct, colors),
        });
    }

    segs
}

/// Build the right-side text segments: stats, git branch, session count, hostname, time.
fn build_right_segments(data: &StatusBarData<'_>, colors: &StatusBarColors) -> Vec<RightSegment> {
    let mut segs = Vec::new();

    if let (Some(stats), Some(config)) = (data.sys_stats, data.stats_config) {
        segs.extend(build_stats_segments(stats, config, colors));
    }

    if let Some(branch) = data.git_branch {
        push_sep(&mut segs, colors.separator);
        segs.push(RightSegment { text: String::from(branch), color: colors.accent });
    }

    if data.session_count > 0 {
        push_sep(&mut segs, colors.separator);
        let label = if data.session_count == 1 {
            String::from("1 session")
        } else {
            format!("{} sessions", data.session_count)
        };
        segs.push(RightSegment { text: label, color: colors.text });
    }

    if !data.hostname.is_empty() {
        push_sep(&mut segs, colors.separator);
        segs.push(RightSegment { text: String::from(data.hostname), color: colors.text });
    }

    if !data.time.is_empty() {
        push_sep(&mut segs, colors.separator);
        segs.push(RightSegment { text: String::from(data.time), color: colors.text });
    }

    segs.push(RightSegment { text: String::from(" "), color: colors.text });

    segs
}

/// Shorten a CWD path by replacing `$HOME` with `~`.
fn shorten_cwd(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Some(home) = home_dir() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = s.strip_prefix(home_str.as_ref()) {
            return format!("~{rest}");
        }
    }
    s.into_owned()
}

/// Read the home directory from `$HOME`.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Fill columns with background quads (no glyph).
#[allow(
    clippy::too_many_arguments,
    reason = "helper function needs position, column count, cell width, and color"
)]
fn build_background(
    out: &mut Vec<CellInstance>,
    x_origin: f32,
    y: f32,
    cols: usize,
    cell_w: f32,
    bg: [f32; 4],
) {
    for col_idx in 0..cols {
        #[allow(
            clippy::cast_precision_loss,
            reason = "column index is a small positive integer fitting in f32"
        )]
        let x = x_origin + col_idx as f32 * cell_w;
        out.push(CellInstance {
            pos: [x, y],
            size: [0.0, 0.0],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: bg,
            bg_color: bg,
        });
    }
}

/// Calculate how many cell-width columns fit in a given pixel width.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "width / cell_w yields a small positive value fitting in usize"
)]
fn columns_in_width(width: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 { 0 } else { (width / cell_w) as usize }
}
