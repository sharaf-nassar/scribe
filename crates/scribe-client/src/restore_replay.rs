//! Restore launch-binding, snapshot, and replay helpers.

use std::collections::{HashMap, VecDeque};
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
use crate::workspace_layout::WindowLayout;

#[derive(Debug, Clone)]
pub enum ReplayCommand {
    Shell,
    Custom(Vec<String>),
    AiTargeted { provider: AiProvider, conversation_id: String },
    AiGeneric { provider: AiProvider },
}

#[derive(Debug, Clone)]
pub struct ReplayLaunch {
    pub placeholder_session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub pane_id: PaneId,
    pub cwd: Option<PathBuf>,
    pub command: ReplayCommand,
}

pub struct ReplayState {
    pub launches: VecDeque<ReplayLaunch>,
}

pub struct RebuiltWindow {
    pub layout: WindowLayout,
    pub panes: HashMap<PaneId, Pane>,
    pub launches: VecDeque<ReplayLaunch>,
}

struct ReplayRebuildContext<'a> {
    layout: &'a mut WindowLayout,
    panes: &'a mut HashMap<PaneId, Pane>,
    launches: &'a mut VecDeque<ReplayLaunch>,
    records: &'a [LaunchRecord],
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

pub fn prepare_replay(
    snapshot: &WindowRestoreState,
    layout: &mut WindowLayout,
    panes: &mut HashMap<PaneId, Pane>,
) -> ReplayState {
    let rebuilt = rebuild_layout_from_snapshot(snapshot);
    *layout = rebuilt.layout;
    *panes = rebuilt.panes;
    ReplayState { launches: rebuilt.launches }
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

pub fn next_launch(replay: &mut ReplayState) -> Option<ReplayLaunch> {
    replay.launches.pop_front()
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

    let _ = context.layout.set_active_tab(workspace.workspace_id, workspace.active_tab_index);
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

    for (launch_id, (placeholder_session_id, pane_id)) in
        launch_ids.into_iter().zip(pane_pairs.into_iter())
    {
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
    let mut pane = Pane::new(
        crate::pane::PaneLayoutState {
            rect: Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            grid: GridSize { cols: 1, rows: 1 },
            edges: PaneEdges::all_external(),
        },
        placeholder_session_id,
        workspace_id,
        binding.clone(),
    );
    pane.cwd.clone_from(&binding.fallback_cwd);
    pane.first_prompt.clone_from(&record.first_prompt);
    pane.latest_prompt.clone_from(&record.latest_prompt);
    pane.prompt_count = record.prompt_count;
    if let LaunchKind::Ai { conversation_id: Some(conv_id), .. } = &record.kind {
        pane.last_conversation_id = Some(conv_id.clone());
    }
    context.panes.insert(pane_id, pane);
    context.launches.push_back(ReplayLaunch {
        placeholder_session_id,
        workspace_id,
        pane_id,
        cwd: binding.fallback_cwd.clone(),
        command: replay_command_from_record(record),
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
                    latest_prompt_at: pane.latest_prompt_at.and_then(system_time_to_unix_seconds),
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

/// Convert a `SystemTime` to Unix-epoch seconds, or `None` if the time is
/// before the epoch (which the prompt-bar timestamp can never be).
fn system_time_to_unix_seconds(time: std::time::SystemTime) -> Option<u64> {
    time.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}
