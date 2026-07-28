//! Cold-restart restore replay for the GPUI client.
//!
//! Ported from the legacy client's `restore_replay.rs` and the
//! `replay_cold_restart` sizing path. Rebuilds a [`WindowLayout`] and its pane
//! metadata from a persisted [`WindowRestoreState`] snapshot, produces the
//! ordered [`ReplayLaunch`] queue that re-creates each saved session, and sizes
//! every pane's terminal grid from the restored geometry *before* the launches
//! are dispatched. The Codex 0x0 exception is preserved: reattaching a Codex
//! session sends a zero-sized [`TerminalSize`] so the server does not pre-size
//! its Ink-rendered PTY. The legacy [`Pane`](crate::) struct has no GPUI
//! equivalent yet, so pane data rides in [`PaneRestore`]; the live-window
//! wiring lands in a later bead of the `gpui-client-rebuild` epic.

use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::path::PathBuf;

use scribe_common::ai_state::AiProvider;
use scribe_common::config::ContentPadding;
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{LayoutDirection, PaneTreeNode, TerminalSize, WorkspaceTreeNode};

use crate::layout::{LayoutNode, PaneEdges, PaneId, Rect, SplitDirection};
use crate::restore_state::{
    AiResumeMode, LaunchBinding, LaunchKind, LaunchRecord, PaneSnapshot, TabSnapshot,
    WindowRestoreState, WorkspaceLayoutSnapshot, WorkspaceSnapshot,
};
use crate::workspace_layout::WindowLayout;

/// Terminal grid dimensions (columns × rows) for a pane.
///
/// The legacy port lived in a retired renderer, on which the GPUI client does
/// not depend, so it is redefined here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

/// The command that recreates one restored pane's session.
#[derive(Debug, Clone)]
pub enum ReplayCommand {
    Shell,
    Custom(Vec<String>),
    AiTargeted { provider: AiProvider, conversation_id: String },
    AiGeneric { provider: AiProvider },
}

/// One session to re-create during cold-restart replay.
#[derive(Debug, Clone)]
pub struct ReplayLaunch {
    pub placeholder_session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub pane_id: PaneId,
    pub cwd: Option<PathBuf>,
    pub command: ReplayCommand,
    /// Persisted launch identifier from the restored snapshot. During
    /// cold-restart replay the client forwards this as
    /// `ClientMessage::CreateSession::env_envelope_id` so the server can look
    /// up and apply the persisted environment envelope keyed by this launch id.
    pub launch_id: String,
}

/// Per-pane restore metadata carried alongside the rebuilt layout.
///
/// Stands in for the legacy `Pane` struct — which the display-only GPUI spike
/// does not yet have — holding the launch binding, working directory, prompt
/// history, and the placeholder session id assigned before the server confirms.
#[derive(Debug, Clone)]
pub struct PaneRestore {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub launch_binding: LaunchBinding,
    pub cwd: Option<PathBuf>,
    pub first_prompt: Option<String>,
    pub latest_prompt: Option<String>,
    pub prompt_count: u32,
    pub last_conversation_id: Option<String>,
    /// Grid dimensions assigned by [`size_replay_pane_grids`]; `None` until the
    /// geometry pass runs.
    pub grid: Option<GridSize>,
}

/// A window rebuilt from a cold-restart snapshot.
pub struct RebuiltWindow {
    pub layout: WindowLayout,
    pub panes: HashMap<PaneId, PaneRestore>,
    pub launches: VecDeque<ReplayLaunch>,
}

/// Ordered replay queue drained one launch at a time.
pub struct ReplayState {
    pub launches: VecDeque<ReplayLaunch>,
}

struct ReplayRebuildContext<'a> {
    layout: &'a mut WindowLayout,
    panes: &'a mut HashMap<PaneId, PaneRestore>,
    launches: &'a mut VecDeque<ReplayLaunch>,
    records: &'a [LaunchRecord],
}

/// Return `true` when `argv` invokes the given provider's binary (optionally as
/// a resume).
#[must_use]
pub fn is_ai_command(argv: &[String], provider: AiProvider, resume: bool) -> bool {
    let tokens: Vec<&str> = argv.iter().flat_map(|part| part.split_whitespace()).collect();
    let binary = provider.binary_name();

    if resume {
        let resume_args = provider.resume_args();
        tokens.windows(1 + resume_args.len()).any(|parts| {
            parts.first().copied() == Some(binary)
                && parts.get(1..).is_some_and(|args| args == resume_args)
        })
    } else {
        tokens.contains(&binary)
    }
}

/// Detect which AI provider (if any) an argv launches.
#[must_use]
pub fn detect_ai_command(argv: &[String], resume: bool) -> Option<AiProvider> {
    AiProvider::all().iter().copied().find(|provider| is_ai_command(argv, *provider, resume))
}

/// Build a fresh shell launch binding with a new launch id.
#[must_use]
pub fn new_shell_binding(cwd: Option<PathBuf>) -> LaunchBinding {
    LaunchBinding {
        launch_id: SessionId::new().to_full_string(),
        kind: LaunchKind::Shell,
        fallback_cwd: cwd,
    }
}

/// Build a fresh custom-command launch binding with a new launch id.
#[must_use]
pub fn new_custom_binding(argv: Vec<String>, cwd: Option<PathBuf>) -> LaunchBinding {
    LaunchBinding {
        launch_id: SessionId::new().to_full_string(),
        kind: LaunchKind::CustomCommand { argv },
        fallback_cwd: cwd,
    }
}

/// Build a fresh AI launch binding with a new launch id.
#[must_use]
pub fn new_ai_binding(
    provider: AiProvider,
    resume_mode: AiResumeMode,
    cwd: Option<PathBuf>,
    conversation_id: Option<String>,
) -> LaunchBinding {
    LaunchBinding {
        launch_id: SessionId::new().to_full_string(),
        kind: LaunchKind::Ai { provider, resume_mode, conversation_id },
        fallback_cwd: cwd,
    }
}

/// Serialise the live window layout and pane metadata into a persistable
/// snapshot for cold-restart recovery.
#[must_use]
pub fn snapshot_window_restore<S: BuildHasher>(
    window_id: WindowId,
    layout: &WindowLayout,
    panes: &HashMap<PaneId, PaneRestore, S>,
) -> WindowRestoreState {
    let pane_to_session: HashMap<PaneId, SessionId> =
        panes.iter().map(|(pane_id, pane)| (*pane_id, pane.session_id)).collect();

    WindowRestoreState {
        version: 1,
        window_id,
        focused_workspace_id: layout.focused_workspace_id(),
        root: snapshot_workspace_tree(&layout.to_tree(&pane_to_session)),
        workspaces: snapshot_workspaces(layout, panes),
        launches: snapshot_launches(layout, panes),
    }
}

/// Rebuild a [`RebuiltWindow`] from a persisted snapshot.
#[must_use]
pub fn prepare_replay(snapshot: &WindowRestoreState) -> RebuiltWindow {
    rebuild_layout_from_snapshot(snapshot)
}

/// Expand a [`ReplayCommand`] into the argv the server should spawn, or `None`
/// for a plain login shell.
#[must_use]
pub fn command_argv(command: &ReplayCommand) -> Option<Vec<String>> {
    match command {
        ReplayCommand::Shell => None,
        ReplayCommand::Custom(argv) => Some(argv.clone()),
        ReplayCommand::AiTargeted { provider, conversation_id } => {
            let conversation_id = shell_single_quote(conversation_id);
            let args = provider.resume_args().join(" ");
            Some(vec![
                scribe_common::shell::default_shell_program(),
                String::from("-lic"),
                format!("exec {} {args} {conversation_id}", provider.binary_name()),
            ])
        }
        ReplayCommand::AiGeneric { provider } => {
            let args = provider.resume_args().join(" ");
            Some(vec![
                scribe_common::shell::default_shell_program(),
                String::from("-lic"),
                format!("exec {} {args}", provider.binary_name()),
            ])
        }
    }
}

fn shell_single_quote(value: &str) -> String {
    if value.is_empty() {
        return String::from("''");
    }

    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            escaped.push_str("'\"'\"'");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

/// Derive a [`ReplayCommand`] from a persisted launch record.
#[must_use]
pub fn replay_command_from_record(record: &LaunchRecord) -> ReplayCommand {
    match &record.kind {
        LaunchKind::Shell => ReplayCommand::Shell,
        LaunchKind::CustomCommand { argv } => ReplayCommand::Custom(argv.clone()),
        LaunchKind::Ai { provider, conversation_id: Some(id), .. } => {
            ReplayCommand::AiTargeted { provider: *provider, conversation_id: id.clone() }
        }
        LaunchKind::Ai { provider, conversation_id: None, .. } => {
            ReplayCommand::AiGeneric { provider: *provider }
        }
    }
}

/// Pop the next launch from the replay queue.
pub fn next_launch(replay: &mut ReplayState) -> Option<ReplayLaunch> {
    replay.launches.pop_front()
}

// ---------------------------------------------------------------------------
// Grid sizing (ported from the legacy `recompute_replay_pane_geometry`)
// ---------------------------------------------------------------------------

/// Inputs to the pre-launch pane grid sizing pass.
///
/// The legacy client sized each restored pane's grid from the geometry it had
/// just re-applied (rather than the pre-restore window hint) so maximized
/// windows did not create PTYs at the startup size and stay undersized.
pub struct ReplayGridParams<'a> {
    /// Content viewport (window size minus status bar), in physical pixels.
    pub viewport: Rect,
    /// Terminal cell size `(width, height)` in physical pixels.
    pub cell_size: (f32, f32),
    /// Height of a workspace's tab bar row, in physical pixels.
    pub tab_bar_height: f32,
    /// Configured content padding (logical pixels; scaled by `scale_factor`).
    pub content_padding: &'a ContentPadding,
    /// Device scale factor applied to the padding.
    pub scale_factor: f32,
}

/// Restrict `padding` to the edges that border the viewport, scaled to physical
/// pixels. Ported from the legacy `pane::effective_padding`.
#[must_use]
pub fn effective_padding(
    padding: &ContentPadding,
    edges: PaneEdges,
    scale_factor: f32,
) -> ContentPadding {
    ContentPadding {
        top: if edges.top() { padding.top * scale_factor } else { 0.0 },
        right: if edges.right() { padding.right * scale_factor } else { 0.0 },
        bottom: if edges.bottom() { padding.bottom * scale_factor } else { 0.0 },
        left: if edges.left() { padding.left * scale_factor } else { 0.0 },
    }
}

/// Compute a pane's terminal grid from its rect, chrome heights, and padding.
/// Ported byte-for-byte from the legacy `pane::compute_pane_grid`.
#[must_use]
pub fn compute_pane_grid(
    rect: Rect,
    cell_size: (f32, f32),
    tab_bar_height: f32,
    prompt_bar_height: f32,
    padding: &ContentPadding,
) -> GridSize {
    let (cell_width, cell_height) = cell_size;
    let content_w = (rect.width - padding.left - padding.right).max(1.0);
    let content_h =
        (rect.height - tab_bar_height - prompt_bar_height - padding.top - padding.bottom).max(1.0);
    grid_from_pixels(content_w, content_h, cell_width, cell_height)
}

fn grid_axis_units(extent: f32, cell_size: f32) -> u16 {
    if cell_size <= 0.0 || !extent.is_finite() || extent <= 0.0 {
        return 1;
    }

    let mut low = 0u16;
    let mut high = u16::MAX;
    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if f32::from(mid) * cell_size <= extent {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    low.max(1)
}

fn grid_from_pixels(width: f32, height: f32, cell_w: f32, cell_h: f32) -> GridSize {
    GridSize { cols: grid_axis_units(width, cell_w), rows: grid_axis_units(height, cell_h) }
}

/// Size every restored pane's grid from the re-applied window geometry, writing
/// the result into each [`PaneRestore::grid`] and returning the map.
///
/// The prompt bar reserves no space here — cold-restart panes have no live
/// prompt state until the session reattaches, mirroring the legacy behaviour of
/// sizing from the pane's current (zero) prompt-bar height.
pub fn size_replay_pane_grids<S: BuildHasher>(
    layout: &WindowLayout,
    panes: &mut HashMap<PaneId, PaneRestore, S>,
    params: &ReplayGridParams<'_>,
) -> HashMap<PaneId, GridSize> {
    // Collect (pane_id, grid) first to avoid borrowing the layout while
    // mutating the pane map, keeping the per-pane computation shallow.
    let sized: Vec<(PaneId, GridSize)> = layout
        .compute_workspace_rects(params.viewport)
        .into_iter()
        .filter_map(|(ws_id, ws_rect)| layout.find_workspace(ws_id).map(|ws| (ws, ws_rect)))
        .flat_map(|(workspace, ws_rect)| {
            workspace.tabs.iter().flat_map(move |tab| {
                tab.pane_layout
                    .compute_rects(ws_rect)
                    .into_iter()
                    .map(|geom| size_one_pane(geom, params))
            })
        })
        .collect();

    let mut grids = HashMap::new();
    for (pane_id, grid) in sized {
        grids.insert(pane_id, grid);
        if let Some(pane) = panes.get_mut(&pane_id) {
            pane.grid = Some(grid);
        }
    }
    grids
}

/// Compute one pane's grid from its rect, edges, and the shared params.
fn size_one_pane(
    (pane_id, pane_rect, pane_edges): (PaneId, Rect, PaneEdges),
    params: &ReplayGridParams<'_>,
) -> (PaneId, GridSize) {
    let eff_tbh = if pane_edges.top() { params.tab_bar_height } else { 0.0 };
    let grid = compute_pane_grid(
        pane_rect,
        params.cell_size,
        eff_tbh,
        0.0,
        &effective_padding(params.content_padding, pane_edges, params.scale_factor),
    );
    (pane_id, grid)
}

/// Convert a pane grid to a [`TerminalSize`] carrying cell dimensions.
#[must_use]
pub fn terminal_size_for_grid(grid: GridSize, cell_size: (f32, f32)) -> TerminalSize {
    TerminalSize {
        cols: grid.cols,
        rows: grid.rows,
        cell_width: round_positive_f32_to_u16(cell_size.0),
        cell_height: round_positive_f32_to_u16(cell_size.1),
    }
}

/// The Codex 0x0 exception: pick the attach dimensions for a reattaching
/// session, sending a zero-sized [`TerminalSize`] for Codex so the server does
/// not pre-size its Ink-rendered PTY (Codex reflows from its own SIGWINCH).
#[must_use]
pub fn attach_dimensions_for_session(
    grid: Option<GridSize>,
    cell_size: (f32, f32),
    is_codex: bool,
) -> TerminalSize {
    if is_codex {
        return TerminalSize::default();
    }
    grid.map_or_else(TerminalSize::default, |grid| terminal_size_for_grid(grid, cell_size))
}

/// Round a positive pixel measurement to a `u16` protocol field.
///
/// Returns 0 for anything non-finite or non-positive. Shared with the client
/// binary so cell metrics reported after a live font reload use the same
/// conversion as the ones reported during restore replay.
#[must_use]
pub fn round_positive_f32_to_u16(value: f32) -> u16 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    // `value.round()` is integer-valued, so the largest `u16` not exceeding it
    // reproduces the rounded integer (clamped to `u16::MAX`) without a
    // lint-tripping float-to-int cast.
    grid_axis_units(value.round(), 1.0)
}

// ---------------------------------------------------------------------------
// --restore-child fan-out (ported from the legacy main.rs helpers)
// ---------------------------------------------------------------------------

/// The CLI flag a fanned-out restore child carries.
pub const RESTORE_CHILD_ARG: &str = "--restore-child";

/// Return `true` when `args` contains the `--restore-child` flag.
///
/// A restore child claims exactly one additional index entry and must never fan
/// out again, so callers gate the spawn loop on this being `false`.
#[must_use]
pub fn is_restore_child<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|arg| arg.as_ref() == RESTORE_CHILD_ARG)
}

/// Spawn `count` fresh `--restore-child` client processes, each of which claims
/// one remaining restore index entry. A detached reaper thread waits on every
/// child so no zombies accumulate. Callers must pass `count = 0` (a no-op) when
/// the current process is itself a restore child.
pub fn spawn_restore_children(count: usize) {
    for _ in 0..count {
        spawn_restore_child();
    }
}

fn spawn_restore_child() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::warn!(error = %error, "cannot resolve current exe to spawn restore child");
            return;
        }
    };
    match std::process::Command::new(&exe).arg(RESTORE_CHILD_ARG).spawn() {
        Ok(mut child) => {
            let pid = child.id();
            tracing::info!(pid, "spawned restore window process");
            std::thread::spawn(move || {
                drop(child.wait());
            });
        }
        Err(error) => {
            tracing::warn!(exe = %exe.display(), error = %error, "failed to spawn restore window");
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot / rebuild internals (ported from the legacy restore_replay.rs)
// ---------------------------------------------------------------------------

fn collect_launch_ids_from_pane_snapshot(node: &PaneSnapshot, out: &mut Vec<String>) {
    match node {
        PaneSnapshot::Leaf { launch_id } => out.push(launch_id.clone()),
        PaneSnapshot::Split { first, second, .. } => {
            collect_launch_ids_from_pane_snapshot(first, out);
            collect_launch_ids_from_pane_snapshot(second, out);
        }
    }
}

fn rebuild_layout_from_snapshot(snapshot: &WindowRestoreState) -> RebuiltWindow {
    let mut layout = layout_from_snapshot(&snapshot.root, snapshot.focused_workspace_id);
    let mut panes = HashMap::new();
    let mut launches = VecDeque::new();
    let mut context = ReplayRebuildContext {
        layout: &mut layout,
        panes: &mut panes,
        launches: &mut launches,
        records: &snapshot.launches,
    };

    for workspace in &snapshot.workspaces {
        apply_workspace_snapshot(workspace, &mut context);
    }

    RebuiltWindow { layout, panes, launches }
}

fn layout_from_snapshot(
    root: &WorkspaceLayoutSnapshot,
    focused_workspace_id: WorkspaceId,
) -> WindowLayout {
    let tree = workspace_tree_from_snapshot(root);
    let mut layout = WindowLayout::from_tree(&tree);
    layout.set_focused_workspace(focused_workspace_id);
    layout
}

fn apply_workspace_snapshot(workspace: &WorkspaceSnapshot, context: &mut ReplayRebuildContext<'_>) {
    if let Some(slot) = context.layout.find_workspace_mut(workspace.workspace_id) {
        slot.name.clone_from(&workspace.name);
        slot.accent_color = workspace.accent_color;
    }

    for tab in &workspace.tabs {
        restore_tab_snapshot(workspace, tab, context);
    }

    context.layout.set_active_tab(workspace.workspace_id, workspace.active_tab_index);
}

fn restore_tab_snapshot(
    workspace: &WorkspaceSnapshot,
    tab: &TabSnapshot,
    context: &mut ReplayRebuildContext<'_>,
) {
    let placeholder_session = SessionId::new();
    let pane_pairs = context
        .layout
        .add_tab_with_pane_tree(
            workspace.workspace_id,
            placeholder_session,
            &restore_pane_tree(&tab.pane_tree),
        )
        .unwrap_or_default();
    let tab_placeholder_session_id =
        pane_pairs.first().map(|(placeholder_session_id, _)| *placeholder_session_id);

    let active_tab_index = context
        .layout
        .find_workspace(workspace.workspace_id)
        .map(|slot| slot.active_tab)
        .unwrap_or_default();

    let mut focused_pane_id = None;
    let mut launch_ids = Vec::new();
    collect_launch_ids_from_pane_snapshot(&tab.pane_tree, &mut launch_ids);

    for (launch_id, (placeholder_session_id, pane_id)) in launch_ids.into_iter().zip(pane_pairs) {
        if let Some(record) = context.records.iter().find(|record| record.launch_id == launch_id) {
            if launch_id == tab.focused_launch_id {
                focused_pane_id = Some(pane_id);
            }
            queue_from_launch_record(
                workspace.workspace_id,
                placeholder_session_id,
                pane_id,
                record,
                context,
            );
        }
    }

    if let Some(focused_pane_id) = focused_pane_id
        && let Some(restored_tab) = context
            .layout
            .find_workspace_mut(workspace.workspace_id)
            .and_then(|slot| slot.tabs.get_mut(active_tab_index))
    {
        if let Some(tab_placeholder_session_id) = tab_placeholder_session_id {
            restored_tab.session_id = tab_placeholder_session_id;
        }
        restored_tab.focused_pane = focused_pane_id;
    }
}

fn restore_pane_tree(snapshot: &PaneSnapshot) -> PaneTreeNode {
    match snapshot {
        PaneSnapshot::Leaf { .. } => PaneTreeNode::Leaf { session_id: SessionId::new() },
        PaneSnapshot::Split { direction, ratio, first, second } => PaneTreeNode::Split {
            direction: *direction,
            ratio: *ratio,
            first: Box::new(restore_pane_tree(first)),
            second: Box::new(restore_pane_tree(second)),
        },
    }
}

fn queue_from_launch_record(
    workspace_id: WorkspaceId,
    placeholder_session_id: SessionId,
    pane_id: PaneId,
    record: &LaunchRecord,
    context: &mut ReplayRebuildContext<'_>,
) {
    let binding = LaunchBinding {
        launch_id: record.launch_id.clone(),
        kind: record.kind.clone(),
        fallback_cwd: record.cwd.clone(),
    };
    let last_conversation_id = match &record.kind {
        LaunchKind::Ai { conversation_id: Some(conv_id), .. } => Some(conv_id.clone()),
        _ => None,
    };
    let pane = PaneRestore {
        session_id: placeholder_session_id,
        workspace_id,
        launch_binding: binding.clone(),
        cwd: binding.fallback_cwd.clone(),
        first_prompt: record.first_prompt.clone(),
        latest_prompt: record.latest_prompt.clone(),
        prompt_count: record.prompt_count,
        last_conversation_id,
        grid: None,
    };
    context.panes.insert(pane_id, pane);
    context.launches.push_back(ReplayLaunch {
        placeholder_session_id,
        workspace_id,
        pane_id,
        cwd: binding.fallback_cwd.clone(),
        command: replay_command_from_record(record),
        launch_id: record.launch_id.clone(),
    });
}

fn snapshot_workspace_tree(node: &WorkspaceTreeNode) -> WorkspaceLayoutSnapshot {
    match node {
        WorkspaceTreeNode::Leaf { workspace_id, .. } => {
            WorkspaceLayoutSnapshot::Leaf { workspace_id: *workspace_id }
        }
        WorkspaceTreeNode::Split { direction, ratio, first, second } => {
            WorkspaceLayoutSnapshot::Split {
                direction: *direction,
                ratio: *ratio,
                first: Box::new(snapshot_workspace_tree(first)),
                second: Box::new(snapshot_workspace_tree(second)),
            }
        }
    }
}

fn workspace_tree_from_snapshot(node: &WorkspaceLayoutSnapshot) -> WorkspaceTreeNode {
    match node {
        WorkspaceLayoutSnapshot::Leaf { workspace_id } => WorkspaceTreeNode::Leaf {
            workspace_id: *workspace_id,
            session_ids: Vec::new(),
            pane_trees: Vec::new(),
            // Active tab is restored later via `apply_workspace_snapshot` →
            // `set_active_tab`; this builder only synthesises the structural
            // tree, so 0 is fine.
            active_tab_index: 0,
        },
        WorkspaceLayoutSnapshot::Split { direction, ratio, first, second } => {
            WorkspaceTreeNode::Split {
                direction: *direction,
                ratio: *ratio,
                first: Box::new(workspace_tree_from_snapshot(first)),
                second: Box::new(workspace_tree_from_snapshot(second)),
            }
        }
    }
}

fn snapshot_workspaces<S: BuildHasher>(
    layout: &WindowLayout,
    panes: &HashMap<PaneId, PaneRestore, S>,
) -> Vec<WorkspaceSnapshot> {
    layout
        .workspace_ids_in_order()
        .into_iter()
        .filter_map(|workspace_id| layout.find_workspace(workspace_id))
        .map(|workspace| WorkspaceSnapshot {
            workspace_id: workspace.workspace_id,
            name: workspace.name.clone(),
            accent_color: workspace.accent_color,
            active_tab_index: workspace.active_tab,
            tabs: workspace
                .tabs
                .iter()
                .map(|tab| TabSnapshot {
                    focused_launch_id: panes
                        .get(&tab.focused_pane)
                        .map(|pane| pane.launch_binding.launch_id.clone())
                        .unwrap_or_default(),
                    pane_tree: snapshot_pane_tree(tab.pane_layout.root(), panes),
                })
                .collect(),
        })
        .collect()
}

fn snapshot_pane_tree<S: BuildHasher>(
    node: &LayoutNode,
    panes: &HashMap<PaneId, PaneRestore, S>,
) -> PaneSnapshot {
    match node {
        LayoutNode::Leaf(pane_id) => PaneSnapshot::Leaf {
            launch_id: panes
                .get(pane_id)
                .map(|pane| pane.launch_binding.launch_id.clone())
                .unwrap_or_default(),
        },
        LayoutNode::Split { direction, ratio, first, second } => PaneSnapshot::Split {
            direction: snapshot_direction(*direction),
            ratio: *ratio,
            first: Box::new(snapshot_pane_tree(first, panes)),
            second: Box::new(snapshot_pane_tree(second, panes)),
        },
    }
}

fn snapshot_launches<S: BuildHasher>(
    layout: &WindowLayout,
    panes: &HashMap<PaneId, PaneRestore, S>,
) -> Vec<LaunchRecord> {
    let mut launches = Vec::new();
    for workspace_id in layout.workspace_ids_in_order() {
        let Some(workspace) = layout.find_workspace(workspace_id) else { continue };
        for tab in &workspace.tabs {
            launches.extend(tab.pane_layout.all_pane_ids().into_iter().filter_map(|pane_id| {
                let pane = panes.get(&pane_id)?;
                Some(LaunchRecord {
                    launch_id: pane.launch_binding.launch_id.clone(),
                    cwd: pane.cwd.clone().or_else(|| pane.launch_binding.fallback_cwd.clone()),
                    kind: pane.launch_binding.kind.clone(),
                    first_prompt: pane.first_prompt.clone(),
                    latest_prompt: pane.latest_prompt.clone(),
                    latest_prompt_at: None,
                    latest_prompt_finished_at: None,
                    prompt_count: pane.prompt_count,
                })
            }));
        }
    }
    launches
}

fn snapshot_direction(direction: SplitDirection) -> LayoutDirection {
    match direction {
        SplitDirection::Horizontal => LayoutDirection::Horizontal,
        SplitDirection::Vertical => LayoutDirection::Vertical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restore_state::{LaunchKind, PaneSnapshot, TabSnapshot, WorkspaceSnapshot};

    fn shell_record(launch_id: &str, cwd: &str) -> LaunchRecord {
        LaunchRecord {
            launch_id: launch_id.to_owned(),
            cwd: Some(PathBuf::from(cwd)),
            kind: LaunchKind::Shell,
            first_prompt: None,
            latest_prompt: None,
            latest_prompt_at: None,
            latest_prompt_finished_at: None,
            prompt_count: 0,
        }
    }

    fn single_pane_snapshot(
        window_id: WindowId,
        workspace_id: WorkspaceId,
        launch_id: &str,
    ) -> WindowRestoreState {
        WindowRestoreState {
            version: 1,
            window_id,
            focused_workspace_id: workspace_id,
            root: WorkspaceLayoutSnapshot::Leaf { workspace_id },
            workspaces: vec![WorkspaceSnapshot {
                workspace_id,
                name: Some("proj".to_owned()),
                accent_color: [0.4, 0.5, 0.6, 1.0],
                active_tab_index: 0,
                tabs: vec![TabSnapshot {
                    focused_launch_id: launch_id.to_owned(),
                    pane_tree: PaneSnapshot::Leaf { launch_id: launch_id.to_owned() },
                }],
            }],
            launches: vec![shell_record(launch_id, "/tmp/proj")],
        }
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Replay rebuilds layout and queue]]
    #[test]
    fn prepare_replay_rebuilds_layout_and_queue() {
        let window_id = WindowId::new();
        let workspace_id = WorkspaceId::new();
        let snapshot = single_pane_snapshot(window_id, workspace_id, "launch-a");

        let rebuilt = prepare_replay(&snapshot);

        assert_eq!(rebuilt.launches.len(), 1);
        let launch = &rebuilt.launches[0];
        assert_eq!(launch.workspace_id, workspace_id);
        assert_eq!(launch.launch_id, "launch-a");
        assert_eq!(launch.cwd.as_deref(), Some(std::path::Path::new("/tmp/proj")));
        assert!(matches!(launch.command, ReplayCommand::Shell));
        // The pane metadata is keyed by the same pane id as the launch.
        let pane = rebuilt.panes.get(&launch.pane_id).expect("pane restored");
        assert_eq!(pane.workspace_id, workspace_id);
        assert!(pane.grid.is_none());
        // The focused workspace and its accent survive the round trip.
        assert_eq!(rebuilt.layout.focused_workspace_id(), workspace_id);
        assert_eq!(
            rebuilt.layout.find_workspace(workspace_id).map(|w| w.accent_color),
            Some([0.4, 0.5, 0.6, 1.0])
        );
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Snapshot survives rebuild round trip]]
    #[test]
    fn snapshot_after_rebuild_matches_original_structure() {
        let window_id = WindowId::new();
        let workspace_id = WorkspaceId::new();
        let snapshot = single_pane_snapshot(window_id, workspace_id, "launch-a");

        let rebuilt = prepare_replay(&snapshot);
        let reserialised = snapshot_window_restore(window_id, &rebuilt.layout, &rebuilt.panes);

        assert_eq!(reserialised.window_id, window_id);
        assert_eq!(reserialised.focused_workspace_id, workspace_id);
        assert_eq!(reserialised.workspaces.len(), 1);
        assert_eq!(reserialised.workspaces[0].tabs.len(), 1);
        assert_eq!(reserialised.workspaces[0].tabs[0].focused_launch_id, "launch-a");
        assert_eq!(reserialised.launches.len(), 1);
        assert_eq!(reserialised.launches[0].launch_id, "launch-a");
        assert!(reserialised.is_replayable());
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Grid sized before launch]]
    #[test]
    fn grids_sized_from_restored_geometry() {
        let window_id = WindowId::new();
        let workspace_id = WorkspaceId::new();
        let snapshot = single_pane_snapshot(window_id, workspace_id, "launch-a");
        let mut rebuilt = prepare_replay(&snapshot);

        let padding = ContentPadding { top: 0.0, right: 0.0, bottom: 0.0, left: 0.0 };
        let params = ReplayGridParams {
            viewport: Rect { x: 0.0, y: 0.0, width: 800.0, height: 480.0 },
            cell_size: (8.0, 16.0),
            tab_bar_height: 0.0,
            content_padding: &padding,
            scale_factor: 1.0,
        };
        let grids = size_replay_pane_grids(&rebuilt.layout, &mut rebuilt.panes, &params);

        let launch = &rebuilt.launches[0];
        let grid = grids.get(&launch.pane_id).copied().expect("grid computed");
        assert_eq!(grid, GridSize { cols: 100, rows: 30 });
        // The grid is written back onto the pane before the launch dispatches.
        assert_eq!(rebuilt.panes[&launch.pane_id].grid, Some(grid));
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Codex reattach sends zero size]]
    #[test]
    fn codex_reattach_keeps_zero_size_exception() {
        let grid = GridSize { cols: 100, rows: 30 };
        let non_codex = attach_dimensions_for_session(Some(grid), (8.0, 16.0), false);
        assert_eq!(non_codex.cols, 100);
        assert_eq!(non_codex.rows, 30);
        assert_eq!(non_codex.cell_width, 8);

        // Codex sessions attach at 0x0 so the server does not pre-size the PTY.
        let codex = attach_dimensions_for_session(Some(grid), (8.0, 16.0), true);
        assert_eq!(codex, TerminalSize::default());
        assert!(!codex.has_grid());
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Restore child never fans out]]
    #[test]
    fn restore_child_flag_detection() {
        assert!(is_restore_child(["scribe", "--restore-child"]));
        assert!(!is_restore_child(["scribe", "--window-id", "abc"]));
        // A restore child passes count 0 so it never fans out again; this is a
        // no-op that must not panic.
        spawn_restore_children(0);
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#AI command detection]]
    #[test]
    fn ai_command_detection_and_replay_argv() {
        let record = LaunchRecord {
            launch_id: "ai-1".to_owned(),
            cwd: None,
            kind: LaunchKind::Ai {
                provider: AiProvider::CodexCode,
                resume_mode: AiResumeMode::Resume,
                conversation_id: Some("conv-42".to_owned()),
            },
            first_prompt: None,
            latest_prompt: None,
            latest_prompt_at: None,
            latest_prompt_finished_at: None,
            prompt_count: 3,
        };
        let command = replay_command_from_record(&record);
        let argv = command_argv(&command).expect("ai resume yields argv");
        // The conversation id is single-quoted and the resume args are present.
        assert!(argv.last().unwrap().contains("codex resume 'conv-42'"));

        let codex_argv = vec!["codex".to_owned(), "resume".to_owned()];
        assert_eq!(detect_ai_command(&codex_argv, true), Some(AiProvider::CodexCode));
    }
}
