//! Collapsed GitHub Actions trace band: pure state/model plus GPUI lowering.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use gpui::{
    Animation, AnimationExt as _, App, ElementId, FocusHandle, KeyDownEvent, MouseButton, Rgba,
    Role, Window, div, linear_color_stop, linear_gradient, prelude::*, px, relative,
};
use scribe_common::{
    protocol::{
        CiRunConclusion, CiRunDelta, CiRunState, CiRunStatus, CiWorkflowRun, CiWorkflowStatus,
    },
    theme::Theme,
};

use crate::{animation::AnimationSettings, layout::Rect, opacity::scale_slot};

/// Fixed height of the collapsed workspace-region band.
pub const CI_BAR_HEIGHT: f32 = 40.0;

/// Server-owned CI snapshots keyed by their trusted repository root.
#[derive(Debug, Default)]
pub struct CiRunBars {
    states: HashMap<PathBuf, CiRunState>,
}

impl CiRunBars {
    /// Apply one full replacement or head-qualified clear.
    pub fn apply(&mut self, repo_root: PathBuf, delta: CiRunDelta) {
        match delta {
            CiRunDelta::Set(state) => {
                self.states.insert(repo_root, state);
            }
            CiRunDelta::Cleared { head_sha } => {
                if self.states.get(&repo_root).is_some_and(|state| state.head_sha == head_sha) {
                    self.states.remove(&repo_root);
                }
            }
        }
    }

    /// Current snapshot for `repo_root`, if its bar is visible.
    #[must_use]
    pub fn get(&self, repo_root: &Path) -> Option<&CiRunState> {
        self.states.get(repo_root)
    }
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
        let terminal = matches!(
            state.rollup,
            CiRunStatus::Success | CiRunStatus::Failure | CiRunStatus::Cancelled
        );
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
pub type CiActionHandler = Box<dyn Fn(&mut Window, &mut App)>;

/// Geometry, ownership color, motion policy, and callbacks for one region band.
pub struct CiBarRender {
    pub id: ElementId,
    pub open_id: ElementId,
    pub dismiss_id: ElementId,
    pub rect: Rect,
    pub accent: [f32; 4],
    pub animations: AnimationSettings,
    pub open_focus: Option<FocusHandle>,
    pub dismiss_focus: Option<FocusHandle>,
    pub on_open: Option<CiActionHandler>,
    pub on_dismiss: Option<CiActionHandler>,
}

/// Lower a pure collapsed model onto the approved 40px trace direction.
pub fn render(model: &CiBarModel, colors: &CiBarColors, render: CiBarRender) -> gpui::AnyElement {
    let CiBarRender {
        id,
        open_id,
        dismiss_id,
        rect,
        accent,
        animations,
        open_focus,
        dismiss_focus,
        on_open,
        on_dismiss,
    } = render;
    let underline = underline_color(model.tone, colors, accent);
    let state = state_summary(model, tone_color(model.tone, colors));
    let trace_cells = model
        .cells
        .iter()
        .enumerate()
        .map(|(index, cell)| trace_cell(cell, model.motion, index, colors));
    let cells = div()
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
        }));
    let metadata = div()
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
            (open_id, dismiss_id),
            open_focus.zip(on_open),
            dismiss_focus.zip(on_dismiss),
        ));
    let sweep = appear_sweep(&model.head_sha, colors, animations);
    div()
        .id(id)
        .role(Role::Status)
        .aria_label(model.accessibility_label.clone())
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width))
        .h(px(rect.height))
        .overflow_hidden()
        .flex()
        .items_center()
        .gap(px(18.0))
        .px(px(14.0))
        .bg(rgba(colors.background))
        .border_b_1()
        .border_color(rgba(underline))
        .font_family("monospace")
        .text_size(px(12.5))
        .text_color(rgba(colors.text))
        .child(state)
        .child(cells)
        .child(metadata)
        .child(sweep)
        .into_any_element()
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
        .on_key_down(|event: &KeyDownEvent, _, cx| {
            if !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                cx.stop_propagation();
            }
        })
        .on_click(move |_, window, cx| {
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
        CiRunConclusion, CiRunDelta, CiRunState, CiRunStatus, CiWorkflowRun, CiWorkflowStatus,
    };

    use super::{CiActionMode, CiBarColors, CiBarModel, CiRunBars, TraceCellKind};

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
        assert_eq!(
            bars.get(Path::new("/work/scribe")).map(|run| run.head_sha.as_str()),
            Some("head-b")
        );

        bars.apply(root, CiRunDelta::Cleared { head_sha: "head-b".to_owned() });
        assert!(bars.get(Path::new("/work/scribe")).is_none());
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
}
