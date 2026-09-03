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
//! that model onto GPUI elements. The sparklines are graph spans painted as
//! quads by [`render`], not block glyphs in the text run: at the band's text
//! size the `▁▂▃` run was a few pixels of smear at the baseline, shaped by
//! whichever fallback face supplied the glyphs. Every size in the band is a
//! fraction of `appearance.status_bar_height` ([`StatusBarMetrics`]), so the
//! type, graphs and gaps grow with the band instead of leaving it empty.
//! Colours stay in sRGB space here (GPUI does its own linear conversion),
//! unlike the legacy renderer which pre-multiplied into linear for the raw
//! GPU pipeline.

use std::path::Path;

use gpui::{App, FocusHandle, Font, FontWeight, Rgba, Role, TextRun, Window, div, prelude::*, px};
use scribe_common::config::StatusBarStatsConfig;
use scribe_common::protocol::{ControllerInfo, EnvStatusState, UpdateProgressState};
use scribe_common::theme::ChromeColors;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::sys_stats::SystemStats;
use crate::{button::stop_activation_key, opacity::scale_slot};

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
/// Fallback blue when ANSI index 4 is unavailable.
const FALLBACK_BLUE: [f32; 4] = [0.45, 0.6, 1.0, 1.0];
/// Fallback magenta when ANSI index 5 is unavailable.
const FALLBACK_MAGENTA: [f32; 4] = [0.75, 0.55, 1.0, 1.0];
/// Fallback cyan when ANSI index 6 is unavailable.
const FALLBACK_CYAN: [f32; 4] = [0.45, 0.8, 1.0, 1.0];

/// Number of sparkline bars for CPU and GPU displays.
const CPU_SPARK_WIDTH: usize = 8;
/// Number of sparkline bars for network displays.
const NET_SPARK_WIDTH: usize = 4;
/// Network sparklines saturate at 100 MB/s.
const NET_SPARK_MAX_BYTES_PER_SEC: u64 = 100_000_000;

/// The band's pixel geometry, every value a fraction of the configured
/// `appearance.status_bar_height` so the bar fills whatever height it is
/// given. The reference is the 36px default: 14px text, 12px semibold stat
/// labels, 16px readouts, 8 bars of 5px with 2px gaps in a 22px box, 36px
/// between chips, 20px between zones, 14px band edge, 28px-wide controls.
///
/// The chip renders at those reference sizes for every band tall enough to
/// hold it, grows past the 36px reference so a taller band is filled rather
/// than padded, and shrinks only below [`MIN_FIT_HEIGHT`], where the graph
/// box no longer fits. Scaling the type down with the band instead made the
/// design appear at exactly one height and left every other band with a
/// miniature of it. The E2E scripts that click the controls derive their
/// offsets from these numbers (`tests/e2e/visual/settings-entry.sh`,
/// `window-chrome-bands.sh`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatusBarMetrics {
    pub height: f32,
    /// Base text: cwd, branch, sessions, host.
    pub text: f32,
    /// Stat chip labels (`CPU`, `MEM`, `↑`), semibold.
    pub label: f32,
    /// Stat readouts, the clock, and the control glyphs.
    pub readout: f32,
    /// Sparkline bar width, the gap between bars, and a full bar's height.
    pub bar_width: f32,
    pub bar_gap: f32,
    pub graph_height: f32,
    /// MEM gauge width and thickness.
    pub gauge_width: f32,
    pub gauge_height: f32,
    /// Space between two chips inside the stats zone.
    pub chip_gap: f32,
    /// Space between the pieces of one chip (label, graph, readout).
    pub piece_gap: f32,
    /// Space between zones, and the band's horizontal edge padding.
    pub zone_gap: f32,
    pub edge: f32,
    /// Width of each trailing control button (balance, settings): a fixed
    /// hit target, never the glyph's advance, floored at 12px.
    pub control: f32,
}

/// Horizontal padding on each side of the trailing controls cluster.
pub const CONTROLS_PADDING: f32 = 4.0;

/// The band the reference geometry is drawn for.
const REFERENCE_HEIGHT: f32 = 36.0;

/// The shortest band that still holds the reference chip: the 22px graph box
/// plus its baseline rule, under the band's 1px top border. Below this the
/// whole geometry scales down rather than clipping.
const MIN_FIT_HEIGHT: f32 = 24.0;

impl StatusBarMetrics {
    /// Scale the reference geometry to `height` pixels.
    #[must_use]
    pub fn for_height(height: f32) -> Self {
        let height = height.max(1.0);
        let scale = if height >= REFERENCE_HEIGHT {
            height / REFERENCE_HEIGHT
        } else if height >= MIN_FIT_HEIGHT {
            1.0
        } else {
            height / MIN_FIT_HEIGHT
        };
        let at = |reference: f32| (reference * scale).round().max(1.0);
        Self {
            height,
            text: at(14.0),
            label: at(12.0),
            readout: at(16.0),
            bar_width: at(5.0),
            bar_gap: at(2.0),
            graph_height: at(22.0),
            gauge_width: at(48.0),
            gauge_height: at(5.0),
            chip_gap: at(36.0),
            piece_gap: at(10.0),
            zone_gap: at(20.0),
            edge: at(14.0),
            control: at(28.0).max(12.0),
        }
    }

    /// Painted width of an `n`-bar graph.
    #[must_use]
    pub fn graph_width(&self, bars: usize) -> f32 {
        if bars == 0 {
            return 0.0;
        }
        let n = f32::from(u8::try_from(bars).unwrap_or(u8::MAX));
        (n - 1.0).mul_add(self.bar_gap, n * self.bar_width)
    }
}

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
    /// Bare-hovered OSC 8 target, replacing the complete left group while set.
    pub hover_uri: Option<&'a str>,
    /// Display columns available to the replacement left group after measured
    /// right/centre/action widths and horizontal paddings are reserved.
    pub left_budget_cols: usize,
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
    /// Moderate usage (70–90%) — ANSI yellow, the shared warn band.
    pub warning: [f32; 4],
    /// High usage (≥90%) — ANSI red, the shared danger band.
    pub critical: [f32; 4],
    /// Dimmed colour for stat labels.
    pub label: [f32; 4],
    /// 1px hairline at the top edge.
    pub top_border: [f32; 4],
    /// CPU identity hue (ANSI blue) — below-warn bars and readout.
    pub stat_cpu: [f32; 4],
    /// Memory identity hue (ANSI magenta).
    pub stat_mem: [f32; 4],
    /// Network identity hue (ANSI cyan).
    pub stat_net: [f32; 4],
    /// GPU identity hue (ANSI green).
    pub stat_gpu: [f32; 4],
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
            stat_cpu: ansi_colors.get(4).copied().unwrap_or(FALLBACK_BLUE),
            stat_mem: ansi_colors.get(5).copied().unwrap_or(FALLBACK_MAGENTA),
            stat_net: ansi_colors.get(6).copied().unwrap_or(FALLBACK_CYAN),
            stat_gpu: ansi_colors.get(2).copied().unwrap_or(FALLBACK_GREEN),
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

/// One painted sparkline bar: its fill level in `0.0..=1.0` and colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    pub level: f32,
    pub color: [f32; 4],
}

/// How [`render`] paints a [`Span`].
#[derive(Debug, Clone, PartialEq, Default)]
pub enum SpanKind {
    /// Base text at [`StatusBarMetrics::text`].
    #[default]
    Text,
    /// A semibold stat label at [`StatusBarMetrics::label`].
    Label,
    /// A medium-weight stat readout at [`StatusBarMetrics::readout`],
    /// right-aligned in a fixed number of cells.
    Readout,
    /// A bar graph; `Span::color` is the graph's identity hue (its baseline).
    Graph(Vec<Bar>),
    /// A horizontal gauge filled to `level`; `Span::color` is the hue.
    Gauge { level: f32, color: [f32; 4] },
    /// Invisible boundary between two chips in the stats zone.
    ChipBreak,
    /// Invisible boundary between two zones: [`render`] starts a new zone
    /// container here instead of painting the span.
    ZoneBreak,
}

/// One styled run of text in the status bar, or one painted graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub color: [f32; 4],
    /// Full copy revealed on hover when the visible text is a compact glyph,
    /// so long error strings do not crowd the band inline.
    pub tooltip: Option<String>,
    pub kind: SpanKind,
}

impl Span {
    fn new(text: impl Into<String>, color: [f32; 4]) -> Self {
        Self { text: text.into(), color, tooltip: None, kind: SpanKind::Text }
    }

    fn kind(kind: SpanKind, text: impl Into<String>, color: [f32; 4]) -> Self {
        Self { kind, ..Self::new(text, color) }
    }

    /// Whether this span is a zone boundary.
    #[must_use]
    pub fn is_zone_break(&self) -> bool {
        self.kind == SpanKind::ZoneBreak
    }

    fn with_tooltip(mut self, tooltip: impl Into<String>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
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
    /// Concise state for the one exposed status node. Decorative spans remain
    /// anonymous so assistive technology does not announce every glyph.
    pub accessibility_label: String,
}

/// Build the pure status-bar model from its inputs. Every segment enumerated in
/// the parity checklist is produced here; [`render`] only maps it to elements.
pub fn build_model(data: &StatusBarData<'_>, colors: &StatusBarColors) -> StatusBarModel {
    let left = build_left(data, colors);
    let right = build_right(data, colors);
    let (center, center_clickable) = build_center(data, colors)
        .map_or((None, false), |(span, clickable)| (Some(span), clickable));
    StatusBarModel {
        left,
        center,
        center_clickable,
        right,
        accessibility_label: accessibility_label(data),
    }
}

/// Summarize state changes without turning decorative status-bar runs into
/// separately announced accessibility nodes.
fn accessibility_label(data: &StatusBarData<'_>) -> String {
    let mut states = vec![if data.connected { "Connected" } else { "Disconnected" }.to_owned()];
    match data.last_command_status {
        Some(CommandStatus::Success) => states.push("Last command succeeded".to_owned()),
        Some(CommandStatus::Failure) => states.push("Last command failed".to_owned()),
        Some(CommandStatus::Unknown) => states.push("Last command status unknown".to_owned()),
        None => {}
    }
    if matches!(data.env_status, Some(EnvStatusState::Degraded { .. })) {
        states.push("Environment capture degraded".to_owned());
    }
    if let Some(uri) = data.hover_uri {
        states.push(format!("Link target {uri}"));
    }
    match data.update_progress {
        Some(UpdateProgressState::Downloading) => states.push("Downloading update".to_owned()),
        Some(UpdateProgressState::Verifying) => states.push("Verifying update".to_owned()),
        Some(UpdateProgressState::Installing) => states.push("Installing update".to_owned()),
        Some(UpdateProgressState::Completed { .. }) => states.push("Update complete".to_owned()),
        Some(UpdateProgressState::CompletedRestartRequired { .. }) => {
            states.push("Update complete; restart required".to_owned());
        }
        Some(UpdateProgressState::Failed { .. }) => states.push("Update failed".to_owned()),
        None => {
            if let Some(version) = data.update_available {
                states.push(format!("Update {version} available"));
            }
        }
    }
    format!("Terminal status: {}", states.join(". "))
}

// ---------------------------------------------------------------------------
// Left side
// ---------------------------------------------------------------------------

/// Left side: connection dot, important status copy, command status, env
/// warning, remote/share surfaces, workspace name, CWD.
fn build_left(data: &StatusBarData<'_>, colors: &StatusBarColors) -> Vec<Span> {
    if let Some(uri) = data.hover_uri {
        return build_hover_left(uri, data.left_budget_cols, colors);
    }

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

fn build_hover_left(uri: &str, budget_cols: usize, colors: &StatusBarColors) -> Vec<Span> {
    match budget_cols {
        0 => Vec::new(),
        1 => vec![Span::new("\u{2192}", colors.label)],
        _ => vec![
            Span::new("\u{2192} ", colors.label),
            Span::new(truncate_url(uri, budget_cols - 2), colors.text),
        ],
    }
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
    spans.push(
        Span::new("\u{26A0}", colors.warning).with_tooltip(
            "Environment capture degraded — retry from Settings → Terminal → General",
        ),
    );
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

    // Repository/session metadata leads and the stats cluster follows: the
    // metadata cluster's width is stable, so putting it left of the stats
    // keeps it still while samples change, and the stats zone itself is
    // width-pinned by [`stats_zone_width`].
    if let Some(branch) = data.git_branch {
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

    if let (Some(stats), Some(config)) = (data.sys_stats, data.stats_config) {
        push_zone_break(&mut spans);
        push_stats(&mut spans, stats, config, colors);
    }

    if !data.time.is_empty() {
        push_zone_break(&mut spans);
        spans.push(Span::new(data.time, colors.text));
    }

    spans
}

/// Push a quiet " · " separator between segments inside one zone.
fn push_sep(spans: &mut Vec<Span>, colors: &StatusBarColors) {
    if !spans.is_empty() {
        spans.push(Span::new(" \u{00B7} ", colors.separator));
    }
}

/// Mark a cluster boundary: [`render`] closes the current zone fill and opens
/// the next one, replacing the legacy " │ " hairline between clusters.
fn push_zone_break(spans: &mut Vec<Span>) {
    if !spans.is_empty() {
        spans.push(Span::kind(SpanKind::ZoneBreak, "", [0.0; 4]));
    }
}

/// CPU / MEM / NET / GPU stat chips, each gated by config. The chips share
/// one zone, spaced by [`StatusBarMetrics::chip_gap`] rather than separator
/// glyphs — each stat's identity hue is what tells them apart. A chip is
/// `label graph readout`: a semibold hue-tinted label, a painted graph (or
/// the MEM gauge), and a readout right-aligned in fixed cells and banded by
/// load. Fixed bar counts and fixed readout cells in the terminal's
/// monospace font keep the zone's width stable while samples change.
fn push_stats(
    spans: &mut Vec<Span>,
    stats: &SystemStats,
    config: &StatusBarStatsConfig,
    colors: &StatusBarColors,
) {
    let zone_start = spans.len();
    let chip_break = |cluster: &mut Vec<Span>| {
        if cluster.len() > zone_start {
            cluster.push(Span::kind(SpanKind::ChipBreak, "", [0.0; 4]));
        }
    };
    if config.usage.compute.cpu {
        chip_break(spans);
        push_cpu(spans, stats, colors);
    }
    if config.usage.memory {
        chip_break(spans);
        push_mem(spans, stats, colors);
    }
    if config.network {
        chip_break(spans);
        push_net(spans, stats, colors);
    }
    if config.usage.compute.gpu && stats.gpu_percent.is_some() {
        chip_break(spans);
        push_gpu(spans, stats, colors);
    }
}

/// A stat label: semibold, the hue pulled toward the dim label colour.
fn label(text: &str, hue: [f32; 4], colors: &StatusBarColors) -> Span {
    Span::kind(SpanKind::Label, text, mix(hue, colors.label, 0.45))
}

/// A readout right-aligned in four cells.
fn readout(text: String, color: [f32; 4]) -> Span {
    Span::kind(SpanKind::Readout, text, color)
}

/// Left-pad a short history with idle stubs in the dim label colour, so the
/// graph keeps its fixed width from the first sample and a missing sample
/// never reads as a real zero.
fn padded_graph(
    width: usize,
    history: impl ExactSizeIterator<Item = Bar>,
    hue: [f32; 4],
    colors: &StatusBarColors,
) -> Span {
    let pad = width.saturating_sub(history.len());
    let bars =
        std::iter::repeat_n(Bar { level: 0.0, color: colors.label }, pad).chain(history).collect();
    Span::kind(SpanKind::Graph(bars), "", hue)
}

/// A usage bar banded by load: the stat's hue at 72% alpha, so a calm graph
/// sits behind its readout, below the warn threshold; the solid shared
/// warn/danger colours above it.
fn usage_bar(pct: f32, hue: [f32; 4], colors: &StatusBarColors) -> Bar {
    let color = if pct >= 70.0 { stat_color(pct, hue, colors) } else { with_alpha(hue, 0.72) };
    Bar { level: usage_level(pct), color }
}

/// CPU: label + 8 usage bars (left-padded) + percentage.
fn push_cpu(spans: &mut Vec<Span>, stats: &SystemStats, colors: &StatusBarColors) {
    let hue = colors.stat_cpu;
    spans.push(label("CPU", hue, colors));
    let history = stats.cpu_history.iter().map(|&v| usage_bar(v, hue, colors));
    spans.push(padded_graph(CPU_SPARK_WIDTH, history, hue, colors));
    let pct = stats.cpu_percent;
    spans.push(readout(format!("{pct:>3.0}%"), stat_color(pct, hue, colors)));
}

/// Memory: label + gauge + percentage. A one-sample bar graph was noise;
/// a horizontal gauge reads as the fraction it is.
fn push_mem(spans: &mut Vec<Span>, stats: &SystemStats, colors: &StatusBarColors) {
    let hue = colors.stat_mem;
    let mem_pct =
        if stats.mem_total_gb > 0.0 { stats.mem_used_gb / stats.mem_total_gb * 100.0 } else { 0.0 };
    spans.push(label("MEM", hue, colors));
    let fill = usage_bar(mem_pct, hue, colors);
    spans.push(Span::kind(SpanKind::Gauge { level: fill.level, color: fill.color }, "", hue));
    spans.push(readout(format!("{mem_pct:>3.0}%"), stat_color(mem_pct, hue, colors)));
}

/// Network: ↑ bars rate ↓ bars rate. Rates are not banded: there is no
/// "too much" bandwidth.
fn push_net(spans: &mut Vec<Span>, stats: &SystemStats, colors: &StatusBarColors) {
    let hue = colors.stat_net;
    let rate_bar = |bytes: &u64| Bar { level: rate_level(*bytes), color: with_alpha(hue, 0.72) };

    spans.push(label("\u{2191}", hue, colors));
    let up = stats.net_up_history.iter().map(rate_bar);
    spans.push(padded_graph(NET_SPARK_WIDTH, up, hue, colors));
    spans.push(readout(format_bytes_rate_fixed(stats.net_up_bytes_sec), colors.text));

    spans.push(label("\u{2193}", hue, colors));
    let down = stats.net_down_history.iter().map(rate_bar);
    spans.push(padded_graph(NET_SPARK_WIDTH, down, hue, colors));
    spans.push(readout(format_bytes_rate_fixed(stats.net_down_bytes_sec), colors.text));
}

/// GPU: label + 8 usage bars (left-padded) + percentage.
fn push_gpu(spans: &mut Vec<Span>, stats: &SystemStats, colors: &StatusBarColors) {
    let Some(gpu_pct) = stats.gpu_percent else { return };
    let hue = colors.stat_gpu;
    spans.push(label("GPU", hue, colors));
    let history = stats.gpu_history.iter().map(|&v| usage_bar(v, hue, colors));
    spans.push(padded_graph(CPU_SPARK_WIDTH, history, hue, colors));
    spans.push(readout(format!("{gpu_pct:>3.0}%"), stat_color(gpu_pct, hue, colors)));
}

// ---------------------------------------------------------------------------
// Pure helpers (ported verbatim from the legacy renderer)
// ---------------------------------------------------------------------------

/// Head+tail-truncate `uri` to at most `max_cols` display columns.
///
/// ASCII retains the legacy head-heavy split. Grapheme boundaries keep wide
/// and combining characters intact, while `unicode-width` budgets their actual
/// terminal columns rather than bytes or scalar count.
#[must_use]
pub fn truncate_url(uri: &str, max_cols: usize) -> String {
    if UnicodeWidthStr::width(uri) <= max_cols {
        return uri.to_owned();
    }
    if max_cols <= 3 {
        return prefix_within(uri, max_cols).0.to_owned();
    }

    let text_budget = max_cols - 3;
    let head_budget = text_budget.div_ceil(2);
    let (head, head_cols) = prefix_within(uri, head_budget);
    let tail_budget = text_budget.saturating_sub(head_cols);
    let tail = suffix_within(uri, head.len(), tail_budget);
    format!("{head}...{tail}")
}

fn prefix_within(text: &str, budget_cols: usize) -> (&str, usize) {
    if budget_cols == 0 {
        return ("", 0);
    }
    let mut end = 0;
    let mut used: usize = 0;
    for (start, grapheme) in text.grapheme_indices(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used.saturating_add(width) > budget_cols {
            break;
        }
        used += width;
        end = start + grapheme.len();
    }
    (&text[..end], used)
}

fn suffix_within(text: &str, min_start: usize, budget_cols: usize) -> &str {
    if budget_cols == 0 {
        return "";
    }
    let mut start = text.len();
    let mut used: usize = 0;
    for (grapheme_start, grapheme) in text.grapheme_indices(true).rev() {
        if grapheme_start < min_start {
            break;
        }
        let width = UnicodeWidthStr::width(grapheme);
        if used.saturating_add(width) > budget_cols {
            break;
        }
        used += width;
        start = grapheme_start;
    }
    &text[start..]
}

/// Map a 0-100 percentage onto a bar level; non-finite input reads as idle.
fn usage_level(pct: f32) -> f32 {
    if pct.is_finite() { (pct / 100.0).clamp(0.0, 1.0) } else { 0.0 }
}

/// Map a byte rate onto a bar level, saturating at [`NET_SPARK_MAX_BYTES_PER_SEC`].
fn rate_level(bytes_per_sec: u64) -> f32 {
    // Per-mille keeps the ratio exact in integers before the one cast.
    let per_mille = bytes_per_sec.min(NET_SPARK_MAX_BYTES_PER_SEC).saturating_mul(1000)
        / NET_SPARK_MAX_BYTES_PER_SEC;
    f32::from(u16::try_from(per_mille).unwrap_or(1000)) / 1000.0
}

fn rounded_div(value: u64, divisor: u64) -> u64 {
    value.saturating_add(divisor / 2) / divisor
}

/// Band a usage percentage: the stat's identity hue below the warn threshold,
/// ANSI yellow from 70%, ANSI red from 90% — the same warn/danger split the AI
/// context bands default to, so "amber means hot" reads identically across
/// the app while a calm graph keeps its own recognisable hue.
fn stat_color(pct: f32, hue: [f32; 4], colors: &StatusBarColors) -> [f32; 4] {
    if pct >= 90.0 {
        colors.critical
    } else if pct >= 70.0 {
        colors.warning
    } else {
        hue
    }
}

/// `color` with its alpha replaced.
fn with_alpha(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0], color[1], color[2], alpha]
}

/// Channel-wise sRGB mix of `a` toward `b` by `t` (0 = a, 1 = b).
fn mix(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
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

/// Measure the display-column budget left after the current right group,
/// centred CTA, action glyphs, and their GPUI paddings reserve their space.
#[must_use]
pub fn measure_left_budget_cols(
    model: &StatusBarModel,
    action_glyphs: &str,
    font_family: &str,
    metrics: &StatusBarMetrics,
    window: &Window,
) -> usize {
    let cell_width = status_text_width("0", font_family, metrics.text, window).max(1.0);
    let right_width = right_group_width(&model.right, font_family, metrics, window);
    let center = model.center.as_ref().map_or("", |span| span.text.as_str());
    let center_width = status_text_width(center, font_family, metrics.text, window)
        + if model.center.is_some() { f32::from(window.rem_size()) } else { 0.0 };
    // Each action is a fixed `control`-wide button, and the trailing
    // controls cluster adds its own zone gap and padding.
    let action_count = action_glyphs.graphemes(true).count();
    let actions_width = f32::from(u8::try_from(action_count).unwrap_or(u8::MAX)) * metrics.control
        + if action_count > 0 { metrics.zone_gap + 2.0 * CONTROLS_PADDING } else { 0.0 };
    let available_px = (f32::from(window.bounds().size.width)
        - 2.0 * metrics.edge
        - right_width
        - center_width
        - actions_width)
        .max(0.0);
    display_cols_in_px(available_px, cell_width)
}

/// The right group's painted width: every zone's spans at their own sizes,
/// plus the chip, zone and pill spacing [`render`] lays them out with.
fn right_group_width(
    spans: &[Span],
    font_family: &str,
    metrics: &StatusBarMetrics,
    window: &Window,
) -> f32 {
    let mut width = 0.0;
    let mut zones = 0.0;
    let mut pieces_in_chip = 0.0;
    for span in spans {
        width += match &span.kind {
            SpanKind::Text => status_text_width(&span.text, font_family, metrics.text, window),
            SpanKind::Label => status_text_width(&span.text, font_family, metrics.label, window),
            SpanKind::Readout => readout_width(span, font_family, metrics, window),
            SpanKind::Graph(bars) => metrics.graph_width(bars.len()),
            SpanKind::Gauge { .. } => metrics.gauge_width,
            SpanKind::ChipBreak => {
                pieces_in_chip = 0.0;
                metrics.chip_gap
            }
            SpanKind::ZoneBreak => {
                zones += 1.0;
                pieces_in_chip = 0.0;
                metrics.zone_gap
            }
        };
        if !matches!(span.kind, SpanKind::Text | SpanKind::ChipBreak | SpanKind::ZoneBreak) {
            // Chip pieces are separated by `piece_gap`; the first piece of a
            // chip has no gap before it.
            if pieces_in_chip > 0.0 {
                width += metrics.piece_gap;
            }
            pieces_in_chip += 1.0;
        }
    }
    // Every zone pill pads 8px each side.
    width + (zones + 1.0) * 16.0
}

/// A readout's reserved width: four cells at readout size, or the shaped
/// text when it is wider (a `>1G` never is).
fn readout_width(
    span: &Span,
    font_family: &str,
    metrics: &StatusBarMetrics,
    window: &Window,
) -> f32 {
    let cells = status_text_width("0000", font_family, metrics.readout, window);
    cells.max(status_text_width(&span.text, font_family, metrics.readout, window))
}

fn display_cols_in_px(extent: f32, cell_width: f32) -> usize {
    if !extent.is_finite() || extent <= 0.0 || !cell_width.is_finite() || cell_width <= 0.0 {
        return 0;
    }
    let mut low = 0u16;
    let mut high = u16::MAX;
    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if f32::from(mid) * cell_width <= extent {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    usize::from(low)
}

/// Shape `text` the way [`render`] paints it: same family, at `font_size`
/// pixels. The family must be a real face name (the terminal font), never
/// the generic `monospace`: cosmic-text has no face by that name, so GPUI
/// would fall through its sans-serif fallback stack and shape the bar
/// proportionally, with hairline spaces.
fn status_text_width(text: &str, font_family: &str, font_size: f32, window: &Window) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let run = TextRun {
        len: text.len(),
        font: Font { family: font_family.to_owned().into(), ..Font::default() },
        ..TextRun::default()
    };
    f32::from(
        window.text_system().shape_line(text.to_owned().into(), px(font_size), &[run], None).width,
    )
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
/// Hover tooltip carrying a span's full copy (the status-message glyph). Uses
/// the band's own palette so it reads as part of the bar.
struct SpanTooltip {
    text: String,
    colors: StatusBarColors,
    font_family: gpui::SharedString,
    text_size: f32,
}

impl gpui::Render for SpanTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .max_w(px(480.0))
            .bg(rgba(self.colors.bg))
            .border_1()
            .border_color(rgba(self.colors.separator))
            .font_family(self.font_family.clone())
            .text_size(px(self.text_size))
            .text_color(rgba(self.colors.text))
            .child(self.text.clone())
    }
}

/// Wrap one cluster in its zone container — spacing and rounding only, no
/// fill: clusters sit flat on the band's base background.
fn zone_pill(inner: gpui::AnyElement) -> gpui::AnyElement {
    div()
        .rounded(px(6.0))
        .px(px(8.0))
        .flex()
        .flex_row()
        .items_center()
        .child(inner)
        .into_any_element()
}

/// Render a span group as zone containers split on [`SpanKind::ZoneBreak`]
/// boundaries, so each segment cluster keeps its own spacing on the flat
/// band background.
fn zoned_row(spans: &[Span], geometry: &StatusBarGeometry<'_>) -> impl IntoElement {
    let mut zones: Vec<Vec<Span>> = vec![Vec::new()];
    for span in spans {
        if span.is_zone_break() {
            if zones.last().is_some_and(|zone| !zone.is_empty()) {
                zones.push(Vec::new());
            }
        } else if let Some(zone) = zones.last_mut() {
            zone.push(span.clone());
        }
    }
    let colors = geometry.colors;
    let metrics = geometry.metrics;
    let font_family: gpui::SharedString = geometry.font_family.to_owned().into();
    div().flex().flex_row().items_center().gap(px(metrics.zone_gap)).children(
        zones.into_iter().filter(|zone| !zone.is_empty()).map(move |zone| {
            zone_pill(span_row(&zone, &colors, metrics, font_family.clone()).into_any_element())
        }),
    )
}

/// Paint a graph span as a row of quads: fixed-width bars rising from a
/// hairline baseline in the stat's hue, so the graph still reads as a graph
/// when every sample is idle.
fn graph_row(hue: [f32; 4], bars: &[Bar], metrics: &StatusBarMetrics) -> gpui::AnyElement {
    let min_height = (metrics.graph_height * 0.1).round().max(1.0);
    div()
        .flex()
        .flex_row()
        .items_end()
        .flex_none()
        .gap(px(metrics.bar_gap))
        .h(px(metrics.graph_height + 1.0))
        .border_b_1()
        .border_color(rgba(with_alpha(hue, hue[3] * 0.25)))
        .children(bars.iter().map(|bar| {
            let height = (bar.level.clamp(0.0, 1.0) * metrics.graph_height).round().max(min_height);
            div().w(px(metrics.bar_width)).h(px(height)).bg(rgba(bar.color))
        }))
        .into_any_element()
}

/// Paint a gauge span: a track in the hue at 18% alpha, filled from the left.
fn gauge_row(
    hue: [f32; 4],
    level: f32,
    color: [f32; 4],
    metrics: &StatusBarMetrics,
) -> gpui::AnyElement {
    let fill = (level.clamp(0.0, 1.0) * metrics.gauge_width).round();
    div()
        .flex_none()
        .w(px(metrics.gauge_width))
        .h(px(metrics.gauge_height))
        .bg(rgba(with_alpha(hue, hue[3] * 0.18)))
        .child(div().w(px(fill)).h_full().bg(rgba(color)))
        .into_any_element()
}

/// Lay one zone's spans out: chip pieces separated by `piece_gap`, chips by
/// `chip_gap`, each kind at its own size and weight.
fn span_row(
    spans: &[Span],
    colors: &StatusBarColors,
    metrics: StatusBarMetrics,
    font_family: gpui::SharedString,
) -> impl IntoElement {
    let colors = *colors;
    let mut chips: Vec<Vec<&Span>> = vec![Vec::new()];
    for span in spans {
        if span.kind == SpanKind::ChipBreak {
            chips.push(Vec::new());
        } else if let Some(chip) = chips.last_mut() {
            chip.push(span);
        }
    }
    div().flex().flex_row().items_center().gap(px(metrics.chip_gap)).children(
        chips.into_iter().filter(|chip| !chip.is_empty()).enumerate().map(
            move |(chip_ix, chip)| {
                let font_family = font_family.clone();
                div().flex().flex_row().items_center().gap(px(metrics.piece_gap)).children(
                    chip.into_iter().enumerate().map(move |(ix, span)| {
                        span_element(span, chip_ix * 64 + ix, colors, metrics, font_family.clone())
                    }),
                )
            },
        ),
    )
}

/// One span as an element: graphs and gauges paint, text kinds shape at
/// their own size and weight, and a tooltip span gets its hover node.
fn span_element(
    span: &Span,
    id: usize,
    colors: StatusBarColors,
    metrics: StatusBarMetrics,
    font_family: gpui::SharedString,
) -> gpui::AnyElement {
    let element = match &span.kind {
        SpanKind::Graph(bars) => return graph_row(span.color, bars, &metrics),
        SpanKind::Gauge { level, color } => return gauge_row(span.color, *level, *color, &metrics),
        SpanKind::Label => div()
            .text_size(px(metrics.label))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgba(span.color))
            .child(span.text.clone()),
        SpanKind::Readout => div()
            .text_size(px(metrics.readout))
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgba(span.color))
            .child(span.text.clone()),
        _ => {
            div().text_size(px(metrics.text)).text_color(rgba(span.color)).child(span.text.clone())
        }
    };
    let Some(tooltip) = span.tooltip.clone() else { return element.into_any_element() };
    element
        .id(("status-span", id))
        .tooltip(move |_window, cx| {
            cx.new(|_| SpanTooltip {
                text: tooltip.clone(),
                colors,
                font_family: font_family.clone(),
                text_size: metrics.text,
            })
            .into()
        })
        .into_any_element()
}

/// Shared update callback used by pointer and AccessKit activation.
pub type UpdateActionHandler = Box<dyn Fn(&mut Window, &mut App)>;

/// Everything [`render`] needs to lay the band out: its colours, the
/// metrics scaled to the configured height, and the family it shapes text
/// in (the terminal font, see [`status_text_width`]).
#[derive(Debug, Clone, Copy)]
pub struct StatusBarGeometry<'a> {
    pub colors: StatusBarColors,
    pub metrics: StatusBarMetrics,
    pub font_family: &'a str,
}

/// Interactive wiring for the band's clickable surfaces: the centred update
/// CTA, and the balance button and settings gear at the far right.
pub struct StatusBarActions<'a> {
    pub update_focus: Option<&'a FocusHandle>,
    pub on_update: Option<UpdateActionHandler>,
    pub on_equalize: Option<UpdateActionHandler>,
    pub on_settings: Option<UpdateActionHandler>,
}

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
    update_focus: Option<&FocusHandle>,
    on_update: Option<UpdateActionHandler>,
) -> gpui::AnyElement {
    let base = div()
        .flex()
        .items_center()
        .px_2()
        .py(px(2.0))
        .rounded(px(6.0))
        .text_color(rgba(span.color))
        .child(span.text.clone());
    match (on_update.filter(|_| clickable), update_focus) {
        (Some(action), Some(focus)) => {
            // An actionable CTA is an accent-tinted chip — a solid fill with a
            // deeper hover layer — while progress labels stay inert plain text.
            let chip_bg = rgba([colors.accent[0], colors.accent[1], colors.accent[2], 0.15]);
            let chip_hover = rgba([colors.accent[0], colors.accent[1], colors.accent[2], 0.26]);
            let chip_text = rgba(mix(colors.accent, span.color, 0.45));
            let hover_text = rgba(span.color);
            let focus_bg = rgba(colors.accent);
            let focus_text = rgba(colors.bg);
            base.id("status-bar-update-cta")
                .track_focus(focus)
                .role(Role::Button)
                .aria_label(span.text.clone())
                .aria_description("Press Enter or Space to open the update confirmation")
                .cursor_pointer()
                .bg(chip_bg)
                .text_color(chip_text)
                .hover(move |style| style.bg(chip_hover).text_color(hover_text))
                .focus_visible(move |style| style.bg(focus_bg).text_color(focus_text))
                // GPUI maps Enter/Space and AccessKit Click onto `on_click`.
                .on_key_down(stop_activation_key)
                .on_click(move |_, window, cx| action(window, cx))
                .into_any_element()
        }
        _ => base.into_any_element(),
    }
}

/// Render the status bar model onto a full-width GPUI flex row.
///
/// The bar is a `height_px`-tall border-box band anchored at the window
/// bottom with a 1px top hairline, shaped in `font_family`: the terminal's
/// own monospace font, so the fixed-width readouts and the column budget
/// actually line up (see [`status_text_width`]). Its clipping and full-height
/// controls keep every action hit target inside the configured band. Left and
/// right groups take natural width; the centred CTA lives in flex-grown space
/// so it stays centred as the window resizes.
pub fn render(
    model: &StatusBarModel,
    geometry: &StatusBarGeometry<'_>,
    actions: StatusBarActions<'_>,
) -> impl IntoElement {
    let colors = geometry.colors;
    let metrics = geometry.metrics;
    let StatusBarActions { update_focus, on_update, on_equalize, on_settings } = actions;
    let center = model
        .center
        .as_ref()
        .map(|span| center_cta(span, model.center_clickable, &colors, update_focus, on_update));
    div()
        .id("terminal-status-bar")
        .role(Role::Status)
        .aria_label(model.accessibility_label.clone())
        .w_full()
        // A fixed-height band, never a flexible one: the shell stacks it under
        // a flex-grown terminal grid, and a shrinkable band is what lets a
        // short window squeeze the bar off screen instead of clipping the grid.
        .flex_none()
        .h(px(metrics.height))
        .overflow_hidden()
        .flex()
        .flex_row()
        .items_center()
        .px(px(metrics.edge))
        .bg(rgba(colors.bg))
        .border_t_1()
        .border_color(rgba(colors.top_border))
        .font_family(geometry.font_family.to_owned())
        .text_size(px(metrics.text))
        .text_color(rgba(colors.text))
        // Every span is its own text node; a squeezed band must clip them at
        // the edge, never fold a readout onto a second line.
        .whitespace_nowrap()
        .child(zoned_row(&model.left, geometry))
        .child(
            div()
                .h_full()
                .flex_1()
                .flex()
                .flex_row()
                .justify_center()
                .items_center()
                .children(center),
        )
        .child(zoned_row(&model.right, geometry))
        .when(on_equalize.is_some() || on_settings.is_some(), |bar| {
            // Window controls share one trailing cluster at the band's far
            // right, flat like the segment clusters; buttons stay full-height
            // inside it so their hit targets keep the whole band. Glyphs sit
            // at readout size so they weigh the same as the numbers.
            bar.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .ml(px(metrics.zone_gap))
                    .px(px(CONTROLS_PADDING))
                    .rounded(px(6.0))
                    .text_size(px(metrics.readout))
                    .children(
                        on_equalize.map(|action| equalize_button(&colors, metrics.control, action)),
                    )
                    .children(on_settings.map(|action| settings_gear(&colors, metrics.control, action))),
            )
        })
}

/// The balance button at the band's bottom-right corner, beside the gear —
/// resets every workspace and pane split to equal space when clicked. Only
/// rendered when the window holds more than one pane.
fn equalize_button(
    colors: &StatusBarColors,
    width: f32,
    action: UpdateActionHandler,
) -> gpui::AnyElement {
    let accent = rgba(colors.accent);
    div()
        .id("status-bar-equalize")
        .role(Role::Button)
        .aria_label("Balance panes")
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .w(px(width))
        .flex_none()
        .cursor_pointer()
        .text_color(rgba(colors.label))
        .hover(move |style| style.text_color(accent))
        .on_click(move |_, window, cx| action(window, cx))
        .child("\u{229E}")
        .into_any_element()
}

/// The settings entry point at the band's far right — the gear moved here
/// from the titlebar, which now holds only tabs and the equalize icon.
fn settings_gear(
    colors: &StatusBarColors,
    width: f32,
    action: UpdateActionHandler,
) -> gpui::AnyElement {
    let accent = rgba(colors.accent);
    div()
        .id("status-bar-settings")
        .role(Role::Button)
        .aria_label("Open settings")
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .w(px(width))
        .flex_none()
        .cursor_pointer()
        .text_color(rgba(colors.label))
        .hover(move |style| style.text_color(accent))
        .on_click(move |_, window, cx| action(window, cx))
        .child("\u{2699}")
        .into_any_element()
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
            stat_cpu: [0.2, 0.4, 1.0, 1.0],
            stat_mem: [0.7, 0.4, 1.0, 1.0],
            stat_net: [0.3, 0.8, 1.0, 1.0],
            stat_gpu: [0.2, 0.9, 0.4, 1.0],
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
            hover_uri: None,
            left_budget_cols: 0,
        }
    }

    fn joined(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn truncate_url_preserves_head_and_tail_within_display_budget() {
        assert_eq!(truncate_url("https://x.dev", 40), "https://x.dev");
        let uri = "https://example.com/very/long/path/segment/that/overflows";
        let out = truncate_url(uri, 20);

        assert_eq!(UnicodeWidthStr::width(out.as_str()), 20);
        assert!(out.starts_with("https://e"));
        assert!(out.ends_with("flows"));
    }

    #[test]
    fn truncate_url_handles_narrow_and_wide_character_budgets() {
        assert_eq!(truncate_url("https://example.com", 0), "");
        assert_eq!(truncate_url("https://example.com", 3), "htt");

        let uri = "https://例え.example/セグメント/終わり";
        let out = truncate_url(uri, 18);
        assert!(UnicodeWidthStr::width(out.as_str()) <= 18);
        assert!(out.contains("..."));
        assert!(out.ends_with("終わり"));
    }

    // @lat: [[test#GPUI Status Bar#OSC 8 hover replaces and restores the live left group]]
    #[test]
    fn osc8_hover_replaces_and_restores_the_live_left_group() {
        let colors = colors();
        let mut d = data();
        d.workspace_name = Some("before");
        d.hover_uri = Some("https://例え.example/very/long/target/終わり");
        d.left_budget_cols = 20;

        let hovered = build_model(&d, &colors);
        assert!(joined(&hovered.left).starts_with("\u{2192} https://"));
        assert!(UnicodeWidthStr::width(joined(&hovered.left).as_str()) <= 20);
        assert!(!joined(&hovered.left).contains("before"));
        assert!(
            hovered.accessibility_label.contains("https://例え.example/very/long/target/終わり")
        );
        crate::assert_rgba_eq(hovered.left[0].color, colors.label);
        crate::assert_rgba_eq(hovered.left[1].color, colors.text);

        d.hover_uri = None;
        d.workspace_name = Some("live");
        let restored = build_model(&d, &colors);
        assert!(joined(&restored.left).contains("live"));
        assert!(!restored.accessibility_label.contains("Link target"));
    }

    #[test]
    fn osc8_hover_never_exceeds_a_narrow_left_budget() {
        let colors = colors();
        let mut d = data();
        d.hover_uri = Some("https://example.com");

        for budget in 0..=3 {
            d.left_budget_cols = budget;
            let left = joined(&build_left(&d, &colors));
            assert!(UnicodeWidthStr::width(left.as_str()) <= budget, "budget {budget}: {left}");
        }
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

    // @lat: [[test#GPUI Status Bar#Pane feedback stays out of the window status bar]]
    #[test]
    fn pane_feedback_stays_out_of_the_window_status_bar() {
        let colors = colors();
        let d = data();

        // Transient connection / pane errors are log-only: no warning glyph
        // and no error copy anywhere in the band. The only ⚠ the bar may show
        // is the env-capture one, absent here because env status is `None`.
        let left = build_left(&d, &colors);
        assert!(left.iter().all(|s| s.text != "\u{26A0}"));
        assert_eq!(accessibility_label(&d), "Terminal status: Connected");
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

    // @lat: [[test#GPUI Status Bar#Sparkline maps percentage to bar level]]
    #[test]
    fn sparkline_maps_percentage_to_bar_level() {
        assert!(usage_level(0.0).abs() < 1e-6);
        assert!((usage_level(100.0) - 1.0).abs() < 1e-6);
        assert!((usage_level(50.0) - 0.5).abs() < 1e-6);
        assert!((usage_level(250.0) - 1.0).abs() < 1e-6);
        // Non-finite input reads as idle.
        assert!(usage_level(f32::NAN).abs() < 1e-6);
        // Network saturates at 100 MB/s.
        assert!(rate_level(0).abs() < 1e-6);
        assert!((rate_level(NET_SPARK_MAX_BYTES_PER_SEC / 2) - 0.5).abs() < 1e-6);
        assert!((rate_level(NET_SPARK_MAX_BYTES_PER_SEC * 10) - 1.0).abs() < 1e-6);
    }

    // @lat: [[test#GPUI Status Bar#Metrics scale with the band height]]
    #[test]
    fn metrics_scale_with_the_band_height() {
        let reference = StatusBarMetrics::for_height(36.0);
        assert!((reference.text - 14.0).abs() < 1e-6);
        assert!((reference.readout - 16.0).abs() < 1e-6);
        assert!((reference.graph_height - 22.0).abs() < 1e-6);
        // 8 bars of 5px with 2px gaps.
        assert!((reference.graph_width(8) - 54.0).abs() < 1e-6);
        assert!(reference.graph_width(0).abs() < 1e-6);
        // Every band that can hold the chip renders it at the reference
        // sizes: the design is not a 36px-only layout with miniatures
        // everywhere else.
        for band in [24.0_f32, 28.0, 30.0, 35.0] {
            let fits = StatusBarMetrics::for_height(band);
            assert!((fits.text - 14.0).abs() < 1e-6, "{band}px text {}", fits.text);
            assert!((fits.readout - 16.0).abs() < 1e-6, "{band}px readout");
            assert!((fits.graph_height - 22.0).abs() < 1e-6, "{band}px graph");
            // The graph box and its baseline rule fit under the top border.
            assert!(fits.graph_height + 1.0 <= band - 1.0, "{band}px graph overflows");
        }
        // A taller band grows the chip instead of padding around it.
        let tall = StatusBarMetrics::for_height(48.0);
        assert!(tall.text > 14.0 && tall.graph_height > 22.0);
        // Nothing collapses to zero at the 8px floor the settings allow, and
        // the trailing controls keep the hit rects the E2E scripts click:
        // `window-chrome-bands.sh` clicks `W-30` (balance) and `W-14` (gear)
        // on the 8px band; `settings-entry.sh` clicks `W-32` on the default.
        let floor = StatusBarMetrics::for_height(8.0);
        assert!(floor.bar_gap >= 1.0 && floor.gauge_height >= 1.0 && floor.text >= 1.0);
        let gear = |m: &StatusBarMetrics| {
            let right = m.edge + CONTROLS_PADDING;
            (right + m.control, right)
        };
        let (floor_left, floor_right) = gear(&floor);
        assert!(floor_left > 14.0 && 14.0 > floor_right, "8px gear {floor_left}..{floor_right}");
        assert!(floor_left + floor.control > 30.0 && 30.0 > floor_left, "8px balance");
        let (ref_left, ref_right) = gear(&reference);
        assert!(ref_left > 32.0 && 32.0 > ref_right, "36px gear spans {ref_left}..{ref_right}");
    }

    // @lat: [[test#GPUI Status Bar#Usage color escalates with load]]
    #[test]
    fn usage_color_escalates_with_load() {
        let colors = colors();
        let hue = [0.1, 0.2, 0.9, 1.0];
        // Below warn the stat keeps its identity hue; the shared 70/90
        // warn/danger split takes over above it.
        crate::assert_rgba_eq(stat_color(10.0, hue, &colors), hue);
        crate::assert_rgba_eq(stat_color(69.9, hue, &colors), hue);
        crate::assert_rgba_eq(stat_color(70.0, hue, &colors), colors.warning);
        crate::assert_rgba_eq(stat_color(95.0, hue, &colors), colors.critical);
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
        // CPU is one graph of 8 bars: 6 idle pads in the label colour, then
        // the 2 real samples in the CPU hue, between the label and readout.
        let SpanKind::Graph(bars) = &spans[1].kind else { panic!("cpu graph span") };
        assert_eq!(bars.len(), CPU_SPARK_WIDTH);
        for pad in &bars[..6] {
            assert!(pad.level.abs() < 1e-6);
            crate::assert_rgba_eq(pad.color, colors.label);
        }
        assert!((bars[6].level - 0.1).abs() < 1e-6);
        assert!((bars[7].level - 0.2).abs() < 1e-6);
        for sample in &bars[6..] {
            crate::assert_rgba_eq(sample.color, with_alpha(colors.stat_cpu, 0.72));
        }
        assert_eq!(spans[0].kind, SpanKind::Label);
        assert_eq!(spans[0].text, "CPU");
        assert_eq!(spans[2].kind, SpanKind::Readout);
        assert_eq!(spans[2].text, " 50%");

        let full = build_right(
            &StatusBarData { sys_stats: Some(&stats), stats_config: Some(&config), ..data() },
            &colors,
        );
        let text = joined(&full);
        assert!(text.contains("CPU"));
        assert!(text.contains("MEM"));
        assert!(text.contains("GPU"));
        assert!(text.contains('\u{2191}'));
        // MEM is a gauge at its fraction, and chips are separated by breaks.
        assert!(full.iter().any(
            |s| matches!(s.kind, SpanKind::Gauge { level, .. } if (level - 0.5).abs() < 1e-6)
        ));
        assert_eq!(full.iter().filter(|s| s.kind == SpanKind::ChipBreak).count(), 3);
    }
}
