//! Collapsed GitHub Actions trace band: pure state/model plus GPUI lowering.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use gpui::{
    Animation, AnimationExt as _, App, ElementId, FocusHandle, MouseButton, Rgba, Role, Window,
    div, linear_color_stop, linear_gradient, prelude::*, px, relative,
};
use scribe_common::{
    protocol::{
        CiJob, CiRunConclusion, CiRunDelta, CiRunDetails, CiRunState, CiRunStatus, CiWorkflowRun,
        CiWorkflowStatus, MAX_CI_TRACKED_HEADS,
    },
    theme::Theme,
};

use crate::{
    animation::AnimationSettings, button::stop_activation_key, layout::Rect, opacity::scale_slot,
};

/// Fixed height of the collapsed workspace-region band.
pub const CI_BAR_HEIGHT: f32 = 40.0;
/// Fixed panel chrome plus one 26px row per returned job.
pub const CI_TRACE_BASE_HEIGHT: f32 = 38.0;
pub const CI_TRACE_ROW_HEIGHT: f32 = 26.0;
/// Loading state shown immediately after expansion and before the first reply.
pub const CI_TRACE_LOADING_HEIGHT: f32 = 54.0;

/// Server-owned CI snapshots keyed by their trusted repository root. One root
/// holds one snapshot per concurrently tracked head, newest first.
#[derive(Debug, Default)]
pub struct CiRunBars {
    states: HashMap<PathBuf, Vec<CiRunState>>,
    details: HashMap<(PathBuf, String), CiRunDetails>,
}

impl CiRunBars {
    /// Apply one full replacement or head-qualified clear.
    pub fn apply(&mut self, repo_root: PathBuf, delta: CiRunDelta) {
        match delta {
            CiRunDelta::Set(state) => {
                let heads = self.states.entry(repo_root.clone()).or_default();
                if let Some(tracked) =
                    heads.iter_mut().find(|tracked| tracked.head_sha == state.head_sha)
                {
                    *tracked = state;
                    return;
                }
                // A new head replaces finished work but never a run still
                // going, so concurrent branches stay visible side by side.
                heads.retain(|tracked| !terminal(tracked.rollup));
                heads.insert(0, state);
                heads.truncate(MAX_CI_TRACKED_HEADS);
                self.retain_open_details(&repo_root);
            }
            CiRunDelta::Cleared { head_sha } => {
                let empty = self.states.get_mut(&repo_root).is_some_and(|heads| {
                    heads.retain(|state| state.head_sha != head_sha);
                    heads.is_empty()
                });
                if empty {
                    self.states.remove(&repo_root);
                }
                self.details.remove(&(repo_root, head_sha));
            }
        }
    }

    /// Drop cached detail for heads this root no longer tracks.
    fn retain_open_details(&mut self, repo_root: &Path) {
        let heads = self.states.get(repo_root).map(Vec::as_slice).unwrap_or_default();
        self.details.retain(|(root, head), _| {
            root != repo_root || heads.iter().any(|state| &state.head_sha == head)
        });
    }

    /// Every visible snapshot for `repo_root`, newest head first.
    #[must_use]
    pub fn get(&self, repo_root: &Path) -> &[CiRunState] {
        self.states.get(repo_root).map(Vec::as_slice).unwrap_or_default()
    }

    /// Store detail only when it belongs to one of the root's visible heads.
    pub fn apply_details(&mut self, repo_root: PathBuf, details: CiRunDetails) {
        if self
            .states
            .get(&repo_root)
            .is_some_and(|heads| heads.iter().any(|state| state.head_sha == details.head_sha))
        {
            self.details.insert((repo_root, details.head_sha.clone()), details);
        }
    }

    /// Detail for exactly `head_sha`; old cached heads never enter a new panel.
    #[must_use]
    pub fn details(&self, repo_root: &Path, head_sha: &str) -> Option<&CiRunDetails> {
        self.details.get(&(repo_root.to_path_buf(), head_sha.to_owned()))
    }
}

/// Whether a rollup has stopped moving.
#[must_use]
pub const fn terminal(rollup: CiRunStatus) -> bool {
    matches!(rollup, CiRunStatus::Success | CiRunStatus::Failure | CiRunStatus::Cancelled)
}

/// Whether the collapsed band's host actions are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiActionMode {
    Owner,
    ReadOnly,
}

/// Visual and textual state of one collapsed workflow trace cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceCellKind {
    Queued,
    Active,
    Success,
    Failure,
    Cancelled,
}

impl TraceCellKind {
    const fn glyph(self) -> &'static str {
        match self {
            Self::Queued => "◌",
            Self::Active => "◐",
            Self::Success => "✓",
            Self::Failure => "✕",
            Self::Cancelled => "⊘",
        }
    }
}

/// Pure collapsed trace cell built from one workflow entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceCellModel {
    pub run_id: u64,
    pub name: String,
    pub kind: TraceCellKind,
    pub glyph: &'static str,
}

impl From<&CiWorkflowRun> for TraceCellModel {
    fn from(workflow: &CiWorkflowRun) -> Self {
        let kind = match (workflow.status, workflow.conclusion) {
            (CiWorkflowStatus::Queued, _) => TraceCellKind::Queued,
            (CiWorkflowStatus::InProgress, _) => TraceCellKind::Active,
            (CiWorkflowStatus::Completed, Some(CiRunConclusion::Success)) => TraceCellKind::Success,
            (CiWorkflowStatus::Completed, Some(CiRunConclusion::Failure)) => TraceCellKind::Failure,
            (CiWorkflowStatus::Completed, Some(CiRunConclusion::Cancelled) | None) => {
                TraceCellKind::Cancelled
            }
        };
        Self { run_id: workflow.run_id, name: workflow.name.clone(), kind, glyph: kind.glyph() }
    }
}

/// Semantic tone used by the state cluster and ownership underline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiTone {
    Running,
    Success,
    Failure,
    Cancelled,
    Stale,
}

/// Display-independent collapsed bar. Rendering does no state interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiBarModel {
    pub state_glyph: &'static str,
    pub state_word: &'static str,
    pub tone: CiTone,
    pub cells: Vec<TraceCellModel>,
    pub hidden_cells: usize,
    pub branch: String,
    pub short_sha: String,
    pub elapsed: String,
    pub completed: usize,
    pub total: usize,
    pub motion: bool,
    pub action_mode: CiActionMode,
    pub open_url: Option<String>,
    pub head_sha: String,
    pub accessibility_label: String,
}

impl CiBarModel {
    /// Build a collapsed trace at epoch `now`, without a GPUI window.
    #[must_use]
    pub fn build(state: &CiRunState, now: u64, owner_controls: bool) -> Self {
        let (state_glyph, state_word, tone) = state_label(state);
        let all_cells = state.workflows.iter().map(TraceCellModel::from).collect::<Vec<_>>();
        let cells = if all_cells.len() <= 6 {
            all_cells.clone()
        } else {
            all_cells
                .iter()
                .filter(|cell| matches!(cell.kind, TraceCellKind::Active | TraceCellKind::Failure))
                .take(5)
                .cloned()
                .collect()
        };
        let hidden_cells = all_cells.len().saturating_sub(cells.len());
        let completed = state
            .workflows
            .iter()
            .filter(|workflow| workflow.status == CiWorkflowStatus::Completed)
            .count();
        let total = state.workflows.len();
        let terminal = terminal(state.rollup);
        let action_mode = if owner_controls { CiActionMode::Owner } else { CiActionMode::ReadOnly };
        let open_url =
            owner_controls.then(|| preferred_run(&state.workflows)).flatten().map(|workflow| {
                format!("https://github.com/{}/actions/runs/{}", state.repository, workflow.run_id)
            });
        let elapsed = elapsed_label(state, now, terminal);
        let short_sha = state.head_sha.chars().take(7).collect::<String>();
        let accessibility_label = format!(
            "CI {state_word}: {} at {short_sha}, {completed} of {total} workflows complete",
            state.branch
        );
        Self {
            state_glyph,
            state_word,
            tone,
            cells,
            hidden_cells,
            branch: state.branch.clone(),
            short_sha,
            elapsed,
            completed,
            total,
            motion: state.rollup == CiRunStatus::Running && !state.stale,
            action_mode,
            open_url,
            head_sha: state.head_sha.clone(),
            accessibility_label,
        }
    }
}

/// One time-positioned row in the expanded trace panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiTraceRowModel {
    pub job_id: u64,
    pub name: String,
    pub workflow_name: String,
    pub kind: TraceCellKind,
    pub glyph: &'static str,
    pub state_word: &'static str,
    pub current_step: String,
    pub elapsed: String,
    /// Start and width on the shared axis, in basis points.
    pub left: u16,
    pub width: u16,
    pub accessibility_label: String,
}

/// Display-independent expanded job trace on one shared minute axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiTraceModel {
    pub axis_labels: Vec<String>,
    pub rows: Vec<CiTraceRowModel>,
    pub motion: bool,
    pub accessibility_label: String,
}

impl CiTraceModel {
    /// Build the approved time-positioned trace without a GPUI window.
    #[must_use]
    // @lat: [[client#GPUI CI Run Bar#Expanded job trace]]
    pub fn build(details: &CiRunDetails, now: u64, stale: bool) -> Self {
        let origin =
            details.jobs.iter().filter_map(|job| job.started_at_epoch_secs).min().unwrap_or(now);
        let live = details.jobs.iter().any(|job| job.status != CiWorkflowStatus::Completed);
        let end = if live {
            now
        } else {
            details
                .jobs
                .iter()
                .filter_map(|job| job.completed_at_epoch_secs)
                .max()
                .unwrap_or(origin)
        };
        let axis_minutes = end.saturating_sub(origin).div_ceil(60).max(4);
        let axis_seconds = axis_minutes.saturating_mul(60);
        let rows = details
            .jobs
            .iter()
            .map(|job| trace_row(job, origin, axis_seconds, now))
            .collect::<Vec<_>>();
        Self {
            axis_labels: (0..=axis_minutes).map(|minute| format!("{minute}m")).collect(),
            motion: !stale
                && details.jobs.iter().any(|job| job.status == CiWorkflowStatus::InProgress),
            accessibility_label: format!("CI job trace, {} jobs", rows.len()),
            rows,
        }
    }

    #[must_use]
    pub fn height(&self) -> f32 {
        let rows = u16::try_from(self.rows.len()).unwrap_or(u16::MAX);
        CI_TRACE_BASE_HEIGHT + f32::from(rows) * CI_TRACE_ROW_HEIGHT
    }
}

fn trace_row(job: &CiJob, origin: u64, axis_seconds: u64, now: u64) -> CiTraceRowModel {
    let (kind, state_word) = job_state(job);
    let start = job.started_at_epoch_secs.unwrap_or(now).max(origin);
    let end = job.completed_at_epoch_secs.unwrap_or(now).max(start);
    let left = axis_basis_points(start.saturating_sub(origin), axis_seconds);
    let width = if kind == TraceCellKind::Queued {
        axis_basis_points(60, axis_seconds).min(10_000_u16.saturating_sub(left))
    } else {
        axis_basis_points(end.saturating_sub(start), axis_seconds)
            .max(1)
            .min(10_000_u16.saturating_sub(left))
    };
    let current_step = job
        .steps
        .iter()
        .find(|step| step.status == CiWorkflowStatus::InProgress)
        .or_else(|| job.steps.iter().rev().find(|step| step.status == CiWorkflowStatus::Completed))
        .map_or_else(|| state_word.to_owned(), |step| step.name.clone());
    let elapsed = job
        .started_at_epoch_secs
        .map_or_else(|| "—".to_owned(), |started| duration_label(end.saturating_sub(started)));
    let glyph = kind.glyph();
    let accessibility_label = format!(
        "{} job {}, workflow {}, {state_word}, step {current_step}, elapsed {elapsed}",
        glyph, job.name, job.workflow_name
    );
    CiTraceRowModel {
        job_id: job.job_id,
        name: job.name.clone(),
        workflow_name: job.workflow_name.clone(),
        kind,
        glyph,
        state_word,
        current_step,
        elapsed,
        left,
        width,
        accessibility_label,
    }
}

fn job_state(job: &CiJob) -> (TraceCellKind, &'static str) {
    match (job.status, job.conclusion) {
        (CiWorkflowStatus::Queued, _) => (TraceCellKind::Queued, "queued"),
        (CiWorkflowStatus::InProgress, _) => (TraceCellKind::Active, "running"),
        (CiWorkflowStatus::Completed, Some(CiRunConclusion::Success)) => {
            (TraceCellKind::Success, "passed")
        }
        (CiWorkflowStatus::Completed, Some(CiRunConclusion::Failure)) => {
            (TraceCellKind::Failure, "failed")
        }
        (CiWorkflowStatus::Completed, Some(CiRunConclusion::Cancelled) | None) => {
            (TraceCellKind::Cancelled, "cancelled")
        }
    }
}

fn axis_basis_points(seconds: u64, axis_seconds: u64) -> u16 {
    seconds.saturating_mul(10_000).checked_div(axis_seconds.max(1)).unwrap_or_default().min(10_000)
        as u16
}

fn duration_label(seconds: u64) -> String {
    if seconds < 3_600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

fn state_label(state: &CiRunState) -> (&'static str, &'static str, CiTone) {
    if state.stale {
        return ("!", "stale", CiTone::Stale);
    }
    match state.rollup {
        CiRunStatus::Queued => ("◌", "queued", CiTone::Running),
        CiRunStatus::Running => ("◐", "running", CiTone::Running),
        CiRunStatus::Success => ("✓", "passed", CiTone::Success),
        CiRunStatus::Failure => ("✕", "failed", CiTone::Failure),
        CiRunStatus::Cancelled => ("⊘", "cancelled", CiTone::Cancelled),
    }
}

fn preferred_run(workflows: &[CiWorkflowRun]) -> Option<&CiWorkflowRun> {
    workflows
        .iter()
        .find(|workflow| workflow.conclusion == Some(CiRunConclusion::Failure))
        .or_else(|| {
            workflows.iter().find(|workflow| workflow.status == CiWorkflowStatus::InProgress)
        })
        .or_else(|| workflows.first())
}

fn elapsed_label(state: &CiRunState, now: u64, terminal: bool) -> String {
    let Some(start) =
        state.workflows.iter().filter_map(|workflow| workflow.started_at_epoch_secs).min()
    else {
        return "queued".to_owned();
    };
    let end = if terminal || state.stale {
        state
            .workflows
            .iter()
            .filter_map(|workflow| workflow.updated_at_epoch_secs)
            .max()
            .unwrap_or(start)
    } else {
        now
    };
    let seconds = end.saturating_sub(start);
    if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// Theme-derived colors for the trace direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CiBarColors {
    pub background: [f32; 4],
    pub panel_background: [f32; 4],
    pub text: [f32; 4],
    pub muted: [f32; 4],
    pub divider: [f32; 4],
    pub running: [f32; 4],
    pub success: [f32; 4],
    pub failure: [f32; 4],
    pub stale: [f32; 4],
    pub cancelled: [f32; 4],
}

impl CiBarColors {
    #[must_use]
    pub fn from_theme(theme: &Theme, opacity: f32) -> Self {
        let background = lift(theme.background, 0.01);
        Self {
            background: scale_slot(background, opacity),
            panel_background: scale_slot(theme.background, opacity),
            text: theme.foreground,
            muted: with_alpha(theme.foreground, 0.55),
            divider: with_alpha(theme.foreground, 0.12),
            running: theme.ansi_colors[4],
            success: theme.ansi_colors[2],
            failure: theme.ansi_colors[1],
            stale: theme.ansi_colors[3],
            cancelled: theme.ansi_colors[8],
        }
    }
}

fn lift(color: [f32; 4], amount: f32) -> [f32; 4] {
    [
        color[0] + (1.0 - color[0]) * amount,
        color[1] + (1.0 - color[1]) * amount,
        color[2] + (1.0 - color[2]) * amount,
        color[3],
    ]
}

const fn with_alpha(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0], color[1], color[2], alpha]
}

fn rgba(color: [f32; 4]) -> Rgba {
    Rgba { r: color[0], g: color[1], b: color[2], a: color[3] }
}

/// User action carried out by the host view.
pub type CiActionHandler = Arc<dyn Fn(&mut Window, &mut App)>;

/// Geometry, ownership color, motion policy, and callbacks for one region band.
pub struct CiBarRender {
    pub id: ElementId,
    pub trace_id: ElementId,
    pub open_id: ElementId,
    pub dismiss_id: ElementId,
    pub rect: Rect,
    pub accent: [f32; 4],
    pub animations: AnimationSettings,
    pub expanded: bool,
    pub trace: Option<CiTraceModel>,
    pub toggle_focus: FocusHandle,
    pub open_focus: Option<FocusHandle>,
    pub dismiss_focus: Option<FocusHandle>,
    pub on_toggle: CiActionHandler,
    pub on_open: Option<CiActionHandler>,
    pub on_dismiss: Option<CiActionHandler>,
}

/// Lower a pure collapsed model onto the approved 40px trace direction.
pub fn render(model: &CiBarModel, colors: &CiBarColors, render: CiBarRender) -> gpui::AnyElement {
    let band = collapsed_band(model, colors, &render);
    let panel = render
        .expanded
        .then(|| trace_panel(render.trace_id, render.trace.as_ref(), colors, render.animations));
    div()
        .absolute()
        .left(px(render.rect.x))
        .top(px(render.rect.y))
        .w(px(render.rect.width))
        .h(px(render.rect.height))
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(band)
        .children(panel)
        .into_any_element()
}

fn collapsed_cells(model: &CiBarModel, colors: &CiBarColors) -> gpui::AnyElement {
    let trace_cells = model
        .cells
        .iter()
        .enumerate()
        .map(|(index, cell)| trace_cell(cell, model.motion, index, colors));
    div()
        .flex()
        .flex_1()
        .min_w(px(220.0))
        .items_center()
        .gap(px(18.0))
        .children(trace_cells)
        .children((model.hidden_cells > 0).then(|| {
            div()
                .flex_none()
                .text_size(px(10.5))
                .text_color(rgba(colors.muted))
                .child(format!("+{}", model.hidden_cells))
        }))
        .into_any_element()
}

fn collapsed_metadata(
    model: &CiBarModel,
    colors: &CiBarColors,
    render: &CiBarRender,
) -> gpui::AnyElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(12.0))
        .text_size(px(11.5))
        .text_color(rgba(colors.muted))
        .child(
            div()
                .flex()
                .items_center()
                .child(div().text_color(rgba(colors.text)).child(model.branch.clone()))
                .child(format!(" @ {} · {}", model.short_sha, model.elapsed)),
        )
        .child(action_cluster(
            model.action_mode,
            colors,
            (render.open_id.clone(), render.dismiss_id.clone()),
            render.open_focus.clone().zip(render.on_open.clone()),
            render.dismiss_focus.clone().zip(render.on_dismiss.clone()),
        ))
        .into_any_element()
}

fn collapsed_toggle(
    model: &CiBarModel,
    colors: &CiBarColors,
    render: &CiBarRender,
) -> gpui::AnyElement {
    let toggle_click = Arc::clone(&render.on_toggle);
    let toggle_focus = render.toggle_focus.clone();
    let toggle_label = format!(
        "{}. CI job trace is {}",
        model.accessibility_label,
        if render.expanded { "expanded" } else { "collapsed" }
    );
    div()
        .id(render.id.clone())
        .role(Role::Button)
        .aria_label(toggle_label)
        .aria_description("Press Enter or Space to toggle job details")
        .aria_expanded(render.expanded)
        .track_focus(&render.toggle_focus)
        .flex()
        .flex_1()
        .min_w(px(0.0))
        .items_center()
        .gap(px(18.0))
        .focus_visible(|style| style.bg(rgba(with_alpha(colors.text, 0.08))))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_key_down(stop_activation_key)
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            window.focus(&toggle_focus, cx);
            toggle_click(window, cx);
        })
        .child(state_summary(model, tone_color(model.tone, colors)))
        .child(collapsed_cells(model, colors))
        .into_any_element()
}

fn collapsed_band(
    model: &CiBarModel,
    colors: &CiBarColors,
    render: &CiBarRender,
) -> gpui::AnyElement {
    div()
        .relative()
        .h(px(CI_BAR_HEIGHT))
        .flex()
        .items_center()
        .gap(px(18.0))
        .px(px(14.0))
        .bg(rgba(colors.background))
        .border_b_1()
        .border_color(rgba(underline_color(model.tone, colors, render.accent)))
        .font_family("monospace")
        .text_size(px(12.5))
        .text_color(rgba(colors.text))
        .child(collapsed_toggle(model, colors, render))
        .child(collapsed_metadata(model, colors, render))
        .child(appear_sweep(&model.head_sha, colors, render.animations))
        .into_any_element()
}

fn trace_panel(
    id: ElementId,
    model: Option<&CiTraceModel>,
    colors: &CiBarColors,
    animations: AnimationSettings,
) -> gpui::AnyElement {
    let panel = div()
        .id(id)
        .role(Role::List)
        .aria_label(
            model.map_or("CI job trace loading", |trace| trace.accessibility_label.as_str()),
        )
        .w_full()
        .flex()
        .flex_col()
        .px(px(14.0))
        .pt(px(12.0))
        .pb(px(14.0))
        .bg(rgba(colors.panel_background))
        .border_b_1()
        .border_color(rgba(colors.divider));
    let Some(model) = model else {
        return panel
            .h(px(CI_TRACE_LOADING_HEIGHT))
            .text_size(px(11.0))
            .text_color(rgba(colors.muted))
            .child("loading job trace…")
            .into_any_element();
    };
    panel
        .h(px(model.height()))
        .child(trace_axis(model, colors))
        .children(
            model
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| trace_row_element(row, index, model, colors, animations)),
        )
        .into_any_element()
}

fn trace_axis(model: &CiTraceModel, colors: &CiBarColors) -> gpui::AnyElement {
    div()
        .h(px(12.0))
        .ml(px(168.0))
        .mr(px(68.0))
        .mb(px(6.0))
        .flex()
        .items_center()
        .justify_between()
        .text_size(px(9.5))
        .text_color(rgba(with_alpha(colors.text, 0.35)))
        .children(model.axis_labels.iter().cloned())
        .into_any_element()
}

fn trace_row_element(
    row: &CiTraceRowModel,
    index: usize,
    model: &CiTraceModel,
    colors: &CiBarColors,
    animations: AnimationSettings,
) -> gpui::AnyElement {
    let name = div()
        .w(px(156.0))
        .flex_none()
        .flex()
        .flex_col()
        .text_size(px(12.0))
        .line_height(px(16.0))
        .child(div().truncate().child(format!("{} {}", row.glyph, row.name)))
        .child(
            div()
                .truncate()
                .text_size(px(10.0))
                .text_color(rgba(colors.muted))
                .child(row.workflow_name.clone()),
        );
    let grid_steps = u16::try_from(model.axis_labels.len().saturating_sub(1)).unwrap_or(u16::MAX);
    let grid = (1..grid_steps).map(|line| {
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(relative(f32::from(line) / f32::from(grid_steps)))
            .border_l_1()
            .border_color(rgba(with_alpha(colors.text, 0.05)))
    });
    let track = div().relative().flex_1().h(px(18.0)).children(grid).child(trace_job_bar(
        row,
        index,
        model.motion,
        colors,
        animations,
    ));
    div()
        .id(("ci-job-row", row.job_id))
        .role(Role::ListItem)
        .aria_label(row.accessibility_label.clone())
        .h(px(CI_TRACE_ROW_HEIGHT))
        .flex()
        .items_center()
        .gap(px(12.0))
        .text_color(rgba(colors.text))
        .child(name)
        .child(track)
        .child(
            div()
                .w(px(56.0))
                .flex_none()
                .text_align(gpui::TextAlign::Right)
                .text_size(px(11.0))
                .text_color(rgba(colors.muted))
                .child(row.elapsed.clone()),
        )
        .into_any_element()
}

fn trace_job_bar(
    row: &CiTraceRowModel,
    index: usize,
    motion: bool,
    colors: &CiBarColors,
    animations: AnimationSettings,
) -> gpui::AnyElement {
    let tone = match row.kind {
        TraceCellKind::Queued => with_alpha(colors.text, 0.25),
        TraceCellKind::Active => colors.running,
        TraceCellKind::Success => colors.success,
        TraceCellKind::Failure => colors.failure,
        TraceCellKind::Cancelled => colors.cancelled,
    };
    let mut bar = div()
        .absolute()
        .top(px(2.0))
        .bottom(px(2.0))
        .left(relative(f32::from(row.left) / 10_000.0))
        .w(relative(f32::from(row.width) / 10_000.0))
        .min_w(px(8.0))
        .overflow_hidden()
        .rounded(px(3.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .truncate()
        .text_size(px(10.0))
        .text_color(rgba(with_alpha(colors.text, 0.85)))
        .child(format!("{} {}", row.glyph, row.current_step));
    if row.kind == TraceCellKind::Queued {
        return bar
            .border_1()
            .border_dashed()
            .border_color(rgba(tone))
            .text_color(rgba(colors.muted))
            .into_any_element();
    }
    bar = bar.bg(rgba(with_alpha(tone, 0.72)));
    if row.kind == TraceCellKind::Active {
        let edge = div().absolute().right_0().top_0().bottom_0().w(px(2.0)).bg(rgba(colors.text));
        bar = bar.child(if motion && animations.enabled() {
            edge.with_animation(
                ElementId::Name(format!("ci-trace-frontier-{index}").into()),
                Animation::new(Duration::from_millis(1_200)).repeat(),
                |edge, delta| edge.opacity(0.3 + 0.7 * (1.0 - (delta * 2.0 - 1.0).abs())),
            )
            .into_any_element()
        } else {
            edge.into_any_element()
        });
    }
    bar.into_any_element()
}

fn state_summary(model: &CiBarModel, tone: [f32; 4]) -> gpui::AnyElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .text_color(rgba(tone))
        .child(model.state_glyph)
        .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(model.state_word))
        .into_any_element()
}

fn tone_color(tone: CiTone, colors: &CiBarColors) -> [f32; 4] {
    match tone {
        CiTone::Running => colors.running,
        CiTone::Success => colors.success,
        CiTone::Failure => colors.failure,
        CiTone::Cancelled => colors.cancelled,
        CiTone::Stale => colors.stale,
    }
}

fn underline_color(tone: CiTone, colors: &CiBarColors, accent: [f32; 4]) -> [f32; 4] {
    match tone {
        CiTone::Failure => with_alpha(colors.failure, 0.60),
        CiTone::Stale => with_alpha(colors.stale, 0.50),
        _ => with_alpha(accent, 0.55),
    }
}

fn trace_cell(
    cell: &TraceCellModel,
    motion: bool,
    index: usize,
    colors: &CiBarColors,
) -> gpui::AnyElement {
    let active = cell.kind == TraceCellKind::Active;
    let name_color = match cell.kind {
        TraceCellKind::Active => colors.text,
        TraceCellKind::Failure => colors.failure,
        _ => colors.muted,
    };
    let name = div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(11.0))
        .text_size(px(10.5))
        .text_color(rgba(name_color))
        .children(active.then(|| live_dot(index, colors.running, motion)))
        .child(cell.name.clone());
    div()
        .flex()
        .flex_1()
        .min_w(px(72.0))
        .flex_col()
        .gap(px(5.0))
        .child(name)
        .child(trace_track(cell.kind, index, colors, motion))
        .into_any_element()
}

fn live_dot(index: usize, color: [f32; 4], motion: bool) -> gpui::AnyElement {
    let dot = div().size(px(5.0)).rounded(px(1.5)).bg(rgba(color));
    if !motion {
        return dot.into_any_element();
    }
    dot.with_animation(
        ElementId::Name(format!("ci-live-dot-{index}").into()),
        Animation::new(Duration::from_millis(1_200)).repeat(),
        |dot, delta| dot.opacity(0.3 + 0.7 * (1.0 - (delta * 2.0 - 1.0).abs())),
    )
    .into_any_element()
}

fn trace_track(
    kind: TraceCellKind,
    index: usize,
    colors: &CiBarColors,
    motion: bool,
) -> gpui::AnyElement {
    let ground = with_alpha(colors.text, 0.09);
    let fill = match kind {
        TraceCellKind::Active => colors.running,
        TraceCellKind::Success => colors.success,
        TraceCellKind::Failure => colors.failure,
        TraceCellKind::Cancelled => colors.cancelled,
        TraceCellKind::Queued => {
            return div()
                .h(px(3.0))
                .border_b_1()
                .border_dashed()
                .border_color(rgba(with_alpha(colors.text, 0.22)))
                .into_any_element();
        }
    };
    let mut track = div()
        .relative()
        .h(px(3.0))
        .overflow_hidden()
        .bg(rgba(ground))
        .child(div().absolute().inset_0().rounded(px(1.5)).bg(rgba(fill)));
    if motion && kind == TraceCellKind::Active {
        let transparent = rgba(with_alpha(colors.text, 0.0));
        let bright = rgba(with_alpha(colors.text, 0.35));
        let shimmer = div()
            .absolute()
            .top_0()
            .bottom_0()
            .w(relative(0.4))
            .bg(linear_gradient(
                90.0,
                linear_color_stop(transparent, 0.0),
                linear_color_stop(bright, 1.0),
            ))
            .with_animation(
                ElementId::Name(format!("ci-track-shimmer-{index}").into()),
                Animation::new(Duration::from_millis(1_600)).repeat(),
                |shimmer, delta| shimmer.left(relative(delta.mul_add(1.8, -0.4))),
            );
        track = track.child(shimmer);
    }
    track.into_any_element()
}

fn action_cluster(
    mode: CiActionMode,
    colors: &CiBarColors,
    action_ids: (ElementId, ElementId),
    on_open: Option<(FocusHandle, CiActionHandler)>,
    on_dismiss: Option<(FocusHandle, CiActionHandler)>,
) -> gpui::AnyElement {
    if mode == CiActionMode::ReadOnly {
        return div()
            .flex_none()
            .rounded(px(4.0))
            .border_1()
            .border_color(rgba(colors.divider))
            .px(px(10.0))
            .py(px(3.0))
            .text_size(px(11.0))
            .child("viewing · read-only")
            .into_any_element();
    }
    let (open_id, dismiss_id) = action_ids;
    div()
        .flex()
        .items_center()
        .gap(px(3.0))
        .child(div().w(px(1.0)).h(px(14.0)).bg(rgba(colors.divider)).mr(px(9.0)))
        .children(on_open.map(|(focus, action)| {
            action_button((open_id, "Open CI run", "open ↗"), colors, &focus, action)
        }))
        .children(on_dismiss.map(|(focus, action)| {
            action_button((dismiss_id, "Dismiss CI run", "✕"), colors, &focus, action)
        }))
        .into_any_element()
}

fn action_button(
    copy: (ElementId, &'static str, &'static str),
    colors: &CiBarColors,
    focus: &FocusHandle,
    action: CiActionHandler,
) -> gpui::AnyElement {
    let (id, label, text) = copy;
    let click_focus = focus.clone();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .aria_description("Press Enter or Space to activate")
        .track_focus(focus)
        .rounded(px(4.0))
        .px(px(9.0))
        .py(px(4.0))
        .cursor_pointer()
        .text_color(rgba(colors.muted))
        .hover(|style| style.bg(rgba(with_alpha(colors.text, 0.08))).text_color(rgba(colors.text)))
        .focus_visible(|style| style.bg(rgba(colors.text)).text_color(rgba(colors.background)))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_key_down(stop_activation_key)
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            window.focus(&click_focus, cx);
            action(window, cx);
        })
        .child(text)
        .into_any_element()
}

fn appear_sweep(
    head_sha: &str,
    colors: &CiBarColors,
    animations: AnimationSettings,
) -> gpui::AnyElement {
    let transparent = rgba(with_alpha(colors.text, 0.0));
    let bright = rgba(with_alpha(colors.text, 0.07));
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(160.0))
        .bg(linear_gradient(
            100.0,
            linear_color_stop(transparent, 0.0),
            linear_color_stop(bright, 1.0),
        ))
        .with_animation(
            ElementId::Name(format!("ci-appear-{head_sha}").into()),
            animations.transition(Duration::from_millis(900)),
            |sweep, delta| sweep.left(relative(delta.mul_add(1.3, -0.25))),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use scribe_common::protocol::{
        CiJob, CiJobStep, CiRunConclusion, CiRunDelta, CiRunDetails, CiRunState, CiRunStatus,
        CiWorkflowRun, CiWorkflowStatus,
    };

    use super::{CiActionMode, CiBarColors, CiBarModel, CiRunBars, CiTraceModel, TraceCellKind};

    fn workflow(
        run_id: u64,
        name: &str,
        status: CiWorkflowStatus,
        conclusion: Option<CiRunConclusion>,
    ) -> CiWorkflowRun {
        CiWorkflowRun {
            run_id,
            name: name.to_owned(),
            status,
            conclusion,
            started_at_epoch_secs: Some(100),
            updated_at_epoch_secs: Some(160),
        }
    }

    fn state(rollup: CiRunStatus, stale: bool) -> CiRunState {
        let (status, conclusion) = match rollup {
            CiRunStatus::Queued => (CiWorkflowStatus::Queued, None),
            CiRunStatus::Running => (CiWorkflowStatus::InProgress, None),
            CiRunStatus::Success => (CiWorkflowStatus::Completed, Some(CiRunConclusion::Success)),
            CiRunStatus::Failure => (CiWorkflowStatus::Completed, Some(CiRunConclusion::Failure)),
            CiRunStatus::Cancelled => {
                (CiWorkflowStatus::Completed, Some(CiRunConclusion::Cancelled))
            }
        };
        CiRunState {
            repository: "acme/scribe".to_owned(),
            head_sha: "0123456789abcdef".to_owned(),
            branch: "main".to_owned(),
            workflows: vec![workflow(42, "quality", status, conclusion)],
            rollup,
            stale,
        }
    }

    fn job(
        job_id: u64,
        workflow_name: &str,
        name: &str,
        state: (CiWorkflowStatus, Option<CiRunConclusion>),
        execution: (Option<u64>, Option<u64>, &str),
    ) -> CiJob {
        let (status, conclusion) = state;
        let (started_at_epoch_secs, completed_at_epoch_secs, step) = execution;
        CiJob {
            job_id,
            workflow_run_id: 42,
            workflow_name: workflow_name.to_owned(),
            name: name.to_owned(),
            status,
            conclusion,
            started_at_epoch_secs,
            completed_at_epoch_secs,
            steps: vec![CiJobStep { name: step.to_owned(), status, conclusion }],
        }
    }

    // @lat: [[test#GPUI CI Run Bar#Every aggregate state has a non-color signifier]]
    #[test]
    fn every_aggregate_state_has_a_non_color_signifier() {
        let cases = [
            (CiRunStatus::Queued, false, "queued", "◌", TraceCellKind::Queued),
            (CiRunStatus::Running, false, "running", "◐", TraceCellKind::Active),
            (CiRunStatus::Success, false, "passed", "✓", TraceCellKind::Success),
            (CiRunStatus::Failure, false, "failed", "✕", TraceCellKind::Failure),
            (CiRunStatus::Cancelled, false, "cancelled", "⊘", TraceCellKind::Cancelled),
            (CiRunStatus::Running, true, "stale", "!", TraceCellKind::Active),
        ];

        for (rollup, stale, word, glyph, cell_kind) in cases {
            let model = CiBarModel::build(&state(rollup, stale), 220, true);
            assert_eq!(model.state_word, word);
            assert_eq!(model.state_glyph, glyph);
            assert_eq!(model.cells[0].kind, cell_kind);
            assert!(!model.cells[0].glyph.is_empty());
            assert_eq!(model.motion, rollup == CiRunStatus::Running && !stale);
            assert!(model.accessibility_label.contains(word));
        }
    }

    // @lat: [[test#GPUI CI Run Bar#Head-qualified clears preserve newer runs]]
    #[test]
    fn head_qualified_clears_preserve_newer_runs() {
        let root = PathBuf::from("/work/scribe");
        let mut bars = CiRunBars::default();
        let mut current = state(CiRunStatus::Running, false);
        current.head_sha = "head-b".to_owned();
        bars.apply(root.clone(), CiRunDelta::Set(current));

        bars.apply(root.clone(), CiRunDelta::Cleared { head_sha: "head-a".to_owned() });
        assert_eq!(heads(&bars), ["head-b"]);

        bars.apply(root, CiRunDelta::Cleared { head_sha: "head-b".to_owned() });
        assert!(bars.get(Path::new("/work/scribe")).is_empty());
    }

    fn heads(bars: &CiRunBars) -> Vec<String> {
        bars.get(Path::new("/work/scribe"))
            .iter()
            .map(|run| run.head_sha.clone())
            .collect::<Vec<_>>()
    }

    // @lat: [[test#GPUI CI Run Bar#Concurrent heads stack]]
    #[test]
    fn running_heads_stack_while_finished_heads_make_way() {
        let root = PathBuf::from("/work/scribe");
        let mut bars = CiRunBars::default();
        let mut finished = state(CiRunStatus::Failure, false);
        finished.head_sha = "head-a".to_owned();
        bars.apply(root.clone(), CiRunDelta::Set(finished));
        for head in ["head-b", "head-c"] {
            let mut running = state(CiRunStatus::Running, false);
            running.head_sha = head.to_owned();
            bars.apply(root.clone(), CiRunDelta::Set(running));
        }
        assert_eq!(heads(&bars), ["head-c", "head-b"], "a finished head must not survive a push");

        for head in ["head-d", "head-e"] {
            let mut running = state(CiRunStatus::Running, false);
            running.head_sha = head.to_owned();
            bars.apply(root.clone(), CiRunDelta::Set(running));
        }
        assert_eq!(
            heads(&bars),
            ["head-e", "head-d", "head-c"],
            "stacked bands stop at MAX_CI_TRACKED_HEADS, retiring the oldest"
        );

        let mut refreshed = state(CiRunStatus::Success, false);
        refreshed.head_sha = "head-d".to_owned();
        bars.apply(root, CiRunDelta::Set(refreshed));
        assert_eq!(heads(&bars), ["head-e", "head-d", "head-c"], "an update must not reorder");
    }

    // @lat: [[test#GPUI CI Run Bar#Owner actions stay local]]
    #[test]
    fn owner_actions_stay_local() {
        let run = state(CiRunStatus::Running, false);
        let owner = CiBarModel::build(&run, 220, true);
        assert_eq!(owner.action_mode, CiActionMode::Owner);
        assert_eq!(
            owner.open_url.as_deref(),
            Some("https://github.com/acme/scribe/actions/runs/42")
        );

        let viewer = CiBarModel::build(&run, 220, false);
        assert_eq!(viewer.action_mode, CiActionMode::ReadOnly);
        assert!(viewer.open_url.is_none());
    }

    // @lat: [[test#GPUI CI Run Bar#Long traces keep actionable cells]]
    #[test]
    fn long_traces_keep_actionable_cells() {
        let mut run = state(CiRunStatus::Running, false);
        run.workflows = (0..7)
            .map(|id| {
                workflow(
                    id,
                    &format!("done-{id}"),
                    CiWorkflowStatus::Completed,
                    Some(CiRunConclusion::Success),
                )
            })
            .chain([
                workflow(70, "broken", CiWorkflowStatus::Completed, Some(CiRunConclusion::Failure)),
                workflow(71, "live", CiWorkflowStatus::InProgress, None),
            ])
            .collect();

        let model = CiBarModel::build(&run, 220, true);
        assert_eq!(
            model.cells.iter().map(|cell| cell.name.as_str()).collect::<Vec<_>>(),
            ["broken", "live"]
        );
        assert_eq!(model.hidden_cells, 7);
    }

    // @lat: [[test#GPUI CI Run Bar#Theme drives every band color]]
    #[test]
    fn theme_drives_every_band_color() {
        let theme = scribe_common::theme::minimal_dark();
        let colors = CiBarColors::from_theme(&theme, 0.75);

        assert_eq!(colors.text.map(f32::to_bits), theme.foreground.map(f32::to_bits));
        assert_eq!(colors.running.map(f32::to_bits), theme.ansi_colors[4].map(f32::to_bits));
        assert_eq!(colors.success.map(f32::to_bits), theme.ansi_colors[2].map(f32::to_bits));
        assert_eq!(colors.failure.map(f32::to_bits), theme.ansi_colors[1].map(f32::to_bits));
        assert_eq!(colors.stale.map(f32::to_bits), theme.ansi_colors[3].map(f32::to_bits));
        assert_eq!(colors.cancelled.map(f32::to_bits), theme.ansi_colors[8].map(f32::to_bits));
        assert_eq!(colors.background[3].to_bits(), 0.75_f32.to_bits());
    }

    // @lat: [[test#GPUI CI Run Bar#Shared minute grid and non-color job cues]]
    #[test]
    fn trace_panel_uses_one_minute_axis_and_non_color_job_cues() {
        let details = CiRunDetails {
            head_sha: "head-a".to_owned(),
            jobs: vec![
                job(
                    1,
                    "quality",
                    "rust-linux",
                    (CiWorkflowStatus::Completed, Some(CiRunConclusion::Success)),
                    (Some(100), Some(160), "just test"),
                ),
                job(
                    2,
                    "quality",
                    "rust-macos",
                    (CiWorkflowStatus::InProgress, None),
                    (Some(115), None, "cargo clippy --workspace"),
                ),
                job(
                    3,
                    "docs",
                    "lat-check",
                    (CiWorkflowStatus::Queued, None),
                    (None, None, "queued"),
                ),
            ],
        };

        let model = CiTraceModel::build(&details, 220, false);

        assert_eq!(model.axis_labels, ["0m", "1m", "2m", "3m", "4m"]);
        assert_eq!((model.rows[0].left, model.rows[0].width), (0, 2_500));
        assert_eq!((model.rows[1].left, model.rows[1].width), (625, 4_375));
        assert_eq!((model.rows[2].left, model.rows[2].width), (5_000, 2_500));
        assert_eq!(
            model
                .rows
                .iter()
                .map(|row| (
                    row.glyph,
                    row.state_word,
                    row.current_step.as_str(),
                    row.elapsed.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("✓", "passed", "just test", "1m 00s"),
                ("◐", "running", "cargo clippy --workspace", "1m 45s"),
                ("◌", "queued", "queued", "—"),
            ]
        );
        assert!(model.rows.iter().all(|row| row.accessibility_label.contains(row.state_word)));
        assert!(model.motion);

        let stale = CiTraceModel::build(&details, 220, true);
        assert!(!stale.motion);
    }

    // @lat: [[test#GPUI CI Run Bar#Detail snapshots follow current head]]
    #[test]
    fn detail_snapshots_are_head_qualified() {
        let root = PathBuf::from("/work/scribe");
        let mut bars = CiRunBars::default();
        let current = state(CiRunStatus::Running, false);
        let head = current.head_sha.clone();
        bars.apply(root.clone(), CiRunDelta::Set(current));

        bars.apply_details(
            root.clone(),
            CiRunDetails { head_sha: "older-head".to_owned(), jobs: Vec::new() },
        );
        assert!(bars.details(&root, &head).is_none());

        bars.apply_details(root.clone(), CiRunDetails { head_sha: head.clone(), jobs: Vec::new() });
        assert!(bars.details(&root, &head).is_some());

        bars.apply(root.clone(), CiRunDelta::Cleared { head_sha: head.clone() });
        assert!(bars.details(&root, &head).is_none());
    }
}
