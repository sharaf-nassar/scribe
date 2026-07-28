//! Window-level status bar, ported from the legacy client's quad renderer.
//!
//! The legacy client built the status bar by emitting GPU
//! quads straight into the terminal grid buffer. The GPUI rebuild keeps the
//! **segment model** byte-for-byte — the same connection dot, command-status
//! glyph, env warning, workspace/CWD/git/host labels, tmux + session badges,
//! clock, CPU/MEM/GPU/NET sparklines, the centered update CTA, and the 013/015
//! remote-control and share-presence surfaces — but lowers it onto a GPUI flex
//! row instead of hand-placed columns.
//!
//! The layout logic splits in two: [`build_model`] is a pure function turning
//! [`StatusBarData`] into a [`StatusBarModel`] of coloured [`Span`]s (left /
//! centre / right groups), unit-tested without a live window; [`render`] maps
//! that model onto GPUI elements. Colours stay in sRGB space here (GPUI does
//! its own linear conversion), unlike the legacy renderer which pre-multiplied
//! into linear for the raw GPU pipeline.

use std::path::Path;

use gpui::{App, ClickEvent, Rgba, Window, div, prelude::*, px};
use scribe_common::config::StatusBarStatsConfig;
use scribe_common::protocol::{ControllerInfo, EnvStatusState, UpdateProgressState};
use scribe_common::theme::ChromeColors;

use crate::opacity::scale_slot;
use crate::sys_stats::SystemStats;

/// Outcome of a focused pane's most-recently-resolved command.
///
/// Ported verbatim from the legacy client's `pane::CommandStatus`. `Unknown`
/// MUST never be rendered with failure styling — an unreported exit status is
/// distinct from a failure (FR-012 / SC-006).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    /// Command reported exit code 0.
    Success,
    /// Command reported a non-zero exit code.
    Failure,
    /// No exit code was resolved before the next prompt.
    Unknown,
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
/// Network sparklines saturate at 100 MB/s.
const NET_SPARK_MAX_BYTES_PER_SEC: u64 = 100_000_000;

/// Feature 015 (T024/T026): the shared-window presence badge inputs.
pub struct SharePresenceData {
    /// Total attached participants (owner + remotes), always ≥ 2 when present.
    pub participant_count: usize,
    /// Display label of the current control holder, or `None` when unheld.
    pub holder: Option<String>,
}

/// Feature 013 (T022): owning-machine remote-control status inputs.
pub struct RemoteStatusData<'a> {
    /// Whether this machine currently allows remote control (`remote.enabled`).
    pub enabled: bool,
    /// One entry per window on this machine a remote peer currently controls.
    pub controllers: &'a [ControllerInfo],
}

/// Data needed to render the window-level status bar.
pub struct StatusBarData<'a> {
    pub connected: bool,
    /// Name of the focused workspace (shown when multiple workspaces exist).
    pub workspace_name: Option<&'a str>,
    /// CWD of the focused pane, displayed as a shortened path.
    pub cwd: Option<&'a Path>,
    /// Git branch of the focused pane.
    pub git_branch: Option<&'a str>,
    /// Outcome of the focused pane's most-recently-resolved command.
    pub last_command_status: Option<CommandStatus>,
    /// Env-capture runtime state for the focused pane (feature 006).
    pub env_status: Option<&'a EnvStatusState>,
    /// Total number of active sessions in this window.
    pub session_count: usize,
    /// Feature 013 (T022): owning-machine remote-control status.
    pub remote: RemoteStatusData<'a>,
    /// Feature 015 (T024/T026): the active share's presence badge.
    pub share_presence: Option<SharePresenceData>,
    /// Remote or local host label for the focused pane.
    pub host_label: &'a str,
    /// Feature 014 (T025): controlling-side transport indicator.
    pub remote_transport: Option<&'a str>,
    /// tmux session label for the focused pane when present.
    pub tmux_label: Option<&'a str>,
    /// Current time string (e.g. "14:32"). Empty renders nothing.
    pub time: &'a str,
    /// Version string for a pending update, if available.
    pub update_available: Option<&'a str>,
    /// Current update progress state, if an update is in progress.
    pub update_progress: Option<&'a UpdateProgressState>,
    pub sys_stats: Option<&'a SystemStats>,
    pub stats_config: Option<&'a StatusBarStatsConfig>,
}

/// sRGB colours for the status bar, derived from the theme's [`ChromeColors`]
/// and ANSI palette. Unlike the legacy renderer these stay in sRGB space; GPUI
/// converts to linear when it paints.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatusBarColors {
    pub bg: [f32; 4],
    pub text: [f32; 4],
    pub accent: [f32; 4],
    pub separator: [f32; 4],
    /// Connection dot when connected (ANSI green).
    pub connected_dot: [f32; 4],
    /// Connection dot when disconnected (ANSI red).
    pub disconnected_dot: [f32; 4],
    /// Moderate usage (60–85%) — ANSI yellow.
    pub warning: [f32; 4],
    /// High usage (>85%) — ANSI red.
    pub critical: [f32; 4],
    /// Dimmed colour for stat labels.
    pub label: [f32; 4],
    /// 1px hairline at the top edge.
    pub top_border: [f32; 4],
}

impl StatusBarColors {
    /// Build status bar colours from chrome colours and the ANSI palette. The
    /// values are kept in sRGB (theme space) for GPUI, mirroring the legacy
    /// slot selection minus its linear conversion.
    pub fn from_theme(chrome: &ChromeColors, ansi_colors: &[[f32; 4]; 16]) -> Self {
        let text = chrome.status_bar_text;
        Self {
            bg: chrome.status_bar_bg,
            text,
            accent: chrome.accent,
            separator: chrome.divider,
            connected_dot: ansi_colors.get(2).copied().unwrap_or(FALLBACK_GREEN),
            disconnected_dot: ansi_colors.get(1).copied().unwrap_or(FALLBACK_RED),
            warning: ansi_colors.get(3).copied().unwrap_or(FALLBACK_YELLOW),
            critical: ansi_colors.get(1).copied().unwrap_or(FALLBACK_RED),
            label: [
                text.first().copied().unwrap_or(0.0),
                text.get(1).copied().unwrap_or(0.0),
                text.get(2).copied().unwrap_or(0.0),
                text.get(3).copied().unwrap_or(1.0) * 0.55,
            ],
            top_border: chrome.status_bar_separator,
        }
    }

    /// Return this palette with `appearance.opacity` folded into the filled
    /// band background.
    ///
    /// Only `bg` scales: the band is a window background, while the text,
    /// sparkline and hairline colours are content that must stay legible over
    /// whatever the translucent window reveals.
    #[must_use]
    pub fn with_opacity(self, opacity: f32) -> Self {
        Self { bg: scale_slot(self.bg, opacity), ..self }
    }
}

/// One styled run of text in the status bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub color: [f32; 4],
}

impl Span {
    fn new(text: impl Into<String>, color: [f32; 4]) -> Self {
        Self { text: text.into(), color }
    }
}

/// The full status-bar layout as three coloured-span groups. The centre group
/// (the update CTA) sits in flex-grown space between left and right so it stays
/// centred as the window resizes, mirroring the legacy empty-span centering.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusBarModel {
    pub left: Vec<Span>,
    pub center: Option<Span>,
    /// Whether the centred CTA should accept clicks (update actionable).
    pub center_clickable: bool,
    pub right: Vec<Span>,
}

/// Build the pure status-bar model from its inputs. Every segment enumerated in
/// the parity checklist is produced here; [`render`] only maps it to elements.
pub fn build_model(data: &StatusBarData<'_>, colors: &StatusBarColors) -> StatusBarModel {
    let left = build_left(data, colors);
    let right = build_right(data, colors);
    let (center, center_clickable) = build_center(data, colors)
        .map_or((None, false), |(span, clickable)| (Some(span), clickable));
    StatusBarModel { left, center, center_clickable, right }
}

// ---------------------------------------------------------------------------
// Left side
// ---------------------------------------------------------------------------

/// Left side: connection dot, command status, env warning, remote/share
/// surfaces, workspace name, CWD.
fn build_left(data: &StatusBarData<'_>, colors: &StatusBarColors) -> Vec<Span> {
    let mut spans = Vec::new();
    spans.push(Span::new(" ", colors.text));

    let dot_color = if data.connected { colors.connected_dot } else { colors.disconnected_dot };
    spans.push(Span::new("\u{25CF}", dot_color));
    spans.push(Span::new(" ", colors.text));

    push_command_status(&mut spans, colors, data.last_command_status);
    push_env_status_warning(&mut spans, colors, data.env_status);
    push_remote_status(&mut spans, colors, data);

    if let Some(name) = data.workspace_name {
        spans.push(Span::new(name, colors.accent));
        spans.push(Span::new("  ", colors.text));
    }

    if let Some(cwd) = data.cwd {
        spans.push(Span::new(shorten_cwd(cwd), colors.text));
    }

    spans
}

/// Command outcome glyph. The distinct glyph is the accessible cue (FR-009);
/// colour is a redundant hint. `None` renders nothing.
fn push_command_status(
    spans: &mut Vec<Span>,
    colors: &StatusBarColors,
    status: Option<CommandStatus>,
) {
    let Some(status) = status else { return };
    let (glyph, color) = match status {
        CommandStatus::Success => ('\u{2713}', colors.connected_dot),
        CommandStatus::Failure => ('\u{2717}', colors.disconnected_dot),
        CommandStatus::Unknown => ('?', colors.label),
    };
    spans.push(Span::new(glyph.to_string(), color));
    spans.push(Span::new(" ", colors.text));
}

/// Env-capture warning glyph (feature 006). Fires only for `Degraded`.
fn push_env_status_warning(
    spans: &mut Vec<Span>,
    colors: &StatusBarColors,
    env_status: Option<&EnvStatusState>,
) {
    let Some(EnvStatusState::Degraded { .. }) = env_status else { return };
    spans.push(Span::new("\u{26A0}", colors.warning));
    spans.push(Span::new(" ", colors.text));
}

/// Owning-machine remote-control (013), transport-agnostic controller summary,
/// and share-presence badge (015).
fn push_remote_status(spans: &mut Vec<Span>, colors: &StatusBarColors, data: &StatusBarData<'_>) {
    if data.remote.enabled {
        spans.push(Span::new("\u{21C5}", colors.label));
        spans.push(Span::new(" ", colors.text));
    }

    if let Some(summary) = build_remote_control_summary(data.remote.controllers) {
        spans.push(Span::new(summary, colors.accent));
        spans.push(Span::new("  ", colors.text));
    }

    if let Some(presence) = &data.share_presence {
        spans.push(Span::new("\u{21C5}", colors.accent));
        spans.push(Span::new(" ", colors.text));
        spans.push(Span::new(share_presence_badge(presence), colors.accent));
        spans.push(Span::new("  ", colors.text));
    }
}

// ---------------------------------------------------------------------------
// Centered update CTA
// ---------------------------------------------------------------------------

/// Resolve the centred CTA span and whether it should accept clicks. GPUI's
/// flex layout centres the span dynamically, so unlike the legacy renderer we
/// always use the full-length label and never fall back to a shorter form.
fn build_center(data: &StatusBarData<'_>, colors: &StatusBarColors) -> Option<(Span, bool)> {
    let (label, clickable) = match data.update_progress {
        Some(UpdateProgressState::Downloading) => ("Downloading...".to_owned(), false),
        Some(UpdateProgressState::Verifying) => ("Verifying...".to_owned(), false),
        Some(UpdateProgressState::Installing) => ("Installing...".to_owned(), false),
        Some(UpdateProgressState::Completed { .. }) => ("Updated!".to_owned(), false),
        Some(UpdateProgressState::CompletedRestartRequired { .. }) => {
            ("Updated! Restart required".to_owned(), true)
        }
        Some(UpdateProgressState::Failed { .. }) => ("Update failed".to_owned(), false),
        None => match data.update_available {
            Some(version) => (format!("\u{2191} Update to v{version}"), true),
            None => return None,
        },
    };
    Some((Span::new(label, colors.text), clickable))
}

// ---------------------------------------------------------------------------
// Right side
// ---------------------------------------------------------------------------

/// Right side: system stats, git branch, session count, tmux, transport, host,
/// clock.
fn build_right(data: &StatusBarData<'_>, colors: &StatusBarColors) -> Vec<Span> {
    let mut spans = Vec::new();

    if let (Some(stats), Some(config)) = (data.sys_stats, data.stats_config) {
        push_stats(&mut spans, stats, config, colors);
    }

    if let Some(branch) = data.git_branch {
        push_sep(&mut spans, colors);
        spans.push(Span::new(branch, colors.accent));
    }

    if data.session_count > 0 {
        push_sep(&mut spans, colors);
        let label = if data.session_count == 1 {
            "1 session".to_owned()
        } else {
            format!("{} sessions", data.session_count)
        };
        spans.push(Span::new(label, colors.text));
    }

    if let Some(tmux_label) = data.tmux_label {
        push_sep(&mut spans, colors);
        spans.push(Span::new(format!("tmux:{tmux_label}"), colors.accent));
    }

    if let Some(transport) = data.remote_transport {
        push_sep(&mut spans, colors);
        spans.push(Span::new(format!("\u{21C5} {transport}"), colors.label));
    }

    if !data.host_label.is_empty() {
        push_sep(&mut spans, colors);
        spans.push(Span::new(data.host_label, colors.text));
    }

    if !data.time.is_empty() {
        push_sep(&mut spans, colors);
        spans.push(Span::new(data.time, colors.text));
    }

    spans.push(Span::new(" ", colors.text));
    spans
}

/// Push a separator " │ " span when `spans` is non-empty.
fn push_sep(spans: &mut Vec<Span>, colors: &StatusBarColors) {
    if !spans.is_empty() {
        spans.push(Span::new(" \u{2502} ", colors.separator));
    }
}

/// CPU / MEM / NET / GPU stat groups, each gated by config.
fn push_stats(
    spans: &mut Vec<Span>,
    stats: &SystemStats,
    config: &StatusBarStatsConfig,
    colors: &StatusBarColors,
) {
    if config.usage.compute.cpu {
        push_sep(spans, colors);
        push_cpu(spans, stats, colors);
    }
    if config.usage.memory {
        push_sep(spans, colors);
        push_mem(spans, stats, colors);
    }
    if config.network {
        push_sep(spans, colors);
        push_net(spans, stats, colors);
    }
    if config.usage.compute.gpu && stats.gpu_percent.is_some() {
        push_sep(spans, colors);
        push_gpu(spans, stats, colors);
    }
}

/// CPU: label + 8 sparkline bars (left-padded) + fixed-width percentage.
fn push_cpu(spans: &mut Vec<Span>, stats: &SystemStats, colors: &StatusBarColors) {
    spans.push(Span::new("CPU ", colors.label));
    let pad = CPU_SPARK_WIDTH.saturating_sub(stats.cpu_history.len());
    for _ in 0..pad {
        spans.push(Span::new("\u{2581}", colors.label));
    }
    for &v in &stats.cpu_history {
        spans.push(Span::new(sparkline_char(v).to_string(), usage_color(v, colors)));
    }
    let pct = stats.cpu_percent;
    spans.push(Span::new(format!(" {pct:>3.0}%"), usage_color(pct, colors)));
}

/// Memory: label + 1 sparkline bar + fixed-width percentage.
fn push_mem(spans: &mut Vec<Span>, stats: &SystemStats, colors: &StatusBarColors) {
    let mem_pct =
        if stats.mem_total_gb > 0.0 { stats.mem_used_gb / stats.mem_total_gb * 100.0 } else { 0.0 };
    spans.push(Span::new("MEM ", colors.label));
    spans.push(Span::new(sparkline_char(mem_pct).to_string(), usage_color(mem_pct, colors)));
    spans.push(Span::new(format!(" {mem_pct:>3.0}%"), usage_color(mem_pct, colors)));
}

/// Network: ↑ sparklines rate ↓ sparklines rate (all fixed-width).
fn push_net(spans: &mut Vec<Span>, stats: &SystemStats, colors: &StatusBarColors) {
    spans.push(Span::new("\u{2191}", colors.label));
    let up_pad = NET_SPARK_WIDTH.saturating_sub(stats.net_up_history.len());
    for _ in 0..up_pad {
        spans.push(Span::new("\u{2581}", colors.label));
    }
    for &v in &stats.net_up_history {
        spans.push(Span::new(sparkline_char_for_network_rate(v).to_string(), colors.accent));
    }
    spans.push(Span::new(
        format!(" {}", format_bytes_rate_fixed(stats.net_up_bytes_sec)),
        colors.text,
    ));

    spans.push(Span::new(" \u{2193}", colors.label));
    let down_pad = NET_SPARK_WIDTH.saturating_sub(stats.net_down_history.len());
    for _ in 0..down_pad {
        spans.push(Span::new("\u{2581}", colors.label));
    }
    for &v in &stats.net_down_history {
        spans.push(Span::new(sparkline_char_for_network_rate(v).to_string(), colors.accent));
    }
    spans.push(Span::new(
        format!(" {}", format_bytes_rate_fixed(stats.net_down_bytes_sec)),
        colors.text,
    ));
}

/// GPU: label + 8 sparkline bars (left-padded) + fixed-width percentage.
fn push_gpu(spans: &mut Vec<Span>, stats: &SystemStats, colors: &StatusBarColors) {
    let Some(gpu_pct) = stats.gpu_percent else { return };
    spans.push(Span::new("GPU ", colors.label));
    let pad = CPU_SPARK_WIDTH.saturating_sub(stats.gpu_history.len());
    for _ in 0..pad {
        spans.push(Span::new("\u{2581}", colors.label));
    }
    for &v in &stats.gpu_history {
        spans.push(Span::new(sparkline_char(v).to_string(), usage_color(v, colors)));
    }
    spans.push(Span::new(format!(" {gpu_pct:>3.0}%"), usage_color(gpu_pct, colors)));
}

// ---------------------------------------------------------------------------
// Pure helpers (ported verbatim from the legacy renderer)
// ---------------------------------------------------------------------------

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

/// Format bytes/sec as a human-readable string of ≤4 chars.
fn format_bytes_rate(bytes_per_sec: u64) -> String {
    if bytes_per_sec >= 1_000_000_000 {
        ">1G".to_owned()
    } else if bytes_per_sec >= 10_000_000 {
        let mb = rounded_div(bytes_per_sec, 1_000_000);
        if mb >= 1_000 { ">1G".to_owned() } else { format!("{mb}M") }
    } else if bytes_per_sec >= 1_000_000 {
        let tenths_mb = rounded_div(bytes_per_sec, 100_000);
        format!("{}.{}M", tenths_mb / 10, tenths_mb % 10)
    } else if bytes_per_sec >= 1_000 {
        let kb = rounded_div(bytes_per_sec, 1_000);
        if kb >= 1_000 { "1.0M".to_owned() } else { format!("{kb}K") }
    } else {
        format!("{bytes_per_sec}B")
    }
}

/// Format bytes/sec right-aligned in exactly 4 characters.
fn format_bytes_rate_fixed(bytes_per_sec: u64) -> String {
    format!("{:>4}", format_bytes_rate(bytes_per_sec))
}

/// Compact presence-badge text (feature 015, T024).
fn share_presence_badge(presence: &SharePresenceData) -> String {
    let count = presence.participant_count;
    presence.holder.as_ref().map_or_else(
        || format!("{count} attached \u{00B7} no one has control"),
        |holder| format!("{count} attached \u{00B7} {holder} has control"),
    )
}

/// Aggregate the per-window controller list into the status-bar summary
/// (FR-009b), deduplicated by device name in first-seen order.
fn build_remote_control_summary(controllers: &[ControllerInfo]) -> Option<String> {
    if controllers.is_empty() {
        return None;
    }
    let mut tallies: Vec<(&str, usize)> = Vec::new();
    for controller in controllers {
        if let Some(entry) =
            tallies.iter_mut().find(|(device, _)| *device == controller.device_name.as_str())
        {
            entry.1 += 1;
        } else {
            tallies.push((controller.device_name.as_str(), 1));
        }
    }
    let parts: Vec<String> = tallies
        .iter()
        .map(|(device, count)| {
            let noun = if *count == 1 { "window" } else { "windows" };
            format!("{device} controls {count} {noun}")
        })
        .collect();
    Some(parts.join(", "))
}

/// Shorten a CWD path by replacing `$HOME` with `~`.
fn shorten_cwd(path: &Path) -> String {
    shorten_cwd_with_home(path, home_dir().as_deref())
}

/// Pure home-relative shortening, split out so it can be tested without
/// mutating the process environment.
fn shorten_cwd_with_home(path: &Path, home: Option<&Path>) -> String {
    let s = path.to_string_lossy();
    if let Some(home) = home {
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

// ---------------------------------------------------------------------------
// GPUI rendering
// ---------------------------------------------------------------------------

/// Convert an sRGB `[f32; 4]` to a GPUI [`Rgba`].
fn rgba(color: [f32; 4]) -> Rgba {
    Rgba {
        r: color.first().copied().unwrap_or(0.0),
        g: color.get(1).copied().unwrap_or(0.0),
        b: color.get(2).copied().unwrap_or(0.0),
        a: color.get(3).copied().unwrap_or(1.0),
    }
}

/// Render one span group as an inline flex row of coloured text runs.
fn span_row(spans: &[Span]) -> impl IntoElement {
    div().flex().flex_row().items_center().children(
        spans.iter().map(|span| {
            div().text_color(rgba(span.color)).child(span.text.clone()).into_any_element()
        }),
    )
}

/// Click listener for the centred update CTA, boxed so [`render`] can stay a
/// plain function while the caller supplies a `cx.listener(..)` closure bound to
/// its own view.
pub type UpdateClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// Render the centred CTA, wiring the click listener when the model says the
/// update is actionable and the caller supplied one.
///
/// An actionable CTA gets a pointer cursor and an accent hover tint so it reads
/// as a control, matching the legacy client's hit-tested update rect; a purely
/// informational label ("Downloading...", "Update failed") stays inert.
fn center_cta(
    span: &Span,
    clickable: bool,
    colors: &StatusBarColors,
    on_click: Option<UpdateClickHandler>,
) -> gpui::AnyElement {
    let base = div().px_2().text_color(rgba(span.color)).child(span.text.clone());
    match on_click.filter(|_| clickable) {
        Some(listener) => base
            .id("status-bar-update-cta")
            .cursor_pointer()
            .hover(|style| style.text_color(rgba(colors.accent)))
            .on_click(listener)
            .into_any_element(),
        None => base.into_any_element(),
    }
}

/// Render the status bar model onto a full-width GPUI flex row.
///
/// The bar is a monospace `height_px`-tall band anchored at the window bottom
/// with a 1px top hairline. Left and right groups take natural width; the
/// centred CTA lives in flex-grown space so it stays centred as the window
/// resizes.
pub fn render(
    model: &StatusBarModel,
    height_px: f32,
    colors: &StatusBarColors,
    on_update_click: Option<UpdateClickHandler>,
) -> impl IntoElement {
    let center = model
        .center
        .as_ref()
        .map(|span| center_cta(span, model.center_clickable, colors, on_update_click));
    div()
        .w_full()
        // A fixed-height band, never a flexible one: the shell stacks it under
        // a flex-grown terminal grid, and a shrinkable band is what lets a
        // short window squeeze the bar off screen instead of clipping the grid.
        .flex_none()
        .h(px(height_px))
        .flex()
        .flex_row()
        .items_center()
        .px_2()
        .bg(rgba(colors.bg))
        .border_t_1()
        .border_color(rgba(colors.top_border))
        .font_family("monospace")
        .text_xs()
        .text_color(rgba(colors.text))
        .child(span_row(&model.left))
        .child(div().flex_1().flex().flex_row().justify_center().children(center))
        .child(span_row(&model.right))
}

#[cfg(test)]
mod tests {
    // @lat: [[test#GPUI Client Headless Suites#Window opacity#Status bar band scales with opacity]]
    #[test]
    fn status_bar_band_scales_with_opacity() {
        let theme = scribe_common::theme::minimal_dark();
        let base = super::StatusBarColors::from_theme(&theme.chrome, &theme.ansi_colors);
        let dimmed = base.with_opacity(0.85);

        assert!((dimmed.bg[3] - base.bg[3] * 0.85).abs() < 1e-6);
        // Text, hairline and stat colours stay fully legible.
        assert!((dimmed.text[3] - base.text[3]).abs() < 1e-6);
        assert!((dimmed.top_border[3] - base.top_border[3]).abs() < 1e-6);
        assert!((dimmed.label[3] - base.label[3]).abs() < 1e-6);
        // Clamping: a nonsense value saturates rather than inverting the band.
        assert!((base.with_opacity(1.5).bg[3] - base.bg[3]).abs() < 1e-6);
        assert!(base.with_opacity(-0.2).bg[3].abs() < 1e-6);
    }

    use super::*;
    use scribe_common::config::{StatusBarComputeStatsConfig, StatusBarUsageStatsConfig};
    use std::collections::VecDeque;

    fn colors() -> StatusBarColors {
        StatusBarColors {
            bg: [0.0, 0.0, 0.0, 1.0],
            text: [0.8, 0.8, 0.8, 1.0],
            accent: [0.2, 0.5, 0.9, 1.0],
            separator: [0.3, 0.3, 0.3, 1.0],
            connected_dot: [0.0, 1.0, 0.0, 1.0],
            disconnected_dot: [1.0, 0.0, 0.0, 1.0],
            warning: [1.0, 1.0, 0.0, 1.0],
            critical: [1.0, 0.0, 0.0, 1.0],
            label: [0.5, 0.5, 0.5, 1.0],
            top_border: [0.1, 0.1, 0.1, 1.0],
        }
    }

    fn data() -> StatusBarData<'static> {
        StatusBarData {
            connected: true,
            workspace_name: None,
            cwd: None,
            git_branch: None,
            last_command_status: None,
            env_status: None,
            session_count: 0,
            remote: RemoteStatusData { enabled: false, controllers: &[] },
            share_presence: None,
            host_label: "",
            remote_transport: None,
            tmux_label: None,
            time: "",
            update_available: None,
            update_progress: None,
            sys_stats: None,
            stats_config: None,
        }
    }

    fn joined(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    // @lat: [[test#GPUI Status Bar#Connection dot reflects connection state]]
    #[test]
    fn connection_dot_reflects_state() {
        let colors = colors();
        let mut d = data();
        d.connected = true;
        let connected = build_left(&d, &colors);
        let dot_connected = connected.iter().find(|s| s.text == "\u{25CF}").unwrap();
        crate::assert_rgba_eq(dot_connected.color, colors.connected_dot);

        d.connected = false;
        let disconnected = build_left(&d, &colors);
        let dot_disconnected = disconnected.iter().find(|s| s.text == "\u{25CF}").unwrap();
        crate::assert_rgba_eq(dot_disconnected.color, colors.disconnected_dot);
    }

    // @lat: [[test#GPUI Status Bar#Command status glyphs distinguish outcomes]]
    #[test]
    fn command_status_glyphs_distinguish_outcomes() {
        let colors = colors();
        let mut success = Vec::new();
        push_command_status(&mut success, &colors, Some(CommandStatus::Success));
        assert_eq!(success[0].text, "\u{2713}");
        crate::assert_rgba_eq(success[0].color, colors.connected_dot);

        let mut failure = Vec::new();
        push_command_status(&mut failure, &colors, Some(CommandStatus::Failure));
        assert_eq!(failure[0].text, "\u{2717}");
        crate::assert_rgba_eq(failure[0].color, colors.disconnected_dot);

        // Unknown is never failure-styled: it uses the dimmed label colour.
        let mut unknown = Vec::new();
        push_command_status(&mut unknown, &colors, Some(CommandStatus::Unknown));
        assert_eq!(unknown[0].text, "?");
        crate::assert_rgba_eq(unknown[0].color, colors.label);

        // None renders nothing.
        let mut none = Vec::new();
        push_command_status(&mut none, &colors, None);
        assert!(none.is_empty());
    }

    // @lat: [[test#GPUI Status Bar#Env warning fires only when degraded]]
    #[test]
    fn env_warning_fires_only_when_degraded() {
        let colors = colors();
        let mut active = Vec::new();
        push_env_status_warning(&mut active, &colors, Some(&EnvStatusState::Active));
        assert!(active.is_empty());

        let degraded = EnvStatusState::Degraded { reason: "keystore".to_owned() };
        let mut warned = Vec::new();
        push_env_status_warning(&mut warned, &colors, Some(&degraded));
        assert_eq!(warned[0].text, "\u{26A0}");
        crate::assert_rgba_eq(warned[0].color, colors.warning);
    }

    // @lat: [[test#GPUI Status Bar#Sparkline maps percentage to block height]]
    #[test]
    fn sparkline_maps_percentage_to_block_height() {
        assert_eq!(sparkline_char(0.0), '▁');
        assert_eq!(sparkline_char(100.0), '█');
        assert_eq!(sparkline_char(50.0), '▄');
        // Non-finite input clamps to the lowest bar.
        assert_eq!(sparkline_char(f32::NAN), '▁');
        // Network saturates at 100 MB/s.
        assert_eq!(sparkline_char_for_network_rate(0), '▁');
        assert_eq!(sparkline_char_for_network_rate(NET_SPARK_MAX_BYTES_PER_SEC), '█');
    }

    // @lat: [[test#GPUI Status Bar#Usage color escalates with load]]
    #[test]
    fn usage_color_escalates_with_load() {
        let colors = colors();
        crate::assert_rgba_eq(usage_color(10.0, &colors), colors.connected_dot);
        crate::assert_rgba_eq(usage_color(70.0, &colors), colors.warning);
        crate::assert_rgba_eq(usage_color(95.0, &colors), colors.critical);
    }

    // @lat: [[test#GPUI Status Bar#Network rate formats to four columns]]
    #[test]
    fn network_rate_formats_to_four_columns() {
        assert_eq!(format_bytes_rate_fixed(0), "  0B");
        assert_eq!(format_bytes_rate(500), "500B");
        assert_eq!(format_bytes_rate(2_000), "2K");
        assert_eq!(format_bytes_rate(1_500_000), "1.5M");
        assert_eq!(format_bytes_rate(2_000_000_000), ">1G");
    }

    // @lat: [[test#GPUI Status Bar#CWD shortens home to tilde]]
    #[test]
    fn cwd_shortens_home_to_tilde() {
        let home = Path::new("/home/tester");
        assert_eq!(
            shorten_cwd_with_home(Path::new("/home/tester/work/scribe"), Some(home)),
            "~/work/scribe"
        );
        assert_eq!(shorten_cwd_with_home(Path::new("/etc/hosts"), Some(home)), "/etc/hosts");
        assert_eq!(shorten_cwd_with_home(Path::new("/etc/hosts"), None), "/etc/hosts");
    }

    // @lat: [[test#GPUI Status Bar#Right side stitches enabled segments in order]]
    #[test]
    fn right_side_stitches_enabled_segments_in_order() {
        let colors = colors();
        let mut d = data();
        d.git_branch = Some("main");
        d.session_count = 2;
        d.tmux_label = Some("dev");
        d.host_label = "laptop";
        d.time = "14:32";
        let right = joined(&build_right(&d, &colors));
        assert!(right.contains("main"));
        assert!(right.contains("2 sessions"));
        assert!(right.contains("tmux:dev"));
        assert!(right.contains("laptop"));
        assert!(right.contains("14:32"));
        // Single session uses the singular label.
        d.session_count = 1;
        assert!(joined(&build_right(&d, &colors)).contains("1 session"));
    }

    // @lat: [[test#GPUI Status Bar#Remote control summary tallies windows per device]]
    #[test]
    fn remote_control_summary_tallies_windows_per_device() {
        assert_eq!(build_remote_control_summary(&[]), None);
        let controllers = vec![
            ControllerInfo { device_name: "laptop".to_owned(), login_name: "a@b".to_owned() },
            ControllerInfo { device_name: "laptop".to_owned(), login_name: "a@b".to_owned() },
            ControllerInfo { device_name: "phone".to_owned(), login_name: "a@b".to_owned() },
        ];
        assert_eq!(
            build_remote_control_summary(&controllers).unwrap(),
            "laptop controls 2 windows, phone controls 1 window"
        );
    }

    // @lat: [[test#GPUI Status Bar#Share presence badge names the control holder]]
    #[test]
    fn share_presence_badge_names_holder() {
        let held = SharePresenceData { participant_count: 3, holder: Some("laptop".to_owned()) };
        assert_eq!(share_presence_badge(&held), "3 attached \u{00B7} laptop has control");
        let unheld = SharePresenceData { participant_count: 2, holder: None };
        assert_eq!(share_presence_badge(&unheld), "2 attached \u{00B7} no one has control");
    }

    // @lat: [[test#GPUI Status Bar#Centered update CTA reflects progress state]]
    #[test]
    fn centered_update_cta_reflects_progress_state() {
        let colors = colors();
        let mut d = data();
        // No update: no centre segment.
        assert!(build_center(&d, &colors).is_none());

        d.update_available = Some("2.0.0");
        let (available, available_clickable) = build_center(&d, &colors).unwrap();
        assert_eq!(available.text, "\u{2191} Update to v2.0.0");
        assert!(available_clickable);

        d.update_available = None;
        d.update_progress = Some(&UpdateProgressState::Downloading);
        let (downloading, downloading_clickable) = build_center(&d, &colors).unwrap();
        assert_eq!(downloading.text, "Downloading...");
        assert!(!downloading_clickable);

        let restart = UpdateProgressState::CompletedRestartRequired { version: "2.0.0".to_owned() };
        d.update_progress = Some(&restart);
        let (restart_span, restart_clickable) = build_center(&d, &colors).unwrap();
        assert_eq!(restart_span.text, "Updated! Restart required");
        assert!(restart_clickable);
    }

    // @lat: [[test#GPUI Status Bar#Sparklines pad short history to fixed width]]
    #[test]
    fn sparklines_pad_short_history_to_fixed_width() {
        let colors = colors();
        let stats = SystemStats {
            cpu_percent: 50.0,
            mem_used_gb: 8.0,
            mem_total_gb: 16.0,
            gpu_percent: Some(25.0),
            net_up_bytes_sec: 1_000,
            net_down_bytes_sec: 2_000,
            cpu_history: VecDeque::from(vec![10.0, 20.0]),
            gpu_history: VecDeque::from(vec![25.0]),
            net_up_history: VecDeque::from(vec![1_000]),
            net_down_history: VecDeque::from(vec![2_000]),
        };
        let config = StatusBarStatsConfig {
            usage: StatusBarUsageStatsConfig {
                compute: StatusBarComputeStatsConfig { cpu: true, gpu: true },
                memory: true,
            },
            network: true,
        };
        let mut spans = Vec::new();
        push_cpu(&mut spans, &stats, &colors);
        // CPU shows 8 bars: 6 padding + 2 history, plus the label and percentage.
        let bars = spans.iter().filter(|s| "▁▂▃▄▅▆▇█".contains(&s.text)).count();
        assert_eq!(bars, CPU_SPARK_WIDTH);

        let full = build_right(
            &StatusBarData { sys_stats: Some(&stats), stats_config: Some(&config), ..data() },
            &colors,
        );
        let text = joined(&full);
        assert!(text.contains("CPU"));
        assert!(text.contains("MEM"));
        assert!(text.contains("GPU"));
        assert!(text.contains('\u{2191}'));
    }
}
