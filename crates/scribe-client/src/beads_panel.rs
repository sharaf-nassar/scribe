//! Read-only Beads issue panel state and rendering.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt as _, AnyElement, ElementId, FontWeight, MouseButton, Rgba, Role,
    SharedString, div, linear_color_stop, linear_gradient, prelude::*, px,
};
use scribe_common::ids::WorkspaceId;
use scribe_common::protocol::{
    BeadsBoardItem, BeadsBoardSnapshot, BeadsBoardState, BeadsIssueComment, BeadsIssueDetail,
    BeadsIssueQueue, BeadsIssueQueueBasis,
};

use crate::animation::AnimationSettings;
use crate::beads_board::BeadsBoardColors;
use crate::layout::Rect;

const PANEL_WIDTH: f32 = 560.0;
const PANEL_MIN_WIDTH: f32 = 400.0;
const PANEL_MARGIN: f32 = 12.0;
const PANEL_BOARD_GAP: f32 = 4.0;
const PANEL_OPEN_DURATION: Duration = Duration::from_millis(120);
const NOTICE_DURATION: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelSection {
    Head,
    Identity,
    Epic,
    Labels,
    Owner,
    Spec,
    Design,
    Queue,
    DependencyThread,
    Blockers,
    Description,
    Acceptance,
    Notes,
    Facts,
    Comments,
    HiddenCount,
    Dependents,
    StatusRail,
}

/// Inert words shown in the read-only slice; they carry no write operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadOnlyVerb {
    Claim,
    CloseIssue,
}

const OPEN_VERBS: [ReadOnlyVerb; 2] = [ReadOnlyVerb::Claim, ReadOnlyVerb::CloseIssue];

/// Data-derived panel shape consumed by the renderer and its build tests.
#[derive(Debug, Clone)]
struct PanelPresentation {
    sections: Vec<PanelSection>,
    blocker_count: usize,
    hidden_comment_count: Option<u32>,
    queue: BeadsIssueQueue,
    queue_basis: BeadsIssueQueueBasis,
    verbs: &'static [ReadOnlyVerb],
}

impl PanelPresentation {
    fn from_detail(detail: &BeadsIssueDetail) -> Self {
        let mut sections = vec![
            PanelSection::Head,
            PanelSection::Identity,
            PanelSection::Queue,
            PanelSection::DependencyThread,
            PanelSection::StatusRail,
        ];
        let optional = [
            (detail.parent_epic_name.is_some(), PanelSection::Epic),
            (!detail.labels.is_empty(), PanelSection::Labels),
            (detail.owner.is_some(), PanelSection::Owner),
            (detail.spec_id.is_some(), PanelSection::Spec),
            (!detail.design.is_empty(), PanelSection::Design),
            (!detail.blockers.is_empty(), PanelSection::Blockers),
            (!detail.description.is_empty(), PanelSection::Description),
            (!detail.acceptance_criteria.is_empty(), PanelSection::Acceptance),
            (!detail.notes.is_empty(), PanelSection::Notes),
            (has_optional_facts(detail), PanelSection::Facts),
            (
                !detail.comments.is_empty() || detail.hidden_comment_count > 0,
                PanelSection::Comments,
            ),
            (detail.hidden_comment_count > 0, PanelSection::HiddenCount),
            (!detail.dependents.is_empty(), PanelSection::Dependents),
        ];
        sections.extend(optional.into_iter().filter_map(|(show, section)| show.then_some(section)));
        Self {
            sections,
            blocker_count: detail.blockers.len(),
            hidden_comment_count: (detail.hidden_comment_count > 0)
                .then_some(detail.hidden_comment_count),
            queue: detail.queue,
            queue_basis: detail.queue_basis,
            verbs: if detail.status == "closed" { &[] } else { &OPEN_VERBS },
        }
    }

    fn has(&self, section: PanelSection) -> bool {
        self.sections.contains(&section)
    }

    fn blocker_count(&self) -> usize {
        self.blocker_count
    }

    fn hidden_comment_count(&self) -> Option<u32> {
        self.hidden_comment_count
    }

    fn queue(&self) -> BeadsIssueQueue {
        self.queue
    }

    fn queue_basis(&self) -> BeadsIssueQueueBasis {
        self.queue_basis
    }

    fn verbs(&self) -> &'static [ReadOnlyVerb] {
        self.verbs
    }
}

fn has_optional_facts(detail: &BeadsIssueDetail) -> bool {
    detail.closed_at.is_some()
        || detail.close_reason.is_some()
        || detail.defer_until.is_some()
        || detail.due_at.is_some()
        || detail.estimated_minutes.is_some()
        || detail.external_ref.is_some()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub max_height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelLayout {
    pub geometry: PanelGeometry,
    pub scale: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct PanelOpenFrame {
    x: f32,
    y: f32,
    width: f32,
    opacity: f32,
}

/// Place a panel under its lane while keeping it inside its workspace region.
pub fn panel_geometry(region: Rect, board: Rect, lane: u8) -> Option<PanelGeometry> {
    let width = PANEL_WIDTH.min(region.width - PANEL_MARGIN * 2.0);
    if width < PANEL_MIN_WIDTH {
        return None;
    }
    let lane_width = (board.width - 16.0) / 5.0;
    let lane_center = board.x + 8.0 + (f32::from(lane) + 0.5) * lane_width;
    let min_x = region.x + PANEL_MARGIN;
    let max_x = region.x + region.width - PANEL_MARGIN - width;
    let y = board.y + board.height + PANEL_BOARD_GAP;
    let max_height =
        (region.height * 0.7).min((region.y + region.height - y - PANEL_MARGIN).max(0.0));
    (max_height > 0.0).then_some(PanelGeometry {
        x: (lane_center - width / 2.0).clamp(min_x, max_x),
        y,
        width,
        max_height,
    })
}

pub fn panel_layout(region: Rect, board: Rect, lane: u8, scale: f32) -> Option<PanelLayout> {
    panel_geometry(region, board, lane).map(|geometry| PanelLayout { geometry, scale })
}

fn panel_open_frame(
    geometry: PanelGeometry,
    board: Rect,
    lane: u8,
    progress: f32,
) -> PanelOpenFrame {
    let progress = progress.clamp(0.0, 1.0);
    let lane_width = (board.width - 16.0) / 5.0;
    let lane_center = board.x + 8.0 + (f32::from(lane) + 0.5) * lane_width;
    let start_width = (lane_width - 16.0).clamp(1.0, geometry.width);
    let start_x = (lane_center - start_width / 2.0)
        .clamp(geometry.x, geometry.x + geometry.width - start_width);
    PanelOpenFrame {
        x: (geometry.x - start_x).mul_add(progress, start_x),
        y: 8.0f32.mul_add(progress, geometry.y - 8.0),
        width: (geometry.width - start_width).mul_add(progress, start_width),
        opacity: 0.25 + 0.75 * progress,
    }
}

fn panel_open_animation(settings: AnimationSettings) -> Animation {
    settings.transition(PANEL_OPEN_DURATION)
}

#[derive(Debug, Clone)]
pub struct BeadsPanel {
    pub card: BeadsBoardItem,
    pub lane: u8,
    pub detail: Option<Box<BeadsIssueDetail>>,
}

impl BeadsPanel {
    fn title(&self) -> &str {
        self.detail.as_deref().map_or(self.card.title.as_str(), |detail| detail.title.as_str())
    }

    fn priority(&self) -> u8 {
        self.detail.as_deref().map_or(self.card.priority, |detail| detail.priority)
    }

    fn epic(&self) -> Option<&str> {
        self.detail
            .as_deref()
            .and_then(|detail| detail.parent_epic_name.as_deref())
            .or(self.card.parent_epic_name.as_deref())
    }

    fn loading_message(&self) -> Option<&'static str> {
        self.detail.is_none().then_some("Loading issue detail…")
    }
}

#[derive(Debug, Clone)]
struct PanelNotice {
    text: String,
    lane: u8,
    expires_at: Instant,
}

impl PanelNotice {
    fn new(text: String, lane: u8) -> Self {
        Self { text, lane, expires_at: Instant::now() + NOTICE_DURATION }
    }

    fn active(&self) -> bool {
        Instant::now() < self.expires_at
    }
}

/// Per-workspace panel state plus intents parked for the owning GPUI view.
#[derive(Debug, Default)]
pub struct BeadsPanels {
    detail_enabled: bool,
    open: HashMap<WorkspaceId, BeadsPanel>,
    pending_requests: VecDeque<(WorkspaceId, String)>,
    pending_navigation: HashMap<WorkspaceId, String>,
    expanded_comments: HashSet<(WorkspaceId, String, usize)>,
    pending_copy: Option<String>,
    notices: HashMap<WorkspaceId, PanelNotice>,
    last_opened: Option<WorkspaceId>,
}

impl BeadsPanels {
    pub fn set_enabled(&mut self, enabled: bool) {
        self.detail_enabled = enabled;
        if !enabled {
            self.open.clear();
            self.pending_requests.clear();
            self.pending_navigation.clear();
            self.expanded_comments.clear();
            self.pending_copy = None;
            self.notices.clear();
            self.last_opened = None;
        }
    }

    pub fn open(&mut self, workspace_id: WorkspaceId, card: BeadsBoardItem, lane: u8) {
        if !self.detail_enabled {
            return;
        }
        let issue_id = card.id.clone();
        self.notices.remove(&workspace_id);
        self.pending_navigation.remove(&workspace_id);
        self.open.insert(workspace_id, BeadsPanel { card, lane, detail: None });
        self.pending_requests.push_back((workspace_id, issue_id));
        self.last_opened = Some(workspace_id);
    }

    pub fn update(
        &mut self,
        workspace_id: WorkspaceId,
        issue_id: &str,
        detail: Option<Box<BeadsIssueDetail>>,
    ) {
        if self.pending_navigation.get(&workspace_id).is_some_and(|target| target == issue_id) {
            self.pending_navigation.remove(&workspace_id);
            let Some(detail) = detail else {
                self.close_missing(workspace_id, issue_id);
                return;
            };
            let Some(panel) = self.open.get_mut(&workspace_id) else { return };
            panel.card = card_from_detail(&detail);
            panel.lane = queue_lane(detail.queue);
            panel.detail = Some(detail);
            return;
        }
        let Some(current) = self.open.get(&workspace_id) else { return };
        if current.card.id != issue_id {
            return;
        }
        let Some(detail) = detail else {
            self.close_missing(workspace_id, issue_id);
            return;
        };
        let Some(panel) = self.open.get_mut(&workspace_id) else { return };
        panel.lane = queue_lane(detail.queue);
        panel.detail = Some(detail);
    }

    fn close_missing(&mut self, workspace_id: WorkspaceId, issue_id: &str) {
        let Some(panel) = self.open.remove(&workspace_id) else { return };
        self.notices.insert(
            workspace_id,
            PanelNotice::new(format!("Issue {issue_id} no longer exists"), panel.lane),
        );
        self.last_opened = Some(workspace_id);
    }

    pub fn visible(&self, workspace_id: WorkspaceId) -> Option<&BeadsPanel> {
        self.open.get(&workspace_id)
    }

    pub fn workspaces(&self) -> Vec<WorkspaceId> {
        self.open
            .keys()
            .copied()
            .chain(
                self.notices
                    .iter()
                    .filter(|(_, notice)| notice.active())
                    .map(|(workspace_id, _)| *workspace_id),
            )
            .collect()
    }

    pub fn take_request(&mut self) -> Option<(WorkspaceId, String)> {
        self.pending_requests.pop_front()
    }

    pub fn dismiss(&mut self, workspace_id: WorkspaceId) -> bool {
        self.pending_navigation.remove(&workspace_id);
        let removed = self.open.remove(&workspace_id).is_some()
            | self.notices.remove(&workspace_id).is_some();
        if self.last_opened == Some(workspace_id) {
            self.last_opened = self.open.keys().next().copied();
        }
        removed
    }

    pub fn dismiss_latest(&mut self) -> bool {
        self.last_opened.is_some_and(|workspace_id| self.dismiss(workspace_id))
    }

    pub fn retain_regions(&mut self, live: &HashSet<WorkspaceId>) {
        self.open.retain(|workspace_id, _| live.contains(workspace_id));
        self.expanded_comments.retain(|(workspace_id, _, _)| live.contains(workspace_id));
        self.pending_requests.retain(|(workspace_id, _)| live.contains(workspace_id));
        self.notices.retain(|workspace_id, _| live.contains(workspace_id));
        self.pending_navigation.retain(|workspace_id, _| live.contains(workspace_id));
        if self.last_opened.is_some_and(|workspace_id| !live.contains(&workspace_id)) {
            self.last_opened = self.open.keys().next().copied();
        }
    }

    pub fn comment_expanded(
        &self,
        workspace_id: WorkspaceId,
        issue_id: &str,
        index: usize,
    ) -> bool {
        self.expanded_comments.contains(&(workspace_id, issue_id.to_owned(), index))
    }

    pub fn toggle_comment(&mut self, workspace_id: WorkspaceId, issue_id: String, index: usize) {
        let key = (workspace_id, issue_id, index);
        if !self.expanded_comments.remove(&key) {
            self.expanded_comments.insert(key);
        }
    }

    pub fn copy_issue_id(&mut self, workspace_id: WorkspaceId) -> bool {
        let Some(panel) = self.open.get(&workspace_id) else { return false };
        let issue_id =
            panel.detail.as_deref().map_or(panel.card.id.as_str(), |detail| detail.id.as_str());
        self.pending_copy = Some(issue_id.to_owned());
        true
    }

    pub fn take_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }

    pub fn notice(&self, workspace_id: WorkspaceId) -> Option<&str> {
        self.notices
            .get(&workspace_id)
            .filter(|notice| notice.active())
            .map(|notice| notice.text.as_str())
    }

    pub fn notice_lane(&self, workspace_id: WorkspaceId) -> Option<u8> {
        self.notices.get(&workspace_id).filter(|notice| notice.active()).map(|notice| notice.lane)
    }

    pub fn expire_notices(&mut self) {
        self.notices.retain(|_, notice| notice.active());
    }

    pub fn sync_board(&mut self, workspace_id: WorkspaceId, state: &BeadsBoardState) -> bool {
        if matches!(state, BeadsBoardState::NotDetected) {
            self.pending_navigation.remove(&workspace_id);
            let Some(panel) = self.open.remove(&workspace_id) else { return false };
            self.notices.insert(
                workspace_id,
                PanelNotice::new("Beads project is no longer detected".into(), panel.lane),
            );
            self.last_opened = Some(workspace_id);
            return true;
        }
        let Some(snapshot) = board_snapshot(state) else { return false };
        let Some(panel) = self.open.get_mut(&workspace_id) else { return false };
        let Some((lane, card)) = snapshot_card(snapshot, &panel.card.id) else { return false };
        let changed = panel.lane != lane || panel.card != *card;
        if changed {
            panel.lane = lane;
            panel.card = card.clone();
        }
        changed
    }

    pub fn navigate_to_dependent(&mut self, workspace_id: WorkspaceId, issue_id: &str) -> bool {
        let Some(detail) = self.open.get(&workspace_id).and_then(|panel| panel.detail.as_deref())
        else {
            return false;
        };
        if !detail.dependents.iter().any(|dependent| dependent.id == issue_id) {
            return false;
        }
        self.pending_navigation.insert(workspace_id, issue_id.to_owned());
        self.pending_requests.push_back((workspace_id, issue_id.to_owned()));
        true
    }
}

fn queue_lane(queue: BeadsIssueQueue) -> u8 {
    match queue {
        BeadsIssueQueue::Backlog => 0,
        BeadsIssueQueue::Ready => 1,
        BeadsIssueQueue::InProgress => 2,
        BeadsIssueQueue::Blocked => 3,
        BeadsIssueQueue::Done => 4,
    }
}

fn board_snapshot(state: &BeadsBoardState) -> Option<&BeadsBoardSnapshot> {
    match state {
        BeadsBoardState::Loading { cached } => cached.as_ref(),
        BeadsBoardState::Ready { snapshot, .. } => Some(snapshot),
        BeadsBoardState::NotDetected | BeadsBoardState::Unavailable { .. } => None,
    }
}

fn snapshot_card<'a>(
    snapshot: &'a BeadsBoardSnapshot,
    issue_id: &str,
) -> Option<(u8, &'a BeadsBoardItem)> {
    [
        (0, snapshot.backlog.as_slice()),
        (1, snapshot.ready.as_slice()),
        (2, snapshot.in_progress.as_slice()),
        (3, snapshot.blocked.as_slice()),
        (4, snapshot.done.as_slice()),
    ]
    .into_iter()
    .find_map(|(lane, cards)| {
        cards.iter().find(|card| card.id == issue_id).map(|card| (lane, card))
    })
}

fn card_from_detail(detail: &BeadsIssueDetail) -> BeadsBoardItem {
    BeadsBoardItem {
        id: detail.id.clone(),
        title: detail.title.clone(),
        priority: detail.priority,
        blocker_ids: detail.blockers.iter().map(|blocker| blocker.id.clone()).collect(),
        parent_epic_name: detail.parent_epic_name.clone(),
    }
}

pub fn comment_line_limit(index: usize, expanded: bool) -> Option<usize> {
    (!expanded).then_some(if index == 0 { 2 } else { 1 })
}

pub struct BeadsPanelRender {
    pub region: Rect,
    pub board: Rect,
    pub workspace_id: WorkspaceId,
    pub state: std::sync::Arc<std::sync::Mutex<BeadsPanels>>,
    pub scale: f32,
    pub colors: BeadsBoardColors,
    pub animations: AnimationSettings,
}

/// Paint one workspace's backdrop and lane-anchored detail panel.
pub fn render(panel: &BeadsPanel, wiring: &BeadsPanelRender) -> Vec<AnyElement> {
    let Some(layout) = panel_layout(wiring.region, wiring.board, panel.lane, wiring.scale) else {
        return Vec::new();
    };
    let workspace_id = wiring.workspace_id;
    let close_state = std::sync::Arc::clone(&wiring.state);
    let backdrop = div()
        .id(SharedString::from(format!("beads-detail-backdrop-{workspace_id}")))
        .absolute()
        .left(px(wiring.region.x))
        .top(px(wiring.region.y))
        .w(px(wiring.region.width))
        .h(px(wiring.region.height))
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, _app| {
            if let Ok(mut panels) = close_state.lock() {
                panels.dismiss(workspace_id);
            }
            window.refresh();
        })
        .into_any_element();
    let body = panel_body(panel, wiring, layout);
    vec![backdrop, body]
}

pub fn render_notice(text: &str, lane: u8, wiring: &BeadsPanelRender) -> Option<AnyElement> {
    let layout = panel_layout(wiring.region, wiring.board, lane, wiring.scale)?;
    Some(
        div()
            .id(SharedString::from(format!("beads-detail-notice-{}", wiring.workspace_id)))
            .absolute()
            .left(px(layout.geometry.x))
            .top(px(layout.geometry.y))
            .w(px(layout.geometry.width))
            .px(px(14.0))
            .py(px(9.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(with_alpha(wiring.colors.blocked_state, 0.65))
            .bg(wiring.colors.card)
            .font_family("monospace")
            .text_size(at(layout.scale, 10.0))
            .line_height(at(layout.scale, 14.0))
            .text_color(wiring.colors.panel_state_ink(wiring.colors.blocked_state))
            .child(text.to_owned())
            .into_any_element(),
    )
}

fn panel_body(panel: &BeadsPanel, wiring: &BeadsPanelRender, layout: PanelLayout) -> AnyElement {
    let geometry = layout.geometry;
    let colors = &wiring.colors;
    let workspace_id = wiring.workspace_id;
    let scale = layout.scale;
    let presentation = panel.detail.as_deref().map(PanelPresentation::from_detail);
    let surface = div()
        .id(SharedString::from(format!("beads-detail-{workspace_id}")))
        .aria_label(format!("Issue {} detail", panel.card.id))
        .absolute()
        .left(px(geometry.x))
        .top(px(geometry.y))
        .w(px(geometry.width))
        .max_h(px(geometry.max_height))
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded(px(4.0))
        .border_1()
        .border_color(colors.card_border_hover)
        .bg(linear_gradient(
            180.0,
            linear_color_stop(colors.card_top, 0.0),
            linear_color_stop(colors.card, 1.0),
        ))
        .shadow_lg()
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(|_, _window, app| app.stop_propagation())
        .child(panel_header(panel, presentation.as_ref(), wiring));
    let surface = if let (Some(detail), Some(presentation)) =
        (panel.detail.as_deref(), presentation.as_ref())
    {
        let content = PanelContentWiring { workspace_id, state: &wiring.state, colors, scale };
        surface.child(detail_content(detail, presentation, content)).child(status_rail(
            detail,
            presentation,
            colors,
            scale,
        ))
    } else {
        surface.child(
            div()
                .h(at(scale, 150.0))
                .flex()
                .items_center()
                .justify_center()
                .text_size(at(scale, 11.0))
                .text_color(colors.muted)
                .child(panel.loading_message().unwrap_or_default()),
        )
    };
    let board = wiring.board;
    let lane = panel.lane;
    surface
        .with_animation(
            ElementId::Name(format!("beads-detail-open-{workspace_id}-{}", panel.card.id).into()),
            panel_open_animation(wiring.animations),
            move |surface, progress| {
                let frame = panel_open_frame(geometry, board, lane, progress);
                surface.left(px(frame.x)).top(px(frame.y)).w(px(frame.width)).opacity(frame.opacity)
            },
        )
        .into_any_element()
}

fn panel_header(
    panel: &BeadsPanel,
    presentation: Option<&PanelPresentation>,
    wiring: &BeadsPanelRender,
) -> AnyElement {
    let colors = &wiring.colors;
    let scale = wiring.scale;
    let title = panel.title();
    let priority = panel.priority();
    let epic = panel.epic();
    let epic =
        presentation.is_none_or(|build| build.has(PanelSection::Epic)).then_some(epic).flatten();
    let close_state = std::sync::Arc::clone(&wiring.state);
    let workspace_id = wiring.workspace_id;
    div()
        .flex_none()
        .px(px(16.0))
        .pt(px(12.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_none()
                        .mr(px(6.0))
                        .font_family("monospace")
                        .text_size(at(scale, 11.0))
                        .line_height(at(scale, 20.0))
                        .font_weight(FontWeight(700.0))
                        .text_color(priority_color(colors, priority))
                        .child(format!("P{priority}")),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(at(scale, 15.0))
                        .line_height(at(scale, 20.0))
                        .font_weight(FontWeight(660.0))
                        .text_color(colors.title)
                        .child(title.to_owned()),
                )
                .children(epic.map(|name| {
                    div()
                        .flex_none()
                        .max_w(px(180.0))
                        .truncate()
                        .text_size(at(scale, 9.5))
                        .line_height(at(scale, 13.0))
                        .text_color(colors.epic)
                        .child(name.to_owned())
                }))
                .child(
                    div()
                        .id(SharedString::from(format!("beads-detail-close-{workspace_id}")))
                        .role(Role::Button)
                        .aria_label("Close issue detail")
                        .flex_none()
                        .cursor_pointer()
                        .text_size(at(scale, 15.0))
                        .line_height(at(scale, 15.0))
                        .text_color(colors.muted)
                        .hover(|close| close.text_color(colors.title))
                        .on_mouse_down(MouseButton::Left, |_, _window, app| {
                            app.stop_propagation();
                        })
                        .on_click(move |_event, window, _app| {
                            if let Ok(mut panels) = close_state.lock() {
                                panels.dismiss(workspace_id);
                            }
                            window.refresh();
                        })
                        .child("×"),
                ),
        )
        .child(identity_row(panel, presentation, wiring))
        .into_any_element()
}

fn identity_row(
    panel: &BeadsPanel,
    presentation: Option<&PanelPresentation>,
    wiring: &BeadsPanelRender,
) -> AnyElement {
    div()
        .mt(px(7.0))
        .pb(px(1.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .overflow_hidden()
        .child(identity_left(panel, presentation, wiring))
        .children(
            panel
                .detail
                .as_deref()
                .filter(|_| {
                    presentation.is_some_and(|build| {
                        build.has(PanelSection::Spec) || build.has(PanelSection::Design)
                    })
                })
                .map(|detail| identity_docs(detail, presentation, wiring)),
        )
        .into_any_element()
}

fn identity_left(
    panel: &BeadsPanel,
    presentation: Option<&PanelPresentation>,
    wiring: &BeadsPanelRender,
) -> AnyElement {
    let detail = panel.detail.as_deref();
    let issue_id = detail.map_or(panel.card.id.as_str(), |issue| issue.id.as_str());
    let copy_state = std::sync::Arc::clone(&wiring.state);
    let workspace_id = wiring.workspace_id;
    let colors = &wiring.colors;
    let copy_group = SharedString::from(format!("beads-detail-copy-{workspace_id}-{issue_id}"));
    div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .items_center()
        .gap(px(7.0))
        .text_size(at(wiring.scale, 9.5))
        .line_height(at(wiring.scale, 13.0))
        .text_color(colors.muted)
        .overflow_hidden()
        .child(
            div()
                .group(copy_group.clone())
                .id(SharedString::from(format!("beads-detail-id-{workspace_id}-{issue_id}")))
                .role(Role::Button)
                .aria_label(format!("Copy issue {issue_id}"))
                .flex()
                .items_center()
                .font_family("monospace")
                .cursor_pointer()
                .text_color(colors.queue_name)
                .hover(|id| id.text_color(colors.title))
                .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
                .on_click(move |_event, window, _app| {
                    if let Ok(mut panels) = copy_state.lock() {
                        panels.copy_issue_id(workspace_id);
                    }
                    window.refresh();
                })
                .child(issue_id.to_owned())
                .child(
                    div()
                        .opacity(0.0)
                        .group_hover(copy_group, |glyph| glyph.opacity(1.0))
                        .child("⧉"),
                ),
        )
        .children(detail.is_some().then(|| separator(colors).into_any_element()))
        .children(detail.map(|issue| issue.issue_type.clone()))
        .children(
            detail
                .filter(|_| presentation.is_some_and(|build| build.has(PanelSection::Labels)))
                .into_iter()
                .flat_map(|issue| {
                    issue.labels.iter().flat_map(|label| {
                        [
                            separator(colors).into_any_element(),
                            div().child(label.clone()).into_any_element(),
                        ]
                    })
                }),
        )
        .children(
            detail
                .filter(|_| presentation.is_some_and(|build| build.has(PanelSection::Owner)))
                .and_then(|issue| issue.owner.as_ref())
                .map(|owner| {
                    div().flex().gap(px(4.0)).child(separator(colors)).child("by").child(
                        div()
                            .text_color(colors.queue_name)
                            .font_weight(FontWeight(500.0))
                            .child(owner.clone()),
                    )
                }),
        )
        .into_any_element()
}

fn identity_docs(
    detail: &BeadsIssueDetail,
    presentation: Option<&PanelPresentation>,
    wiring: &BeadsPanelRender,
) -> AnyElement {
    let colors = &wiring.colors;
    div()
        .ml_auto()
        .flex_none()
        .max_w(px(220.0))
        .min_w(px(0.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .text_size(at(wiring.scale, 9.5))
        .line_height(at(wiring.scale, 13.0))
        .text_color(colors.queue_name)
        .overflow_hidden()
        .children(
            presentation
                .is_some_and(|build| build.has(PanelSection::Spec))
                .then_some(detail.spec_id.as_ref())
                .flatten()
                .map(|spec| {
                    div()
                        .min_w(px(0.0))
                        .flex()
                        .gap(px(4.0))
                        .child(runin("Spec", colors, wiring.scale))
                        .child(div().truncate().child(spec.clone()))
                }),
        )
        .children(presentation.is_some_and(|build| build.has(PanelSection::Design)).then(|| {
            div()
                .max_w(px(130.0))
                .flex()
                .gap(px(4.0))
                .child(runin("Design", colors, wiring.scale))
                .child(div().truncate().child(detail.design.clone()))
        }))
        .into_any_element()
}

#[derive(Clone, Copy)]
struct PanelContentWiring<'a> {
    workspace_id: WorkspaceId,
    state: &'a std::sync::Arc<std::sync::Mutex<BeadsPanels>>,
    colors: &'a BeadsBoardColors,
    scale: f32,
}

fn detail_content(
    detail: &BeadsIssueDetail,
    presentation: &PanelPresentation,
    wiring: PanelContentWiring<'_>,
) -> AnyElement {
    let PanelContentWiring { workspace_id, state: _, colors, scale } = wiring;
    let queue = queue_color(colors, presentation.queue());
    let blockers = detail.blockers.iter().take(presentation.blocker_count());
    div()
        .id(SharedString::from(format!("beads-detail-scroll-{workspace_id}")))
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .relative()
        .pt(px(12.0))
        .pr(px(16.0))
        .pb(px(9.0))
        .pl(px(40.0))
        .child(div().absolute().left(px(18.0)).top(px(19.0)).bottom_0().w(px(1.0)).bg(
            linear_gradient(
                180.0,
                linear_color_stop(with_alpha(queue, 0.4), 0.0),
                linear_color_stop(with_alpha(colors.blocked_state, 0.33), 1.0),
            ),
        ))
        .child(
            div()
                .absolute()
                .left(px(12.0))
                .top(px(13.0))
                .size(px(9.0))
                .rounded_full()
                .bg(queue)
                .border_2()
                .border_color(colors.card)
                .shadow_sm(),
        )
        .children(blockers.map(|blocker| blocker_row(blocker, colors, scale)))
        .child(queue_row(detail, presentation, colors, scale))
        .children(
            presentation
                .has(PanelSection::Description)
                .then(|| paragraph(&detail.description, colors.title, scale, 11.0, 1.55)),
        )
        .children(
            presentation
                .has(PanelSection::Acceptance)
                .then(|| passage("Acceptance", &detail.acceptance_criteria, colors, scale)),
        )
        .children(
            presentation
                .has(PanelSection::Notes)
                .then(|| passage("Notes", &detail.notes, colors, scale)),
        )
        .children(
            presentation.has(PanelSection::Facts).then(|| optional_facts(detail, colors, scale)),
        )
        .children(
            presentation
                .has(PanelSection::Comments)
                .then(|| comments(detail, presentation, wiring)),
        )
        .children(presentation.has(PanelSection::Dependents).then(|| unblocks(detail, wiring)))
        .into_any_element()
}

fn blocker_row(
    blocker: &scribe_common::protocol::BeadsIssueLink,
    colors: &BeadsBoardColors,
    scale: f32,
) -> AnyElement {
    div()
        .mb(px(4.0))
        .flex()
        .items_center()
        .gap(px(7.0))
        .text_size(at(scale, 9.5))
        .line_height(at(scale, 14.0))
        .text_color(colors.muted)
        .child(div().size(px(6.0)).rounded_full().bg(colors.blocked_state))
        .child(div().font_family("monospace").child(blocker.id.clone()))
        .child(div().truncate().text_color(colors.queue_name).child(blocker.title.clone()))
        .into_any_element()
}

fn queue_row(
    detail: &BeadsIssueDetail,
    presentation: &PanelPresentation,
    colors: &BeadsBoardColors,
    scale: f32,
) -> AnyElement {
    let assignee = detail.assignee.as_deref().unwrap_or("unclaimed");
    let queue = panel_queue_ink(colors, presentation.queue());
    div()
        .mb(px(8.0))
        .flex()
        .items_center()
        .gap(px(7.0))
        .text_size(at(scale, 9.5))
        .line_height(at(scale, 16.0))
        .text_color(colors.muted)
        .child(
            div()
                .flex_none()
                .text_size(at(scale, 11.0))
                .font_weight(FontWeight(650.0))
                .text_color(queue)
                .child(queue_name(presentation.queue())),
        )
        .child(queue_basis(presentation))
        .child(div().flex_1())
        .child(div().truncate().child(format!(
            "{assignee} · {} → {}",
            short_date(&detail.created_at),
            short_date(&detail.updated_at)
        )))
        .into_any_element()
}

fn paragraph(text: &str, color: Rgba, scale: f32, size: f32, line: f32) -> AnyElement {
    div()
        .text_size(at(scale, size))
        .line_height(at(scale, size * line))
        .text_color(color)
        .child(text.to_owned())
        .into_any_element()
}

fn passage(label: &'static str, text: &str, colors: &BeadsBoardColors, scale: f32) -> AnyElement {
    div()
        .mt(px(8.0))
        .flex()
        .items_start()
        .gap(px(7.0))
        .child(runin(label, colors, scale))
        .child(
            div()
                .flex_1()
                .text_size(at(scale, 11.0))
                .line_height(at(scale, 16.5))
                .text_color(colors.queue_name)
                .child(text.to_owned()),
        )
        .into_any_element()
}

fn optional_facts(detail: &BeadsIssueDetail, colors: &BeadsBoardColors, scale: f32) -> AnyElement {
    let mut facts = Vec::new();
    if let Some(closed) = detail.closed_at.as_deref() {
        facts.push(format!("closed {}", short_date(closed)));
    }
    if let Some(due) = detail.due_at.as_deref() {
        facts.push(format!("due {}", short_date(due)));
    }
    if let Some(defer) = detail.defer_until.as_deref() {
        facts.push(format!("deferred {}", short_date(defer)));
    }
    if let Some(minutes) = detail.estimated_minutes {
        facts.push(format!("{minutes} min"));
    }
    if let Some(reference) = detail.external_ref.as_deref() {
        facts.push(reference.to_owned());
    }
    if let Some(reason) = detail.close_reason.as_deref() {
        facts.push(reason.to_owned());
    }
    div()
        .mt(px(8.0))
        .truncate()
        .font_family("monospace")
        .text_size(at(scale, 9.5))
        .line_height(at(scale, 14.0))
        .text_color(colors.muted)
        .child(facts.join(" · "))
        .into_any_element()
}

fn comments(
    detail: &BeadsIssueDetail,
    presentation: &PanelPresentation,
    wiring: PanelContentWiring<'_>,
) -> AnyElement {
    let PanelContentWiring { workspace_id, state, colors, scale } = wiring;
    let comment_wiring = CommentWiring { workspace_id, issue_id: &detail.id, state, colors, scale };
    let rows = detail
        .comments
        .iter()
        .enumerate()
        .map(|(index, comment)| comment_row(comment, index, comment_wiring));
    div()
        .mt(px(10.0))
        .pt(px(7.0))
        .border_t_1()
        .border_color(with_alpha(colors.hairline, 0.6))
        .children(rows)
        .children(presentation.hidden_comment_count().map(|hidden| {
            div()
                .mt(px(4.0))
                .font_family("monospace")
                .text_size(at(scale, 9.5))
                .text_color(colors.muted)
                .child(format!("{hidden} older comments hidden"))
        }))
        .into_any_element()
}

#[derive(Clone, Copy)]
struct CommentWiring<'a> {
    workspace_id: WorkspaceId,
    state: &'a std::sync::Arc<std::sync::Mutex<BeadsPanels>>,
    colors: &'a BeadsBoardColors,
    scale: f32,
    issue_id: &'a str,
}

fn comment_row(comment: &BeadsIssueComment, index: usize, wiring: CommentWiring<'_>) -> AnyElement {
    let CommentWiring { workspace_id, state, colors, scale, issue_id } = wiring;
    let expanded =
        state.lock().is_ok_and(|panels| panels.comment_expanded(workspace_id, issue_id, index));
    let click_state = std::sync::Arc::clone(state);
    let click_issue = issue_id.to_owned();
    let body = div()
        .min_w(px(0.0))
        .text_size(at(scale, if index == 0 { 11.0 } else { 10.5 }))
        .line_height(at(scale, if index == 0 { 15.95 } else { 14.0 }))
        .text_color(if index == 0 { colors.queue_name } else { colors.muted })
        .child(comment.body.clone());
    let body = match comment_line_limit(index, expanded) {
        Some(lines) => body.line_clamp(lines).text_ellipsis(),
        None => body,
    };
    div()
        .id(SharedString::from(format!("beads-comment-{workspace_id}-{issue_id}-{index}")))
        .role(Role::Button)
        .aria_label(if expanded { "Collapse comment" } else { "Expand comment" })
        .mt(if index == 0 { px(0.0) } else { px(4.0) })
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, _app| {
            if let Ok(mut panels) = click_state.lock() {
                panels.toggle_comment(workspace_id, click_issue.clone(), index);
            }
            window.refresh();
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(at(scale, 9.5))
                .line_height(at(scale, 13.0))
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight(600.0))
                        .text_color(colors.queue_name)
                        .child(comment.author.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(colors.muted)
                        .child(short_date(&comment.created_at)),
                )
                .children((index == 0).then(|| {
                    div()
                        .ml_auto()
                        .text_size(at(scale, 10.0))
                        .text_color(colors.muted)
                        .child("add a comment…")
                })),
        )
        .child(body)
        .into_any_element()
}

fn unblocks(detail: &BeadsIssueDetail, wiring: PanelContentWiring<'_>) -> AnyElement {
    let PanelContentWiring { workspace_id, state, colors, scale } = wiring;
    div()
        .relative()
        .mt(px(10.0))
        .flex()
        .flex_wrap()
        .gap(px(7.0))
        .text_size(at(scale, 9.5))
        .line_height(at(scale, 14.0))
        .child(runin("Unblocks", colors, scale))
        .children(detail.dependents.iter().map(|dependent| {
            let target = dependent.id.clone();
            let navigate_state = std::sync::Arc::clone(state);
            div()
                .id(SharedString::from(format!("beads-dependent-{workspace_id}-{}", dependent.id)))
                .role(Role::Button)
                .aria_label(format!("Open dependent {}", dependent.id))
                .flex()
                .gap(px(5.0))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
                .on_click(move |_event, window, _app| {
                    if let Ok(mut panels) = navigate_state.lock() {
                        panels.navigate_to_dependent(workspace_id, &target);
                    }
                    window.refresh();
                })
                .child(
                    div()
                        .font_family("monospace")
                        .text_color(colors.queue_name)
                        .child(dependent.id.clone()),
                )
                .child(
                    div()
                        .font_weight(FontWeight(600.0))
                        .text_color(colors.title)
                        .child(dependent.title.clone()),
                )
        }))
        .into_any_element()
}

fn status_rail(
    detail: &BeadsIssueDetail,
    presentation: &PanelPresentation,
    colors: &BeadsBoardColors,
    scale: f32,
) -> AnyElement {
    let current = detail.status.as_str();
    div()
        .relative()
        .flex_none()
        .flex()
        .items_center()
        .px(px(14.0))
        .pt(px(7.0))
        .pb(px(9.0))
        .child(
            div()
                .absolute()
                .left(px(14.0))
                .right(px(168.0))
                .top_1_2()
                .h(px(1.0))
                .bg(colors.hairline),
        )
        .children([("open", "open"), ("in progress", "in_progress"), ("closed", "closed")].map(
            |(shown, status)| {
                let active = current == status;
                div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .px(px(9.0))
                    .bg(colors.card)
                    .text_size(at(scale, 10.0))
                    .line_height(at(scale, 16.0))
                    .font_weight(if active { FontWeight(600.0) } else { FontWeight(400.0) })
                    .text_color(if active { colors.title } else { colors.muted })
                    .children(active.then(|| {
                        div().size(px(7.0)).rounded_full().bg(colors.ready_state).shadow_sm()
                    }))
                    .child(shown)
            },
        ))
        .children(presentation.verbs().iter().map(|verb| {
            let (label, state) = match verb {
                ReadOnlyVerb::Claim => ("claim", colors.ready_state),
                ReadOnlyVerb::CloseIssue => ("close issue", colors.done_state),
            };
            let word = div()
                .font_family("monospace")
                .text_size(at(scale, 10.0))
                .line_height(at(scale, 16.0))
                .font_weight(FontWeight(600.0))
                .text_color(colors.panel_state_ink(state))
                .child(label);
            match verb {
                ReadOnlyVerb::Claim => word.ml_auto(),
                ReadOnlyVerb::CloseIssue => word.ml(px(16.0)),
            }
        }))
        .into_any_element()
}

fn queue_basis(presentation: &PanelPresentation) -> String {
    match presentation.queue_basis() {
        BeadsIssueQueueBasis::ClosedStatus => "closed state".into(),
        BeadsIssueQueueBasis::BlockedStatus => "explicitly blocked".into(),
        BeadsIssueQueueBasis::OpenBlockers => {
            format!("{} upstream blocker(s)", presentation.blocker_count())
        }
        BeadsIssueQueueBasis::InProgressStatus => "claimed work in progress".into(),
        BeadsIssueQueueBasis::ReadySet => "upstream clear · nothing blocks this bead".into(),
        BeadsIssueQueueBasis::BacklogFallback => "outside the ready set".into(),
    }
}

fn queue_name(queue: BeadsIssueQueue) -> &'static str {
    match queue {
        BeadsIssueQueue::Backlog => "Backlog",
        BeadsIssueQueue::Ready => "Ready",
        BeadsIssueQueue::InProgress => "In progress",
        BeadsIssueQueue::Blocked => "Blocked",
        BeadsIssueQueue::Done => "Done",
    }
}

fn queue_color(colors: &BeadsBoardColors, queue: BeadsIssueQueue) -> Rgba {
    match queue {
        BeadsIssueQueue::Backlog => colors.backlog_state,
        BeadsIssueQueue::Ready => colors.ready_state,
        BeadsIssueQueue::InProgress => colors.progress_state,
        BeadsIssueQueue::Blocked => colors.blocked_state,
        BeadsIssueQueue::Done => colors.done_state,
    }
}

fn panel_queue_ink(colors: &BeadsBoardColors, queue: BeadsIssueQueue) -> Rgba {
    colors.panel_state_ink(queue_color(colors, queue))
}

fn priority_color(colors: &BeadsBoardColors, priority: u8) -> Rgba {
    colors.priorities.get(usize::from(priority)).copied().unwrap_or(colors.muted)
}

fn runin(label: &'static str, colors: &BeadsBoardColors, scale: f32) -> gpui::Div {
    div()
        .flex_none()
        .text_size(at(scale, 8.5))
        .line_height(at(scale, 13.0))
        .font_weight(FontWeight(600.0))
        .text_color(colors.muted)
        .child(label.to_uppercase())
}

fn separator(colors: &BeadsBoardColors) -> gpui::Div {
    div().text_color(with_alpha(colors.muted, 0.6)).child("·")
}

fn short_date(value: &str) -> String {
    let Some(date) = value.get(..10) else { return value.to_owned() };
    let mut parts = date.split('-');
    let (Some(_year), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next()) else {
        return value.to_owned();
    };
    let month = match month {
        "01" => "Jan",
        "02" => "Feb",
        "03" => "Mar",
        "04" => "Apr",
        "05" => "May",
        "06" => "Jun",
        "07" => "Jul",
        "08" => "Aug",
        "09" => "Sep",
        "10" => "Oct",
        "11" => "Nov",
        "12" => "Dec",
        _ => return value.to_owned(),
    };
    format!("{month} {}", day.trim_start_matches('0'))
}

fn at(scale: f32, value: f32) -> gpui::Pixels {
    px(scale * value)
}

fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    Rgba { a: color.a * alpha, ..color }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use scribe_common::ids::WorkspaceId;
    use scribe_common::protocol::{
        BeadsBoardItem, BeadsBoardSnapshot, BeadsBoardState, BeadsIssueComment, BeadsIssueDetail,
        BeadsIssueLink, BeadsIssueQueue, BeadsIssueQueueBasis,
    };
    use scribe_common::theme::ChromeColors;

    use crate::animation::AnimationSettings;
    use crate::beads_board::{BODY_CONTRAST, contrast};
    use crate::layout::Rect;

    use super::*;

    fn item() -> BeadsBoardItem {
        BeadsBoardItem {
            id: "scribe-5wh1.4".into(),
            title: "Render the read-only detail panel".into(),
            priority: 1,
            blocker_ids: Vec::new(),
            parent_epic_name: Some("Beads card detail".into()),
        }
    }

    fn detail() -> BeadsIssueDetail {
        BeadsIssueDetail {
            id: "scribe-5wh1.4".into(),
            title: "Render the read-only detail panel".into(),
            description: "Description".into(),
            acceptance_criteria: "Acceptance".into(),
            notes: "Notes".into(),
            design: "Design".into(),
            spec_id: Some("024-beads-card-detail".into()),
            status: "open".into(),
            priority: 1,
            issue_type: "task".into(),
            labels: vec!["client".into()],
            parent_epic_name: Some("Beads card detail".into()),
            assignee: None,
            owner: Some("maintainer".into()),
            created_at: "2026-08-14T18:00:00Z".into(),
            updated_at: "2026-08-15T04:00:00Z".into(),
            closed_at: None,
            close_reason: None,
            defer_until: None,
            due_at: None,
            estimated_minutes: None,
            external_ref: None,
            blockers: Vec::new(),
            dependents: Vec::new(),
            comments: Vec::new(),
            hidden_comment_count: 0,
            queue: BeadsIssueQueue::Ready,
            queue_basis: BeadsIssueQueueBasis::ReadySet,
        }
    }

    fn full_detail() -> BeadsIssueDetail {
        BeadsIssueDetail {
            due_at: Some("2026-08-20T00:00:00Z".into()),
            blockers: vec![
                BeadsIssueLink { id: "gate-1".into(), title: "First gate".into() },
                BeadsIssueLink { id: "gate-2".into(), title: "Second gate".into() },
            ],
            dependents: vec![BeadsIssueLink {
                id: "next-1".into(),
                title: "Dependent work".into(),
            }],
            comments: vec![BeadsIssueComment {
                author: "reviewer".into(),
                created_at: "2026-08-15T04:00:00Z".into(),
                body: "Latest review".into(),
            }],
            hidden_comment_count: 7,
            ..detail()
        }
    }

    fn board_with(item: BeadsBoardItem, lane: u8) -> BeadsBoardState {
        let mut snapshot = BeadsBoardSnapshot::default();
        match lane {
            0 => snapshot.backlog.push(item),
            1 => snapshot.ready.push(item),
            2 => snapshot.in_progress.push(item),
            3 => snapshot.blocked.push(item),
            4 => snapshot.done.push(item),
            _ => panic!("invalid fixture lane"),
        }
        BeadsBoardState::Ready { snapshot, stale: false, refresh_error: None }
    }

    fn chrome_slots(fill: [f32; 4]) -> ChromeColors {
        ChromeColors {
            tab_bar_bg: fill,
            tab_bar_active_bg: fill,
            tab_text: fill,
            tab_text_active: fill,
            tab_separator: fill,
            status_bar_bg: fill,
            status_bar_text: fill,
            divider: fill,
            accent: fill,
            scrollbar: fill,
            tab_bar_gradient_top: fill,
            status_bar_separator: fill,
            prompt_bar_first_row_bg: fill,
            prompt_bar_second_row_bg: fill,
            prompt_bar_text: fill,
            prompt_bar_icon_first: fill,
            prompt_bar_icon_latest: fill,
        }
    }

    #[test]
    fn panel_geometry_anchors_under_the_lane_and_obeys_the_narrow_floor() {
        let region = Rect { x: 100.0, y: 40.0, width: 800.0, height: 600.0 };
        let board = Rect { x: 100.0, y: 40.0, width: 800.0, height: 178.0 };

        assert_eq!(
            panel_geometry(region, board, 4),
            Some(PanelGeometry { x: 328.0, y: 222.0, width: 560.0, max_height: 406.0 })
        );
        assert_eq!(
            panel_geometry(
                Rect { x: 0.0, y: 0.0, width: 420.0, height: 600.0 },
                Rect { x: 0.0, y: 0.0, width: 420.0, height: 178.0 },
                0,
            ),
            None
        );
    }

    #[test]
    fn named_board_height_and_text_scale_samples_stay_inside_the_region() {
        let region = Rect { x: 10.0, y: 20.0, width: 800.0, height: 600.0 };
        let samples = [
            (
                "minimum board at 0.8x",
                Rect { x: 10.0, y: 20.0, width: 800.0, height: 79.8 },
                0,
                0.8,
                PanelLayout {
                    geometry: PanelGeometry { x: 22.0, y: 103.8, width: 560.0, max_height: 420.0 },
                    scale: 0.8,
                },
            ),
            (
                "maximum board at 1.6x",
                Rect { x: 10.0, y: 20.0, width: 800.0, height: 520.0 },
                4,
                1.6,
                PanelLayout {
                    geometry: PanelGeometry { x: 238.0, y: 544.0, width: 560.0, max_height: 64.0 },
                    scale: 1.6,
                },
            ),
        ];

        for (name, board, lane, scale, expected) in samples {
            assert_eq!(panel_layout(region, board, lane, scale), Some(expected), "{name}");
        }
        let too_narrow = Rect { width: 423.0, ..region };
        let too_narrow_board = Rect { height: 197.0, ..too_narrow };
        assert!(panel_layout(too_narrow, too_narrow_board, 0, 1.0).is_none());
        let floor = Rect { width: 424.0, ..region };
        let floor_board = Rect { height: 197.0, ..floor };
        let width =
            panel_layout(floor, floor_board, 0, 1.0).expect("400px panel floor").geometry.width;
        assert!((width - 400.0).abs() < f32::EPSILON);
    }

    #[test]
    fn open_animation_finishes_at_the_asserted_layout_after_120ms() {
        let geometry = PanelGeometry { x: 22.0, y: 221.0, width: 560.0, max_height: 420.0 };
        let board = Rect { x: 10.0, y: 20.0, width: 800.0, height: 197.0 };
        let start = panel_open_frame(geometry, board, 0, 0.0);
        let end = panel_open_frame(geometry, board, 0, 1.0);
        let animation = panel_open_animation(AnimationSettings::resolve_with_env(true, None));

        assert!(start.width < geometry.width);
        assert!(start.x >= board.x + 8.0);
        assert_eq!(end, PanelOpenFrame { x: 22.0, y: 221.0, width: 560.0, opacity: 1.0 });
        assert_eq!(animation.duration, Duration::from_millis(120));
    }

    #[test]
    fn loading_panel_keeps_the_clicked_card_head_over_its_placeholder() {
        let workspace = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.open(workspace, item(), 1);

        let panel = panels.visible(workspace).expect("loading panel");
        assert_eq!(panel.title(), "Render the read-only detail panel");
        assert_eq!(panel.priority(), 1);
        assert_eq!(panel.epic(), Some("Beads card detail"));
        assert_eq!(panel.loading_message(), Some("Loading issue detail…"));
    }

    #[test]
    fn detail_and_board_updates_reanchor_only_the_matching_workspace() {
        let left = WorkspaceId::new();
        let right = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.open(left, item(), 0);
        let mut right_item = item();
        right_item.id = "scribe-right".into();
        panels.open(right, right_item.clone(), 4);

        let mut left_detail = detail();
        left_detail.queue = BeadsIssueQueue::InProgress;
        panels.update(left, "scribe-5wh1.4", Some(Box::new(left_detail)));
        assert_eq!(panels.visible(left).map(|panel| panel.lane), Some(2));
        assert_eq!(panels.visible(right).map(|panel| panel.lane), Some(4));

        assert!(panels.sync_board(right, &board_with(right_item, 1)));
        assert_eq!(panels.visible(left).map(|panel| panel.lane), Some(2));
        assert_eq!(panels.visible(right).map(|panel| panel.lane), Some(1));
        assert!(panels.dismiss(left));
        assert!(panels.visible(right).is_some());
    }

    #[test]
    fn vanished_issue_and_not_detected_workspace_close_with_a_notice() {
        let vanished = WorkspaceId::new();
        let missing_project = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.open(vanished, item(), 0);
        panels.open(missing_project, item(), 1);

        panels.update(vanished, "scribe-5wh1.4", None);
        assert!(panels.visible(vanished).is_none());
        assert_eq!(panels.notice(vanished), Some("Issue scribe-5wh1.4 no longer exists"));

        assert!(panels.sync_board(missing_project, &BeadsBoardState::NotDetected));
        assert!(panels.visible(missing_project).is_none());
        assert_eq!(panels.notice(missing_project), Some("Beads project is no longer detected"));
    }

    #[test]
    fn detail_reply_only_fills_the_panel_that_requested_it() {
        let workspace = WorkspaceId::new();
        let other = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.open(workspace, item(), 1);

        assert_eq!(panels.take_request(), Some((workspace, "scribe-5wh1.4".into())));
        panels.update(other, "scribe-5wh1.4", Some(Box::new(detail())));
        assert!(panels.visible(workspace).is_some_and(|panel| panel.detail.is_none()));
        panels.update(workspace, "wrong", Some(Box::new(detail())));
        assert!(panels.visible(workspace).is_some_and(|panel| panel.detail.is_none()));
        panels.update(workspace, "scribe-5wh1.4", Some(Box::new(detail())));
        assert!(panels.visible(workspace).is_some_and(|panel| panel.detail.is_some()));
    }

    #[test]
    fn issue_id_copy_is_exactly_once_through_the_parked_surface() {
        let workspace = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.open(workspace, item(), 1);
        panels.update(workspace, "scribe-5wh1.4", Some(Box::new(detail())));

        assert!(panels.copy_issue_id(workspace));
        assert_eq!(panels.take_copy().as_deref(), Some("scribe-5wh1.4"));
        assert_eq!(panels.take_copy(), None, "one click must yield one clipboard write");
    }

    #[test]
    fn dependent_navigation_waits_for_its_matching_detail_reply() {
        let workspace = WorkspaceId::new();
        let other = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.open(workspace, item(), 1);
        assert_eq!(panels.take_request(), Some((workspace, "scribe-5wh1.4".into())));
        panels.update(workspace, "scribe-5wh1.4", Some(Box::new(full_detail())));

        assert!(panels.navigate_to_dependent(workspace, "next-1"));
        assert_eq!(panels.take_request(), Some((workspace, "next-1".into())));
        assert_eq!(
            panels.visible(workspace).map(|panel| panel.card.id.as_str()),
            Some("scribe-5wh1.4")
        );

        let mut next = detail();
        next.id = "next-1".into();
        next.title = "Dependent work".into();
        next.queue = BeadsIssueQueue::Blocked;
        next.queue_basis = BeadsIssueQueueBasis::OpenBlockers;
        panels.update(other, "next-1", Some(Box::new(next.clone())));
        panels.update(workspace, "wrong-id", Some(Box::new(next.clone())));
        assert_eq!(
            panels.visible(workspace).map(|panel| panel.card.id.as_str()),
            Some("scribe-5wh1.4")
        );

        panels.update(workspace, "next-1", Some(Box::new(next)));
        let panel = panels.visible(workspace).expect("matching reply swaps the panel");
        assert_eq!(panel.card.id, "next-1");
        assert_eq!(panel.card.title, "Dependent work");
        assert_eq!(panel.lane, 3);
        assert_eq!(panel.detail.as_deref().map(|detail| detail.id.as_str()), Some("next-1"));
    }

    #[test]
    fn missing_dependent_closes_the_panel_with_the_lifecycle_notice() {
        let workspace = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.open(workspace, item(), 1);
        panels.update(workspace, "scribe-5wh1.4", Some(Box::new(full_detail())));

        assert!(panels.navigate_to_dependent(workspace, "next-1"));
        panels.update(workspace, "next-1", None);

        assert!(panels.visible(workspace).is_none());
        assert_eq!(panels.notice(workspace), Some("Issue next-1 no longer exists"));
    }

    #[test]
    fn missing_detail_capability_leaves_the_board_without_a_panel() {
        let workspace = WorkspaceId::new();
        let mut panels = BeadsPanels::default();

        panels.set_enabled(false);
        panels.open(workspace, item(), 1);

        assert!(panels.visible(workspace).is_none());
        assert_eq!(panels.take_request(), None);
    }

    #[test]
    fn newest_comment_folds_to_two_lines_and_older_comments_to_one() {
        assert_eq!(comment_line_limit(0, false), Some(2));
        assert_eq!(comment_line_limit(1, false), Some(1));
        assert_eq!(comment_line_limit(8, true), None);
    }

    #[test]
    fn full_detail_build_contains_every_panel_anatomy_section() {
        let presentation = PanelPresentation::from_detail(&full_detail());

        for section in [
            PanelSection::Head,
            PanelSection::Identity,
            PanelSection::Queue,
            PanelSection::DependencyThread,
            PanelSection::Blockers,
            PanelSection::Epic,
            PanelSection::Labels,
            PanelSection::Owner,
            PanelSection::Spec,
            PanelSection::Design,
            PanelSection::Description,
            PanelSection::Acceptance,
            PanelSection::Notes,
            PanelSection::Facts,
            PanelSection::Comments,
            PanelSection::HiddenCount,
            PanelSection::Dependents,
            PanelSection::StatusRail,
        ] {
            assert!(presentation.has(section), "missing {section:?}");
        }
    }

    #[test]
    fn empty_detail_build_omits_every_sparse_section() {
        let mut empty = detail();
        empty.description.clear();
        empty.acceptance_criteria.clear();
        empty.notes.clear();
        empty.design.clear();
        empty.spec_id = None;
        empty.labels.clear();
        empty.owner = None;
        empty.parent_epic_name = None;
        let presentation = PanelPresentation::from_detail(&empty);

        for section in [
            PanelSection::Blockers,
            PanelSection::Epic,
            PanelSection::Labels,
            PanelSection::Owner,
            PanelSection::Spec,
            PanelSection::Design,
            PanelSection::Description,
            PanelSection::Acceptance,
            PanelSection::Notes,
            PanelSection::Facts,
            PanelSection::Comments,
            PanelSection::HiddenCount,
            PanelSection::Dependents,
        ] {
            assert!(!presentation.has(section), "unexpected {section:?}");
        }
        assert!(presentation.has(PanelSection::Head));
        assert!(presentation.has(PanelSection::Identity));
        assert!(presentation.has(PanelSection::Queue));
        assert!(presentation.has(PanelSection::StatusRail));
    }

    #[test]
    fn closed_detail_build_keeps_closed_facts_and_removes_verbs() {
        let mut closed = detail();
        closed.status = "closed".into();
        closed.queue = BeadsIssueQueue::Done;
        closed.queue_basis = BeadsIssueQueueBasis::ClosedStatus;
        closed.closed_at = Some("2026-08-15T05:00:00Z".into());
        closed.close_reason = Some("Delivered".into());
        let presentation = PanelPresentation::from_detail(&closed);

        assert_eq!(presentation.queue(), BeadsIssueQueue::Done);
        assert_eq!(presentation.queue_basis(), BeadsIssueQueueBasis::ClosedStatus);
        assert!(presentation.has(PanelSection::Facts));
        assert!(presentation.verbs().is_empty());
    }

    #[test]
    fn blocked_detail_build_counts_every_upstream_node() {
        let presentation = PanelPresentation::from_detail(&full_detail());

        assert!(presentation.has(PanelSection::Blockers));
        assert_eq!(presentation.blocker_count(), 2);
    }

    #[test]
    fn hidden_comment_build_carries_the_omitted_count_line() {
        let presentation = PanelPresentation::from_detail(&full_detail());

        assert!(presentation.has(PanelSection::Comments));
        assert!(presentation.has(PanelSection::HiddenCount));
        assert_eq!(presentation.hidden_comment_count(), Some(7));
    }

    #[test]
    fn viewer_build_exposes_only_inert_read_only_verbs() {
        let presentation = PanelPresentation::from_detail(&detail());

        assert_eq!(presentation.verbs(), &[ReadOnlyVerb::Claim, ReadOnlyVerb::CloseIssue]);
    }

    #[test]
    fn panel_text_clears_the_board_palettes_body_contrast_floor() {
        let ground = [0.06, 0.08, 0.07, 1.0];
        let dim = [0.22, 0.24, 0.23, 1.0];
        let mut ansi = [[0.24, 0.24, 0.24, 1.0]; 16];
        ansi[9] = [0.35, 0.12, 0.12, 1.0];
        ansi[10] = [0.12, 0.3, 0.15, 1.0];
        ansi[11] = [0.5, 0.42, 0.15, 1.0];
        ansi[12] = [0.15, 0.15, 0.4, 1.0];
        ansi[13] = [0.3, 0.15, 0.3, 1.0];
        ansi[14] = [0.1, 0.3, 0.3, 1.0];
        let chrome = ChromeColors {
            tab_bar_bg: ground,
            tab_text: dim,
            tab_text_active: [0.4, 0.42, 0.41, 1.0],
            ..chrome_slots(ground)
        };
        let colors = BeadsBoardColors::from_theme(&chrome, &ansi, 1.0);
        let queue_inks = [
            BeadsIssueQueue::Backlog,
            BeadsIssueQueue::Ready,
            BeadsIssueQueue::InProgress,
            BeadsIssueQueue::Blocked,
            BeadsIssueQueue::Done,
        ]
        .map(|queue| panel_queue_ink(&colors, queue));

        for color in [colors.title, colors.queue_name, colors.muted, colors.epic]
            .into_iter()
            .chain(colors.priorities)
            .chain(queue_inks)
        {
            let ratio = contrast(color, colors.card);
            assert!(ratio >= BODY_CONTRAST - 0.01, "panel text reads at {ratio:.2}:1");
        }
    }
}
