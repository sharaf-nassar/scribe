//! Restore launch-binding, snapshot, and replay helpers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use scribe_common::ai_state::AiProvider;
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{LayoutDirection, PaneTreeNode, WorkspaceTreeNode};
use scribe_renderer::types::GridSize;

use crate::layout::{LayoutNode, PaneEdges, PaneId, Rect, SplitDirection};
use crate::pane::Pane;
use crate::restore_state::{
    AiResumeMode, LaunchBinding, LaunchKind, LaunchRecord, PaneSnapshot, TabSnapshot,
    WindowRestoreState, WorkspaceLayoutSnapshot, WorkspaceSnapshot,
};
use crate::workspace_layout::{TabState, WindowLayout};

#[allow(dead_code, reason = "used by apply_saved_bindings for hot-restart tab matching")]
type LiveTabCandidate = (usize, Vec<PaneId>);
#[allow(dead_code, reason = "used by apply_saved_bindings for hot-restart tab matching")]
type SavedTabCandidates = (usize, Vec<LiveTabCandidate>);

#[derive(Debug, Clone)]
pub enum ReplayCommand {
    Shell,
    Custom(Vec<String>),
    AiTargeted { provider: AiProvider, conversation_id: String },
    AiGeneric { provider: AiProvider },
}

#[derive(Debug, Clone)]
pub struct ReplayLaunch {
    #[allow(dead_code, reason = "saved launch identity is preserved for replay bookkeeping")]
    pub launch_id: String,
    pub placeholder_session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub pane_id: PaneId,
    pub cwd: Option<PathBuf>,
    pub command: ReplayCommand,
    #[allow(dead_code, reason = "consumed by current_launch_failed fallback retry logic")]
    fallbacks: VecDeque<ReplayCommand>,
}

pub struct ReplayState {
    #[allow(
        dead_code,
        reason = "window identity is preserved for multi-window replay bookkeeping"
    )]
    pub window_id: WindowId,
    pub launches: VecDeque<ReplayLaunch>,
    pub current: Option<ReplayLaunch>,
}

pub struct RebuiltWindow {
    pub layout: WindowLayout,
    pub panes: HashMap<PaneId, Pane>,
    pub launches: VecDeque<ReplayLaunch>,
}

pub fn is_ai_command(argv: &[String], provider: AiProvider, resume: bool) -> bool {
    let tokens: Vec<&str> = argv.iter().flat_map(|part| part.split_whitespace()).collect();
    let binary = match provider {
        AiProvider::ClaudeCode => "claude",
        AiProvider::CodexCode => "codex",
    };

    if resume {
        let resume_flag = match provider {
            AiProvider::ClaudeCode => "--resume",
            AiProvider::CodexCode => "resume",
        };
        tokens.windows(2).any(|parts| {
            parts.first().copied() == Some(binary) && parts.get(1).copied() == Some(resume_flag)
        })
    } else {
        tokens.contains(&binary)
    }
}

pub fn new_shell_binding(cwd: Option<PathBuf>) -> LaunchBinding {
    LaunchBinding {
        launch_id: SessionId::new().to_full_string(),
        kind: LaunchKind::Shell,
        fallback_cwd: cwd,
    }
}

pub fn new_custom_binding(argv: Vec<String>, cwd: Option<PathBuf>) -> LaunchBinding {
    LaunchBinding {
        launch_id: SessionId::new().to_full_string(),
        kind: LaunchKind::CustomCommand { argv },
        fallback_cwd: cwd,
    }
}

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

#[allow(dead_code, reason = "hot-restart binding hydration — wired after reconnect")]
pub fn apply_saved_bindings(
    saved: &WindowRestoreState,
    layout: &WindowLayout,
    session_to_pane: &HashMap<SessionId, PaneId>,
    panes: &mut HashMap<PaneId, Pane>,
) {
    let live_pane_ids: HashSet<PaneId> = session_to_pane.values().copied().collect();
    let launch_records: HashMap<&str, &LaunchRecord> =
        saved.launches.iter().map(|record| (record.launch_id.as_str(), record)).collect();

    for saved_workspace in &saved.workspaces {
        let Some(live_workspace) = layout.find_workspace(saved_workspace.workspace_id) else {
            continue;
        };
        let tab_matches =
            match_saved_tabs_to_live_tabs(saved_workspace, &live_workspace.tabs, &live_pane_ids);

        for (saved_tab_index, _, live_tab_pane_ids) in tab_matches {
            let Some(saved_tab) = saved_workspace.tabs.get(saved_tab_index) else { continue };
            hydrate_tab_bindings(saved_tab, live_tab_pane_ids, panes, &launch_records);
        }
    }
}

pub fn snapshot_window_restore(
    window_id: WindowId,
    layout: &WindowLayout,
    panes: &HashMap<PaneId, Pane>,
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

#[allow(dead_code, reason = "hot-restart focus restoration — wired after reconnect")]
pub fn apply_saved_focus(
    saved: &WindowRestoreState,
    layout: &mut WindowLayout,
    session_to_pane: &HashMap<SessionId, PaneId>,
    panes: &HashMap<PaneId, Pane>,
) {
    let live_pane_ids: HashSet<PaneId> = session_to_pane.values().copied().collect();
    let mut focused_workspace_matched = false;

    for saved_workspace in &saved.workspaces {
        let Some(live_workspace) = layout.find_workspace_mut(saved_workspace.workspace_id) else {
            continue;
        };
        let tab_matches =
            match_saved_tabs_to_live_tabs(saved_workspace, &live_workspace.tabs, &live_pane_ids);

        if let Some((_, live_tab_index, _)) = tab_matches
            .iter()
            .find(|(saved_tab_index, _, _)| *saved_tab_index == saved_workspace.active_tab_index)
        {
            live_workspace.active_tab = *live_tab_index;
            if saved_workspace.workspace_id == saved.focused_workspace_id {
                focused_workspace_matched = true;
            }
        }

        for (saved_tab_index, live_tab_index, _) in tab_matches {
            let Some(saved_tab) = saved_workspace.tabs.get(saved_tab_index) else { continue };
            let Some(live_tab) = live_workspace.tabs.get_mut(live_tab_index) else { continue };
            let focused_launch_id = saved_tab.focused_launch_id.as_str();
            let focused_pane = live_tab
                .pane_layout
                .all_pane_ids()
                .into_iter()
                .find(|pane_id| pane_launch_id(panes, *pane_id) == Some(focused_launch_id));
            if let Some(pane_id) = focused_pane {
                live_tab.focused_pane = pane_id;
            }
        }
    }

    if focused_workspace_matched && layout.find_workspace(saved.focused_workspace_id).is_some() {
        layout.set_focused_workspace(saved.focused_workspace_id);
    }
}

pub fn prepare_replay(
    snapshot: &WindowRestoreState,
    layout: &mut WindowLayout,
    panes: &mut HashMap<PaneId, Pane>,
) -> ReplayState {
    let rebuilt = rebuild_layout_from_snapshot(snapshot);
    *layout = rebuilt.layout;
    *panes = rebuilt.panes;
    ReplayState { window_id: snapshot.window_id, launches: rebuilt.launches, current: None }
}

pub fn command_argv(command: &ReplayCommand) -> Option<Vec<String>> {
    match command {
        ReplayCommand::Shell => None,
        ReplayCommand::Custom(argv) => Some(argv.clone()),
        ReplayCommand::AiTargeted { provider: AiProvider::ClaudeCode, conversation_id } => {
            let conversation_id = shell_single_quote(conversation_id);
            Some(vec![
                scribe_common::shell::default_shell_program(),
                String::from("-lic"),
                format!("exec claude --resume {conversation_id}"),
            ])
        }
        ReplayCommand::AiTargeted { provider: AiProvider::CodexCode, conversation_id } => {
            let conversation_id = shell_single_quote(conversation_id);
            Some(vec![
                scribe_common::shell::default_shell_program(),
                String::from("-lic"),
                format!("exec codex resume {conversation_id}"),
            ])
        }
        ReplayCommand::AiGeneric { provider: AiProvider::ClaudeCode } => Some(vec![
            scribe_common::shell::default_shell_program(),
            String::from("-lic"),
            String::from("exec claude --resume"),
        ]),
        ReplayCommand::AiGeneric { provider: AiProvider::CodexCode } => Some(vec![
            scribe_common::shell::default_shell_program(),
            String::from("-lic"),
            String::from("exec codex resume"),
        ]),
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

#[allow(dead_code, reason = "replay error handling — wired with fallback retry logic")]
pub fn replay_command_to_launch_kind(command: &ReplayCommand) -> LaunchKind {
    match command {
        ReplayCommand::Shell => LaunchKind::Shell,
        ReplayCommand::Custom(argv) => LaunchKind::CustomCommand { argv: argv.clone() },
        ReplayCommand::AiTargeted { provider, conversation_id } => LaunchKind::Ai {
            provider: *provider,
            resume_mode: AiResumeMode::Resume,
            conversation_id: Some(conversation_id.clone()),
        },
        ReplayCommand::AiGeneric { provider } => LaunchKind::Ai {
            provider: *provider,
            resume_mode: AiResumeMode::Resume,
            conversation_id: None,
        },
    }
}

#[allow(dead_code, reason = "replay error handling — syncs binding after fallback")]
pub fn sync_launch_binding_kind(
    panes: &mut HashMap<PaneId, Pane>,
    pane_id: PaneId,
    command: &ReplayCommand,
) {
    if let Some(pane) = panes.get_mut(&pane_id) {
        pane.launch_binding.kind = replay_command_to_launch_kind(command);
    }
}

pub fn next_launch(replay: &mut ReplayState) -> Option<ReplayLaunch> {
    let launch = replay.launches.pop_front()?;
    replay.current = Some(launch.clone());
    Some(launch)
}

#[allow(dead_code, reason = "replay lifecycle — marks current launch done after session created")]
pub fn finish_current_launch(
    replay: &mut ReplayState,
    _session_id: SessionId,
    _panes: &mut HashMap<PaneId, Pane>,
    _session_to_pane: &HashMap<SessionId, PaneId>,
) {
    replay.current = None;
}

#[allow(dead_code, reason = "replay fallback retry — tries next command when session fails")]
pub fn current_launch_failed(replay: &mut ReplayState) -> Option<ReplayLaunch> {
    let current = replay.current.as_mut()?;
    let Some(next_command) = current.fallbacks.pop_front() else {
        replay.current = None;
        return None;
    };
    current.command = next_command;
    Some(current.clone())
}

#[allow(dead_code, reason = "used by apply_saved_bindings")]
fn hydrate_tab_bindings(
    saved_tab: &TabSnapshot,
    live_tab_pane_ids: Vec<PaneId>,
    panes: &mut HashMap<PaneId, Pane>,
    launch_records: &HashMap<&str, &LaunchRecord>,
) {
    let saved_launch_ids = collect_tab_launch_ids(&saved_tab.pane_tree);

    for (launch_id, pane_id) in saved_launch_ids.into_iter().zip(live_tab_pane_ids) {
        let Some(record) = launch_records.get(launch_id.as_str()) else { continue };
        let Some(pane) = panes.get_mut(&pane_id) else { continue };
        pane.launch_binding = LaunchBinding {
            launch_id: record.launch_id.clone(),
            kind: record.kind.clone(),
            fallback_cwd: record.cwd.clone().or_else(|| pane.cwd.clone()),
        };
    }
}

#[allow(dead_code, reason = "used by apply_saved_bindings and apply_saved_focus")]
fn match_saved_tabs_to_live_tabs(
    saved_workspace: &WorkspaceSnapshot,
    live_tabs: &[TabState],
    live_pane_ids: &HashSet<PaneId>,
) -> Vec<(usize, usize, Vec<PaneId>)> {
    let mut candidates_by_saved: Vec<SavedTabCandidates> = saved_workspace
        .tabs
        .iter()
        .enumerate()
        .map(|(saved_tab_index, saved_tab)| {
            let candidates = live_tabs
                .iter()
                .enumerate()
                .filter_map(|(live_tab_index, live_tab)| {
                    matching_live_tab_pane_ids(saved_tab, live_tab, live_pane_ids)
                        .map(|pane_ids| (live_tab_index, pane_ids))
                })
                .collect();
            (saved_tab_index, candidates)
        })
        .collect();
    let mut candidate_counts: HashMap<usize, usize> = HashMap::new();

    for (_, candidates) in &candidates_by_saved {
        for (live_tab_index, _) in candidates {
            *candidate_counts.entry(*live_tab_index).or_default() += 1;
        }
    }

    let mut matches = Vec::new();

    for (saved_tab_index, candidates) in candidates_by_saved.drain(..) {
        if let [single_match] = candidates.as_slice() {
            if candidate_counts.get(&single_match.0) == Some(&1) {
                matches.push((saved_tab_index, single_match.0, single_match.1.clone()));
            }
        }
    }

    matches
}

#[allow(dead_code, reason = "used by hydrate_tab_bindings")]
fn collect_tab_launch_ids(node: &PaneSnapshot) -> Vec<String> {
    let mut out = Vec::new();
    collect_launch_ids_from_pane_snapshot(node, &mut out);
    out
}

fn collect_launch_ids_from_pane_snapshot(node: &PaneSnapshot, out: &mut Vec<String>) {
    match node {
        PaneSnapshot::Leaf { launch_id } => out.push(launch_id.clone()),
        PaneSnapshot::Split { first, second, .. } => {
            collect_launch_ids_from_pane_snapshot(first, out);
            collect_launch_ids_from_pane_snapshot(second, out);
        }
    }
}

#[allow(dead_code, reason = "used by match_saved_tabs_to_live_tabs")]
fn matching_live_tab_pane_ids(
    saved_tab: &TabSnapshot,
    live_tab: &TabState,
    live_pane_ids: &HashSet<PaneId>,
) -> Option<Vec<PaneId>> {
    if !pane_tree_shape_matches(&saved_tab.pane_tree, live_tab.pane_layout.root()) {
        return None;
    }

    let live_tab_pane_ids: Vec<PaneId> = live_tab
        .pane_layout
        .all_pane_ids()
        .into_iter()
        .filter(|pane_id| live_pane_ids.contains(pane_id))
        .collect();

    (collect_tab_launch_ids(&saved_tab.pane_tree).len() == live_tab_pane_ids.len())
        .then_some(live_tab_pane_ids)
}

#[allow(dead_code, reason = "used by matching_live_tab_pane_ids")]
fn pane_tree_shape_matches(saved: &PaneSnapshot, live: &LayoutNode) -> bool {
    match (saved, live) {
        (PaneSnapshot::Leaf { .. }, LayoutNode::Leaf(_)) => true,
        (
            PaneSnapshot::Split {
                direction: saved_direction,
                first: saved_first,
                second: saved_second,
                ..
            },
            LayoutNode::Split {
                direction: live_direction,
                first: live_first,
                second: live_second,
                ..
            },
        ) => {
            *saved_direction == snapshot_direction(*live_direction)
                && pane_tree_shape_matches(saved_first, live_first)
                && pane_tree_shape_matches(saved_second, live_second)
        }
        _ => false,
    }
}

#[allow(dead_code, reason = "used by apply_saved_focus")]
fn pane_launch_id(panes: &HashMap<PaneId, Pane>, pane_id: PaneId) -> Option<&str> {
    panes.get(&pane_id).map(|pane| pane.launch_binding.launch_id.as_str())
}

fn rebuild_layout_from_snapshot(snapshot: &WindowRestoreState) -> RebuiltWindow {
    let mut layout = layout_from_snapshot(&snapshot.root, snapshot.focused_workspace_id);
    let mut panes = HashMap::new();
    let mut launches = VecDeque::new();

    for workspace in &snapshot.workspaces {
        apply_workspace_snapshot(
            workspace,
            &mut layout,
            &mut panes,
            &mut launches,
            &snapshot.launches,
        );
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

fn apply_workspace_snapshot(
    workspace: &WorkspaceSnapshot,
    layout: &mut WindowLayout,
    panes: &mut HashMap<PaneId, Pane>,
    launches: &mut VecDeque<ReplayLaunch>,
    records: &[LaunchRecord],
) {
    if let Some(slot) = layout.find_workspace_mut(workspace.workspace_id) {
        slot.name.clone_from(&workspace.name);
        slot.accent_color = workspace.accent_color;
    }

    for tab in &workspace.tabs {
        restore_tab_snapshot(workspace, tab, layout, panes, launches, records);
    }

    let _ = layout.set_active_tab(workspace.workspace_id, workspace.active_tab_index);
}

#[allow(
    clippy::too_many_arguments,
    reason = "tab replay reconstruction needs snapshot state, layout state, pane map, launch queue, and saved records"
)]
fn restore_tab_snapshot(
    workspace: &WorkspaceSnapshot,
    tab: &TabSnapshot,
    layout: &mut WindowLayout,
    panes: &mut HashMap<PaneId, Pane>,
    launches: &mut VecDeque<ReplayLaunch>,
    records: &[LaunchRecord],
) {
    let placeholder_session = SessionId::new();
    let pane_pairs = layout
        .add_tab_with_pane_tree(
            workspace.workspace_id,
            placeholder_session,
            &restore_pane_tree(&tab.pane_tree),
        )
        .unwrap_or_default();
    let tab_placeholder_session_id =
        pane_pairs.first().map(|(placeholder_session_id, _)| *placeholder_session_id);

    let active_tab_index = layout
        .find_workspace(workspace.workspace_id)
        .map(|slot| slot.active_tab)
        .unwrap_or_default();

    let mut focused_pane_id = None;
    let mut launch_ids = Vec::new();
    collect_launch_ids_from_pane_snapshot(&tab.pane_tree, &mut launch_ids);

    for (launch_id, (placeholder_session_id, pane_id)) in
        launch_ids.into_iter().zip(pane_pairs.into_iter())
    {
        if let Some(record) = records.iter().find(|record| record.launch_id == launch_id) {
            if launch_id == tab.focused_launch_id {
                focused_pane_id = Some(pane_id);
            }
            queue_from_launch_record(
                workspace.workspace_id,
                placeholder_session_id,
                pane_id,
                record,
                panes,
                launches,
            );
        }
    }

    if let Some(focused_pane_id) = focused_pane_id
        && let Some(restored_tab) = layout
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

#[allow(
    clippy::too_many_arguments,
    reason = "replay queueing needs launch metadata plus the mutable pane and queue registries"
)]
fn queue_from_launch_record(
    workspace_id: WorkspaceId,
    placeholder_session_id: SessionId,
    pane_id: PaneId,
    record: &LaunchRecord,
    panes: &mut HashMap<PaneId, Pane>,
    launches: &mut VecDeque<ReplayLaunch>,
) {
    let binding = LaunchBinding {
        launch_id: record.launch_id.clone(),
        kind: record.kind.clone(),
        fallback_cwd: record.cwd.clone(),
    };
    let mut pane = Pane::new(
        Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
        GridSize { cols: 1, rows: 1 },
        placeholder_session_id,
        workspace_id,
        PaneEdges::all_external(),
        binding.clone(),
    );
    pane.cwd.clone_from(&binding.fallback_cwd);
    pane.first_prompt.clone_from(&record.first_prompt);
    pane.latest_prompt.clone_from(&record.latest_prompt);
    pane.prompt_count = record.prompt_count;
    if let LaunchKind::Ai { conversation_id: Some(conv_id), .. } = &record.kind {
        pane.last_conversation_id = Some(conv_id.clone());
    }
    panes.insert(pane_id, pane);
    launches.push_back(ReplayLaunch {
        launch_id: binding.launch_id.clone(),
        placeholder_session_id,
        workspace_id,
        pane_id,
        cwd: binding.fallback_cwd.clone(),
        command: replay_command_from_record(record),
        fallbacks: fallback_commands(record),
    });
}

fn fallback_commands(record: &LaunchRecord) -> VecDeque<ReplayCommand> {
    match &record.kind {
        LaunchKind::Shell => VecDeque::new(),
        LaunchKind::CustomCommand { .. } => VecDeque::from([ReplayCommand::Shell]),
        LaunchKind::Ai { provider, conversation_id: Some(_), .. } => {
            VecDeque::from([ReplayCommand::AiGeneric { provider: *provider }, ReplayCommand::Shell])
        }
        LaunchKind::Ai { provider: _, conversation_id: None, .. } => {
            VecDeque::from([ReplayCommand::Shell])
        }
    }
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

fn snapshot_workspaces(
    layout: &WindowLayout,
    panes: &HashMap<PaneId, Pane>,
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

fn snapshot_pane_tree(node: &LayoutNode, panes: &HashMap<PaneId, Pane>) -> PaneSnapshot {
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

fn snapshot_launches(layout: &WindowLayout, panes: &HashMap<PaneId, Pane>) -> Vec<LaunchRecord> {
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
