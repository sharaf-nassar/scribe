//! GPU-rendered window-level status bar.
//!
//! Generates [`CellInstance`] quads for a single status bar spanning the full
//! window width at the bottom. The instances are collected into the same
//! buffer as the terminal grid and drawn in a single render pass.

use std::path::Path;

use scribe_common::protocol::UpdateProgressState;
use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;
use crate::sys_stats::SystemStats;
use crate::tooltip::TooltipAnchor;

/// Clickable regions produced by [`build_status_bar`].
pub struct StatusBarHitTargets {
    /// Clickable rect for the equalize icon.
    pub equalize_rect: Option<Rect>,
    /// Clickable rect for the settings gear icon.
    pub gear_rect: Option<Rect>,
    /// Clickable rect for the centered update status segment.
    pub update_rect: Option<Rect>,
    /// Tooltip hover targets for each status bar segment.
    pub tooltip_targets: Vec<TooltipAnchor>,
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
    /// Remote or local host label for the focused pane.
    pub host_label: &'a str,
    /// tmux session label for the focused pane when present.
    pub tmux_label: Option<&'a str>,
    /// Current time string (e.g. "14:32").
    pub time: &'a str,
    /// Version string for a pending update, if available.
    pub update_available: Option<&'a str>,
    /// Current update progress state, if an update is in progress.
    pub update_progress: Option<&'a UpdateProgressState>,
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
/// UI chrome never needs more than this many columns; keeping counts in the
/// `u16` range lets us convert exactly into `f32` for pixel math.
const MAX_RENDER_COLUMNS: usize = 65_535;
/// Network sparklines saturate at 100 MB/s.
const NET_SPARK_MAX_BYTES_PER_SEC: u64 = 100_000_000;
type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

pub struct StatusBarBuildContext<'a, 'data> {
    pub out: &'a mut Vec<CellInstance>,
    pub window_rect: Rect,
    pub cell_size: (f32, f32),
    pub status_bar_height: f32,
    pub colors: &'a StatusBarColors,
    pub data: &'data StatusBarData<'data>,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

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
    /// 1px hairline at the top edge of the status bar.
    pub top_border: [f32; 4],
    /// Lighter top half for subtle gradient depth.
    pub gradient_top: [f32; 4],
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
            top_border: srgb_to_linear_rgba(chrome.status_bar_separator),
            gradient_top: srgb_to_linear_rgba(chrome.tab_bar_gradient_top),
        }
    }
}

/// Build cell instances for the window-level status bar.
///
/// The bar spans the full `window_rect` width and is anchored at
/// `window_rect.y + window_rect.height - status_bar_height`.
pub fn build_status_bar(ctx: StatusBarBuildContext<'_, '_>) -> StatusBarHitTargets {
    let StatusBarBuildContext {
        out,
        window_rect,
        cell_size,
        status_bar_height,
        colors,
        data,
        resolve_glyph,
    } = ctx;
    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 {
        return StatusBarHitTargets {
            equalize_rect: None,
            gear_rect: None,
            update_rect: None,
            tooltip_targets: Vec::new(),
        };
    }

    let bar_y = window_rect.y + window_rect.height - status_bar_height;
    let max_cols = columns_in_width(window_rect.width, cell_w);
    let mut w = BarWriter {
        out,
        x_origin: window_rect.x,
        y: bar_y + ((status_bar_height - cell_h) / 2.0).max(0.0),
        bar_y,
        cell_w,
        max_cols,
        col: 0,
        bar_height: status_bar_height,
        resolve_glyph,
    };

    // 1px hairline separator at the top edge.
    w.out.push(scribe_renderer::chrome::solid_quad(
        window_rect.x,
        bar_y,
        window_rect.width,
        1.0,
        colors.top_border,
    ));

    // Two-tone gradient background: lighter top half, darker bottom half.
    let half = status_bar_height / 2.0;
    build_background(
        w.out,
        BackgroundBand {
            x_origin: w.x_origin,
            y: bar_y,
            cols: w.max_cols,
            cell_w: w.cell_w,
            total_width: window_rect.width,
            bg: colors.gradient_top,
            height: half,
        },
    );
    build_background(
        w.out,
        BackgroundBand {
            x_origin: w.x_origin,
            y: bar_y + half,
            cols: w.max_cols,
            cell_w: w.cell_w,
            total_width: window_rect.width,
            bg: colors.bg,
            height: half,
        },
    );

    let mut tooltips: Vec<TooltipAnchor> = Vec::new();
    let left_end = render_left_side(&mut w, colors, data, &mut tooltips);
    w.col = left_end;

    let (equalize_rect, gear_rect, right_start) =
        render_right_side(&mut w, colors, data, &mut tooltips);

    let update_rect = render_centered_update(&mut w, colors, data, left_end, right_start);

    StatusBarHitTargets { equalize_rect, gear_rect, update_rect, tooltip_targets: tooltips }
}

fn render_column_units(columns: usize) -> u16 {
    u16::try_from(columns).unwrap_or(u16::MAX)
}

fn status_col_x(x_origin: f32, column: usize, cell_w: f32) -> f32 {
    x_origin + f32::from(render_column_units(column)) * cell_w
}

fn status_cols_width(columns: usize, cell_w: f32) -> f32 {
    f32::from(render_column_units(columns)) * cell_w
}

/// Mutable writer state for emitting status bar characters.
struct BarWriter<'a> {
    out: &'a mut Vec<CellInstance>,
    x_origin: f32,
    y: f32,
    bar_y: f32,
    cell_w: f32,
    max_cols: usize,
    col: usize,
    bar_height: f32,
    resolve_glyph: &'a mut GlyphResolver<'a>,
}

impl BarWriter<'_> {
    /// Compute a [`Rect`] spanning `start_col..self.col` at the bar's Y position.
    fn col_rect(&self, start_col: usize) -> Rect {
        let x = status_col_x(self.x_origin, start_col, self.cell_w);
        let width = status_cols_width(self.col.saturating_sub(start_col), self.cell_w);
        Rect { x, y: self.bar_y, width, height: self.bar_height }
    }

    /// Compute a [`Rect`] spanning `start_col..end_col` at the bar's Y position.
    fn col_rect_range(&self, start_col: usize, end_col: usize) -> Rect {
        let x = status_col_x(self.x_origin, start_col, self.cell_w);
        let width = status_cols_width(end_col.saturating_sub(start_col), self.cell_w);
        Rect { x, y: self.bar_y, width, height: self.bar_height }
    }

    /// Emit a single character at the current column with the given colors.
    fn put(&mut self, ch: char, fg: [f32; 4], bg: [f32; 4]) {
        if self.col >= self.max_cols {
            return;
        }
        let x = status_col_x(self.x_origin, self.col, self.cell_w);
        let (uv_min, uv_max) = (self.resolve_glyph)(ch);
        self.out.push(CellInstance {
            pos: [x, self.y],
            size: [0.0, 0.0],
            uv_min,
            uv_max,
            fg_color: fg,
            bg_color: bg,
            corner_radius: 0.0,
        });
        self.col += 1;
    }

    /// Emit a single character at an explicit column index.
    fn put_at(&mut self, col: usize, ch: char, fg: [f32; 4], bg: [f32; 4]) {
        if col >= self.max_cols {
            return;
        }
        let x = status_col_x(self.x_origin, col, self.cell_w);
        let (uv_min, uv_max) = (self.resolve_glyph)(ch);
        self.out.push(CellInstance {
            pos: [x, self.y],
            size: [0.0, 0.0],
            uv_min,
            uv_max,
            fg_color: fg,
            bg_color: bg,
            corner_radius: 0.0,
        });
    }

    /// Emit a string with the given colors.
    fn put_str(&mut self, s: &str, fg: [f32; 4], bg: [f32; 4]) {
        for ch in s.chars() {
            self.put(ch, fg, bg);
        }
    }

    /// Fill from current column to target with spaces.
    fn pad_to(&mut self, target: usize, fg: [f32; 4], bg: [f32; 4]) {
        while self.col < target {
            self.put(' ', fg, bg);
        }
    }

    /// Emit a string at an explicit column start index and return the end column.
    fn put_str_at(&mut self, start_col: usize, s: &str, fg: [f32; 4], bg: [f32; 4]) -> usize {
        let mut end_col = start_col;
        for (offset, ch) in s.chars().enumerate() {
            let col = start_col + offset;
            if col >= self.max_cols {
                break;
            }
            self.put_at(col, ch, fg, bg);
            end_col = col + 1;
        }
        end_col
    }
}

/// Information about the centered update CTA area and whether it should handle clicks.
struct CenteredUpdateSegment {
    labels: Vec<String>,
    clickable: bool,
}

/// Resolve the centered status-bar update label and clickability from update state.
fn resolve_centered_update_segment(
    update_available: Option<&str>,
    update_progress: Option<&UpdateProgressState>,
) -> Option<CenteredUpdateSegment> {
    match update_progress {
        Some(UpdateProgressState::Downloading) => Some(CenteredUpdateSegment {
            labels: vec![String::from("Downloading...")],
            clickable: false,
        }),
        Some(UpdateProgressState::Verifying) => Some(CenteredUpdateSegment {
            labels: vec![String::from("Verifying...")],
            clickable: false,
        }),
        Some(UpdateProgressState::Installing) => Some(CenteredUpdateSegment {
            labels: vec![String::from("Installing...")],
            clickable: false,
        }),
        Some(UpdateProgressState::Completed { .. }) => {
            Some(CenteredUpdateSegment { labels: vec![String::from("Updated!")], clickable: false })
        }
        Some(UpdateProgressState::CompletedRestartRequired { .. }) => Some(CenteredUpdateSegment {
            labels: vec![String::from("Updated! Restart required")],
            clickable: true,
        }),
        Some(UpdateProgressState::Failed { .. }) => Some(CenteredUpdateSegment {
            labels: vec![String::from("Update failed")],
            clickable: false,
        }),
        None => update_available.map(|version| CenteredUpdateSegment {
            labels: vec![format!("↑ Update to v{version}"), String::from("↑ Update")],
            clickable: true,
        }),
    }
}

/// Pick the start column to center a `label_cols`-wide label inside the empty
/// span `[left_end, right_start)`. Returns `None` when the empty span is
/// missing or too narrow to hold the label — callers should then try a shorter
/// fallback label. Centering is done relative to the empty span, not the full
/// bar, so the label stays visible on narrow windows.
fn centered_start_col(left_end: usize, right_start: usize, label_cols: usize) -> Option<usize> {
    if label_cols == 0 || left_end >= right_start {
        return None;
    }
    let available = right_start - left_end;
    if label_cols > available {
        return None;
    }
    Some(left_end + (available - label_cols) / 2)
}

/// Render the centered update segment and return the clickable rectangle if appropriate.
fn render_centered_update(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    left_end: usize,
    right_start: usize,
) -> Option<Rect> {
    let segment = resolve_centered_update_segment(data.update_available, data.update_progress)?;

    if w.max_cols == 0 {
        return None;
    }

    for label in &segment.labels {
        let label_cols = label.chars().count();
        let Some(start_col) = centered_start_col(left_end, right_start, label_cols) else {
            continue;
        };
        let end_col = start_col + label_cols;

        w.put_str_at(start_col, label, colors.text, colors.bg);
        return segment.clickable.then_some(w.col_rect_range(start_col, end_col));
    }

    None
}

/// Render the left side: connection dot, workspace name (if multi), CWD.
fn render_left_side(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    tooltips: &mut Vec<TooltipAnchor>,
) -> usize {
    w.put(' ', colors.text, colors.bg);

    // Connection dot tooltip.
    let dot_col = w.col;
    let dot_color = if data.connected { colors.connected_dot } else { colors.disconnected_dot };
    w.put('\u{25CF}', dot_color, colors.bg);
    let dot_text =
        if data.connected { String::from("Connected") } else { String::from("Disconnected") };
    tooltips.push(TooltipAnchor { text: dot_text, rect: w.col_rect(dot_col) });

    w.put(' ', colors.text, colors.bg);

    if let Some(name) = data.workspace_name {
        let ws_col = w.col;
        w.put_str(name, colors.accent, colors.bg);
        tooltips.push(TooltipAnchor {
            text: String::from("Focused workspace"),
            rect: w.col_rect(ws_col),
        });
        w.put_str("  ", colors.text, colors.bg);
    }

    if let Some(cwd) = data.cwd {
        let short = shorten_cwd(cwd);
        let cwd_col = w.col;
        w.put_str(&short, colors.text, colors.bg);
        tooltips.push(TooltipAnchor {
            text: format!("Current directory: {}", cwd.to_string_lossy()),
            rect: w.col_rect(cwd_col),
        });
    }

    w.col
}

/// Render the right side: git branch | session count | tmux | host | time | equalize | gear.
///
/// Returns `(equalize_rect, gear_rect)` — clickable rects for equalize and gear icons.
fn render_right_side(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    tooltips: &mut Vec<TooltipAnchor>,
) -> (Option<Rect>, Option<Rect>, usize) {
    // +3 for the gear segment: " ⚙ " (space, gear, space)
    let gear_cols: usize = 3;
    // +2 for the equalize segment: " ⊞" (space, icon) — gear provides the trailing space
    let equalize_cols: usize = if data.show_equalize { 2 } else { 0 };
    let segments = build_right_segments(data, colors);
    let right_cols: usize =
        segments.iter().map(|s| s.text.chars().count()).sum::<usize>() + equalize_cols + gear_cols;
    let right_start = w.max_cols.saturating_sub(right_cols + 1);

    w.pad_to(right_start, colors.text, colors.bg);

    // Render segments tracking per-named-item rects for tooltips.
    let groups = build_segment_groups(data);
    render_right_segments_with_tooltips(w, colors.bg, &segments, tooltips, &groups);

    // Equalize icon: " ⊞" left of the gear, only when multiple workspaces exist.
    let equalize_rect = render_equalize(w, colors, data);
    if let Some(eq_rect) = equalize_rect {
        tooltips.push(TooltipAnchor { text: String::from("Equalize workspaces"), rect: eq_rect });
    }

    // Gear icon: " ⚙ " at the far right.
    let gear_rect = render_gear(w, colors);
    if let Some(g_rect) = gear_rect {
        tooltips.push(TooltipAnchor { text: String::from("Settings (Ctrl+,)"), rect: g_rect });
    }

    w.pad_to(w.max_cols, colors.text, colors.bg);

    (equalize_rect, gear_rect, right_start)
}

/// Build the `(tooltip_text, segment_count)` group list from `data`.
///
/// Segment counts mirror the layout logic in `build_stats_segments` /
/// `build_right_segments` so that we can slice the flat segment list into
/// named groups for tooltip rect assignment.
fn build_segment_groups(data: &StatusBarData<'_>) -> Vec<(String, usize)> {
    let mut groups: Vec<(String, usize)> = Vec::new();

    if let (Some(stats), Some(config)) = (data.sys_stats, data.stats_config) {
        if config.usage.compute.cpu {
            let sep = usize::from(!groups.is_empty());
            groups.push((String::from("CPU usage"), sep + 1 + CPU_SPARK_WIDTH + 1));
        }
        if config.usage.memory {
            let sep = usize::from(!groups.is_empty());
            groups.push((String::from("Memory usage"), sep + 3));
        }
        if config.network {
            let sep = usize::from(!groups.is_empty());
            let count = sep + 1 + NET_SPARK_WIDTH + 1 + 1 + NET_SPARK_WIDTH + 1;
            groups.push((String::from("Network activity"), count));
        }
        if config.usage.compute.gpu && stats.gpu_percent.is_some() {
            let sep = usize::from(!groups.is_empty());
            groups.push((String::from("GPU usage"), sep + 1 + CPU_SPARK_WIDTH + 1));
        }
    }

    if data.git_branch.is_some() {
        let sep = usize::from(!groups.is_empty());
        groups.push((String::from("Git branch"), sep + 1));
    }

    if data.session_count > 0 {
        let sep = usize::from(!groups.is_empty());
        groups.push((String::from("Active sessions"), sep + 1));
    }

    if let Some(tmux_label) = data.tmux_label {
        if !tmux_label.is_empty() {
            let sep = usize::from(!groups.is_empty());
            groups.push((String::from("tmux session"), sep + 1));
        }
    }

    if !data.host_label.is_empty() {
        let sep = usize::from(!groups.is_empty());
        groups.push((String::from("Host"), sep + 1));
    }

    if !data.time.is_empty() {
        let sep = usize::from(!groups.is_empty());
        groups.push((String::from("Current time"), sep + 1));
    }

    groups
}

/// Render right-side segments and push tooltip anchors for each named item.
fn render_right_segments_with_tooltips(
    w: &mut BarWriter<'_>,
    bg: [f32; 4],
    segments: &[RightSegment],
    tooltips: &mut Vec<TooltipAnchor>,
    groups: &[(String, usize)],
) {
    let mut seg_idx: usize = 0;

    // Render each named group and record its tooltip rect.
    for (tooltip_text, count) in groups {
        let start_col = w.col;
        let end_seg = seg_idx + count;
        while seg_idx < end_seg {
            if let Some(seg) = segments.get(seg_idx) {
                w.put_str(&seg.text, seg.color, bg);
                seg_idx += 1;
            } else {
                break;
            }
        }
        if w.col > start_col {
            tooltips
                .push(TooltipAnchor { text: tooltip_text.clone(), rect: w.col_rect(start_col) });
        }
    }

    // Trailing space segment (1 segment, no tooltip).
    if let Some(seg) = segments.get(seg_idx) {
        w.put_str(&seg.text, seg.color, bg);
    }
}

/// Render the equalize icon and return its clickable rect, or `None` when hidden.
fn render_equalize(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
) -> Option<Rect> {
    if !data.show_equalize || w.col >= w.max_cols {
        return None;
    }
    w.put(' ', colors.text, colors.bg);
    let eq_col = w.col;
    w.put('\u{229E}', colors.text, colors.bg);
    Some(w.col_rect(eq_col))
}

/// Render the gear icon and return its clickable rect.
fn render_gear(w: &mut BarWriter<'_>, colors: &StatusBarColors) -> Option<Rect> {
    if w.col >= w.max_cols {
        return None;
    }
    w.put(' ', colors.text, colors.bg);
    let gear_col = w.col;
    w.put('\u{2699}', colors.text, colors.bg);
    w.put(' ', colors.text, colors.bg);
    Some(w.col_rect(gear_col))
}

/// Map a 0-100 percentage to a Unicode block element (▁▂▃▄▅▆▇█).
fn sparkline_char(pct: f32) -> char {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    const THRESHOLDS: [f32; 7] =
        [7.142_857, 21.428_572, 35.714_287, 50.0, 64.285_71, 78.571_43, 92.857_14];

    if !pct.is_finite() {
        return BLOCKS.first().copied().unwrap_or('▁');
    }

    let index = THRESHOLDS.iter().position(|threshold| pct <= *threshold).unwrap_or(7);
    BLOCKS.get(index).copied().unwrap_or('▁')
}

fn sparkline_char_for_network_rate(bytes_per_sec: u64) -> char {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    let capped = bytes_per_sec.min(NET_SPARK_MAX_BYTES_PER_SEC);
    let rounded_index = capped.saturating_mul(7).saturating_add(NET_SPARK_MAX_BYTES_PER_SEC / 2)
        / NET_SPARK_MAX_BYTES_PER_SEC;
    let index = usize::try_from(rounded_index).unwrap_or(BLOCKS.len().saturating_sub(1));
    BLOCKS.get(index).copied().unwrap_or('▁')
}

fn rounded_div(value: u64, divisor: u64) -> u64 {
    value.saturating_add(divisor / 2) / divisor
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
        let mb = rounded_div(bytes_per_sec, 1_000_000);
        if mb >= 1_000 { String::from(">1G") } else { format!("{mb}M") }
    } else if bytes_per_sec >= 1_000_000 {
        let tenths_mb = rounded_div(bytes_per_sec, 100_000);
        format!("{}.{}M", tenths_mb / 10, tenths_mb % 10)
    } else if bytes_per_sec >= 1_000 {
        let kb = rounded_div(bytes_per_sec, 1_000);
        if kb >= 1_000 { String::from("1.0M") } else { format!("{kb}K") }
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

    if config.usage.compute.cpu {
        push_sep(&mut segs, colors.separator);
        segs.extend(build_cpu_segments(stats, colors));
    }

    if config.usage.memory {
        push_sep(&mut segs, colors.separator);
        segs.extend(build_mem_segments(stats, colors));
    }

    if config.network {
        push_sep(&mut segs, colors.separator);
        segs.extend(build_net_segments(stats, colors));
    }

    if config.usage.compute.gpu && stats.gpu_percent.is_some() {
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
        segs.push(RightSegment {
            text: sparkline_char_for_network_rate(v).to_string(),
            color: colors.accent,
        });
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
        segs.push(RightSegment {
            text: sparkline_char_for_network_rate(v).to_string(),
            color: colors.accent,
        });
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

/// Build the right-side text segments: stats, git branch, session count, tmux, host, time.
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

    if let Some(tmux_label) = data.tmux_label {
        push_sep(&mut segs, colors.separator);
        segs.push(RightSegment { text: format!("tmux:{tmux_label}"), color: colors.accent });
    }

    if !data.host_label.is_empty() {
        push_sep(&mut segs, colors.separator);
        segs.push(RightSegment { text: String::from(data.host_label), color: colors.text });
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
#[derive(Clone, Copy)]
struct BackgroundBand {
    x_origin: f32,
    y: f32,
    cols: usize,
    cell_w: f32,
    total_width: f32,
    bg: [f32; 4],
    height: f32,
}

fn build_background(out: &mut Vec<CellInstance>, band: BackgroundBand) {
    let BackgroundBand { x_origin, y, cols, cell_w, total_width, bg, height } = band;
    for col_idx in 0..cols {
        let x = status_col_x(x_origin, col_idx, cell_w);
        out.push(CellInstance {
            pos: [x, y],
            size: [cell_w, height],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: bg,
            bg_color: bg,
            corner_radius: 0.0,
        });
    }

    // Fill the fractional-pixel remainder at the right edge.
    let remainder = total_width - status_cols_width(cols, cell_w);
    if remainder > 0.0 {
        let x = status_col_x(x_origin, cols, cell_w);
        out.push(CellInstance {
            pos: [x, y],
            size: [remainder, height],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: bg,
            bg_color: bg,
            corner_radius: 0.0,
        });
    }
}

/// Calculate how many cell-width columns fit in a given pixel width.
fn columns_in_width(width: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 || !width.is_finite() || width <= 0.0 {
        return 0;
    }

    let mut low = 0usize;
    let mut high = 1usize;
    while high < MAX_RENDER_COLUMNS && status_cols_width(high, cell_w) <= width {
        low = high;
        high = high.saturating_mul(2).min(MAX_RENDER_COLUMNS);
        if high == low {
            break;
        }
    }

    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if status_cols_width(mid, cell_w) <= width {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    low
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centers_label_in_empty_span_not_full_bar() {
        // Narrow bar where the full-bar midpoint lands inside the left side: the
        // label only fits when centered inside the empty span [50, 70).
        assert_eq!(centered_start_col(50, 70, 19), Some(50));
    }

    #[test]
    fn centers_inside_a_far_off_center_empty_span() {
        assert_eq!(centered_start_col(150, 200, 10), Some(170));
    }

    #[test]
    fn returns_none_when_label_wider_than_empty_span() {
        assert_eq!(centered_start_col(50, 60, 19), None);
    }

    #[test]
    fn returns_none_when_no_empty_span() {
        assert_eq!(centered_start_col(80, 80, 5), None);
        // saturating math elsewhere can produce right_start < left_end; treat as no room.
        assert_eq!(centered_start_col(90, 80, 5), None);
    }

    #[test]
    fn returns_none_for_zero_width_label() {
        assert_eq!(centered_start_col(0, 100, 0), None);
    }

    #[test]
    fn fits_label_that_exactly_fills_empty_span() {
        // No slack: label aligns flush against left_end.
        assert_eq!(centered_start_col(10, 18, 8), Some(10));
    }
}
