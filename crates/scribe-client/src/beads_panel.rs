//! Beads issue panel state, inline editing, guarded writes, and rendering.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui::{
    AccessibleAction, Animation, AnimationExt as _, AnyElement, App, Bounds, Context, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, FontWeight, KeyDownEvent,
    MouseButton, Pixels, Point, Rgba, Role, SharedString, StyledText, Subscription, TextLayout,
    UTF16Selection, Window, canvas, div, linear_color_stop, linear_gradient, prelude::*, px,
};
use scribe_common::ids::WorkspaceId;
use scribe_common::protocol::{
    BeadsBoardItem, BeadsBoardSnapshot, BeadsBoardState, BeadsIssueComment, BeadsIssueDetail,
    BeadsIssueQueue, BeadsIssueQueueBasis, BeadsIssueWrite, BeadsIssueWriteGuards,
    BeadsIssueWriteResult,
};

use crate::animation::AnimationSettings;
use crate::beads_board::{BeadsBoardColors, CardDragState};
use crate::layout::Rect;
use crate::settings::window::{utf8_range_to_utf16, utf16_range_to_utf8};
use unicode_segmentation::UnicodeSegmentation;

const PANEL_WIDTH: f32 = 560.0;
const PANEL_MIN_WIDTH: f32 = 400.0;
const PANEL_MARGIN: f32 = 12.0;
const PANEL_BOARD_GAP: f32 = 4.0;
const PANEL_OPEN_DURATION: Duration = Duration::from_millis(120);
const NOTICE_DURATION: Duration = Duration::from_secs(5);
const BD_ISSUE_TYPES: [&str; 12] = [
    "bug",
    "feature",
    "task",
    "epic",
    "chore",
    "decision",
    "message",
    "molecule",
    "gate",
    "spike",
    "story",
    "milestone",
];
const WRITE_DEADLINE: Duration = Duration::from_secs(15);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelVerb {
    Claim,
    CloseIssue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelPickRow {
    Priority,
    IssueType,
}

const OPEN_VERBS: [PanelVerb; 2] = [PanelVerb::Claim, PanelVerb::CloseIssue];

/// Data-derived panel shape consumed by the renderer and its build tests.
#[derive(Debug, Clone)]
struct PanelPresentation {
    sections: Vec<PanelSection>,
    blocker_count: usize,
    hidden_comment_count: Option<u32>,
    queue: BeadsIssueQueue,
    queue_basis: BeadsIssueQueueBasis,
    verbs: &'static [PanelVerb],
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

    fn verbs(&self) -> &'static [PanelVerb] {
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

/// Place a panel below its board, centered inside its workspace region.
pub fn panel_geometry(region: Rect, board: Rect, _lane: u8) -> Option<PanelGeometry> {
    let width = PANEL_WIDTH.min(region.width - PANEL_MARGIN * 2.0);
    if width < PANEL_MIN_WIDTH {
        return None;
    }
    let min_x = region.x + PANEL_MARGIN;
    let max_x = region.x + region.width - PANEL_MARGIN - width;
    let y = board.y + board.height + PANEL_BOARD_GAP;
    let max_height =
        (region.height * 0.7).min((region.y + region.height - y - PANEL_MARGIN).max(0.0));
    (max_height > 0.0).then_some(PanelGeometry {
        x: (region.x + (region.width - width) / 2.0).clamp(min_x, max_x),
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
    undo: Option<UndoClose>,
}

#[derive(Debug, Clone)]
struct UndoClose {
    issue_id: String,
    assignee: String,
}

impl PanelNotice {
    fn new(text: String, lane: u8) -> Self {
        Self::new_at(text, lane, None, Instant::now())
    }

    fn closed_at(issue_id: String, assignee: String, lane: u8, now: Instant) -> Self {
        Self::new_at(
            format!("closed {issue_id} · undo"),
            lane,
            Some(UndoClose { issue_id, assignee }),
            now,
        )
    }

    fn new_at(text: String, lane: u8, undo: Option<UndoClose>, now: Instant) -> Self {
        Self { text, lane, expires_at: now + NOTICE_DURATION, undo }
    }

    fn active_at(&self, now: Instant) -> bool {
        now < self.expires_at
    }

    fn active(&self) -> bool {
        self.active_at(Instant::now())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EditField {
    Title,
    Description,
    Acceptance,
    Notes,
    Design,
    SpecId,
    Labels,
    Comment,
}

impl EditField {
    fn id(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Description => "description",
            Self::Acceptance => "acceptance",
            Self::Notes => "notes",
            Self::Design => "design",
            Self::SpecId => "spec-id",
            Self::Labels => "labels",
            Self::Comment => "comment",
        }
    }

    fn multiline(self) -> bool {
        matches!(
            self,
            Self::Description | Self::Acceptance | Self::Notes | Self::Design | Self::Comment
        )
    }

    fn verb(self, value: String) -> BeadsIssueWrite {
        match self {
            Self::Title => BeadsIssueWrite::SetTitle { title: value },
            Self::Description => BeadsIssueWrite::SetDescription { description: value },
            Self::Acceptance => BeadsIssueWrite::SetAcceptance { acceptance: value },
            Self::Notes => BeadsIssueWrite::SetNotes { notes: value },
            Self::Design => BeadsIssueWrite::SetDesign { design: value },
            Self::SpecId => {
                BeadsIssueWrite::SetSpecId { spec_id: (!value.is_empty()).then_some(value) }
            }
            Self::Labels => BeadsIssueWrite::SetLabels { labels: parse_labels(&value) },
            Self::Comment => BeadsIssueWrite::AddComment { body: value },
        }
    }

    fn changed(self, original: &str, input: &str) -> bool {
        input != original && (self != Self::Comment || !input.trim().is_empty())
    }
}

fn parse_labels(value: &str) -> Vec<String> {
    value
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|label| !label.is_empty())
        .fold(Vec::new(), |mut labels, label| {
            if !labels.iter().any(|existing| existing == label) {
                labels.push(label.to_owned());
            }
            labels
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKeyAction {
    Commit,
    Cancel,
    SelectAll,
    Text,
    Consume,
}

fn edit_key_action(field: EditField, event: &KeyDownEvent) -> EditKeyAction {
    if event.keystroke.key == "a"
        && (event.keystroke.modifiers.control || event.keystroke.modifiers.platform)
    {
        return EditKeyAction::SelectAll;
    }
    match event.keystroke.key.as_str() {
        "escape" => EditKeyAction::Cancel,
        "enter" if !field.multiline() || event.keystroke.modifiers.modified() => {
            EditKeyAction::Commit
        }
        "backspace" | "delete" | "tab" | "up" | "down" | "left" | "right" => EditKeyAction::Consume,
        _ => EditKeyAction::Text,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeadsEditIntent {
    pub workspace_id: WorkspaceId,
    pub issue_id: String,
    pub verb: BeadsIssueWrite,
}

#[derive(Debug, Clone)]
struct ActiveEdit {
    workspace_id: WorkspaceId,
    issue_id: String,
    field: EditField,
    original: String,
    input: String,
    selection: Range<usize>,
    selection_reversed: bool,
    marked: Option<Range<usize>>,
    selecting: bool,
}

#[derive(Clone, Copy)]
struct EditTarget<'a> {
    workspace_id: WorkspaceId,
    issue_id: &'a str,
    field: EditField,
    value: &'a str,
}

struct BeginEdit {
    cursor: Option<usize>,
    layout: Option<TextLayout>,
    extend_selection: bool,
}

#[derive(Debug, Default)]
struct EditSession {
    active: Option<ActiveEdit>,
}

impl EditSession {
    fn begin(
        &mut self,
        workspace_id: WorkspaceId,
        issue_id: &str,
        field: EditField,
        value: &str,
    ) -> Option<BeadsEditIntent> {
        if self.active.as_ref().is_some_and(|active| {
            active.workspace_id == workspace_id
                && active.issue_id == issue_id
                && active.field == field
        }) {
            return None;
        }
        let pending = self.finish();
        self.active = Some(ActiveEdit {
            workspace_id,
            issue_id: issue_id.to_owned(),
            field,
            original: value.to_owned(),
            input: value.to_owned(),
            selection: value.len()..value.len(),
            selection_reversed: false,
            marked: None,
            selecting: false,
        });
        pending
    }

    fn finish(&mut self) -> Option<BeadsEditIntent> {
        let active = self.active.take()?;
        active.field.changed(&active.original, &active.input).then(|| BeadsEditIntent {
            workspace_id: active.workspace_id,
            issue_id: active.issue_id,
            verb: active.field.verb(active.input),
        })
    }

    fn cancel(&mut self) {
        self.active = None;
    }

    #[cfg(test)]
    fn replace_all(&mut self, value: &str) {
        if let Some(active) = self.active.as_mut() {
            value.clone_into(&mut active.input);
            active.selection = active.input.len()..active.input.len();
            active.selection_reversed = false;
            active.marked = None;
        }
    }

    fn input(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.input.as_str())
    }

    fn backspace(&mut self) {
        let Some(active) = self.active.as_mut() else { return };
        if active.selection.is_empty() {
            let cursor = active_cursor(active);
            let previous = previous_grapheme_boundary(&active.input, cursor);
            active.selection = previous..cursor;
        }
        delete_selection(active);
    }

    fn delete(&mut self) {
        let Some(active) = self.active.as_mut() else { return };
        if active.selection.is_empty() {
            let cursor = active_cursor(active);
            let next = next_grapheme_boundary(&active.input, cursor);
            active.selection = cursor..next;
        }
        delete_selection(active);
    }

    fn move_left(&mut self, extend: bool) {
        let Some(active) = self.active.as_mut() else { return };
        let cursor = active_cursor(active);
        let target = if active.selection.is_empty() || extend {
            previous_grapheme_boundary(&active.input, cursor)
        } else {
            active.selection.start
        };
        if extend {
            select_to(active, target);
        } else {
            move_to(active, target);
        }
    }

    fn move_right(&mut self, extend: bool) {
        let Some(active) = self.active.as_mut() else { return };
        let cursor = active_cursor(active);
        let target = if active.selection.is_empty() || extend {
            next_grapheme_boundary(&active.input, cursor)
        } else {
            active.selection.end
        };
        if extend {
            select_to(active, target);
        } else {
            move_to(active, target);
        }
    }

    fn select_all(&mut self) {
        let Some(active) = self.active.as_mut() else { return };
        active.selection = 0..active.input.len();
        active.selection_reversed = false;
        active.marked = None;
    }

    fn move_to(&mut self, offset: usize) {
        if let Some(active) = self.active.as_mut() {
            move_to(active, offset);
        }
    }

    fn select_to(&mut self, offset: usize) {
        if let Some(active) = self.active.as_mut() {
            select_to(active, offset);
        }
    }

    fn is_active(&self, target: EditTarget<'_>) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.workspace_id == target.workspace_id
                && active.issue_id == target.issue_id
                && active.field == target.field
        })
    }

    fn set_selecting(&mut self, selecting: bool) {
        if let Some(active) = self.active.as_mut() {
            active.selecting = selecting;
        }
    }

    fn is_selecting(&self, target: EditTarget<'_>) -> bool {
        self.is_active(target) && self.active.as_ref().is_some_and(|active| active.selecting)
    }
}

fn active_cursor(active: &ActiveEdit) -> usize {
    if active.selection_reversed { active.selection.start } else { active.selection.end }
}

fn move_to(active: &mut ActiveEdit, offset: usize) {
    let offset = nearest_grapheme_boundary(&active.input, offset);
    active.selection = offset..offset;
    active.selection_reversed = false;
    active.marked = None;
}

fn select_to(active: &mut ActiveEdit, offset: usize) {
    let offset = nearest_grapheme_boundary(&active.input, offset);
    if active.selection_reversed {
        active.selection.start = offset;
    } else {
        active.selection.end = offset;
    }
    if active.selection.end < active.selection.start {
        active.selection_reversed = !active.selection_reversed;
        active.selection = active.selection.end..active.selection.start;
    }
    active.marked = None;
}

fn delete_selection(active: &mut ActiveEdit) {
    let range = grapheme_range(&active.input, active.selection.clone());
    active.input.replace_range(range.clone(), "");
    active.selection = range.start..range.start;
    active.selection_reversed = false;
    active.marked = None;
}

fn previous_grapheme_boundary(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .rev()
        .find_map(|(index, _)| (index < offset).then_some(index))
        .unwrap_or(0)
}

fn next_grapheme_boundary(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .find_map(|(index, _)| (index > offset).then_some(index))
        .unwrap_or(text.len())
}

fn nearest_grapheme_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let mut previous = 0;
    for (next, _) in text.grapheme_indices(true).skip(1) {
        if next >= offset {
            return if offset - previous <= next - offset { previous } else { next };
        }
        previous = next;
    }
    text.len()
}

fn grapheme_range(text: &str, range: Range<usize>) -> Range<usize> {
    if range.is_empty() {
        let caret = nearest_grapheme_boundary(text, range.start);
        return caret..caret;
    }
    let start = previous_grapheme_boundary(text, range.start.saturating_add(1));
    let end = next_grapheme_boundary(text, range.end.saturating_sub(1));
    start..end
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeadsEditorKeyRoute {
    Inactive,
    Text,
    Consumed,
    Finished,
}

/// One native text-input owner per terminal window.
pub struct BeadsEditor {
    focus: FocusHandle,
    session: EditSession,
    layout: Option<TextLayout>,
    panels: Arc<Mutex<BeadsPanels>>,
    _blur: Subscription,
}

impl BeadsEditor {
    pub fn new(
        panels: Arc<Mutex<BeadsPanels>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, |editor, _window, cx| editor.commit(cx));
        Self { focus, session: EditSession::default(), layout: None, panels, _blur: blur }
    }

    pub fn has_keyboard_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }

    fn begin(
        &mut self,
        target: EditTarget<'_>,
        activation: BeginEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_active = self.session.is_active(target);
        let selecting = activation.layout.is_some();
        if let Some(intent) =
            self.session.begin(target.workspace_id, target.issue_id, target.field, target.value)
        {
            self.queue(intent);
        }
        let cursor = activation.cursor.unwrap_or_else(|| self.session.input().map_or(0, str::len));
        if activation.extend_selection && was_active {
            self.session.select_to(cursor);
        } else {
            self.session.move_to(cursor);
        }
        self.session.set_selecting(selecting);
        if let Some(layout) = activation.layout {
            self.layout = Some(layout);
        } else if !was_active {
            self.layout = None;
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn set_value(
        &mut self,
        target: EditTarget<'_>,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(intent) =
            self.session.begin(target.workspace_id, target.issue_id, target.field, target.value)
        {
            self.queue(intent);
        }
        if let Some(active) = self.session.active.as_mut() {
            active.input = value;
            active.selection = active.input.len()..active.input.len();
            active.selection_reversed = false;
            active.marked = None;
            active.selecting = false;
        }
        self.layout = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn extend_pointer_selection(
        &mut self,
        target: EditTarget<'_>,
        cursor: usize,
        cx: &mut Context<Self>,
    ) {
        if self.session.is_selecting(target) {
            self.session.select_to(cursor);
            cx.notify();
        }
    }

    fn end_pointer_selection(&mut self, target: EditTarget<'_>) {
        if self.session.is_active(target) {
            self.session.set_selecting(false);
        }
    }

    pub fn route_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> BeadsEditorKeyRoute {
        let Some(field) = self.session.active.as_ref().map(|active| active.field) else {
            return BeadsEditorKeyRoute::Inactive;
        };
        match edit_key_action(field, event) {
            EditKeyAction::Commit => {
                self.commit(cx);
                BeadsEditorKeyRoute::Finished
            }
            EditKeyAction::Cancel => {
                self.session.cancel();
                self.layout = None;
                cx.notify();
                BeadsEditorKeyRoute::Finished
            }
            EditKeyAction::SelectAll => {
                self.session.select_all();
                cx.notify();
                BeadsEditorKeyRoute::Consumed
            }
            EditKeyAction::Text => BeadsEditorKeyRoute::Text,
            EditKeyAction::Consume => {
                match event.keystroke.key.as_str() {
                    "backspace" => self.session.backspace(),
                    "delete" => self.session.delete(),
                    "left" => self.session.move_left(event.keystroke.modifiers.shift),
                    "right" => self.session.move_right(event.keystroke.modifiers.shift),
                    _ => return BeadsEditorKeyRoute::Consumed,
                }
                cx.notify();
                BeadsEditorKeyRoute::Consumed
            }
        }
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.session.active.is_some() {
            self.session.cancel();
            self.layout = None;
            cx.notify();
        }
    }

    pub fn commit(&mut self, cx: &mut Context<Self>) {
        if let Some(intent) = self.session.finish() {
            self.queue(intent);
        }
        self.layout = None;
        cx.notify();
    }

    fn queue(&self, intent: BeadsEditIntent) {
        if let Ok(mut panels) = self.panels.lock() {
            panels.queue_edit(intent);
        }
    }

    fn active_text(
        &self,
        workspace_id: WorkspaceId,
        issue_id: &str,
        field: EditField,
    ) -> Option<&str> {
        self.session.active.as_ref().and_then(|active| {
            (active.workspace_id == workspace_id
                && active.issue_id == issue_id
                && active.field == field)
                .then_some(active.input.as_str())
        })
    }
}

impl EntityInputHandler for BeadsEditor {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let input = self.session.input()?;
        let range = grapheme_range(input, utf16_range_to_utf8(input, range));
        actual_range.replace(utf8_range_to_utf16(input, &range));
        Some(input[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let active = self.session.active.as_ref()?;
        Some(UTF16Selection {
            range: utf8_range_to_utf16(&active.input, &active.selection),
            reversed: active.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let active = self.session.active.as_ref()?;
        active.marked.as_ref().map(|range| utf8_range_to_utf16(&active.input, range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.active.as_mut().is_some_and(|active| active.marked.take().is_some()) {
            cx.notify();
            window.refresh();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.session.active.as_mut() else { return };
        let range = range
            .map(|range| utf16_range_to_utf8(&active.input, range))
            .or_else(|| active.marked.take())
            .unwrap_or_else(|| active.selection.clone());
        let cursor = range.start + text.len();
        active.input.replace_range(range, text);
        active.selection = cursor..cursor;
        active.selection_reversed = false;
        active.marked = None;
        cx.notify();
        window.refresh();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.session.active.as_mut() else { return };
        let range = range
            .map(|range| utf16_range_to_utf8(&active.input, range))
            .or_else(|| active.marked.take())
            .unwrap_or_else(|| active.selection.clone());
        let start = range.start;
        active.input.replace_range(range, text);
        active.marked = (!text.is_empty()).then_some(start..start + text.len());
        let selected = new_selected_range.map_or_else(
            || start + text.len()..start + text.len(),
            |selected_range| {
                let selected_range = utf16_range_to_utf8(text, selected_range);
                start + selected_range.start..start + selected_range.end
            },
        );
        active.selection = selected;
        active.selection_reversed = false;
        cx.notify();
        window.refresh();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let input = self.session.input()?;
        let layout = self.layout.as_ref()?;
        let range = utf16_range_to_utf8(input, range);
        let start = layout.position_for_index(range.start)?;
        let end = layout.position_for_index(range.end)?;
        let width =
            if start.y == end.y { (end.x - start.x).max(Pixels::ZERO) } else { Pixels::ZERO };
        Some(Bounds::new(start, gpui::size(width, layout.line_height())))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let input = self.session.input()?;
        let layout = self.layout.as_ref()?;
        let index = layout.index_for_position(point).unwrap_or_else(|index| index);
        let index = nearest_grapheme_boundary(input, index);
        Some(utf8_range_to_utf16(input, &(index..index)).start)
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.session.input()?.encode_utf16().count())
    }
}

/// One guarded issue mutation waiting for the owning view's IPC sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelWriteIntent {
    pub workspace_id: WorkspaceId,
    pub issue_id: String,
    pub verb: BeadsIssueWrite,
    pub guards: BeadsIssueWriteGuards,
}

/// Per-workspace panel state plus intents parked for the owning GPUI view.
#[derive(Debug, Default)]
pub struct BeadsPanels {
    detail_enabled: bool,
    write_enabled: bool,
    open: HashMap<WorkspaceId, BeadsPanel>,
    pending_requests: VecDeque<(WorkspaceId, String)>,
    pending_navigation: HashMap<WorkspaceId, String>,
    pending_writes: VecDeque<PanelWriteIntent>,
    in_flight_writes: HashMap<(WorkspaceId, String), PanelWriteIntent>,
    pick_rows: HashMap<WorkspaceId, PanelPickRow>,
    write_deadlines: HashMap<(WorkspaceId, String), Instant>,
    pending_board_refreshes: HashSet<WorkspaceId>,
    reconcile_on_snapshot: HashSet<WorkspaceId>,
    expanded_comments: HashSet<(WorkspaceId, String, usize)>,
    pending_copy: Option<String>,
    notices: HashMap<WorkspaceId, PanelNotice>,
    last_opened: Option<WorkspaceId>,
}

impl BeadsPanels {
    pub fn set_enabled(&mut self, enabled: bool) {
        self.detail_enabled = enabled;
        if !enabled {
            self.write_enabled = false;
            self.open.clear();
            self.pending_requests.clear();
            self.pending_navigation.clear();
            self.pending_writes.clear();
            self.in_flight_writes.clear();
            self.pick_rows.clear();
            self.write_deadlines.clear();
            self.pending_board_refreshes.clear();
            self.reconcile_on_snapshot.clear();
            self.expanded_comments.clear();
            self.pending_copy = None;
            self.notices.clear();
            self.last_opened = None;
        }
    }

    pub fn set_write_enabled(&mut self, enabled: bool) {
        self.write_enabled = self.detail_enabled && enabled;
        if !self.write_enabled {
            self.pending_writes.clear();
            self.pick_rows.clear();
        }
    }

    pub fn write_enabled(&self) -> bool {
        self.write_enabled
    }

    pub fn open(&mut self, workspace_id: WorkspaceId, card: BeadsBoardItem, lane: u8) {
        if !self.detail_enabled {
            return;
        }
        let issue_id = card.id.clone();
        self.notices.remove(&workspace_id);
        self.pick_rows.remove(&workspace_id);
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
            self.pick_rows.remove(&workspace_id);
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
        self.pick_rows.remove(&workspace_id);
        let Some(panel) = self.open.get_mut(&workspace_id) else { return };
        panel.lane = queue_lane(detail.queue);
        panel.detail = Some(detail);
    }

    fn close_missing(&mut self, workspace_id: WorkspaceId, issue_id: &str) {
        let Some(panel) = self.open.remove(&workspace_id) else { return };
        self.pick_rows.remove(&workspace_id);
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
                    .filter(|(workspace_id, notice)| {
                        !self.open.contains_key(workspace_id) && notice.active()
                    })
                    .map(|(workspace_id, _)| *workspace_id),
            )
            .collect()
    }

    pub fn take_request(&mut self) -> Option<(WorkspaceId, String)> {
        self.pending_requests.pop_front()
    }

    fn queue_edit(&mut self, intent: BeadsEditIntent) -> bool {
        if self.open.get(&intent.workspace_id).is_none_or(|panel| panel.card.id != intent.issue_id)
        {
            return false;
        }
        self.queue_write(intent.workspace_id, intent.verb)
    }

    pub fn write_status(&mut self, workspace_id: WorkspaceId, status: &str) -> bool {
        if !matches!(status, "open" | "in_progress" | "closed") {
            return false;
        }
        self.queue_write(
            workspace_id,
            BeadsIssueWrite::SetStatus { status: status.into(), clear_defer: false },
        )
    }

    pub fn claim(&mut self, workspace_id: WorkspaceId) -> bool {
        self.queue_write(workspace_id, BeadsIssueWrite::Claim)
    }

    pub fn close_issue(&mut self, workspace_id: WorkspaceId) -> bool {
        self.queue_write(workspace_id, BeadsIssueWrite::CloseIssue)
    }

    fn pick_row(&self, workspace_id: WorkspaceId) -> Option<PanelPickRow> {
        self.pick_rows.get(&workspace_id).copied()
    }

    fn toggle_pick_row(&mut self, workspace_id: WorkspaceId, row: PanelPickRow) -> bool {
        if !self.can_write(workspace_id) {
            return false;
        }
        if self.pick_rows.get(&workspace_id) == Some(&row) {
            self.pick_rows.remove(&workspace_id);
        } else {
            self.pick_rows.insert(workspace_id, row);
        }
        true
    }

    fn set_priority(&mut self, workspace_id: WorkspaceId, priority: u8) -> bool {
        if priority > 4
            || !self.queue_write(workspace_id, BeadsIssueWrite::SetPriority { priority })
        {
            return false;
        }
        self.pick_rows.remove(&workspace_id);
        true
    }

    fn set_issue_type(&mut self, workspace_id: WorkspaceId, issue_type: &str) -> bool {
        if !BD_ISSUE_TYPES.contains(&issue_type)
            || !self.queue_write(
                workspace_id,
                BeadsIssueWrite::SetType { issue_type: issue_type.to_owned() },
            )
        {
            return false;
        }
        self.pick_rows.remove(&workspace_id);
        true
    }

    pub fn can_write(&self, workspace_id: WorkspaceId) -> bool {
        self.write_enabled
            && self.open.get(&workspace_id).is_some_and(|panel| {
                panel.detail.as_deref().is_some_and(|detail| detail.status != "closed")
            })
    }

    fn queue_write(&mut self, workspace_id: WorkspaceId, verb: BeadsIssueWrite) -> bool {
        let Some(panel) = self.open.get(&workspace_id) else { return false };
        let Some(detail) = panel.detail.as_deref() else { return false };
        if !self.write_enabled || detail.status == "closed" {
            return false;
        }
        let intent = PanelWriteIntent {
            workspace_id,
            issue_id: panel.card.id.clone(),
            verb,
            guards: BeadsIssueWriteGuards {
                if_status: Some(detail.status.clone()),
                if_assignee: Some(detail.assignee.clone().unwrap_or_default()),
            },
        };
        self.park_write(intent)
    }

    /// Translate one completed board gesture into the existing guarded write
    /// queue. Derived lanes and same-lane drops never enter the queue.
    pub fn queue_card_drop(&mut self, drag: &CardDragState) -> bool {
        if !self.write_enabled || drag.source_lane > 2 {
            return false;
        }
        let Some(target_lane) = drag.hovered_lane else { return false };
        let verb = match target_lane {
            1 if drag.source_lane != 1 => {
                BeadsIssueWrite::SetStatus { status: "open".into(), clear_defer: true }
            }
            2 if drag.source_lane != 2 => BeadsIssueWrite::Claim,
            4 => BeadsIssueWrite::CloseIssue,
            _ => return false,
        };
        let detail = self.open.get(&drag.workspace_id).and_then(|panel| {
            (panel.card.id == drag.source.id).then_some(panel.detail.as_deref()).flatten()
        });
        if detail.is_some_and(|detail| detail.status == "closed") {
            return false;
        }
        let guards = detail.map_or_else(
            || BeadsIssueWriteGuards {
                if_status: match drag.source_lane {
                    1 => Some("open".into()),
                    2 => Some("in_progress".into()),
                    _ => None,
                },
                if_assignee: None,
            },
            |detail| BeadsIssueWriteGuards {
                if_status: Some(detail.status.clone()),
                if_assignee: Some(detail.assignee.clone().unwrap_or_default()),
            },
        );
        self.park_write(PanelWriteIntent {
            workspace_id: drag.workspace_id,
            issue_id: drag.source.id.clone(),
            verb,
            guards,
        })
    }

    fn park_write(&mut self, intent: PanelWriteIntent) -> bool {
        let key = (intent.workspace_id, intent.issue_id.clone());
        if self.reconcile_on_snapshot.contains(&intent.workspace_id)
            || self.in_flight_writes.contains_key(&key)
            || self.pending_writes.iter().any(|write| {
                write.workspace_id == intent.workspace_id && write.issue_id == intent.issue_id
            })
        {
            return false;
        }
        self.pending_writes.push_back(intent);
        true
    }

    pub fn take_write(&mut self) -> Option<PanelWriteIntent> {
        self.take_write_at(Instant::now())
    }

    fn take_write_at(&mut self, now: Instant) -> Option<PanelWriteIntent> {
        let intent = self.pending_writes.pop_front()?;
        let key = (intent.workspace_id, intent.issue_id.clone());
        self.in_flight_writes.insert(key.clone(), intent.clone());
        self.write_deadlines.insert(key, now + WRITE_DEADLINE);
        Some(intent)
    }

    pub fn write_send_failed(&mut self, workspace_id: WorkspaceId, issue_id: &str, reason: &str) {
        let key = (workspace_id, issue_id.to_owned());
        self.in_flight_writes.remove(&key);
        self.write_deadlines.remove(&key);
        let lane = self.open.get(&workspace_id).map_or(4, |panel| panel.lane);
        self.notices
            .insert(workspace_id, PanelNotice::new(format!("Issue write dropped: {reason}"), lane));
    }

    pub fn classifier_won(&mut self, workspace_id: WorkspaceId, issue_id: &str, lane: u8) {
        let lane_name = ["Backlog", "Ready", "In progress", "Blocked", "Done"]
            .get(usize::from(lane))
            .copied()
            .unwrap_or("board");
        self.notices.insert(
            workspace_id,
            PanelNotice::new(format!("{issue_id} stayed {lane_name}; classifier won"), lane),
        );
        self.last_opened = Some(workspace_id);
    }

    pub fn finish_write(
        &mut self,
        workspace_id: WorkspaceId,
        issue_id: &str,
        result: BeadsIssueWriteResult,
    ) {
        self.finish_write_at(workspace_id, issue_id, result, Instant::now());
    }

    fn finish_write_at(
        &mut self,
        workspace_id: WorkspaceId,
        issue_id: &str,
        result: BeadsIssueWriteResult,
        now: Instant,
    ) {
        let key = (workspace_id, issue_id.to_owned());
        let Some(intent) = self.in_flight_writes.remove(&key) else {
            return;
        };
        self.write_deadlines.remove(&key);
        let lane = self.open.get(&workspace_id).map_or(4, |panel| panel.lane);
        match result {
            BeadsIssueWriteResult::Applied { .. }
                if matches!(intent.verb, BeadsIssueWrite::CloseIssue) =>
            {
                self.open.remove(&workspace_id);
                self.notices.insert(
                    workspace_id,
                    PanelNotice::closed_at(
                        issue_id.to_owned(),
                        intent.guards.if_assignee.unwrap_or_default(),
                        lane,
                        now,
                    ),
                );
                self.last_opened = Some(workspace_id);
            }
            BeadsIssueWriteResult::Applied { .. } => {
                self.notices.remove(&workspace_id);
                self.refresh_open_issue(workspace_id, issue_id);
            }
            BeadsIssueWriteResult::PreconditionFailed => {
                self.notices.insert(
                    workspace_id,
                    PanelNotice::new_at(
                        "Someone else won; refreshing issue detail".into(),
                        lane,
                        None,
                        now,
                    ),
                );
                self.refresh_open_issue(workspace_id, issue_id);
            }
            BeadsIssueWriteResult::Failed { reason } => {
                let timed_out = reason.contains("timed out");
                self.notices.insert(
                    workspace_id,
                    PanelNotice::new_at(format!("Issue write failed: {reason}"), lane, None, now),
                );
                if timed_out {
                    self.force_convergence(workspace_id, issue_id);
                }
            }
        }
    }

    pub fn expire_writes(&mut self) -> bool {
        self.expire_writes_at(Instant::now())
    }

    fn expire_writes_at(&mut self, now: Instant) -> bool {
        let expired: Vec<_> = self
            .write_deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for (workspace_id, issue_id) in &expired {
            let key = (*workspace_id, issue_id.clone());
            self.write_deadlines.remove(&key);
            let lane = self.open.get(workspace_id).map_or(4, |panel| panel.lane);
            self.notices.insert(
                *workspace_id,
                PanelNotice::new_at("Issue write timed out; refreshing".into(), lane, None, now),
            );
            self.force_convergence(*workspace_id, issue_id);
        }
        !expired.is_empty()
    }

    fn force_convergence(&mut self, workspace_id: WorkspaceId, issue_id: &str) {
        self.reconcile_on_snapshot.insert(workspace_id);
        self.pending_board_refreshes.insert(workspace_id);
        self.refresh_open_issue(workspace_id, issue_id);
    }

    pub fn take_board_refresh(&mut self) -> Option<WorkspaceId> {
        let workspace_id = self.pending_board_refreshes.iter().next().copied()?;
        self.pending_board_refreshes.take(&workspace_id)
    }

    pub fn reconnected(&mut self) {
        let workspaces: Vec<_> =
            self.in_flight_writes.keys().map(|(workspace_id, _)| *workspace_id).collect();
        self.reconcile_on_snapshot.extend(workspaces.iter().copied());
        self.pending_board_refreshes.extend(workspaces);
    }

    fn reconcile_snapshot(&mut self, workspace_id: WorkspaceId, reread: bool) -> bool {
        if !self.reconcile_on_snapshot.remove(&workspace_id) {
            return false;
        }
        let issue_ids: Vec<_> = self
            .in_flight_writes
            .keys()
            .filter(|(candidate, _)| *candidate == workspace_id)
            .map(|(_, issue_id)| issue_id.clone())
            .collect();
        for issue_id in issue_ids {
            let key = (workspace_id, issue_id.clone());
            self.in_flight_writes.remove(&key);
            self.write_deadlines.remove(&key);
            if reread {
                self.refresh_open_issue(workspace_id, &issue_id);
            }
        }
        true
    }

    fn refresh_open_issue(&mut self, workspace_id: WorkspaceId, issue_id: &str) {
        if self.open.get(&workspace_id).is_some_and(|panel| panel.card.id == issue_id)
            && !self
                .pending_requests
                .iter()
                .any(|request| request.0 == workspace_id && request.1 == issue_id)
        {
            self.pending_requests.push_back((workspace_id, issue_id.to_owned()));
        }
    }

    pub fn undo(&mut self, workspace_id: WorkspaceId) -> bool {
        self.undo_at(workspace_id, Instant::now())
    }

    fn undo_at(&mut self, workspace_id: WorkspaceId, now: Instant) -> bool {
        if !self.write_enabled {
            return false;
        }
        let Some(notice) = self.notices.remove(&workspace_id) else { return false };
        if !notice.active_at(now) {
            return false;
        }
        let Some(undo) = notice.undo.clone() else {
            self.notices.insert(workspace_id, notice);
            return false;
        };
        let key = (workspace_id, undo.issue_id.clone());
        if self.in_flight_writes.contains_key(&key)
            || self
                .pending_writes
                .iter()
                .any(|write| write.workspace_id == workspace_id && write.issue_id == undo.issue_id)
        {
            self.notices.insert(workspace_id, notice);
            return false;
        }
        self.pending_writes.push_back(PanelWriteIntent {
            workspace_id,
            issue_id: undo.issue_id,
            verb: BeadsIssueWrite::UndoClose,
            guards: BeadsIssueWriteGuards {
                if_status: Some("closed".into()),
                if_assignee: Some(undo.assignee),
            },
        });
        true
    }

    pub fn undo_available(&self, workspace_id: WorkspaceId) -> bool {
        self.notices
            .get(&workspace_id)
            .is_some_and(|notice| notice.active() && notice.undo.is_some())
    }

    pub fn dismiss(&mut self, workspace_id: WorkspaceId) -> bool {
        self.pending_navigation.remove(&workspace_id);
        self.pick_rows.remove(&workspace_id);
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
        self.pending_writes.retain(|write| live.contains(&write.workspace_id));
        self.in_flight_writes.retain(|(workspace_id, _), _| live.contains(workspace_id));
        self.pick_rows.retain(|workspace_id, _| live.contains(workspace_id));
        self.write_deadlines.retain(|(workspace_id, _), _| live.contains(workspace_id));
        self.pending_board_refreshes.retain(|workspace_id| live.contains(workspace_id));
        self.reconcile_on_snapshot.retain(|workspace_id| live.contains(workspace_id));
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
        self.notice_at(workspace_id, Instant::now())
    }

    fn notice_at(&self, workspace_id: WorkspaceId, now: Instant) -> Option<&str> {
        self.notices
            .get(&workspace_id)
            .filter(|notice| notice.active_at(now))
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
            self.reconcile_snapshot(workspace_id, false);
            self.pending_navigation.remove(&workspace_id);
            self.pick_rows.remove(&workspace_id);
            let Some(panel) = self.open.remove(&workspace_id) else { return false };
            self.notices.insert(
                workspace_id,
                PanelNotice::new("Beads project is no longer detected".into(), panel.lane),
            );
            self.last_opened = Some(workspace_id);
            return true;
        }
        let reconciled = matches!(state, BeadsBoardState::Ready { .. })
            && self.reconcile_snapshot(workspace_id, true);
        let Some(snapshot) = board_snapshot(state) else { return false };
        let Some(panel) = self.open.get_mut(&workspace_id) else { return reconciled };
        let Some((lane, card)) = snapshot_card(snapshot, &panel.card.id) else { return reconciled };
        let changed = panel.lane != lane || panel.card != *card;
        if changed {
            panel.lane = lane;
            panel.card = card.clone();
        }
        changed || reconciled
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
        self.pick_rows.remove(&workspace_id);
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

pub struct BeadsPanelRender<'a> {
    pub region: Rect,
    pub board: Rect,
    pub workspace_id: WorkspaceId,
    pub state: Arc<Mutex<BeadsPanels>>,
    pub editor: Entity<BeadsEditor>,
    pub terminal_focus: FocusHandle,
    pub app: &'a App,
    pub write_enabled: bool,
    pub scale: f32,
    pub colors: BeadsBoardColors,
    pub animations: AnimationSettings,
}

#[derive(Clone, Copy)]
struct EditWiring<'a> {
    workspace_id: WorkspaceId,
    editor: &'a Entity<BeadsEditor>,
    app: &'a App,
    write_enabled: bool,
    colors: &'a BeadsBoardColors,
}

impl BeadsPanelRender<'_> {
    fn edit_wiring(&self) -> EditWiring<'_> {
        EditWiring {
            workspace_id: self.workspace_id,
            editor: &self.editor,
            app: self.app,
            write_enabled: self.write_enabled,
            colors: &self.colors,
        }
    }
}

/// Paint one workspace's backdrop and lane-anchored detail panel.
pub fn render(panel: &BeadsPanel, wiring: &BeadsPanelRender<'_>) -> Vec<AnyElement> {
    let Some(layout) = panel_layout(wiring.region, wiring.board, panel.lane, wiring.scale) else {
        return Vec::new();
    };
    let workspace_id = wiring.workspace_id;
    let close_state = std::sync::Arc::clone(&wiring.state);
    let close_editor = wiring.editor.clone();
    let close_focus = wiring.terminal_focus.clone();
    let backdrop = div()
        .id(SharedString::from(format!("beads-detail-backdrop-{workspace_id}")))
        .absolute()
        .left(px(wiring.region.x))
        .top(px(wiring.region.y))
        .w(px(wiring.region.width))
        .h(px(wiring.region.height))
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, app| {
            close_editor.update(app, BeadsEditor::commit);
            window.focus(&close_focus, app);
            if let Ok(mut panels) = close_state.lock() {
                panels.dismiss(workspace_id);
            }
            window.refresh();
        })
        .into_any_element();
    let body = panel_body(panel, wiring, layout);
    vec![backdrop, body]
}

pub fn render_notice(text: &str, lane: u8, wiring: &BeadsPanelRender<'_>) -> Option<AnyElement> {
    let layout = panel_layout(wiring.region, wiring.board, lane, wiring.scale)?;
    let workspace_id = wiring.workspace_id;
    let undo = wiring.state.lock().is_ok_and(|panels| panels.undo_available(workspace_id));
    let notice = div()
        .id(SharedString::from(format!("beads-detail-notice-{workspace_id}")))
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
        .child(text.to_owned());
    let notice = if undo {
        let state = std::sync::Arc::clone(&wiring.state);
        notice
            .role(Role::Button)
            .aria_label("Undo issue close")
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
            .on_click(move |_event, window, _app| {
                if let Ok(mut panels) = state.lock() {
                    panels.undo(workspace_id);
                }
                window.refresh();
            })
    } else {
        notice
    };
    Some(notice.into_any_element())
}

fn panel_body(
    panel: &BeadsPanel,
    wiring: &BeadsPanelRender<'_>,
    layout: PanelLayout,
) -> AnyElement {
    let geometry = layout.geometry;
    let colors = &wiring.colors;
    let workspace_id = wiring.workspace_id;
    let scale = layout.scale;
    let presentation = panel.detail.as_deref().map(PanelPresentation::from_detail);
    let surface = div()
        .id(SharedString::from(format!("beads-detail-{workspace_id}")))
        .track_focus(&wiring.editor.read(wiring.app).focus)
        .tab_stop(false)
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
        let content = PanelContentWiring {
            workspace_id,
            state: &wiring.state,
            editor: &wiring.editor,
            app: wiring.app,
            write_enabled: wiring.write_enabled,
            colors,
            scale,
        };
        surface.child(detail_content(detail, presentation, content)).child(status_rail(
            detail,
            presentation,
            content,
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
    wiring: &BeadsPanelRender<'_>,
) -> AnyElement {
    let colors = &wiring.colors;
    let scale = wiring.scale;
    let detail = panel.detail.as_deref();
    let title = panel.title();
    let priority = panel.priority();
    let epic = panel.epic();
    let epic =
        presentation.is_none_or(|build| build.has(PanelSection::Epic)).then_some(epic).flatten();
    let close_state = std::sync::Arc::clone(&wiring.state);
    let close_editor = wiring.editor.clone();
    let close_focus = wiring.terminal_focus.clone();
    let workspace_id = wiring.workspace_id;
    let title = header_title(detail, title, wiring);
    div()
        .flex_none()
        .px(px(16.0))
        .pt(px(12.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(priority_pick_row(detail, priority, wiring))
                .child(title)
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
                        .on_click(move |_event, window, app| {
                            close_editor.update(app, BeadsEditor::commit);
                            window.focus(&close_focus, app);
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

fn priority_pick_row(
    detail: Option<&BeadsIssueDetail>,
    priority: u8,
    wiring: &BeadsPanelRender<'_>,
) -> AnyElement {
    let workspace_id = wiring.workspace_id;
    let writable = wiring.write_enabled && detail.is_some_and(|issue| issue.status != "closed");
    let expanded = writable
        && wiring
            .state
            .lock()
            .is_ok_and(|panels| panels.pick_row(workspace_id) == Some(PanelPickRow::Priority));
    let mark = div()
        .flex_none()
        .mr(px(6.0))
        .font_family("monospace")
        .text_size(at(wiring.scale, 11.0))
        .line_height(at(wiring.scale, 20.0))
        .font_weight(FontWeight(700.0))
        .text_color(priority_color(&wiring.colors, priority));
    if expanded {
        return mark
            .flex()
            .gap(px(6.0))
            .children((0..=4).map(|choice| {
                let state = Arc::clone(&wiring.state);
                div()
                    .id(SharedString::from(format!(
                        "beads-detail-priority-{workspace_id}-{choice}"
                    )))
                    .role(Role::Button)
                    .aria_label(format!("Set issue priority to P{choice}"))
                    .cursor_pointer()
                    .font_weight(if choice == priority {
                        FontWeight(700.0)
                    } else {
                        FontWeight(400.0)
                    })
                    .text_color(priority_color(&wiring.colors, choice))
                    .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
                    .on_click(move |_event, window, _app| {
                        queue_priority(&state, workspace_id, choice);
                        window.refresh();
                    })
                    .child(format!("P{choice}"))
            }))
            .into_any_element();
    }
    let mark = mark.child(format!("P{priority}"));
    if !writable {
        return mark.into_any_element();
    }
    let state = Arc::clone(&wiring.state);
    mark.id(SharedString::from(format!("beads-detail-priority-{workspace_id}")))
        .role(Role::Button)
        .aria_label("Edit issue priority")
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, _app| {
            if let Ok(mut panels) = state.lock() {
                panels.toggle_pick_row(workspace_id, PanelPickRow::Priority);
            }
            window.refresh();
        })
        .into_any_element()
}

fn header_title(
    detail: Option<&BeadsIssueDetail>,
    title: &str,
    wiring: &BeadsPanelRender<'_>,
) -> AnyElement {
    let colors = &wiring.colors;
    let scale = wiring.scale;
    detail.map_or_else(
        || {
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(at(scale, 15.0))
                .line_height(at(scale, 20.0))
                .font_weight(FontWeight(660.0))
                .text_color(colors.title)
                .child(title.to_owned())
                .into_any_element()
        },
        |detail| {
            editable_text(
                wiring.edit_wiring(),
                &detail.id,
                EditField::Title,
                title,
                div()
                    .truncate()
                    .text_size(at(scale, 15.0))
                    .line_height(at(scale, 20.0))
                    .font_weight(FontWeight(660.0))
                    .text_color(colors.title),
            )
            .flex_1()
            .into_any_element()
        },
    )
}

fn identity_row(
    panel: &BeadsPanel,
    presentation: Option<&PanelPresentation>,
    wiring: &BeadsPanelRender<'_>,
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
    wiring: &BeadsPanelRender<'_>,
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
        .children(detail.map(|issue| type_pick_row(issue, wiring)))
        .children(detail.and_then(|issue| identity_labels(issue, presentation, wiring)))
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

fn identity_labels(
    issue: &BeadsIssueDetail,
    presentation: Option<&PanelPresentation>,
    wiring: &BeadsPanelRender<'_>,
) -> Option<AnyElement> {
    if !presentation.is_some_and(|build| build.has(PanelSection::Labels))
        && (!wiring.write_enabled || issue.status == "closed")
    {
        return None;
    }
    let labels = if issue.labels.is_empty() { "+label".into() } else { issue.labels.join(" ") };
    Some(
        div()
            .flex()
            .items_center()
            .gap(px(7.0))
            .child(separator(&wiring.colors))
            .child(editable_text(
                wiring.edit_wiring(),
                &issue.id,
                EditField::Labels,
                &labels,
                div().font_family("monospace"),
            ))
            .into_any_element(),
    )
}

fn type_pick_row(issue: &BeadsIssueDetail, wiring: &BeadsPanelRender<'_>) -> AnyElement {
    let workspace_id = wiring.workspace_id;
    let writable = wiring.write_enabled && issue.status != "closed";
    let expanded = writable
        && wiring
            .state
            .lock()
            .is_ok_and(|panels| panels.pick_row(workspace_id) == Some(PanelPickRow::IssueType));
    if expanded {
        return div()
            .flex()
            .flex_1()
            .min_w(px(0.0))
            .flex_wrap()
            .gap(px(6.0))
            .children(BD_ISSUE_TYPES.map(|issue_type| {
                let state = Arc::clone(&wiring.state);
                div()
                    .id(SharedString::from(format!(
                        "beads-detail-type-{workspace_id}-{issue_type}"
                    )))
                    .role(Role::Button)
                    .aria_label(format!("Set issue type to {issue_type}"))
                    .cursor_pointer()
                    .font_family("monospace")
                    .font_weight(if issue_type == issue.issue_type {
                        FontWeight(600.0)
                    } else {
                        FontWeight(400.0)
                    })
                    .text_color(if issue_type == issue.issue_type {
                        wiring.colors.title
                    } else {
                        wiring.colors.muted
                    })
                    .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
                    .on_click(move |_event, window, _app| {
                        queue_issue_type(&state, workspace_id, issue_type);
                        window.refresh();
                    })
                    .child(issue_type)
            }))
            .into_any_element();
    }
    let shown = div().font_family("monospace").child(issue.issue_type.clone());
    if !writable {
        return shown.into_any_element();
    }
    let state = Arc::clone(&wiring.state);
    shown
        .id(SharedString::from(format!("beads-detail-type-{workspace_id}")))
        .role(Role::Button)
        .aria_label("Edit issue type")
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, _app| {
            if let Ok(mut panels) = state.lock() {
                panels.toggle_pick_row(workspace_id, PanelPickRow::IssueType);
            }
            window.refresh();
        })
        .into_any_element()
}

fn identity_docs(
    detail: &BeadsIssueDetail,
    presentation: Option<&PanelPresentation>,
    wiring: &BeadsPanelRender<'_>,
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
                        .child(
                            editable_text(
                                wiring.edit_wiring(),
                                &detail.id,
                                EditField::SpecId,
                                spec,
                                div().truncate(),
                            )
                            .flex_1(),
                        )
                }),
        )
        .children(presentation.is_some_and(|build| build.has(PanelSection::Design)).then(|| {
            div()
                .max_w(px(130.0))
                .flex()
                .gap(px(4.0))
                .child(runin("Design", colors, wiring.scale))
                .child(
                    editable_text(
                        wiring.edit_wiring(),
                        &detail.id,
                        EditField::Design,
                        &detail.design,
                        div().min_w(px(0.0)),
                    )
                    .flex_1(),
                )
        }))
        .into_any_element()
}

#[derive(Clone)]
struct EditableTextState {
    workspace_id: WorkspaceId,
    editor: Entity<BeadsEditor>,
    issue_id: String,
    field: EditField,
    value: String,
    layout: TextLayout,
}

impl EditableTextState {
    fn target(&self) -> EditTarget<'_> {
        EditTarget {
            workspace_id: self.workspace_id,
            issue_id: &self.issue_id,
            field: self.field,
            value: &self.value,
        }
    }
}

fn editable_text(
    wiring: EditWiring<'_>,
    issue_id: &str,
    field: EditField,
    value: &str,
    mut text: gpui::Div,
) -> gpui::Stateful<gpui::Div> {
    let id =
        SharedString::from(format!("beads-edit-{}-{issue_id}-{}", wiring.workspace_id, field.id()));
    if !wiring.write_enabled {
        return text.id(id).min_w(px(0.0)).child(value.to_owned());
    }
    let beads_editor = wiring.editor.clone();
    let active_text = beads_editor
        .read(wiring.app)
        .active_text(wiring.workspace_id, issue_id, field)
        .map(str::to_owned);
    let active = active_text.is_some();
    if active {
        text.text_style().text_overflow = None;
    }
    let shown = active_text.unwrap_or_else(|| value.to_owned());
    let display = if field == EditField::Comment && shown.is_empty() {
        "add a comment…".to_owned()
    } else {
        shown.clone()
    };
    let styled = StyledText::new(display);
    let focus = beads_editor.read(wiring.app).focus.clone();
    let state = EditableTextState {
        workspace_id: wiring.workspace_id,
        editor: beads_editor,
        issue_id: issue_id.to_owned(),
        field,
        value: value.to_owned(),
        layout: styled.layout().clone(),
    };
    let hover = with_alpha(wiring.colors.title, 0.07);
    let surface = div()
        .id(id)
        .role(Role::TextInput)
        .aria_label(format!("Edit issue {}", field.id()))
        .aria_description("Press Enter or Space to edit")
        .aria_value(shown.clone())
        .focusable()
        .tab_stop(true)
        .min_w(px(0.0))
        .relative()
        .rounded(px(2.0))
        .cursor_text()
        .when(active, |surface| surface.bg(hover).track_focus(&focus))
        .when(!active, |surface| surface.hover(move |hovered| hovered.bg(hover).shadow_sm()));
    let surface = editable_a11y_surface(surface, state.clone());
    let surface = editable_keyboard_surface(surface, state.clone());
    let surface = editable_pointer_start_surface(
        surface,
        state.clone(),
        field == EditField::Comment && shown.is_empty(),
    );
    let surface = editable_pointer_selection_surface(surface, state.clone());
    let surface = surface.child(text.child(styled));
    if active {
        editable_input_surface(surface, focus, state.editor, state.layout)
    } else {
        surface
    }
}

fn editable_a11y_surface(
    surface: gpui::Stateful<gpui::Div>,
    state: EditableTextState,
) -> gpui::Stateful<gpui::Div> {
    let click_state = state.clone();
    surface
        .on_a11y_action(AccessibleAction::SetValue, move |data, window, app| {
            handle_editable_accessible_action(
                AccessibleAction::SetValue,
                data,
                &state,
                window,
                app,
            );
        })
        .on_a11y_action(AccessibleAction::Click, move |data, window, app| {
            handle_editable_accessible_action(
                AccessibleAction::Click,
                data,
                &click_state,
                window,
                app,
            );
        })
}

fn handle_editable_accessible_action(
    action: AccessibleAction,
    data: Option<&gpui::accesskit::ActionData>,
    state: &EditableTextState,
    window: &mut Window,
    app: &mut App,
) {
    match (action, data) {
        (AccessibleAction::Click, _) => {
            state.editor.update(app, |beads_editor, cx| {
                beads_editor.begin(
                    state.target(),
                    BeginEdit { cursor: None, layout: None, extend_selection: false },
                    window,
                    cx,
                );
            });
        }
        (AccessibleAction::SetValue, Some(gpui::accesskit::ActionData::Value(replacement))) => {
            state.editor.update(app, |beads_editor, cx| {
                beads_editor.set_value(state.target(), replacement.to_string(), window, cx);
            });
        }
        _ => return,
    }
    app.stop_propagation();
}

fn editable_keyboard_surface(
    surface: gpui::Stateful<gpui::Div>,
    state: EditableTextState,
) -> gpui::Stateful<gpui::Div> {
    surface.on_key_down(move |event: &KeyDownEvent, window, app| {
        if event.keystroke.modifiers.modified()
            || !matches!(event.keystroke.key.as_str(), "enter" | "space")
            || state.editor.read(app).session.is_active(state.target())
        {
            return;
        }
        state.editor.update(app, |beads_editor, cx| {
            beads_editor.begin(
                state.target(),
                BeginEdit { cursor: None, layout: None, extend_selection: false },
                window,
                cx,
            );
        });
        app.stop_propagation();
    })
}

fn editable_pointer_start_surface(
    surface: gpui::Stateful<gpui::Div>,
    state: EditableTextState,
    pointer_empty: bool,
) -> gpui::Stateful<gpui::Div> {
    surface.on_mouse_down(MouseButton::Left, move |event, window, app| {
        app.stop_propagation();
        let cursor = (!pointer_empty)
            .then(|| state.layout.index_for_position(event.position).unwrap_or_else(|index| index));
        state.editor.update(app, |beads_editor, cx| {
            beads_editor.begin(
                state.target(),
                BeginEdit {
                    cursor,
                    layout: Some(state.layout.clone()),
                    extend_selection: event.modifiers.shift,
                },
                window,
                cx,
            );
        });
        window.refresh();
    })
}

fn editable_pointer_selection_surface(
    surface: gpui::Stateful<gpui::Div>,
    state: EditableTextState,
) -> gpui::Stateful<gpui::Div> {
    let move_state = state.clone();
    let release_state = state.clone();
    surface
        .on_mouse_move(move |event, window, app| {
            let cursor =
                move_state.layout.index_for_position(event.position).unwrap_or_else(|index| index);
            move_state.editor.update(app, |beads_editor, cx| {
                beads_editor.extend_pointer_selection(move_state.target(), cursor, cx);
            });
            window.refresh();
        })
        .on_mouse_up(MouseButton::Left, move |_event, _window, app| {
            release_state.editor.update(app, |beads_editor, _| {
                beads_editor.end_pointer_selection(release_state.target());
            });
        })
        .on_mouse_up_out(MouseButton::Left, move |event, window, app| {
            let input_len = state.editor.read(app).session.input().map_or(0, str::len);
            let cursor = match state.layout.index_for_position(event.position) {
                Ok(index) => index,
                Err(0) => 0,
                Err(_) => input_len,
            };
            state.editor.update(app, |beads_editor, cx| {
                beads_editor.extend_pointer_selection(state.target(), cursor, cx);
                beads_editor.end_pointer_selection(state.target());
            });
            window.refresh();
        })
}

fn editable_input_surface(
    surface: gpui::Stateful<gpui::Div>,
    focus: FocusHandle,
    editor: Entity<BeadsEditor>,
    layout: TextLayout,
) -> gpui::Stateful<gpui::Div> {
    surface.child(
        canvas(
            |_, _, _| {},
            move |bounds, (), window, app| {
                editor.update(app, |editor, _| editor.layout = Some(layout.clone()));
                window.handle_input(&focus, ElementInputHandler::new(bounds, editor.clone()), app);
            },
        )
        .absolute()
        .size_full(),
    )
}

#[derive(Clone, Copy)]
struct PanelContentWiring<'a> {
    workspace_id: WorkspaceId,
    state: &'a Arc<Mutex<BeadsPanels>>,
    editor: &'a Entity<BeadsEditor>,
    app: &'a App,
    write_enabled: bool,
    colors: &'a BeadsBoardColors,
    scale: f32,
}

impl PanelContentWiring<'_> {
    fn edit_wiring(&self) -> EditWiring<'_> {
        EditWiring {
            workspace_id: self.workspace_id,
            editor: self.editor,
            app: self.app,
            write_enabled: self.write_enabled,
            colors: self.colors,
        }
    }
}

fn detail_content(
    detail: &BeadsIssueDetail,
    presentation: &PanelPresentation,
    wiring: PanelContentWiring<'_>,
) -> AnyElement {
    let PanelContentWiring { workspace_id, colors, scale, .. } = wiring;
    let queue = queue_color(colors, presentation.queue());
    let blockers = detail.blockers.iter().take(presentation.blocker_count());
    let facts =
        presentation.has(PanelSection::Facts).then(|| optional_facts(detail, colors, scale));
    let comments =
        presentation.has(PanelSection::Comments).then(|| comments(detail, presentation, wiring));
    let dependents = presentation.has(PanelSection::Dependents).then(|| unblocks(detail, wiring));
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
        .children(presentation.has(PanelSection::Description).then(|| {
            editable_passage(
                detail,
                PassageEdit {
                    field: EditField::Description,
                    label: None,
                    value: &detail.description,
                    color: colors.title,
                },
                wiring,
            )
        }))
        .children(presentation.has(PanelSection::Acceptance).then(|| {
            editable_passage(
                detail,
                PassageEdit {
                    field: EditField::Acceptance,
                    label: Some("Acceptance"),
                    value: &detail.acceptance_criteria,
                    color: colors.queue_name,
                },
                wiring,
            )
        }))
        .children(presentation.has(PanelSection::Notes).then(|| {
            editable_passage(
                detail,
                PassageEdit {
                    field: EditField::Notes,
                    label: Some("Notes"),
                    value: &detail.notes,
                    color: colors.queue_name,
                },
                wiring,
            )
        }))
        .children(facts)
        .children(comments)
        .children(dependents)
        .into_any_element()
}

#[derive(Clone, Copy)]
struct PassageEdit<'a> {
    field: EditField,
    label: Option<&'static str>,
    value: &'a str,
    color: Rgba,
}

fn editable_passage(
    detail: &BeadsIssueDetail,
    passage: PassageEdit<'_>,
    wiring: PanelContentWiring<'_>,
) -> AnyElement {
    let text = div()
        .flex_1()
        .text_size(at(wiring.scale, 11.0))
        .line_height(at(wiring.scale, if passage.label.is_some() { 16.5 } else { 17.05 }))
        .text_color(passage.color);
    let content =
        editable_text(wiring.edit_wiring(), &detail.id, passage.field, passage.value, text)
            .flex_1();
    match passage.label {
        Some(label) => div()
            .mt(px(8.0))
            .flex()
            .items_start()
            .gap(px(7.0))
            .child(runin(label, wiring.colors, wiring.scale))
            .child(content)
            .into_any_element(),
        None => content.into_any_element(),
    }
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
    let edit = wiring.edit_wiring();
    let PanelContentWiring { workspace_id, state, colors, scale, .. } = wiring;
    let comment_wiring =
        CommentWiring { workspace_id, issue_id: &detail.id, state, edit, colors, scale };
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
    edit: EditWiring<'a>,
    colors: &'a BeadsBoardColors,
    scale: f32,
    issue_id: &'a str,
}

fn comment_row(comment: &BeadsIssueComment, index: usize, wiring: CommentWiring<'_>) -> AnyElement {
    let CommentWiring { workspace_id, state, edit, colors, scale, issue_id } = wiring;
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
                .children((index == 0 && edit.write_enabled).then(|| {
                    editable_text(
                        edit,
                        issue_id,
                        EditField::Comment,
                        "",
                        div().text_size(at(scale, 10.0)).text_color(colors.muted),
                    )
                    .ml_auto()
                })),
        )
        .child(body)
        .into_any_element()
}

fn unblocks(detail: &BeadsIssueDetail, wiring: PanelContentWiring<'_>) -> AnyElement {
    let PanelContentWiring { workspace_id, state, colors, scale, .. } = wiring;
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
    wiring: PanelContentWiring<'_>,
) -> AnyElement {
    let PanelContentWiring { workspace_id, state, colors, .. } = wiring;
    let current = detail.status.as_str();
    let writable = state.lock().is_ok_and(|panels| panels.can_write(workspace_id));
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
        .children(
            [("open", "open"), ("in progress", "in_progress"), ("closed", "closed")]
                .map(|(shown, status)| status_word(shown, status, current, writable, wiring)),
        )
        .children(presentation.verbs().iter().map(|verb| panel_verb_word(*verb, writable, wiring)))
        .into_any_element()
}

fn status_word(
    shown: &'static str,
    status: &'static str,
    current: &str,
    writable: bool,
    wiring: PanelContentWiring<'_>,
) -> AnyElement {
    let PanelContentWiring { workspace_id, state, colors, scale, .. } = wiring;
    let active = current == status;
    let word = div()
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
        .children(
            active.then(|| div().size(px(7.0)).rounded_full().bg(colors.ready_state).shadow_sm()),
        )
        .child(shown);
    if !writable {
        return word.into_any_element();
    }
    let click_state = std::sync::Arc::clone(state);
    word.id(SharedString::from(format!("beads-detail-status-{workspace_id}-{status}")))
        .role(Role::Button)
        .aria_label(format!("Set issue status to {shown}"))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, _app| {
            queue_status(&click_state, workspace_id, status);
            window.refresh();
        })
        .into_any_element()
}

fn panel_verb_word(verb: PanelVerb, writable: bool, wiring: PanelContentWiring<'_>) -> AnyElement {
    let PanelContentWiring { workspace_id, state, colors, scale, .. } = wiring;
    let (label, tone, key) = match verb {
        PanelVerb::Claim => ("claim", colors.ready_state, "claim"),
        PanelVerb::CloseIssue => ("close issue", colors.done_state, "close"),
    };
    let word = div()
        .font_family("monospace")
        .text_size(at(scale, 10.0))
        .line_height(at(scale, 16.0))
        .font_weight(FontWeight(600.0))
        .text_color(colors.panel_state_ink(tone))
        .child(label);
    let word = match verb {
        PanelVerb::Claim => word.ml_auto(),
        PanelVerb::CloseIssue => word.ml(px(16.0)),
    };
    if !writable {
        return word.into_any_element();
    }
    let click_state = std::sync::Arc::clone(state);
    word.id(SharedString::from(format!("beads-detail-verb-{workspace_id}-{key}")))
        .role(Role::Button)
        .aria_label(label)
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, _app| {
            queue_panel_verb(&click_state, workspace_id, verb);
            window.refresh();
        })
        .into_any_element()
}

fn queue_status(state: &std::sync::Mutex<BeadsPanels>, workspace_id: WorkspaceId, status: &str) {
    if let Ok(mut panels) = state.lock() {
        panels.write_status(workspace_id, status);
    }
}

fn queue_priority(state: &std::sync::Mutex<BeadsPanels>, workspace_id: WorkspaceId, priority: u8) {
    if let Ok(mut panels) = state.lock() {
        panels.set_priority(workspace_id, priority);
    }
}

fn queue_issue_type(
    state: &std::sync::Mutex<BeadsPanels>,
    workspace_id: WorkspaceId,
    issue_type: &str,
) {
    if let Ok(mut panels) = state.lock() {
        panels.set_issue_type(workspace_id, issue_type);
    }
}

fn queue_panel_verb(
    state: &std::sync::Mutex<BeadsPanels>,
    workspace_id: WorkspaceId,
    verb: PanelVerb,
) {
    if let Ok(mut panels) = state.lock() {
        match verb {
            PanelVerb::Claim => panels.claim(workspace_id),
            PanelVerb::CloseIssue => panels.close_issue(workspace_id),
        };
    }
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
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use gpui::{
        EntityInputHandler, Modifiers, Render, WindowHandle, WindowOptions, div, point, px,
    };

    use scribe_common::ids::WorkspaceId;
    use scribe_common::protocol::{
        BeadsBoardItem, BeadsBoardSnapshot, BeadsBoardState, BeadsIssueComment, BeadsIssueDetail,
        BeadsIssueLink, BeadsIssueQueue, BeadsIssueQueueBasis, BeadsIssueWrite,
        BeadsIssueWriteGuards, BeadsIssueWriteResult,
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

    fn loaded_writable_panels(mut issue: BeadsIssueDetail) -> (WorkspaceId, BeadsPanels) {
        issue.assignee = Some("maintainer".into());
        let workspace = WorkspaceId::new();
        let issue_id = issue.id.clone();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.set_write_enabled(true);
        panels.open(workspace, item(), 1);
        assert_eq!(panels.take_request(), Some((workspace, issue_id.clone())));
        panels.update(workspace, &issue_id, Some(Box::new(issue)));
        (workspace, panels)
    }

    fn loaded_detail(panels: &BeadsPanels, workspace_id: WorkspaceId) -> &BeadsIssueDetail {
        panels
            .visible(workspace_id)
            .and_then(|panel| panel.detail.as_deref())
            .expect("loaded issue detail")
    }

    fn card_drop(source_lane: u8, target_lane: u8) -> crate::beads_board::CardDragState {
        crate::beads_board::CardDragState {
            workspace_id: WorkspaceId::new(),
            source: item(),
            source_lane,
            pointer: crate::beads_board::CardDragPoint { x: 0.0, y: 0.0 },
            hovered_lane: Some(target_lane),
        }
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
    fn panel_geometry_centers_in_its_region_and_obeys_the_narrow_floor() {
        let region = Rect { x: 100.0, y: 40.0, width: 800.0, height: 600.0 };
        let board = Rect { x: 100.0, y: 40.0, width: 800.0, height: 178.0 };

        assert_eq!(
            panel_geometry(region, board, 4),
            Some(PanelGeometry { x: 220.0, y: 222.0, width: 560.0, max_height: 406.0 })
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
    fn panel_layout_centers_in_its_terminal_region_across_splits_resizes_and_scales() {
        let samples = [
            (
                "full window",
                Rect { x: 0.0, y: 0.0, width: 1310.0, height: 871.0 },
                Rect { x: 0.0, y: 0.0, width: 1310.0, height: 197.0 },
                4,
                1.0,
                375.0,
                655.0,
            ),
            (
                "right active split at 0.8x",
                Rect { x: 655.0, y: 0.0, width: 655.0, height: 871.0 },
                Rect { x: 655.0, y: 0.0, width: 655.0, height: 197.0 },
                4,
                0.8,
                702.5,
                982.5,
            ),
            (
                "resized active region at 1.6x",
                Rect { x: 200.0, y: 0.0, width: 960.0, height: 871.0 },
                Rect { x: 200.0, y: 0.0, width: 960.0, height: 197.0 },
                4,
                1.6,
                400.0,
                680.0,
            ),
        ];

        for (name, region, board, lane, scale, expected_x, expected_midpoint) in samples {
            let loading = panel_layout(region, board, lane, scale).expect("loading panel layout");
            let resolved = panel_layout(region, board, lane, scale).expect("resolved panel layout");

            assert!((loading.scale - scale).abs() < f32::EPSILON, "{name}");
            assert!((loading.geometry.x - expected_x).abs() < f32::EPSILON, "{name}");
            assert!(
                (loading.geometry.x + loading.geometry.width / 2.0 - expected_midpoint).abs()
                    < f32::EPSILON,
                "{name}"
            );
            assert!(
                (resolved.geometry.x - loading.geometry.x).abs() < f32::EPSILON,
                "{name} arrival x"
            );
            assert!(
                (resolved.geometry.x + resolved.geometry.width / 2.0
                    - (loading.geometry.x + loading.geometry.width / 2.0))
                    .abs()
                    < f32::EPSILON,
                "{name} arrival midpoint"
            );
        }
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
                    geometry: PanelGeometry { x: 130.0, y: 103.8, width: 560.0, max_height: 420.0 },
                    scale: 0.8,
                },
            ),
            (
                "maximum board at 1.6x",
                Rect { x: 10.0, y: 20.0, width: 800.0, height: 520.0 },
                4,
                1.6,
                PanelLayout {
                    geometry: PanelGeometry { x: 130.0, y: 544.0, width: 560.0, max_height: 64.0 },
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

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Applied edits wait for persisted detail]]
    #[test]
    fn applied_edit_uses_guards_and_waits_for_persisted_detail() {
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.queue_edit(BeadsEditIntent {
            workspace_id: workspace,
            issue_id: "scribe-5wh1.4".into(),
            verb: BeadsIssueWrite::SetTitle { title: "Draft title".into() },
        }));

        assert_eq!(
            panels.take_write(),
            Some(PanelWriteIntent {
                workspace_id: workspace,
                issue_id: "scribe-5wh1.4".into(),
                verb: BeadsIssueWrite::SetTitle { title: "Draft title".into() },
                guards: BeadsIssueWriteGuards {
                    if_status: Some("open".into()),
                    if_assignee: Some("maintainer".into()),
                },
            })
        );
        assert_eq!(panels.take_write(), None);
        assert_eq!(loaded_detail(&panels, workspace).title, "Render the read-only detail panel");

        panels.finish_write_at(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Applied { generation: 9 },
            Instant::now(),
        );
        assert_eq!(loaded_detail(&panels, workspace).title, "Render the read-only detail panel");
        assert_eq!(panels.take_request(), Some((workspace, "scribe-5wh1.4".into())));

        let mut persisted = detail();
        persisted.title = "Persisted title".into();
        panels.update(workspace, "scribe-5wh1.4", Some(Box::new(persisted)));
        assert_eq!(loaded_detail(&panels, workspace).title, "Persisted title");
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Text fields map to typed writes]]
    #[test]
    fn every_text_field_maps_to_its_typed_write() {
        for (field, value, expected) in [
            (
                EditField::Title,
                "New title",
                BeadsIssueWrite::SetTitle { title: "New title".into() },
            ),
            (
                EditField::Description,
                "New description",
                BeadsIssueWrite::SetDescription { description: "New description".into() },
            ),
            (
                EditField::Acceptance,
                "New acceptance",
                BeadsIssueWrite::SetAcceptance { acceptance: "New acceptance".into() },
            ),
            (
                EditField::Notes,
                "New notes",
                BeadsIssueWrite::SetNotes { notes: "New notes".into() },
            ),
            (
                EditField::Design,
                "New design",
                BeadsIssueWrite::SetDesign { design: "New design".into() },
            ),
            (
                EditField::SpecId,
                "025-next-spec",
                BeadsIssueWrite::SetSpecId { spec_id: Some("025-next-spec".into()) },
            ),
        ] {
            assert_eq!(field.verb(value.into()), expected);
        }
        assert_eq!(
            EditField::SpecId.verb(String::new()),
            BeadsIssueWrite::SetSpecId { spec_id: None }
        );
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Failed edits retain persisted detail]]
    #[test]
    fn failed_edit_retains_persisted_detail_and_shows_the_notice() {
        let now = Instant::now();
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.queue_edit(BeadsEditIntent {
            workspace_id: workspace,
            issue_id: "scribe-5wh1.4".into(),
            verb: BeadsIssueWrite::SetDescription { description: "Draft body".into() },
        }));
        assert!(panels.take_write().is_some());

        panels.finish_write_at(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Failed { reason: "bd rejected edit".into() },
            now,
        );

        assert_eq!(loaded_detail(&panels, workspace).description, "Description");
        assert_eq!(panels.notice_at(workspace, now), Some("Issue write failed: bd rejected edit"));
        assert_eq!(panels.take_request(), None);
    }

    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Priority type and label editing]]
    #[test]
    fn priority_and_type_pick_rows_unfold_exclusively_for_writable_details() {
        let (workspace, mut panels) = loaded_writable_panels(detail());

        assert_eq!(panels.pick_row(workspace), None);
        assert!(panels.toggle_pick_row(workspace, PanelPickRow::Priority));
        assert_eq!(panels.pick_row(workspace), Some(PanelPickRow::Priority));
        assert!(panels.toggle_pick_row(workspace, PanelPickRow::IssueType));
        assert_eq!(panels.pick_row(workspace), Some(PanelPickRow::IssueType));
        assert!(panels.toggle_pick_row(workspace, PanelPickRow::IssueType));
        assert_eq!(panels.pick_row(workspace), None);

        panels.set_write_enabled(false);
        assert!(!panels.toggle_pick_row(workspace, PanelPickRow::Priority));
        assert_eq!(panels.pick_row(workspace), None);
    }

    #[test]
    fn type_pick_row_uses_the_pinned_bd_builtin_enum() {
        assert_eq!(
            BD_ISSUE_TYPES,
            [
                "bug",
                "feature",
                "task",
                "epic",
                "chore",
                "decision",
                "message",
                "molecule",
                "gate",
                "spike",
                "story",
                "milestone",
            ]
        );
    }

    #[test]
    fn picker_selections_queue_one_typed_guarded_write() {
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.toggle_pick_row(workspace, PanelPickRow::Priority));
        assert!(panels.set_priority(workspace, 4));
        assert_eq!(panels.pick_row(workspace), None);
        assert_eq!(
            panels.take_write(),
            Some(PanelWriteIntent {
                workspace_id: workspace,
                issue_id: "scribe-5wh1.4".into(),
                verb: BeadsIssueWrite::SetPriority { priority: 4 },
                guards: BeadsIssueWriteGuards {
                    if_status: Some("open".into()),
                    if_assignee: Some("maintainer".into()),
                },
            })
        );
        assert_eq!(panels.take_write(), None);

        let (type_workspace, mut type_panels) = loaded_writable_panels(detail());
        assert!(type_panels.toggle_pick_row(type_workspace, PanelPickRow::IssueType));
        assert!(type_panels.set_issue_type(type_workspace, "decision"));
        assert_eq!(type_panels.pick_row(type_workspace), None);
        assert_eq!(
            type_panels.take_write(),
            Some(PanelWriteIntent {
                workspace_id: type_workspace,
                issue_id: "scribe-5wh1.4".into(),
                verb: BeadsIssueWrite::SetType { issue_type: "decision".into() },
                guards: BeadsIssueWriteGuards {
                    if_status: Some("open".into()),
                    if_assignee: Some("maintainer".into()),
                },
            })
        );
        assert_eq!(type_panels.take_write(), None);
    }

    #[test]
    fn label_editor_composes_add_remove_and_set_into_one_set_labels_verb() {
        assert_eq!(
            EditField::Labels.verb("client,server client ui".into()),
            BeadsIssueWrite::SetLabels {
                labels: vec!["client".into(), "server".into(), "ui".into()],
            }
        );

        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.queue_edit(BeadsEditIntent {
            workspace_id: workspace,
            issue_id: "scribe-5wh1.4".into(),
            verb: EditField::Labels.verb("server,docs".into()),
        }));
        assert_eq!(
            panels.take_write().map(|write| (write.verb, write.guards)),
            Some((
                BeadsIssueWrite::SetLabels { labels: vec!["server".into(), "docs".into()] },
                BeadsIssueWriteGuards {
                    if_status: Some("open".into()),
                    if_assignee: Some("maintainer".into()),
                },
            ))
        );
        assert_eq!(panels.take_write(), None);
    }

    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Persisted picker repaint]]
    #[test]
    fn picker_write_repaints_only_after_persisted_detail_reply() {
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.set_priority(workspace, 4));
        assert_eq!(
            panels.visible(workspace).and_then(|panel| panel.detail.as_deref()).unwrap().priority,
            1
        );

        assert!(panels.take_write().is_some());
        panels.finish_write(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Applied { generation: 9 },
        );
        assert_eq!(
            panels.visible(workspace).and_then(|panel| panel.detail.as_deref()).unwrap().priority,
            1
        );
        assert_eq!(panels.take_request(), Some((workspace, "scribe-5wh1.4".into())));

        let mut persisted = detail();
        persisted.priority = 4;
        panels.update(workspace, "scribe-5wh1.4", Some(Box::new(persisted)));
        assert_eq!(
            panels.visible(workspace).and_then(|panel| panel.detail.as_deref()).unwrap().priority,
            4
        );
    }

    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Comment composer authoritative refresh]]
    #[test]
    fn comment_composer_queues_guarded_verb_without_repainting_the_thread() {
        let loaded = full_detail();
        let saved_comments = loaded.comments.clone();
        let (workspace, mut panels) = loaded_writable_panels(loaded.clone());
        let mut edit = EditSession::default();
        edit.begin(workspace, &loaded.id, EditField::Comment, "");
        edit.replace_all("Authoritative refresh owns this row");

        assert!(panels.queue_edit(edit.finish().expect("changed comment")));
        assert_eq!(
            panels.take_write(),
            Some(PanelWriteIntent {
                workspace_id: workspace,
                issue_id: loaded.id.clone(),
                verb: BeadsIssueWrite::AddComment {
                    body: "Authoritative refresh owns this row".into(),
                },
                guards: BeadsIssueWriteGuards {
                    if_status: Some("open".into()),
                    if_assignee: Some("maintainer".into()),
                },
            })
        );
        assert_eq!(
            panels
                .visible(workspace)
                .and_then(|panel| panel.detail.as_deref())
                .map(|detail| &detail.comments),
            Some(&saved_comments),
            "draft and send must not paint an optimistic comment"
        );

        panels.finish_write_at(
            workspace,
            &loaded.id,
            BeadsIssueWriteResult::Applied { generation: 9 },
            Instant::now(),
        );
        assert_eq!(panels.take_request(), Some((workspace, loaded.id.clone())));
        assert_eq!(
            panels
                .visible(workspace)
                .and_then(|panel| panel.detail.as_deref())
                .map(|detail| &detail.comments),
            Some(&saved_comments),
            "applied result only requests the authoritative detail"
        );
    }

    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Write failure notice lifecycle]]
    #[test]
    fn write_failure_notice_clears_on_the_next_success() {
        let now = Instant::now();
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.claim(workspace));
        assert!(panels.take_write().is_some());
        panels.finish_write_at(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Failed { reason: "permission denied".into() },
            now,
        );
        assert_eq!(panels.notice_at(workspace, now), Some("Issue write failed: permission denied"));

        assert!(panels.claim(workspace));
        assert!(panels.take_write().is_some());
        panels.finish_write_at(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Applied { generation: 10 },
            now,
        );
        assert_eq!(panels.notice_at(workspace, now), None);
    }

    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Write timeout convergence]]
    #[test]
    fn timed_out_write_forces_board_and_detail_convergence() {
        let now = Instant::now();
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.claim(workspace));
        assert!(panels.take_write_at(now).is_some());

        let just_before = (now + WRITE_DEADLINE)
            .checked_sub(Duration::from_millis(1))
            .expect("write deadline exceeds one millisecond");
        assert!(!panels.expire_writes_at(just_before));
        assert!(panels.expire_writes_at(now + WRITE_DEADLINE));
        assert_eq!(panels.take_board_refresh(), Some(workspace));
        assert_eq!(panels.take_request(), Some((workspace, "scribe-5wh1.4".into())));
        assert_eq!(
            panels.notice_at(workspace, now + WRITE_DEADLINE),
            Some("Issue write timed out; refreshing")
        );
        assert!(!panels.claim(workspace), "unknown timeout result blocks another write");
        panels.sync_board(workspace, &board_with(item(), 1));
        assert!(panels.claim(workspace), "authoritative board releases the timeout fence");

        let (server_workspace, mut server_panels) = loaded_writable_panels(detail());
        assert!(server_panels.claim(server_workspace));
        assert!(server_panels.take_write_at(now).is_some());
        server_panels.finish_write_at(
            server_workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Failed { reason: "bd issue write timed out".into() },
            now + WRITE_DEADLINE,
        );
        assert_eq!(server_panels.take_board_refresh(), Some(server_workspace));
        assert_eq!(server_panels.take_request(), Some((server_workspace, "scribe-5wh1.4".into())));
        assert!(!server_panels.claim(server_workspace));
        server_panels.sync_board(server_workspace, &board_with(item(), 1));
        assert!(server_panels.claim(server_workspace));
    }

    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Reconnect write reconciliation]]
    #[test]
    fn first_post_reconnect_snapshot_reconciles_in_flight_write_once() {
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.claim(workspace));
        assert!(panels.take_write().is_some());
        panels.reconnected();
        assert_eq!(panels.take_board_refresh(), Some(workspace));

        let snapshot = board_with(item(), 1);
        panels.sync_board(workspace, &snapshot);
        assert_eq!(panels.take_request(), Some((workspace, "scribe-5wh1.4".into())));
        assert!(panels.claim(workspace), "first snapshot releases the unknown write outcome");

        panels.sync_board(workspace, &snapshot);
        assert_eq!(panels.take_request(), None, "later snapshots do not replay reconciliation");
    }

    #[test]
    fn card_drop_matrix_queues_only_native_writable_targets() {
        for source_lane in 0..=2 {
            let source_status = match source_lane {
                1 => Some("open"),
                2 => Some("in_progress"),
                _ => None,
            };
            for target_lane in 0..=4 {
                let drag = card_drop(source_lane, target_lane);
                let mut panels = BeadsPanels::default();
                panels.set_enabled(true);
                panels.set_write_enabled(true);

                let expected = match target_lane {
                    1 if source_lane != 1 => Some(BeadsIssueWrite::SetStatus {
                        status: "open".into(),
                        clear_defer: true,
                    }),
                    2 if source_lane != 2 => Some(BeadsIssueWrite::Claim),
                    4 => Some(BeadsIssueWrite::CloseIssue),
                    _ => None,
                };
                assert_eq!(
                    panels.queue_card_drop(&drag),
                    expected.is_some(),
                    "source {source_lane} -> target {target_lane} acceptance"
                );
                let write = panels.take_write();
                assert_eq!(write.as_ref().map(|write| &write.verb), expected.as_ref());
                assert_eq!(
                    write.as_ref().map(|write| write.workspace_id),
                    expected.as_ref().map(|_| drag.workspace_id)
                );
                assert_eq!(
                    write.as_ref().map(|write| write.issue_id.as_str()),
                    expected.as_ref().map(|_| drag.source.id.as_str())
                );
                assert_eq!(
                    write.as_ref().map(|write| write.guards.if_status.as_deref()),
                    expected.as_ref().map(|_| source_status)
                );
                assert_eq!(
                    write.as_ref().map(|write| write.guards.if_assignee.as_deref()),
                    expected.as_ref().map(|_| None)
                );
            }
        }
    }

    #[test]
    fn card_drop_claim_reuses_fresh_detail_guards() {
        let (workspace, mut panels) = loaded_writable_panels(detail());
        let drag = crate::beads_board::CardDragState {
            workspace_id: workspace,
            source: item(),
            source_lane: 1,
            pointer: crate::beads_board::CardDragPoint { x: 0.0, y: 0.0 },
            hovered_lane: Some(2),
        };

        assert!(panels.queue_card_drop(&drag));
        let write = panels.take_write().expect("guarded claim");
        assert_eq!(write.verb, BeadsIssueWrite::Claim);
        assert_eq!(write.guards.if_status.as_deref(), Some("open"));
        assert_eq!(write.guards.if_assignee.as_deref(), Some("maintainer"));
    }

    #[test]
    fn card_drop_queue_honors_in_flight_and_reconnect_fences() {
        let mut first = card_drop(1, 2);
        let workspace = first.workspace_id;
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.set_write_enabled(true);

        assert!(panels.queue_card_drop(&first));
        assert!(panels.take_write().is_some());
        assert!(!panels.queue_card_drop(&first), "same issue is already in flight");

        panels.reconnected();
        first.source.id = "scribe-5wh1.5".into();
        assert!(!panels.queue_card_drop(&first), "workspace is awaiting a snapshot");
        panels.sync_board(workspace, &board_with(item(), 1));
        assert!(panels.queue_card_drop(&first), "authoritative snapshot releases the fence");
    }

    #[test]
    fn classifier_won_drop_surfaces_a_lane_notice() {
        let workspace = WorkspaceId::new();
        let mut panels = BeadsPanels::default();

        panels.classifier_won(workspace, "scribe-5wh1.4", 3);

        assert_eq!(panels.notice(workspace), Some("scribe-5wh1.4 stayed Blocked; classifier won"));
        assert_eq!(panels.notice_lane(workspace), Some(3));
    }

    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Guarded status and claim intents]]
    #[test]
    fn status_rail_and_claim_queue_native_verbs_with_fresh_detail_guards() {
        for (target, expected) in [
            ("open", BeadsIssueWrite::SetStatus { status: "open".into(), clear_defer: false }),
            (
                "in_progress",
                BeadsIssueWrite::SetStatus { status: "in_progress".into(), clear_defer: false },
            ),
            ("closed", BeadsIssueWrite::SetStatus { status: "closed".into(), clear_defer: false }),
        ] {
            let (workspace, mut panels) = loaded_writable_panels(detail());
            assert!(panels.write_status(workspace, target));
            assert_eq!(
                panels.take_write(),
                Some(PanelWriteIntent {
                    workspace_id: workspace,
                    issue_id: "scribe-5wh1.4".into(),
                    verb: expected,
                    guards: BeadsIssueWriteGuards {
                        if_status: Some("open".into()),
                        if_assignee: Some("maintainer".into()),
                    },
                })
            );
        }

        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.claim(workspace));
        assert_eq!(
            panels.take_write().map(|intent| (intent.verb, intent.guards)),
            Some((
                BeadsIssueWrite::Claim,
                BeadsIssueWriteGuards {
                    if_status: Some("open".into()),
                    if_assignee: Some("maintainer".into()),
                },
            ))
        );
    }

    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Close undo deadline]]
    #[test]
    fn applied_close_opens_an_exact_five_second_guarded_undo_window() {
        let now = Instant::now();
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.close_issue(workspace));
        assert_eq!(
            panels.take_write().map(|intent| (intent.verb, intent.guards)),
            Some((
                BeadsIssueWrite::CloseIssue,
                BeadsIssueWriteGuards {
                    if_status: Some("open".into()),
                    if_assignee: Some("maintainer".into()),
                },
            ))
        );

        panels.finish_write_at(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Applied { generation: 7 },
            now,
        );
        assert!(panels.visible(workspace).is_none());
        assert_eq!(
            panels.notice_at(workspace, now + Duration::from_millis(4_999)),
            Some("closed scribe-5wh1.4 · undo")
        );
        assert!(panels.undo_at(workspace, now + Duration::from_millis(4_999)));
        assert_eq!(
            panels.take_write().map(|intent| (intent.verb, intent.guards)),
            Some((
                BeadsIssueWrite::UndoClose,
                BeadsIssueWriteGuards {
                    if_status: Some("closed".into()),
                    if_assignee: Some("maintainer".into()),
                },
            ))
        );
    }

    #[test]
    fn undo_at_or_after_the_five_second_deadline_writes_nothing() {
        let now = Instant::now();
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.close_issue(workspace));
        assert!(panels.take_write().is_some());
        panels.finish_write_at(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Applied { generation: 8 },
            now,
        );

        assert!(!panels.undo_at(workspace, now + Duration::from_secs(5)));
        assert_eq!(panels.take_write(), None);
        assert_eq!(panels.notice_at(workspace, now + Duration::from_secs(5)), None);
    }

    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Write capability and closed issue gates]]
    #[test]
    fn missing_write_capability_and_closed_details_queue_no_verbs() {
        let workspace = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.set_write_enabled(false);
        panels.open(workspace, item(), 1);
        panels.take_request();
        panels.update(workspace, "scribe-5wh1.4", Some(Box::new(detail())));
        assert!(!panels.write_status(workspace, "closed"));
        assert!(!panels.claim(workspace));
        assert!(!panels.close_issue(workspace));
        assert_eq!(panels.take_write(), None);

        let mut closed = detail();
        closed.status = "closed".into();
        let (closed_workspace, mut closed_panels) = loaded_writable_panels(closed);
        assert!(!closed_panels.write_status(closed_workspace, "open"));
        assert!(!closed_panels.claim(closed_workspace));
        assert!(!closed_panels.close_issue(closed_workspace));
        assert_eq!(closed_panels.take_write(), None);
    }

    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Conflict result notice]]
    #[test]
    fn precondition_failure_surfaces_someone_else_won_and_refreshes_detail() {
        let now = Instant::now();
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.claim(workspace));
        assert!(panels.take_write().is_some());

        panels.finish_write_at(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::PreconditionFailed,
            now,
        );

        assert_eq!(
            panels.notice_at(workspace, now),
            Some("Someone else won; refreshing issue detail")
        );
        assert_eq!(panels.take_request(), Some((workspace, "scribe-5wh1.4".into())));
        assert!(panels.visible(workspace).is_some());
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
    fn open_detail_build_exposes_claim_and_close_verbs() {
        let presentation = PanelPresentation::from_detail(&detail());

        assert_eq!(presentation.verbs(), &[PanelVerb::Claim, PanelVerb::CloseIssue]);
    }

    fn key_down(key: &str, modifiers: gpui::Modifiers) -> gpui::KeyDownEvent {
        gpui::KeyDownEvent {
            keystroke: gpui::Keystroke { modifiers, key: key.into(), key_char: None },
            is_held: false,
            prefer_character_input: false,
        }
    }

    struct PointerEditorProbe {
        editor: Entity<BeadsEditor>,
        root_focus: FocusHandle,
        workspace_id: WorkspaceId,
        colors: BeadsBoardColors,
        field: EditField,
        value: &'static str,
    }

    impl PointerEditorProbe {
        fn route_editor_key(
            &mut self,
            event: &KeyDownEvent,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            match self.editor.update(cx, |editor, editor_cx| editor.route_key(event, editor_cx)) {
                BeadsEditorKeyRoute::Text | BeadsEditorKeyRoute::Inactive => {}
                BeadsEditorKeyRoute::Consumed => {
                    cx.stop_propagation();
                    cx.notify();
                }
                BeadsEditorKeyRoute::Finished => {
                    cx.stop_propagation();
                    window.focus(&self.root_focus, cx);
                    cx.notify();
                }
            }
        }
    }

    impl Render for PointerEditorProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if !self.root_focus.is_focused(window)
                && !self.editor.read(cx).has_keyboard_focus(window, cx)
            {
                window.focus(&self.root_focus, cx);
            }
            let editor_focus = self.editor.read(cx).focus.clone();
            let text = div()
                .w(px(if matches!(self.field, EditField::Title | EditField::SpecId) {
                    48.0
                } else {
                    160.0
                }))
                .text_size(px(16.0))
                .line_height(px(22.0));
            let text = if matches!(self.field, EditField::Title | EditField::SpecId) {
                text.truncate()
            } else {
                text
            };
            div()
                .track_focus(&self.root_focus)
                .on_key_down(cx.listener(Self::route_editor_key))
                .size_full()
                .p(px(8.0))
                .child(div().track_focus(&editor_focus).tab_stop(false).child(editable_text(
                    EditWiring {
                        workspace_id: self.workspace_id,
                        editor: &self.editor,
                        app: cx,
                        write_enabled: true,
                        colors: &self.colors,
                    },
                    "scribe-pointer",
                    self.field,
                    self.value,
                    text,
                )))
        }
    }

    fn editor_probe_window(
        cx: &mut gpui::TestAppContext,
        field: EditField,
        value: &'static str,
    ) -> (WindowHandle<PointerEditorProbe>, Entity<BeadsEditor>) {
        let panels = Arc::new(Mutex::new(BeadsPanels::default()));
        let workspace_id = WorkspaceId::new();
        let colors = BeadsBoardColors::from_theme(
            &chrome_slots([0.12, 0.12, 0.12, 1.0]),
            &[[0.5, 0.5, 0.5, 1.0]; 16],
            1.0,
        );
        let window = cx.update(|app| {
            let panels = Arc::clone(&panels);
            app.open_window(WindowOptions::default(), move |window, app| {
                let editor =
                    app.new(|editor_cx| BeadsEditor::new(Arc::clone(&panels), window, editor_cx));
                app.new(|app| PointerEditorProbe {
                    editor,
                    root_focus: app.focus_handle(),
                    workspace_id,
                    colors,
                    field,
                    value,
                })
            })
            .expect("open pointer editor probe")
        });
        let editor = window
            .update(cx, |probe, _, _| probe.editor.clone())
            .expect("read pointer editor probe");
        (window, editor)
    }

    fn pointer_editor_probe_window(
        cx: &mut gpui::TestAppContext,
    ) -> (WindowHandle<PointerEditorProbe>, Entity<BeadsEditor>) {
        editor_probe_window(cx, EditField::Description, "prefix middle suffix")
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Pointer activation keeps a collapsed caret]]
    #[gpui::test]
    fn pointer_activation_keeps_a_collapsed_native_selection(cx: &mut gpui::TestAppContext) {
        let mut carets = Vec::new();
        for position in
            [point(px(9.0), px(14.0)), point(px(76.0), px(14.0)), point(px(164.0), px(36.0))]
        {
            let (window, editor) = pointer_editor_probe_window(cx);
            cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
                .expect("draw probe");
            let mut test_window = gpui::VisualTestContext::from_window(window.into(), cx);
            test_window.simulate_click(position, Modifiers::default());
            let selection = cx
                .update_window(window.into(), |_, window, app| {
                    editor.update(app, |editor, editor_cx| {
                        editor
                            .selected_text_range(false, window, editor_cx)
                            .expect("active selection")
                    })
                })
                .expect("read native selection");

            assert!(selection.range.is_empty(), "pointer selected the entire field");
            carets.push(selection.range.start);
            test_window.update(|test_window_ref, app| test_window_ref.draw(app).clear());
            test_window.simulate_input("!");
            let input = editor
                .read_with(&test_window, |editor, _| editor.session.input().map(str::to_owned));
            assert!(input.as_deref().is_some_and(|input| {
                input.contains("prefix") && input.contains("middle") && input.contains("suffix")
            }));
        }
        assert_eq!(carets[0], 0);
        assert!(carets[0] < carets[1] && carets[1] < carets[2]);
        assert_eq!(carets[2], "prefix middle suffix".encode_utf16().count());
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Pointer drag updates the native selection]]
    #[gpui::test]
    fn pointer_drag_updates_the_real_native_selection(cx: &mut gpui::TestAppContext) {
        let (window, editor) = editor_probe_window(cx, EditField::Description, "first\nsecond");
        cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
            .expect("draw drag probe");
        let mut test_window = gpui::VisualTestContext::from_window(window.into(), cx);
        test_window.simulate_mouse_down(
            point(px(10.0), px(14.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        test_window.simulate_mouse_move(
            point(px(55.0), px(36.0)),
            MouseButton::Left,
            Modifiers::default(),
        );
        test_window.simulate_mouse_up(
            point(px(55.0), px(36.0)),
            MouseButton::Left,
            Modifiers::default(),
        );

        let selection = cx
            .update_window(window.into(), |_, window, app| {
                editor.update(app, |editor, editor_cx| {
                    editor.selected_text_range(false, window, editor_cx).expect("drag selection")
                })
            })
            .expect("read drag selection");

        assert!(!selection.range.is_empty());
        assert!(selection.range.start < 6 && selection.range.end > 6);
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Pointer release outside extends the native selection]]
    #[gpui::test]
    fn pointer_release_outside_extends_selection_to_both_text_edges(cx: &mut gpui::TestAppContext) {
        let value = "prefix middle suffix";
        for (release, edge) in [(point(px(-10.0), px(14.0)), 0), (point(px(2_000.0), px(14.0)), 1)]
        {
            let (window, editor) = pointer_editor_probe_window(cx);
            cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
                .expect("draw release-out probe");
            let mut test_window = gpui::VisualTestContext::from_window(window.into(), cx);
            test_window.simulate_mouse_down(
                point(px(80.0), px(14.0)),
                MouseButton::Left,
                Modifiers::default(),
            );
            test_window.simulate_mouse_up(release, MouseButton::Left, Modifiers::default());

            let selection = cx
                .update_window(window.into(), |_, window, app| {
                    editor.update(app, |editor, editor_cx| {
                        editor
                            .selected_text_range(false, window, editor_cx)
                            .expect("release-out selection")
                    })
                })
                .expect("read release-out selection");

            if edge == 0 {
                assert_eq!(selection.range.start, 0);
            } else {
                assert_eq!(selection.range.end, value.encode_utf16().count());
            }
            assert!(!selection.range.is_empty());
        }
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Active truncated fields retain the full native layout]]
    #[gpui::test]
    fn active_truncated_fields_keep_full_logical_layout(cx: &mut gpui::TestAppContext) {
        for (field, value) in [
            (EditField::Title, "title-abcdefghijklmnopqrstuvwxyz-0123456789"),
            (EditField::SpecId, "spec-abcdefghijklmnopqrstuvwxyz-0123456789"),
        ] {
            let (window, editor) = editor_probe_window(cx, field, value);
            cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
                .expect("draw narrow truncated probe");
            cx.update_window(window.into(), |_, window, app| window.focus_next(app))
                .expect("focus narrow field");
            cx.dispatch_keystroke(
                window.into(),
                gpui::Keystroke::parse("space").expect("parse Space"),
            );
            cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
                .expect("draw active narrow field");

            let end = value.encode_utf16().count();
            let (bounds, pointer_index) = cx
                .update_window(window.into(), |_, window, app| {
                    editor.update(app, |editor, editor_cx| {
                        let bounds = editor
                            .bounds_for_range(
                                end..end,
                                Bounds::new(
                                    point(px(8.0), px(8.0)),
                                    gpui::size(px(48.0), px(22.0)),
                                ),
                                window,
                                editor_cx,
                            )
                            .expect("logical end bounds");
                        let pointer_index = editor
                            .character_index_for_point(bounds.origin, window, editor_cx)
                            .expect("logical pointer index");
                        (bounds, pointer_index)
                    })
                })
                .expect("read full active layout");

            assert!(bounds.origin.x > px(48.0));
            assert_eq!(pointer_index, end);
        }
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Literal newlines produce distinct native caret bounds]]
    #[gpui::test]
    fn multiline_native_caret_bounds_follow_the_requested_line(cx: &mut gpui::TestAppContext) {
        let (window, editor) = editor_probe_window(cx, EditField::Description, "first\nsecond");
        cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
            .expect("draw multiline probe");
        let mut test_window = gpui::VisualTestContext::from_window(window.into(), cx);
        test_window.simulate_click(point(px(20.0), px(14.0)), Modifiers::default());
        test_window.update(|test_window_ref, app| test_window_ref.draw(app).clear());

        let field_bounds = Bounds::new(point(px(8.0), px(8.0)), gpui::size(px(160.0), px(44.0)));
        let (first, second) = cx
            .update_window(window.into(), |_, window, app| {
                editor.update(app, |editor, editor_cx| {
                    (
                        editor
                            .bounds_for_range(0..0, field_bounds, window, editor_cx)
                            .expect("first-line caret bounds"),
                        editor
                            .bounds_for_range(6..6, field_bounds, window, editor_cx)
                            .expect("second-line caret bounds"),
                    )
                })
            })
            .expect("read multiline caret bounds");

        assert_eq!(first.origin.y, px(8.0));
        assert_eq!(second.origin.y, px(30.0));
        assert_ne!(first.origin, second.origin);
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Inserted text refreshes native layout]]
    #[gpui::test]
    fn native_layout_tracks_text_inserted_after_activation(cx: &mut gpui::TestAppContext) {
        let (window, editor) = editor_probe_window(cx, EditField::Description, "ab\ncd");
        cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
            .expect("draw insertion probe");
        let mut test_window = gpui::VisualTestContext::from_window(window.into(), cx);
        test_window.simulate_click(point(px(164.0), px(36.0)), Modifiers::default());
        test_window.update(|test_window_ref, app| test_window_ref.draw(app).clear());
        test_window.simulate_input("X");
        test_window.update(|test_window_ref, app| test_window_ref.draw(app).clear());

        let caret = cx
            .update_window(window.into(), |_, window, app| {
                editor.update(app, |editor, editor_cx| {
                    editor.bounds_for_range(
                        6..6,
                        Bounds::new(point(px(8.0), px(8.0)), gpui::size(px(160.0), px(44.0))),
                        window,
                        editor_cx,
                    )
                })
            })
            .expect("read inserted caret bounds")
            .expect("current layout maps inserted text");

        assert_eq!(caret.origin.y, px(30.0));
        assert!(caret.origin.x > px(8.0));
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Marked composition refreshes native layout]]
    #[gpui::test]
    fn native_layout_tracks_marked_composition_after_activation(cx: &mut gpui::TestAppContext) {
        let (window, editor) = editor_probe_window(cx, EditField::Description, "ab\ncd");
        cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
            .expect("draw composition probe");
        let mut test_window = gpui::VisualTestContext::from_window(window.into(), cx);
        test_window.simulate_click(point(px(164.0), px(36.0)), Modifiers::default());
        test_window.update(|test_window_ref, app| test_window_ref.draw(app).clear());
        test_window.update(|test_window_ref, app| {
            editor.update(app, |editor, editor_cx| {
                editor.replace_and_mark_text_in_range(None, "XY", None, test_window_ref, editor_cx);
            });
        });
        test_window.update(|test_window_ref, app| test_window_ref.draw(app).clear());

        let caret = cx
            .update_window(window.into(), |_, window, app| {
                editor.update(app, |editor, editor_cx| {
                    editor.bounds_for_range(
                        7..7,
                        Bounds::new(point(px(8.0), px(8.0)), gpui::size(px(160.0), px(44.0))),
                        window,
                        editor_cx,
                    )
                })
            })
            .expect("read composition caret bounds")
            .expect("current layout maps marked composition");

        assert_eq!(caret.origin.y, px(30.0));
        assert!(caret.origin.x > px(8.0));
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Keyboard activation enters the shared editor]]
    #[gpui::test]
    fn space_activation_survives_a_real_focus_repair_render(cx: &mut gpui::TestAppContext) {
        let (window, editor) = pointer_editor_probe_window(cx);
        cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
            .expect("draw probe");
        cx.update_window(window.into(), |_, window, app| window.focus_next(app))
            .expect("focus editable field");
        cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
            .expect("render between focus and activation");

        cx.dispatch_keystroke(window.into(), gpui::Keystroke::parse("space").expect("parse Space"));

        assert_eq!(
            editor.read_with(cx, |editor, _| editor.session.input().map(str::to_owned)),
            Some("prefix middle suffix".to_owned())
        );
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Accessible click enters the shared editor]]
    #[gpui::test]
    fn accessible_click_enters_the_shared_editor(cx: &mut gpui::TestAppContext) {
        let (window, editor) = pointer_editor_probe_window(cx);
        let workspace_id =
            window.update(cx, |probe, _, _| probe.workspace_id).expect("read probe workspace");
        cx.update_window(window.into(), |_, window, app| {
            let state = EditableTextState {
                workspace_id,
                editor: editor.clone(),
                issue_id: "scribe-pointer".to_owned(),
                field: EditField::Description,
                value: "prefix middle suffix".to_owned(),
                layout: TextLayout::default(),
            };
            handle_editable_accessible_action(AccessibleAction::Click, None, &state, window, app);
        })
        .expect("dispatch accessible click");

        assert_eq!(
            editor.read_with(cx, |editor, _| editor.session.input().map(str::to_owned)),
            Some("prefix middle suffix".to_owned())
        );
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Active title Enter commits through the shared input]]
    #[gpui::test]
    fn active_title_enter_commits_through_the_shared_input(cx: &mut gpui::TestAppContext) {
        let (window, editor) = editor_probe_window(cx, EditField::Title, "saved title");
        cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
            .expect("draw title probe");
        let mut test_window = gpui::VisualTestContext::from_window(window.into(), cx);
        test_window.simulate_click(point(px(48.0), px(14.0)), Modifiers::default());
        test_window.update(|test_window_ref, app| test_window_ref.draw(app).clear());
        test_window.simulate_input(" revised");

        assert!(editor.read_with(&test_window, |editor, _| {
            editor.session.input().is_some_and(|input| input != "saved title")
        }));
        cx.dispatch_keystroke(window.into(), gpui::Keystroke::parse("enter").expect("parse Enter"));

        assert!(editor.read_with(cx, |editor, _| editor.session.input().is_none()));
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Grapheme-aware deletion]]
    #[test]
    fn backspace_removes_one_grapheme_without_clearing_the_draft() {
        let mut edit = EditSession::default();
        edit.begin(WorkspaceId::new(), "scribe-5wh1.13", EditField::Notes, "draft 👩‍👩‍👧‍👦");

        edit.backspace();

        assert_eq!(edit.input(), Some("draft "));
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Grapheme-aware cursor navigation]]
    #[test]
    fn navigation_selection_and_delete_keep_graphemes_intact() {
        let mut edit = EditSession::default();
        edit.begin(WorkspaceId::new(), "scribe-5wh1.13", EditField::Notes, "a👩‍👩‍👧‍👦b");

        edit.move_left(false);
        edit.move_left(true);
        edit.backspace();
        edit.delete();

        assert_eq!(edit.input(), Some("a"));
        edit.select_all();
        edit.backspace();
        assert_eq!(edit.input(), Some(""));
    }

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#IME ranges preserve adjacent graphemes]]
    #[gpui::test]
    fn ime_marked_ranges_preserve_adjacent_combining_graphemes(cx: &mut gpui::TestAppContext) {
        let (window, editor) = editor_probe_window(cx, EditField::Description, "a\u{301}b");
        let workspace_id =
            window.update(cx, |probe, _, _| probe.workspace_id).expect("read probe workspace");
        cx.update_window(window.into(), |_, window, app| {
            editor.update(app, |editor, editor_cx| {
                editor.begin(
                    EditTarget {
                        workspace_id,
                        issue_id: "scribe-pointer",
                        field: EditField::Description,
                        value: "a\u{301}b",
                    },
                    BeginEdit { cursor: Some(0), layout: None, extend_selection: false },
                    window,
                    editor_cx,
                );
                editor.replace_and_mark_text_in_range(
                    Some(1..2),
                    "\u{302}",
                    Some(1..1),
                    window,
                    editor_cx,
                );
                assert_eq!(editor.session.input(), Some("a\u{302}b"));
                assert_eq!(editor.marked_text_range(window, editor_cx), Some(1..2));
                editor.replace_text_in_range(None, "", window, editor_cx);
                assert_eq!(editor.session.input(), Some("ab"));
            });
        })
        .expect("replace combining mark through native input");
    }

    #[test]
    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Enter commit matrix]]
    fn editor_enter_matrix_distinguishes_single_and_multiline_fields() {
        let plain_enter = key_down("enter", gpui::Modifiers::default());
        let modified_enter =
            key_down("enter", gpui::Modifiers { shift: true, ..gpui::Modifiers::default() });

        assert_eq!(edit_key_action(EditField::Title, &plain_enter), EditKeyAction::Commit);
        assert_eq!(edit_key_action(EditField::Description, &plain_enter), EditKeyAction::Text);
        assert_eq!(edit_key_action(EditField::Description, &modified_enter), EditKeyAction::Commit);
        assert_eq!(
            edit_key_action(
                EditField::Title,
                &key_down("x", gpui::Modifiers { alt: true, ..gpui::Modifiers::default() },),
            ),
            EditKeyAction::Text
        );
    }

    #[test]
    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Passage changes commit drafts]]
    fn switching_passages_commits_the_previous_value() {
        let workspace_id = WorkspaceId::new();
        let mut edit = EditSession::default();
        assert_eq!(edit.begin(workspace_id, "scribe-5wh1.13", EditField::Title, "Old title"), None);
        edit.replace_all("New title");

        let committed = edit.begin(workspace_id, "scribe-5wh1.13", EditField::Description, "Body");

        assert_eq!(
            committed,
            Some(BeadsEditIntent {
                workspace_id,
                issue_id: "scribe-5wh1.13".into(),
                verb: BeadsIssueWrite::SetTitle { title: "New title".into() },
            })
        );
        assert_eq!(edit.input(), Some("Body"));
    }

    #[test]
    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Repeated clicks preserve drafts]]
    fn clicking_the_active_passage_keeps_its_draft() {
        let workspace_id = WorkspaceId::new();
        let mut edit = EditSession::default();
        edit.begin(workspace_id, "scribe-5wh1.13", EditField::Title, "Old title");
        edit.replace_all("Draft title");

        let committed = edit.begin(workspace_id, "scribe-5wh1.13", EditField::Title, "Old title");

        assert_eq!(committed, None);
        assert_eq!(edit.input(), Some("Draft title"));
    }

    #[test]
    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Escape cancels editing]]
    fn escape_cancels_without_emitting_a_write() {
        let workspace_id = WorkspaceId::new();
        let mut edit = EditSession::default();
        edit.begin(workspace_id, "scribe-5wh1.13", EditField::Notes, "Saved notes");
        edit.replace_all("Half typed");

        edit.cancel();

        assert_eq!(edit.finish(), None);
        assert_eq!(edit.input(), None);
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
