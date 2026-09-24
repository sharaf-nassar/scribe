//! Beads issue panel state, inline editing, guarded writes, and rendering.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui::{
    AccessibleAction, Animation, AnimationExt as _, AnyElement, App, Bounds, BoxShadow, Context,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, FontWeight,
    HighlightStyle, KeyDownEvent, MouseButton, Pixels, Point, Rgba, Role, SharedString, StyledText,
    Subscription, TextLayout, UTF16Selection, UnderlineStyle, Window, canvas, combine_highlights,
    div, fill, hsla, linear_color_stop, linear_gradient, prelude::*, px, size,
};
use scribe_common::ids::WorkspaceId;
use scribe_common::protocol::{
    BeadsBoardItem, BeadsBoardSnapshot, BeadsBoardState, BeadsIssueComment, BeadsIssueDetail,
    BeadsIssueQueue, BeadsIssueQueueBasis, BeadsIssueWrite, BeadsIssueWriteGuards,
    BeadsIssueWriteResult,
};

use crate::animation::AnimationSettings;
use crate::beads_board::{BeadsBoardColors, CardDragState, card_drop_verb};
use crate::beads_board_a2::queue_name;
use crate::fonts::TERMINAL_FONT_FAMILY;
use crate::layout::Rect;
use crate::settings::window::{utf8_range_to_utf16, utf16_range_to_utf8};
use unicode_segmentation::UnicodeSegmentation;

const PANEL_WIDTH: f32 = 560.0;
const PANEL_MIN_WIDTH: f32 = 400.0;
const PANEL_MARGIN: f32 = 12.0;
const PANEL_BOARD_GAP: f32 = 4.0;
const PANEL_OPEN_DURATION: Duration = Duration::from_millis(120);
const NOTICE_DURATION: Duration = Duration::from_secs(5);
/// A notice toast is this wide at text scale 1.0 and hangs this far under
/// the board. A narrower region or less room under the board gets no toast.
const NOTICE_WIDTH: f32 = 340.0;
const NOTICE_BOARD_GAP: f32 = 8.0;
const NOTICE_MIN_WIDTH: f32 = 220.0;
const NOTICE_MIN_ROOM: f32 = 44.0;
const NOTICE_PAD_Y: f32 = 12.0;
const NOTICE_PAD_LEFT: f32 = 12.0;
/// Tighter than the left: the close mark's ink sits inside its 20px box, so
/// this leaves it as far from the right edge as the tone glyph is from the left.
const NOTICE_PAD_RIGHT: f32 = 6.0;
/// The headline's line box, which the tone glyph and both controls centre on.
const NOTICE_TITLE_LINE: f32 = 18.0;
const NOTICE_UNDO_WIDTH: f32 = 56.0;
const NOTICE_UNDO_HEIGHT: f32 = 24.0;
const NOTICE_DISMISS_SIZE: f32 = 20.0;
const NOTICE_ACTION_GAP: f32 = 6.0;
/// How long a toast stays once the pointer that held it leaves.
const NOTICE_LINGER: Duration = Duration::from_secs(2);
/// A hold is a courtesy to a reader, not a pin. Past this the toast leaves
/// even under the pointer, so one whose slot vanished mid-hover, and so never
/// hears the pointer leave, cannot stay up (and keep its board open) forever.
const NOTICE_HOLD_MAX: Duration = Duration::from_secs(30);
/// The explanation may wrap this far before it ellipsizes; the accessible
/// name keeps all of it.
const NOTICE_MESSAGE_LINES: usize = 3;
const NOTICE_ENTRANCE: Duration = Duration::from_millis(150);
/// The embedded icon face the settings window already draws its glyphs from.
const NERD_SYMBOLS: &str = "Symbols Nerd Font Mono";
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
        y: geometry.y,
        width: (geometry.width - start_width).mul_add(progress, start_width),
        opacity: 0.25 + 0.75 * progress,
    }
}

fn panel_open_animation(settings: AnimationSettings) -> Animation {
    settings.transition(PANEL_OPEN_DURATION)
}

#[derive(Debug, Clone, PartialEq)]
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

/// One workspace's five-second outcome toast.
///
/// It reads top to bottom: a headline that says what happened, one plain
/// sentence that says why or what it means, then the issue it happened to.
/// Tool output never reaches any of the three; see [`reason_sentence`].
#[derive(Debug, Clone, PartialEq)]
pub struct PanelNotice {
    tone: NoticeTone,
    title: String,
    message: Option<String>,
    subject: Option<NoticeSubject>,
    expires_at: Instant,
    /// When the pointer came to rest on the toast, while it still does: one
    /// being read never leaves from under it, up to [`NOTICE_HOLD_MAX`].
    held_since: Option<Instant>,
    undo: Option<UndoClose>,
}

/// The issue a notice names: its id and, when the gesture knew it, its title.
#[derive(Debug, Clone, PartialEq)]
struct NoticeSubject {
    id: String,
    title: String,
}

/// What a notice reports. Each tone pairs its hue with its own glyph, so the
/// state never rests on colour alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum NoticeTone {
    /// A write landed.
    Success,
    /// The tracker placed or removed something on its own.
    Info,
    /// The outcome is unknown, or someone else's change won.
    Warning,
    /// A write failed or never left the client.
    Error,
}

/// A close's guarded reopen. Its deadline is its own: holding the toast
/// keeps the words up, never the five-second Undo window.
#[derive(Debug, Clone, PartialEq)]
struct UndoClose {
    issue_id: String,
    assignee: String,
    deadline: Instant,
}

const TIMEOUT_TITLE: &str = "Beads didn’t respond in time";
const TIMEOUT_MESSAGE: &str = "The change may still have saved. Reloading to check.";

impl PanelNotice {
    fn at(tone: NoticeTone, title: impl Into<String>, now: Instant) -> Self {
        Self {
            tone,
            title: title.into(),
            message: None,
            subject: None,
            expires_at: now + NOTICE_DURATION,
            held_since: None,
            undo: None,
        }
    }

    fn saying(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    fn about(mut self, id: &str, title: &str) -> Self {
        self.subject = Some(NoticeSubject { id: id.to_owned(), title: title.trim().to_owned() });
        self
    }

    fn closed_at(issue_id: &str, title: &str, assignee: String, now: Instant) -> Self {
        Self {
            undo: Some(UndoClose {
                issue_id: issue_id.to_owned(),
                assignee,
                deadline: now + NOTICE_DURATION,
            }),
            ..Self::at(NoticeTone::Success, "Issue closed", now).about(issue_id, title)
        }
    }

    /// The Undo this toast still offers at `now`, if any.
    fn live_undo(&self, now: Instant) -> Option<&UndoClose> {
        self.undo.as_ref().filter(|undo| now < undo.deadline)
    }

    /// Identifies what the toast says, so new words replay the entrance while
    /// an identical replacement, such as the server's timeout landing after
    /// the client's own deadline already said the same, stays still.
    fn words_key(&self) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.tone.hash(&mut hasher);
        self.title.hash(&mut hasher);
        self.message.hash(&mut hasher);
        self.subject.as_ref().map(|subject| (&subject.id, &subject.title)).hash(&mut hasher);
        hasher.finish()
    }

    /// Everything the toast shows, in reading order, for its accessible name.
    /// Every message already ends its own sentence.
    fn spoken(&self) -> String {
        let mut spoken = format!("{}.", self.title);
        if let Some(message) = &self.message {
            spoken.push(' ');
            spoken.push_str(message);
        }
        if let Some(subject) = &self.subject {
            spoken.push(' ');
            spoken.push_str(&subject.id);
            if !subject.title.is_empty() {
                spoken.push_str(": ");
                spoken.push_str(&subject.title);
            }
        }
        spoken
    }

    fn active_at(&self, now: Instant) -> bool {
        now < self.expires_at || self.held_since.is_some_and(|since| now < since + NOTICE_HOLD_MAX)
    }

    fn active(&self) -> bool {
        self.active_at(Instant::now())
    }
}

/// A failed write's headline, named for what the user was doing.
fn failure_title(verb: &BeadsIssueWrite) -> &'static str {
    match verb {
        BeadsIssueWrite::SetTitle { .. } => "Couldn’t save the title",
        BeadsIssueWrite::SetDescription { .. } => "Couldn’t save the description",
        BeadsIssueWrite::SetAcceptance { .. } => "Couldn’t save the acceptance criteria",
        BeadsIssueWrite::SetNotes { .. } => "Couldn’t save the notes",
        BeadsIssueWrite::SetDesign { .. } => "Couldn’t save the design",
        BeadsIssueWrite::SetSpecId { .. } => "Couldn’t save the spec",
        BeadsIssueWrite::SetPriority { .. } => "Couldn’t change the priority",
        BeadsIssueWrite::SetType { .. } => "Couldn’t change the type",
        BeadsIssueWrite::SetLabels { .. } => "Couldn’t save the labels",
        BeadsIssueWrite::SetStatus { .. } => "Couldn’t change the status",
        BeadsIssueWrite::Claim => "Couldn’t claim the issue",
        BeadsIssueWrite::CloseIssue => "Couldn’t close the issue",
        BeadsIssueWrite::UndoClose => "Couldn’t reopen the issue",
        BeadsIssueWrite::AddComment { .. } => "Couldn’t add the comment",
    }
}

/// The longest reason a toast will lay out; three clamped lines show less.
const REASON_MAX_CHARS: usize = 240;

/// bd's own reason as one readable sentence, or `None` when nothing in it is
/// fit to show. The server's `bd failed:` framing and a leading `Error`
/// label go, only the first line stays, and anything still shaped like JSON
/// is withheld rather than printed: a server older than this client may
/// still forward bd's raw envelope.
fn reason_sentence(reason: &str) -> Option<String> {
    let reason = reason.trim();
    let reason = reason.strip_prefix("bd failed:").unwrap_or(reason);
    let line = reason.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = match (line.get(..5), line.get(5..)) {
        (Some(word), Some(rest))
            if word.eq_ignore_ascii_case("error") && rest.starts_with([':', ' ']) =>
        {
            rest.trim_start_matches([':', ' '])
        }
        _ => line,
    };
    if line.is_empty() || line.starts_with(['{', '[']) || line.starts_with("exited with") {
        return None;
    }
    let clipped = line.chars().count() > REASON_MAX_CHARS;
    let mut sentence: String = line.chars().take(REASON_MAX_CHARS).collect();
    // Sentence case, except where the sentence opens on bd's own lowercase name.
    if !sentence.starts_with("bd ") {
        let mut chars = sentence.chars();
        if let Some(first) = chars.next() {
            sentence = first.to_uppercase().chain(chars).collect();
        }
    }
    if clipped {
        sentence.push('…');
    } else if !sentence.ends_with(['.', '!', '?']) {
        sentence.push('.');
    }
    Some(sentence)
}

const LANE_NAMES: [&str; 5] = ["Backlog", "Ready", "In progress", "Blocked", "Done"];

/// Why the tracker filed a dropped card somewhere other than its target.
fn placement_reason(lane: u8) -> &'static str {
    match lane {
        0 => "Beads doesn’t list it as ready yet.",
        1 => "Nothing blocks it, so Beads lists it as ready.",
        2 => "Its status is in progress.",
        3 => "Another issue is blocking it.",
        _ => "Its status is closed.",
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

pub(crate) fn previous_grapheme_boundary(text: &str, offset: usize) -> usize {
    text.grapheme_indices(true)
        .rev()
        .find_map(|(index, _)| (index < offset).then_some(index))
        .unwrap_or(0)
}

pub(crate) fn next_grapheme_boundary(text: &str, offset: usize) -> usize {
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

    fn visual_feedback(
        &self,
        workspace_id: WorkspaceId,
        issue_id: &str,
        field: EditField,
        color: Rgba,
    ) -> EditorVisualFeedback {
        self.session
            .active
            .as_ref()
            .filter(|edit| {
                edit.workspace_id == workspace_id
                    && edit.issue_id == issue_id
                    && edit.field == field
            })
            .map_or_else(EditorVisualFeedback::default, |edit| {
                editor_visual_feedback(edit.selection.clone(), edit.marked.clone(), color)
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
    /// The issue's title as the gesture saw it. It names the issue in the
    /// outcome toast and never leaves the client.
    pub title: String,
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
        // A pending navigation makes the target the only interesting reply.
        // The issue the panel is still displaying has its own request in
        // flight, and applying that late answer would repaint the pane the
        // reader just navigated away from.
        if let Some(target) = self.pending_navigation.get(&workspace_id) {
            if target != issue_id {
                return;
            }
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
            PanelNotice::at(NoticeTone::Info, "Issue not found", Instant::now())
                .saying("Beads can’t find it anymore, so its panel closed.")
                .about(issue_id, panel.title()),
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
            title: panel.title().to_owned(),
        };
        self.park_write(intent)
    }

    /// Translate one completed board gesture into the existing guarded write
    /// queue. Rejected targets never enter the queue.
    pub fn queue_card_drop(&mut self, drag: &CardDragState) -> bool {
        if !self.write_enabled {
            return false;
        }
        let Some(target_lane) = drag.hovered_lane else { return false };
        let Some(verb) = card_drop_verb(drag.source_lane, target_lane) else { return false };
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
            title: drag.source.title.clone(),
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

    pub fn write_send_failed(&mut self, workspace_id: WorkspaceId, issue_id: &str) {
        let key = (workspace_id, issue_id.to_owned());
        let intent = self.in_flight_writes.remove(&key);
        self.write_deadlines.remove(&key);
        let headline = intent
            .as_ref()
            .map_or("Couldn’t send the change", |intent| failure_title(&intent.verb));
        self.notices.insert(
            workspace_id,
            PanelNotice::at(NoticeTone::Error, headline, Instant::now())
                .saying("Scribe couldn’t reach its server, so nothing was saved.")
                .about(issue_id, intent.as_ref().map_or("", |intent| intent.title.as_str())),
        );
    }

    pub fn classifier_won(&mut self, workspace_id: WorkspaceId, card: &BeadsBoardItem, lane: u8) {
        let lane_name = LANE_NAMES.get(usize::from(lane)).copied().unwrap_or("another lane");
        self.notices.insert(
            workspace_id,
            PanelNotice::at(
                NoticeTone::Info,
                format!("Moved to {lane_name} instead"),
                Instant::now(),
            )
            .saying(placement_reason(lane))
            .about(&card.id, &card.title),
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
        let title = intent.title.as_str();
        match result {
            BeadsIssueWriteResult::Applied { .. }
                if matches!(intent.verb, BeadsIssueWrite::CloseIssue) =>
            {
                self.open.remove(&workspace_id);
                let assignee = intent.guards.if_assignee.clone().unwrap_or_default();
                self.notices
                    .insert(workspace_id, PanelNotice::closed_at(issue_id, title, assignee, now));
                self.last_opened = Some(workspace_id);
            }
            BeadsIssueWriteResult::Applied { .. } => {
                self.notices.remove(&workspace_id);
                self.refresh_open_issue(workspace_id, issue_id);
            }
            BeadsIssueWriteResult::PreconditionFailed => {
                self.notices.insert(
                    workspace_id,
                    PanelNotice::at(NoticeTone::Warning, "Issue changed elsewhere", now)
                        .saying(
                            "It changed since Scribe last loaded it, so your change wasn’t saved.",
                        )
                        .about(issue_id, title),
                );
                self.refresh_open_issue(workspace_id, issue_id);
            }
            BeadsIssueWriteResult::Failed { reason } if reason.contains("timed out") => {
                self.notices.insert(
                    workspace_id,
                    PanelNotice::at(NoticeTone::Warning, TIMEOUT_TITLE, now)
                        .saying(TIMEOUT_MESSAGE)
                        .about(issue_id, title),
                );
                self.force_convergence(workspace_id, issue_id);
            }
            BeadsIssueWriteResult::Failed { reason } => {
                // The toast shows bd's reason cleaned into a sentence; the
                // log keeps it verbatim for whoever has to chase it.
                tracing::warn!(%workspace_id, issue_id, reason, "Beads issue write failed");
                let message = reason_sentence(&reason)
                    .unwrap_or_else(|| "Beads reported an error, so nothing was saved.".to_owned());
                self.notices.insert(
                    workspace_id,
                    PanelNotice::at(NoticeTone::Error, failure_title(&intent.verb), now)
                        .saying(message)
                        .about(issue_id, title),
                );
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
            let title = self
                .in_flight_writes
                .get(&key)
                .map(|intent| intent.title.clone())
                .unwrap_or_default();
            self.notices.insert(
                *workspace_id,
                PanelNotice::at(NoticeTone::Warning, TIMEOUT_TITLE, now)
                    .saying(TIMEOUT_MESSAGE)
                    .about(issue_id, &title),
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
        let Some(undo) = notice.live_undo(now).cloned() else {
            if notice.active_at(now) {
                self.notices.insert(workspace_id, notice);
            }
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
            title: notice.subject.map(|subject| subject.title).unwrap_or_default(),
        });
        true
    }

    /// Take a workspace's toast down early, leaving any open panel alone.
    pub fn dismiss_notice(&mut self, workspace_id: WorkspaceId) -> bool {
        self.notices.remove(&workspace_id).is_some()
    }

    /// Hold a workspace's toast while the pointer rests on it. Letting go
    /// leaves it up for at least [`NOTICE_LINGER`] more, so it never vanishes
    /// the instant the pointer moves off.
    pub fn hold_notice(&mut self, workspace_id: WorkspaceId, held: bool) {
        self.hold_notice_at(workspace_id, held, Instant::now());
    }

    fn hold_notice_at(&mut self, workspace_id: WorkspaceId, held: bool, now: Instant) {
        let Some(notice) = self.notices.get_mut(&workspace_id) else { return };
        if held {
            if notice.held_since.is_none() {
                notice.held_since = Some(now);
            }
        } else if notice.held_since.take().is_some() {
            notice.expires_at = notice.expires_at.max(now + NOTICE_LINGER);
        }
    }

    pub fn undo_available(&self, workspace_id: WorkspaceId) -> bool {
        let now = Instant::now();
        self.notices.get(&workspace_id).is_some_and(|notice| notice.live_undo(now).is_some())
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

    fn notice_at(&self, workspace_id: WorkspaceId, now: Instant) -> Option<&PanelNotice> {
        self.notices.get(&workspace_id).filter(|notice| notice.active_at(now))
    }

    /// The live notice a workspace's toast paints, if any.
    pub fn active_notice(&self, workspace_id: WorkspaceId) -> Option<&PanelNotice> {
        self.notice_at(workspace_id, Instant::now())
    }

    /// Drop every toast whose five seconds are up, reporting whether one went.
    pub fn expire_notices(&mut self) -> bool {
        self.expire_notices_at(Instant::now())
    }

    fn expire_notices_at(&mut self, now: Instant) -> bool {
        let mut changed = false;
        self.notices.retain(|_, notice| {
            // A held close toast outlives its Undo: the button goes at the
            // exact deadline even while the words stay up.
            if notice.undo.is_some() && notice.live_undo(now).is_none() {
                notice.undo = None;
                changed = true;
            }
            let live = notice.active_at(now);
            changed |= !live;
            live
        });
        changed
    }

    pub fn sync_board(&mut self, workspace_id: WorkspaceId, state: &BeadsBoardState) -> bool {
        if matches!(state, BeadsBoardState::NotDetected) {
            self.reconcile_snapshot(workspace_id, false);
            self.pending_navigation.remove(&workspace_id);
            self.pick_rows.remove(&workspace_id);
            if self.open.remove(&workspace_id).is_none() {
                return false;
            }
            self.notices.insert(
                workspace_id,
                PanelNotice::at(NoticeTone::Info, "Beads project not found", Instant::now())
                    .saying("This workspace is no longer inside a Beads project, so the issue panel closed."),
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
        self.navigate_to_issue(workspace_id, issue_id)
    }

    /// Retarget an open panel at `issue_id`, whatever selected it.
    ///
    /// A Flow node click reaches an issue the open detail never listed, so
    /// eligibility belongs to the caller: the board has already proved the
    /// node is in its frozen graph. What is shared is the fence — the target
    /// is recorded before the request leaves, so `update` can discard the
    /// answer to the issue the reader navigated away from.
    pub fn navigate_to_issue(&mut self, workspace_id: WorkspaceId, issue_id: &str) -> bool {
        if !self.detail_enabled || !self.open.contains_key(&workspace_id) {
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

pub(crate) fn snapshot_card<'a>(
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
        // A detail read resolves the parent epic to a title, never an id, so a
        // card synthesized here cannot state Flow eligibility. Read it from the
        // board snapshot's card instead.
        parent_epic_id: None,
        updated_at: detail.updated_at.clone(),
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

/// One workspace's issue-detail overlay as a cached GPUI view, the panel
/// twin of `beads_board::BoardStrip`.
///
/// The wrapped subtree replays until this entity is notified. Every editor
/// mutation path already forces its own uncached frame — text input and IME
/// go through `replace_text_in_range`/`unmark_text` which call
/// `window.refresh()`, caret and selection keys land in the editor key
/// router's `Consumed` arm which does the same, and closing the editor moves
/// focus, which refreshes from inside GPUI — so the live editor never
/// depends on this cache invalidating. Everything else the panel paints is
/// owned data diffed by [`PanelLayer::same_inputs`] on the root's frame.
pub struct PanelLayer {
    pub inputs: PanelLayerInputs,
}

/// The owned inputs one panel overlay paints from, stored on [`PanelLayer`]
/// and rebuilt into a borrowed [`BeadsPanelRender`] each real render.
pub struct PanelLayerInputs {
    pub region: Rect,
    pub board: Rect,
    pub workspace_id: WorkspaceId,
    pub state: Arc<Mutex<BeadsPanels>>,
    pub editor: Entity<BeadsEditor>,
    pub terminal_focus: FocusHandle,
    pub write_enabled: bool,
    pub scale: f32,
    pub colors: BeadsBoardColors,
    pub animations: AnimationSettings,
    pub panel: Option<BeadsPanel>,
    pub notice: Option<PanelNotice>,
}

impl PanelLayer {
    /// True when a fresh render would paint exactly what the cached subtree
    /// already shows. The editor entity and shared stores are deliberately
    /// absent: their identities are process-stable and their visual changes
    /// arrive through `window.refresh()`, never through this diff.
    pub fn same_inputs(&self, inputs: &PanelLayerInputs) -> bool {
        let ours = &self.inputs;
        ours.region == inputs.region
            && ours.board == inputs.board
            && ours.workspace_id == inputs.workspace_id
            && ours.terminal_focus == inputs.terminal_focus
            && ours.write_enabled == inputs.write_enabled
            // Exact-bits, as in `BoardStrip::same_inputs`: any change
            // repaints, equal bits paint equal panels.
            && ours.scale.to_bits() == inputs.scale.to_bits()
            && ours.colors == inputs.colors
            && ours.animations == inputs.animations
            && ours.panel == inputs.panel
            && ours.notice == inputs.notice
    }
}

impl gpui::Render for PanelLayer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let app: &App = cx;
        let inputs = &self.inputs;
        let wiring = BeadsPanelRender {
            region: inputs.region,
            board: inputs.board,
            workspace_id: inputs.workspace_id,
            state: Arc::clone(&inputs.state),
            editor: inputs.editor.clone(),
            terminal_focus: inputs.terminal_focus.clone(),
            app,
            write_enabled: inputs.write_enabled,
            scale: inputs.scale,
            colors: inputs.colors,
            animations: inputs.animations,
        };
        let mut layers =
            inputs.panel.as_ref().map_or_else(Vec::new, |panel| render(panel, &wiring));
        // The toast paints after the panel, so where a narrow section puts it
        // over the panel's corner it stacks on top and takes the press.
        layers.extend(inputs.notice.as_ref().and_then(|notice| render_notice(notice, &wiring)));
        // The overlay children position themselves absolutely in band
        // coordinates; the wrapper spans the band so those coordinates keep
        // meaning what they meant when the root painted them inline.
        gpui::div().absolute().inset_0().children(layers)
    }
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
    // The panel starts below the board. Keeping its backdrop there leaves a
    // Flow strip both visible and clickable while retaining outside-click
    // dismissal for the panel's own part of the region.
    let backdrop = div()
        .id(SharedString::from(format!("beads-detail-backdrop-{workspace_id}")))
        .absolute()
        .left(px(wiring.region.x))
        .top(px(layout.geometry.y))
        .w(px(wiring.region.width))
        .h(px((wiring.region.y + wiring.region.height - layout.geometry.y).max(0.0)))
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

/// Where a region's toast hangs, as `(x, top, width)`: one gap under the
/// board, inset from the section's right edge by [`PANEL_MARGIN`]. Below the
/// board rather than on it, because the board's right edge holds the Blocked
/// and Done tabs a drop is aimed at.
fn notice_slot(region: Rect, board: Rect, scale: f32) -> Option<(f32, f32, f32)> {
    let width = (NOTICE_WIDTH * scale).min(region.width - PANEL_MARGIN * 2.0);
    let top = board.y + board.height + NOTICE_BOARD_GAP;
    let room = region.y + region.height - top;
    (width >= NOTICE_MIN_WIDTH && room >= NOTICE_MIN_ROOM * scale).then_some((
        region.x + region.width - PANEL_MARGIN - width,
        top,
        width,
    ))
}

/// Paint one workspace's notice as a toast in its section's top-right
/// corner. The headline, the plain sentence under it, and the issue it
/// names each take their own line and their own weight, and the tone reads
/// from a glyph as well as a hue.
pub fn render_notice(notice: &PanelNotice, wiring: &BeadsPanelRender<'_>) -> Option<AnyElement> {
    let scale = wiring.scale;
    let (x, top, width) = notice_slot(wiring.region, wiring.board, scale)?;
    let colors = &wiring.colors;
    let workspace_id = wiring.workspace_id;
    let urgent = matches!(notice.tone, NoticeTone::Warning | NoticeTone::Error);
    let toast = div()
        .id(SharedString::from(format!("beads-notice-{workspace_id}")))
        .debug_selector(|| "beads-notice".to_owned())
        .role(if urgent { Role::Alert } else { Role::Status })
        .aria_label(notice.spoken())
        .occlude()
        .absolute()
        .left(px(x))
        .top(px(top))
        .w(px(width))
        .flex()
        .items_start()
        .gap(at(scale, 10.0))
        .py(at(scale, NOTICE_PAD_Y))
        .pl(at(scale, NOTICE_PAD_LEFT))
        .pr(at(scale, NOTICE_PAD_RIGHT))
        .rounded(px(4.0))
        .border_1()
        .border_color(colors.card_border_hover)
        .bg(linear_gradient(
            180.0,
            linear_color_stop(colors.card_top, 0.0),
            linear_color_stop(colors.card, 1.0),
        ))
        // The approved panel mock's lift, scaled down to a toast: an offset
        // shadow reads on a dark ground where a flat 10% one vanishes.
        .shadow(vec![
            BoxShadow::new(px(0.0), px(8.0), hsla(0.0, 0.0, 0.0, 0.45)).blur_radius(px(24.0)),
            BoxShadow::new(px(0.0), px(2.0), hsla(0.0, 0.0, 0.0, 0.3)).blur_radius(px(6.0)),
        ])
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        // The press stops here, so its release does too (scribe-uu2y),
        // except under a lifted card, whose release `release_board` owns.
        .on_mouse_up(MouseButton::Left, |_, _window, app| {
            if !app.has_active_drag() {
                app.stop_propagation();
            }
        })
        .on_hover({
            let state = Arc::clone(&wiring.state);
            move |hovered, _window, _app| {
                if let Ok(mut panels) = state.lock() {
                    panels.hold_notice(workspace_id, *hovered);
                }
            }
        })
        .child(notice_glyph(notice.tone, colors, scale))
        .child(notice_text(notice, colors, scale))
        .child(
            div()
                .flex_none()
                .h(at(scale, NOTICE_TITLE_LINE))
                .flex()
                .items_center()
                .gap(at(scale, NOTICE_ACTION_GAP))
                .children(
                    (notice.undo.is_some() && wiring.write_enabled).then(|| notice_undo(wiring)),
                )
                .child(notice_dismiss(wiring)),
        )
        .with_animation(
            ElementId::NamedInteger(
                format!("beads-notice-in-{workspace_id}").into(),
                notice.words_key(),
            ),
            wiring.animations.transition(NOTICE_ENTRANCE),
            move |toast, progress| {
                toast.opacity(progress).top(px((1.0 - progress).mul_add(-6.0, top)))
            },
        );
    Some(toast.into_any_element())
}

/// The tone's outline codicon, the VS Code family a developer already reads
/// at a glance, centred on the headline's line box. The line weight matches
/// the board's hairlines where a filled disc would outweigh the headline.
fn notice_glyph(tone: NoticeTone, colors: &BeadsBoardColors, scale: f32) -> gpui::Div {
    let (glyph, hue) = match tone {
        NoticeTone::Success => ("\u{eba4}", colors.done_state),
        NoticeTone::Info => ("\u{ea74}", colors.progress_state),
        NoticeTone::Warning => ("\u{ea6c}", priority_color(colors, 1)),
        NoticeTone::Error => ("\u{ea87}", colors.blocked_state),
    };
    div()
        .flex_none()
        .size(at(scale, 16.0))
        .mt(at(scale, 1.0))
        .flex()
        .items_center()
        .justify_center()
        .font_family(NERD_SYMBOLS)
        .text_size(at(scale, 16.0))
        .line_height(at(scale, 16.0))
        .text_color(colors.panel_state_ink(hue))
        .child(glyph)
}

/// The headline with the sentence under it, then the issue it names as a
/// quieter footnote set a step further down.
fn notice_text(notice: &PanelNotice, colors: &BeadsBoardColors, scale: f32) -> gpui::Div {
    let headline = div()
        .text_size(at(scale, 13.0))
        .line_height(at(scale, NOTICE_TITLE_LINE))
        .font_weight(FontWeight(600.0))
        .text_color(colors.title)
        .line_clamp(2)
        .text_ellipsis()
        .child(notice.title.clone());
    let sentence = notice.message.as_ref().map(|message| {
        div()
            .mt(at(scale, 2.0))
            .text_size(at(scale, 12.0))
            .line_height(at(scale, 17.0))
            .text_color(colors.queue_name)
            .line_clamp(NOTICE_MESSAGE_LINES)
            .text_ellipsis()
            .child(message.clone())
    });
    div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .child(headline)
        .children(sentence)
        .children(notice.subject.as_ref().map(|subject| notice_subject(subject, colors, scale)))
}

/// `id · title`, the id in the terminal's data face as it is everywhere a
/// value is shown, the title truncating into whatever width is left.
fn notice_subject(subject: &NoticeSubject, colors: &BeadsBoardColors, scale: f32) -> gpui::Div {
    let row = div()
        .mt(at(scale, 7.0))
        .flex()
        .items_baseline()
        .gap(at(scale, 5.0))
        .min_w(px(0.0))
        .text_size(at(scale, 11.5))
        .line_height(at(scale, 16.0))
        .text_color(colors.muted)
        .child(
            div()
                .flex_none()
                .font_family(TERMINAL_FONT_FAMILY)
                .text_size(at(scale, 11.0))
                .text_color(colors.queue_name)
                .child(subject.id.clone()),
        );
    if subject.title.is_empty() {
        return row;
    }
    // The title grows into the rest of the line: without `flex_1` its
    // truncating basis collapses to the ellipsis alone.
    row.child(separator(colors).flex_none())
        .child(div().flex_1().min_w(px(0.0)).truncate().child(subject.title.clone()))
}

/// A close toast's Undo, a fixed-size button so its target never moves with
/// the font.
fn notice_undo(wiring: &BeadsPanelRender<'_>) -> gpui::Stateful<gpui::Div> {
    let colors = &wiring.colors;
    let scale = wiring.scale;
    let workspace_id = wiring.workspace_id;
    let state = Arc::clone(&wiring.state);
    div()
        .id(SharedString::from(format!("beads-notice-undo-{workspace_id}")))
        .debug_selector(|| "beads-notice-undo".to_owned())
        .role(Role::Button)
        .aria_label("Undo close")
        .flex_none()
        .w(at(scale, NOTICE_UNDO_WIDTH))
        .h(at(scale, NOTICE_UNDO_HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .border_1()
        .border_color(colors.card_border_hover)
        .text_size(at(scale, 12.0))
        .font_weight(FontWeight(600.0))
        .text_color(colors.title)
        .cursor_pointer()
        .hover(|button| button.bg(colors.button_hover).border_color(colors.chevron))
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, _app| {
            if let Ok(mut panels) = state.lock() {
                panels.undo(workspace_id);
            }
            window.refresh();
        })
        .child("Undo")
}

/// The toast's own close mark, so it never has to be waited out.
fn notice_dismiss(wiring: &BeadsPanelRender<'_>) -> gpui::Stateful<gpui::Div> {
    let colors = &wiring.colors;
    let scale = wiring.scale;
    let workspace_id = wiring.workspace_id;
    let state = Arc::clone(&wiring.state);
    div()
        .id(SharedString::from(format!("beads-notice-dismiss-{workspace_id}")))
        .debug_selector(|| "beads-notice-dismiss".to_owned())
        .role(Role::Button)
        .aria_label("Dismiss notification")
        .flex_none()
        .size(at(scale, NOTICE_DISMISS_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        // The detail panel's own close mark, so the two read as one control,
        // one step quieter than the words it sits beside.
        .text_size(at(scale, 15.0))
        .line_height(at(scale, 15.0))
        .text_color(colors.chevron)
        .cursor_pointer()
        .hover(|button| button.bg(colors.button_hover).text_color(colors.title))
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, _app| {
            if let Ok(mut panels) = state.lock() {
                panels.dismiss_notice(workspace_id);
            }
            window.refresh();
        })
        .child("×")
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
                        .debug_selector(|| "beads-detail-close".to_owned())
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
        .font_family(TERMINAL_FONT_FAMILY)
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
                .filter(|_| presentation.is_some_and(|build| build.has(PanelSection::Spec)))
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
                .font_family(TERMINAL_FONT_FAMILY)
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
                // The codicon copy mark, from the embedded icon face: `⧉` is
                // in neither embedded face, so it only ever painted through
                // whatever system font happened to cover it, or as a box.
                .child(
                    div()
                        .ml(at(wiring.scale, 4.0))
                        .font_family(NERD_SYMBOLS)
                        .opacity(0.0)
                        .group_hover(copy_group, |glyph| glyph.opacity(1.0))
                        .child("\u{ebcc}"),
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
                div().font_family(TERMINAL_FONT_FAMILY),
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
    // The type is a word in the identity row, set in its UI face as the
    // approved mock does; the id and labels beside it are the data.
    let shown = div().child(issue.issue_type.clone());
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
        .into_any_element()
}

#[derive(Default)]
struct EditorVisualFeedback {
    caret: Option<usize>,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
}

fn editor_visual_feedback(
    selection: Range<usize>,
    marked: Option<Range<usize>>,
    color: Rgba,
) -> EditorVisualFeedback {
    if selection.is_empty() && marked.is_none() {
        return EditorVisualFeedback { caret: Some(selection.start), highlights: Vec::new() };
    }

    let caret = selection.is_empty().then_some(selection.start);
    let selection_highlight = (!selection.is_empty()).then(|| {
        (
            selection,
            HighlightStyle {
                background_color: Some(with_alpha(color, 0.28).into()),
                ..HighlightStyle::default()
            },
        )
    });
    let marked_highlight = marked.filter(|range| !range.is_empty()).map(|range| {
        (
            range,
            HighlightStyle {
                underline: Some(UnderlineStyle {
                    color: Some(color.into()),
                    thickness: px(1.0),
                    wavy: false,
                }),
                ..HighlightStyle::default()
            },
        )
    });
    let highlights = combine_highlights(selection_highlight, marked_highlight).collect();

    EditorVisualFeedback { caret, highlights }
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
    let visual = beads_editor.read(wiring.app).visual_feedback(
        wiring.workspace_id,
        issue_id,
        field,
        wiring.colors.title,
    );
    let styled = StyledText::new(display).with_highlights(visual.highlights);
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
        editable_input_surface(
            surface,
            focus,
            state.editor,
            state.layout,
            visual.caret.map(|caret| (caret, wiring.colors.title)),
        )
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
    caret: Option<(usize, Rgba)>,
) -> gpui::Stateful<gpui::Div> {
    surface.child(
        canvas(
            |_, _, _| {},
            move |bounds, (), window, app| {
                editor.update(app, |editor, _| editor.layout = Some(layout.clone()));
                window.handle_input(&focus, ElementInputHandler::new(bounds, editor.clone()), app);
                if focus.is_focused(window)
                    && let Some((caret, color)) = caret
                    && let Some(origin) = layout.position_for_index(caret)
                {
                    window.paint_quad(fill(
                        Bounds::new(origin, size(px(2.0), layout.line_height())),
                        color,
                    ));
                }
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
    let passages = detail_passages(detail, colors);
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
            passages
                .into_iter()
                .filter(|(section, _)| presentation.has(*section))
                .map(|(_, passage)| editable_passage(detail, passage, wiring)),
        )
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

fn detail_passages<'a>(
    detail: &'a BeadsIssueDetail,
    colors: &BeadsBoardColors,
) -> [(PanelSection, PassageEdit<'a>); 4] {
    [
        (
            PanelSection::Description,
            PassageEdit {
                field: EditField::Description,
                label: None,
                value: &detail.description,
                color: colors.title,
            },
        ),
        (
            PanelSection::Design,
            PassageEdit {
                field: EditField::Design,
                label: Some("Design"),
                value: &detail.design,
                color: colors.queue_name,
            },
        ),
        (
            PanelSection::Acceptance,
            PassageEdit {
                field: EditField::Acceptance,
                label: Some("Acceptance"),
                value: &detail.acceptance_criteria,
                color: colors.queue_name,
            },
        ),
        (
            PanelSection::Notes,
            PassageEdit {
                field: EditField::Notes,
                label: Some("Notes"),
                value: &detail.notes,
                color: colors.queue_name,
            },
        ),
    ]
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
        .child(div().font_family(TERMINAL_FONT_FAMILY).child(blocker.id.clone()))
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
        .font_family(TERMINAL_FONT_FAMILY)
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
                .font_family(TERMINAL_FONT_FAMILY)
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
                        .font_family(TERMINAL_FONT_FAMILY)
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
        .font_family(TERMINAL_FONT_FAMILY)
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
            parent_epic_id: Some("scribe-5wh1".into()),
            updated_at: String::new(),
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

    /// A toast's headline and sentence, the two strings a reader acts on.
    fn copy(notice: Option<&PanelNotice>) -> Option<(&str, Option<&str>)> {
        notice.map(|notice| (notice.title.as_str(), notice.message.as_deref()))
    }

    fn subject_id(notice: Option<&PanelNotice>) -> Option<&str> {
        notice.and_then(|notice| notice.subject.as_ref()).map(|subject| subject.id.as_str())
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

        let narrow = Rect { width: crate::beads_board_a2::MIN_BOARD_W, ..region };
        for (height, scale) in [(116.0, 0.8_f32), (116.0, 1.6), (520.0, 1.6)] {
            let board = Rect { height, ..narrow };
            let layout = panel_layout(narrow, board, 4, scale).expect("narrow panel layout");
            assert!(layout.geometry.x >= narrow.x);
            assert!(layout.geometry.x + layout.geometry.width <= narrow.x + narrow.width);
            assert!(layout.geometry.y >= board.y + board.height);
            assert!(layout.geometry.y + layout.geometry.max_height <= narrow.y + narrow.height);
        }
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
        assert!((start.y - geometry.y).abs() < f32::EPSILON, "opening never covers the board");
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
        assert_eq!(
            copy(panels.active_notice(vanished)),
            Some(("Issue not found", Some("Beads can’t find it anymore, so its panel closed.")))
        );
        assert_eq!(subject_id(panels.active_notice(vanished)), Some("scribe-5wh1.4"));

        assert!(panels.sync_board(missing_project, &BeadsBoardState::NotDetected));
        assert!(panels.visible(missing_project).is_none());
        assert_eq!(
            copy(panels.active_notice(missing_project)),
            Some((
                "Beads project not found",
                Some(
                    "This workspace is no longer inside a Beads project, so the issue panel closed."
                )
            ))
        );
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
    fn a_pending_navigation_discards_the_reply_for_the_issue_it_left() {
        let workspace = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.open(workspace, item(), 1);
        assert_eq!(panels.take_request(), Some((workspace, "scribe-5wh1.4".into())));

        // Retarget before the first request has been answered, so both are
        // in flight and the slow one belongs to the issue just left.
        assert!(panels.navigate_to_issue(workspace, "flow-node-2"));
        assert_eq!(panels.take_request(), Some((workspace, "flow-node-2".into())));

        panels.update(workspace, "scribe-5wh1.4", Some(Box::new(full_detail())));
        assert!(
            panels.visible(workspace).is_some_and(|panel| panel.detail.is_none()),
            "the answer to the issue the reader navigated away from must not paint"
        );

        let mut arrived = detail();
        arrived.id = "flow-node-2".into();
        arrived.title = "Second node".into();
        panels.update(workspace, "flow-node-2", Some(Box::new(arrived)));
        let panel = panels.visible(workspace).expect("the target's own reply lands");
        assert_eq!(panel.card.id, "flow-node-2");
        assert_eq!(panel.detail.as_deref().map(|detail| detail.id.as_str()), Some("flow-node-2"));
    }

    #[test]
    fn retargeting_reaches_an_issue_the_open_detail_never_listed() {
        let workspace = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        panels.open(workspace, item(), 1);
        panels.update(workspace, "scribe-5wh1.4", Some(Box::new(full_detail())));
        assert_eq!(panels.take_request(), Some((workspace, "scribe-5wh1.4".into())));

        // A Flow graph node is not necessarily a dependent of the open issue;
        // the board's frozen graph is what proved it reachable.
        assert!(!panels.navigate_to_dependent(workspace, "unrelated-node"));
        assert_eq!(panels.take_request(), None, "a rejected dependent sends nothing");

        assert!(panels.navigate_to_issue(workspace, "unrelated-node"));
        assert_eq!(panels.take_request(), Some((workspace, "unrelated-node".into())));
    }

    #[test]
    fn retargeting_needs_an_open_panel_and_the_detail_capability() {
        let workspace = WorkspaceId::new();
        let mut panels = BeadsPanels::default();
        panels.set_enabled(true);
        assert!(!panels.navigate_to_issue(workspace, "flow-node-2"), "no panel to retarget");
        assert_eq!(panels.take_request(), None);

        panels.open(workspace, item(), 1);
        assert_eq!(panels.take_request(), Some((workspace, "scribe-5wh1.4".into())));
        panels.set_enabled(false);
        assert!(!panels.navigate_to_issue(workspace, "flow-node-2"));
        assert_eq!(panels.take_request(), None);
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
        assert_eq!(
            copy(panels.active_notice(workspace)).map(|(title, _)| title),
            Some("Issue not found")
        );
        assert_eq!(subject_id(panels.active_notice(workspace)), Some("next-1"));
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
                title: "Render the read-only detail panel".into(),
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
        assert_eq!(
            copy(panels.notice_at(workspace, now)),
            Some(("Couldn’t save the description", Some("bd rejected edit.")))
        );
        assert_eq!(
            panels.notice_at(workspace, now).map(|notice| notice.tone),
            Some(NoticeTone::Error)
        );
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
                title: "Render the read-only detail panel".into(),
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
                title: "Render the read-only detail panel".into(),
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
                title: "Render the read-only detail panel".into(),
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
        assert_eq!(
            copy(panels.notice_at(workspace, now)),
            Some(("Couldn’t claim the issue", Some("Permission denied.")))
        );
        let just_before = (now + NOTICE_DURATION)
            .checked_sub(Duration::from_millis(1))
            .expect("notice duration exceeds one millisecond");
        assert!(!panels.expire_notices_at(just_before), "a live toast stays up");
        assert!(panels.notice_at(workspace, just_before).is_some());

        assert!(panels.claim(workspace));
        assert!(panels.take_write().is_some());
        panels.finish_write_at(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Applied { generation: 10 },
            now,
        );
        assert_eq!(panels.notice_at(workspace, now), None);

        // Left alone, a toast is removed at its exact deadline, and the
        // lifecycle tick learns that it has a frame to paint.
        panels
            .notices
            .insert(workspace, PanelNotice::at(NoticeTone::Error, "Couldn’t claim the issue", now));
        assert!(panels.expire_notices_at(now + NOTICE_DURATION));
        assert!(panels.notices.is_empty());
        assert!(!panels.expire_notices_at(now + NOTICE_DURATION), "nothing left to expire");
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
            copy(panels.notice_at(workspace, now + WRITE_DEADLINE)),
            Some((TIMEOUT_TITLE, Some(TIMEOUT_MESSAGE)))
        );
        assert_eq!(
            panels.notice_at(workspace, now + WRITE_DEADLINE).map(|notice| notice.tone),
            Some(NoticeTone::Warning),
            "an unknown outcome is a warning, not a failure"
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
    fn card_drop_queue_carries_shared_verbs_and_guards() {
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

                let expected = card_drop_verb(source_lane, target_lane);
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

        panels.classifier_won(workspace, &item(), 3);

        let notice = panels.active_notice(workspace);
        assert_eq!(
            copy(notice),
            Some(("Moved to Blocked instead", Some("Another issue is blocking it.")))
        );
        assert_eq!(notice.map(|notice| notice.tone), Some(NoticeTone::Info));
        assert_eq!(
            notice.and_then(|notice| notice.subject.clone()),
            Some(NoticeSubject {
                id: "scribe-5wh1.4".into(),
                title: "Render the read-only detail panel".into(),
            })
        );
    }

    /// Every failure a toast can report, reduced to what a person should
    /// read. Nothing shaped like JSON ever survives, whatever sent it.
    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Write failure notice copy]]
    #[test]
    fn failure_reasons_become_sentences_and_json_never_shows() {
        for (reason, expected) in [
            ("bd failed: forced nonzero write", Some("Forced nonzero write.")),
            (
                r#"bd failed: resolving issue: no issue found matching "nope-123""#,
                Some(r#"Resolving issue: no issue found matching "nope-123"."#),
            ),
            ("bd failed: Error: database is locked", Some("Database is locked.")),
            ("bd issue write timed out", Some("bd issue write timed out.")),
            ("Permission denied!", Some("Permission denied!")),
            (r#"bd failed: {"error":"forced nonzero write"}"#, None),
            ("bd failed: [1, 2]", None),
            ("bd failed: exited with exit status: 9", None),
            ("  ", None),
        ] {
            assert_eq!(reason_sentence(reason).as_deref(), expected, "{reason}");
        }
        let long = format!("bd failed: {}", "x".repeat(REASON_MAX_CHARS + 20));
        let clipped = reason_sentence(&long).expect("a long reason is clipped, not dropped");
        assert_eq!(clipped.chars().count(), REASON_MAX_CHARS + 1);
        assert!(clipped.ends_with('…'));

        // An older server can still forward bd's raw envelope: the toast
        // then says what failed and withholds the JSON entirely.
        let now = Instant::now();
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.claim(workspace));
        assert!(panels.take_write().is_some());
        panels.finish_write_at(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Failed {
                reason: r#"bd failed: {"data":{"error":"boom"},"schema_version":1}"#.into(),
            },
            now,
        );
        let notice = panels.notice_at(workspace, now).expect("a failure raises its toast");
        assert_eq!(
            copy(Some(notice)),
            Some((
                "Couldn’t claim the issue",
                Some("Beads reported an error, so nothing was saved.")
            ))
        );
        assert!(!notice.spoken().contains('{'), "{}", notice.spoken());
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
                    title: "Render the read-only detail panel".into(),
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
        let closed = panels.notice_at(workspace, now + Duration::from_millis(4_999));
        assert_eq!(copy(closed), Some(("Issue closed", None)));
        assert_eq!(subject_id(closed), Some("scribe-5wh1.4"));
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

    /// Resting the pointer on a toast keeps it up past its five seconds and
    /// letting go leaves it for [`NOTICE_LINGER`] more, but the Undo it
    /// carries keeps its exact deadline: holding the words never holds the
    /// window a reopen may still land in.
    // @lat: [[test#Test Harness#Visual E2E Tests#Beads card-detail fixtures#Hovered toasts hold]]
    #[test]
    fn a_hovered_toast_holds_but_its_undo_keeps_the_exact_deadline() {
        let now = Instant::now();
        let (workspace, mut panels) = loaded_writable_panels(detail());
        assert!(panels.close_issue(workspace));
        assert!(panels.take_write().is_some());
        panels.finish_write_at(
            workspace,
            "scribe-5wh1.4",
            BeadsIssueWriteResult::Applied { generation: 3 },
            now,
        );
        panels.hold_notice_at(workspace, true, now + Duration::from_secs(1));

        let deadline = now + NOTICE_DURATION;
        assert!(!panels.undo_at(workspace, deadline), "held or not, the deadline is exact");
        assert!(panels.notice_at(workspace, deadline).is_some(), "a refused Undo keeps the toast");
        assert!(panels.expire_notices_at(deadline), "the lapsed Undo leaves the held toast");
        let late = deadline + Duration::from_secs(10);
        assert!(
            panels.notice_at(workspace, late).is_some_and(|notice| notice.undo.is_none()),
            "the words stay up while held, without the Undo"
        );
        assert!(!panels.expire_notices_at(late), "nothing leaves while held");

        panels.hold_notice_at(workspace, false, late);
        let lingered = late + NOTICE_LINGER;
        let just_before =
            lingered.checked_sub(Duration::from_millis(1)).expect("linger exceeds one millisecond");
        assert!(!panels.expire_notices_at(just_before), "letting go lingers");
        assert!(panels.expire_notices_at(lingered));
        assert!(panels.notices.is_empty());

        // A hold that never hears the pointer leave still ends at its cap.
        panels.notices.insert(workspace, PanelNotice::at(NoticeTone::Error, "Held", now));
        panels.hold_notice_at(workspace, true, now);
        let capped = now + NOTICE_HOLD_MAX;
        let before_cap =
            capped.checked_sub(Duration::from_millis(1)).expect("hold cap exceeds one millisecond");
        assert!(!panels.expire_notices_at(before_cap), "a hold outlives the five seconds");
        assert!(panels.expire_notices_at(capped), "but not its own cap");
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
            copy(panels.notice_at(workspace, now)),
            Some((
                "Issue changed elsewhere",
                Some("It changed since Scribe last loaded it, so your change wasn’t saved.")
            ))
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

    // @lat: [[test#Test Harness#GPUI Beads Inline Editing#Editor visual feedback]]
    #[test]
    fn editor_visual_feedback_covers_caret_selection_and_ime_marking() {
        let color = gpui::rgba(0x66aa_ffff);
        let collapsed = editor_visual_feedback(3..3, None, color);
        assert_eq!(collapsed.caret, Some(3));
        assert!(collapsed.highlights.is_empty());

        let selected = editor_visual_feedback(1..4, Some(2..5), color);
        assert_eq!(selected.caret, None);
        assert_eq!(
            selected.highlights.iter().map(|(range, _)| range.clone()).collect::<Vec<_>>(),
            vec![1..2, 2..4, 4..5]
        );
        assert!(selected.highlights[0].1.background_color.is_some());
        assert!(selected.highlights[0].1.underline.is_none());
        assert!(selected.highlights[1].1.background_color.is_some());
        assert!(selected.highlights[1].1.underline.is_some());
        assert!(selected.highlights[2].1.background_color.is_none());
        assert!(selected.highlights[2].1.underline.is_some());
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

#[cfg(test)]
mod panel_cache_tests {
    use scribe_common::ids::WorkspaceId;
    use scribe_common::protocol::BeadsBoardItem;

    use super::*;
    use crate::beads_board::BeadsBoardColors;

    fn colors() -> BeadsBoardColors {
        let fill = [0.15, 0.16, 0.17, 1.0];
        let chrome = scribe_common::theme::ChromeColors {
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
        };
        BeadsBoardColors::from_theme(&chrome, &[[0.5, 0.5, 0.5, 1.0]; 16], 1.0)
    }

    fn panel() -> BeadsPanel {
        BeadsPanel {
            card: BeadsBoardItem {
                id: "scribe-panel.1".into(),
                title: "Panel issue".into(),
                priority: 2,
                blocker_ids: Vec::new(),
                parent_epic_name: None,
                parent_epic_id: None,
                updated_at: String::new(),
            },
            lane: 1,
            detail: None,
        }
    }

    fn layer(cx: &mut gpui::TestAppContext) -> PanelLayer {
        let window = cx.add_empty_window();
        let panels = Arc::new(Mutex::new(BeadsPanels::default()));
        let editor = window.update(|window, app| {
            app.new(|editor_cx| BeadsEditor::new(Arc::clone(&panels), window, editor_cx))
        });
        cx.update(|app| PanelLayer {
            inputs: PanelLayerInputs {
                region: Rect { x: 0.0, y: 0.0, width: 1600.0, height: 900.0 },
                board: Rect { x: 0.0, y: 0.0, width: 1600.0, height: 160.0 },
                workspace_id: WorkspaceId::new(),
                state: panels,
                editor,
                terminal_focus: app.focus_handle(),
                write_enabled: true,
                scale: 1.0,
                colors: colors(),
                animations: AnimationSettings::resolve_with_env(false, None),
                panel: Some(panel()),
                notice: None,
            },
        })
    }

    fn clone_inputs(layer: &PanelLayer) -> PanelLayerInputs {
        let inputs = &layer.inputs;
        PanelLayerInputs {
            region: inputs.region,
            board: inputs.board,
            workspace_id: inputs.workspace_id,
            state: Arc::clone(&inputs.state),
            editor: inputs.editor.clone(),
            terminal_focus: inputs.terminal_focus.clone(),
            write_enabled: inputs.write_enabled,
            scale: inputs.scale,
            colors: inputs.colors,
            animations: inputs.animations,
            panel: inputs.panel.clone(),
            notice: inputs.notice.clone(),
        }
    }

    #[gpui::test]
    fn identical_inputs_leave_the_cache_alone(cx: &mut gpui::TestAppContext) {
        let layer = layer(cx);
        assert!(layer.same_inputs(&clone_inputs(&layer)));
    }

    #[gpui::test]
    fn every_owned_input_invalidates(cx: &mut gpui::TestAppContext) {
        let layer = layer(cx);

        let mut moved = clone_inputs(&layer);
        moved.board.height += 40.0;
        assert!(!layer.same_inputs(&moved), "board rect");

        let mut resized = clone_inputs(&layer);
        resized.region.width -= 200.0;
        assert!(!layer.same_inputs(&resized), "region rect");

        let mut gated = clone_inputs(&layer);
        gated.write_enabled = false;
        assert!(!layer.same_inputs(&gated), "write gate");

        let mut retitled = clone_inputs(&layer);
        retitled.panel.as_mut().unwrap().card.title = "Renamed".into();
        assert!(!layer.same_inputs(&retitled), "server-pushed panel content");

        let mut closed = clone_inputs(&layer);
        closed.panel = None;
        assert!(!layer.same_inputs(&closed), "panel closed");

        let mut noticed = clone_inputs(&layer);
        noticed.notice =
            Some(PanelNotice::at(NoticeTone::Info, "Issue closed", std::time::Instant::now()));
        assert!(!layer.same_inputs(&noticed), "notice");

        let mut zoomed = clone_inputs(&layer);
        zoomed.scale = 1.1;
        assert!(!layer.same_inputs(&zoomed), "scale");
    }

    /// Two regions' overlays embedded exactly the way the root embeds them:
    /// one cached entity each, wrapped at the full grid band.
    struct CachedLayersProbe {
        layers: Vec<gpui::Entity<PanelLayer>>,
    }

    impl Render for CachedLayersProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::div().relative().size_full().children(
                self.layers
                    .iter()
                    .map(|layer| {
                        layer
                            .clone()
                            .cached(gpui::StyleRefinement::default().absolute().inset_0())
                            .into_any_element()
                    })
                    .collect::<Vec<_>>(),
            )
        }
    }

    /// The `BoardStrip` twin of the placement probe: a cached overlay reaches
    /// only into the region that opened it.
    ///
    /// `PanelLayer::render` roots a band-spanning container and hangs every
    /// absolutely-positioned overlay child off it, so taffy pinning a layout
    /// root to the origin costs the panel nothing -- but only measurement
    /// tells that apart from the strip's arrangement, which lost its origin
    /// exactly that way. The backdrop's own dismiss hitbox is the probe.
    // @lat: [[test#Test Harness#GPUI Client Headless Suites#Cached view placement]]
    #[gpui::test]
    fn each_region_reaches_only_its_own_cached_overlay(cx: &mut gpui::TestAppContext) {
        let (left, right) = (WorkspaceId::new(), WorkspaceId::new());
        let panels = Arc::new(Mutex::new(BeadsPanels::default()));
        panels.lock().expect("probe store").set_enabled(true);
        for workspace_id in [left, right] {
            panels.lock().expect("probe store").open(workspace_id, panel().card, panel().lane);
        }
        let window = cx
            .update(|app| {
                AnimationSettings::resolve_with_env(false, None).apply_to_app(app);
                app.open_window(
                    gpui::WindowOptions {
                        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds {
                            origin: gpui::point(px(0.0), px(0.0)),
                            size: gpui::size(px(2.0 * REGION_WIDTH), px(REGION_HEIGHT)),
                        })),
                        ..Default::default()
                    },
                    |window, app| {
                        let editor = app.new(|editor_cx| {
                            BeadsEditor::new(Arc::clone(&panels), window, editor_cx)
                        });
                        let layers = [(left, 0.0), (right, REGION_WIDTH)]
                            .map(|(workspace_id, x)| {
                                region_layer(app, &panels, &editor, workspace_id, x)
                            })
                            .to_vec();
                        app.new(|_| CachedLayersProbe { layers })
                    },
                )
            })
            .expect("open the two-region cached-overlay probe");
        cx.update_window(window.into(), |_, window, app| window.draw(app).clear())
            .expect("draw both cached overlays");
        let mut test_window = gpui::VisualTestContext::from_window(window.into(), cx);

        // Inside the second region's backdrop and clear of its centred body,
        // which starts one panel margin in.
        test_window.simulate_click(
            gpui::point(px(REGION_WIDTH + PANEL_MARGIN / 2.0), px(300.0)),
            gpui::Modifiers::default(),
        );
        let store = panels.lock().expect("probe store");
        assert!(
            store.visible(right).is_none(),
            "the second region's cached overlay never took the click its own backdrop painted"
        );
        assert!(store.visible(left).is_some(), "dismissing one region dismissed the other");
    }

    /// Paint `notice` in the second of two regions, over any panel `panels`
    /// holds open there, the way the root mounts them, with `panels` holding
    /// the same toast as live state.
    fn notice_probe(
        cx: &mut gpui::TestAppContext,
        panels: &Arc<Mutex<BeadsPanels>>,
        workspace: WorkspaceId,
        notice: PanelNotice,
    ) -> gpui::VisualTestContext {
        {
            let mut store = panels.lock().expect("probe store");
            store.set_enabled(true);
            store.set_write_enabled(true);
            store.notices.insert(workspace, notice.clone());
        }
        let options = gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(Bounds {
                origin: gpui::point(px(0.0), px(0.0)),
                size: gpui::size(px(2.0 * REGION_WIDTH), px(REGION_HEIGHT)),
            })),
            ..Default::default()
        };
        let window = cx
            .update(|app| {
                AnimationSettings::resolve_with_env(false, None).apply_to_app(app);
                app.open_window(options, |window, app| {
                    notice_probe_root(window, app, panels, workspace, notice)
                })
            })
            .expect("open the notice probe");
        // Opening already drew once, and a cached replay records no debug
        // bounds, so force the uncached frame an interaction would.
        cx.update_window(window.into(), |_, window, app| {
            window.refresh();
            window.draw(app).clear();
        })
        .expect("draw the notice toast");
        gpui::VisualTestContext::from_window(window.into(), cx)
    }

    /// The probe window's root: one region's layer carrying `notice` and the
    /// panel `panels` holds open there, if any.
    fn notice_probe_root(
        window: &mut Window,
        app: &mut App,
        panels: &Arc<Mutex<BeadsPanels>>,
        workspace: WorkspaceId,
        notice: PanelNotice,
    ) -> Entity<CachedLayersProbe> {
        let editor = app.new(|editor_cx| BeadsEditor::new(Arc::clone(panels), window, editor_cx));
        let layer = region_layer(app, panels, &editor, workspace, REGION_WIDTH);
        let panel = panels.lock().expect("probe store").visible(workspace).cloned();
        layer.update(app, |layer, _| {
            layer.inputs.panel = panel;
            layer.inputs.notice = Some(notice);
        });
        app.new(|_| CachedLayersProbe { layers: vec![layer] })
    }

    /// A close's toast hangs in its own section's top-right corner, one gap
    /// under the board, and its Undo is a real target there. Sited on the
    /// second of two regions so region-anchored and window-anchored differ.
    // @lat: [[test#Test Harness#GPUI Client Headless Suites#Beads notice toast placement]]
    #[gpui::test]
    fn close_toast_hangs_in_its_sections_top_right_and_undoes(cx: &mut gpui::TestAppContext) {
        let workspace = WorkspaceId::new();
        let panels = Arc::new(Mutex::new(BeadsPanels::default()));
        let closed = PanelNotice::closed_at(
            "scribe-panel.1",
            "Panel issue",
            "maintainer".into(),
            std::time::Instant::now(),
        );
        let mut test_window = notice_probe(cx, &panels, workspace, closed);

        let toast = test_window.debug_bounds("beads-notice").expect("the toast painted");
        let region_right = 2.0 * REGION_WIDTH;
        assert!(
            (f32::from(toast.right()) - (region_right - PANEL_MARGIN)).abs() < 0.5,
            "toast right edge {:?} is not the section's inset top-right corner",
            toast.right()
        );
        assert!(
            (f32::from(toast.top()) - (197.0 + NOTICE_BOARD_GAP)).abs() < 0.5,
            "toast top {:?} is not one gap under the board",
            toast.top()
        );
        assert!(f32::from(toast.left()) > REGION_WIDTH, "toast escaped its own region");
        assert!(
            (f32::from(toast.size.width) - NOTICE_WIDTH).abs() < 0.5,
            "toast is not its fixed width"
        );

        // The functional E2E clicks Undo from these same offsets: the button
        // ends one border, the right padding, the close mark, and one gap in
        // from the toast's right edge, centred on the headline's line box.
        let undo = test_window.debug_bounds("beads-notice-undo").expect("close offers Undo");
        let undo_right = f32::from(toast.right())
            - 1.0
            - NOTICE_PAD_RIGHT
            - NOTICE_DISMISS_SIZE
            - NOTICE_ACTION_GAP;
        assert!((f32::from(undo.right()) - undo_right).abs() < 0.5, "Undo moved: {undo:?}");
        let headline_centre = f32::from(toast.top()) + 1.0 + NOTICE_PAD_Y + NOTICE_TITLE_LINE / 2.0;
        assert!(
            (f32::from(undo.center().y) - headline_centre).abs() < 0.5,
            "Undo left the headline's line: {undo:?}"
        );
        test_window.simulate_click(undo.center(), gpui::Modifiers::default());
        assert_eq!(
            panels
                .lock()
                .expect("probe store")
                .take_write()
                .map(|intent| (intent.verb, intent.title)),
            Some((scribe_common::protocol::BeadsIssueWrite::UndoClose, "Panel issue".into())),
            "the toast's Undo queued the guarded reopen"
        );
    }

    /// Every toast carries its own close mark, so none has to be waited out.
    #[gpui::test]
    fn a_toast_close_mark_takes_it_down(cx: &mut gpui::TestAppContext) {
        let workspace = WorkspaceId::new();
        let panels = Arc::new(Mutex::new(BeadsPanels::default()));
        let failed = PanelNotice::at(
            NoticeTone::Error,
            "Couldn’t claim the issue",
            std::time::Instant::now(),
        )
        .saying("Permission denied.");
        let mut test_window = notice_probe(cx, &panels, workspace, failed);

        assert!(
            test_window.debug_bounds("beads-notice-undo").is_none(),
            "only a close offers Undo"
        );
        let toast = test_window.debug_bounds("beads-notice").expect("the toast painted");
        test_window.simulate_mouse_move(toast.center(), None, gpui::Modifiers::default());
        assert!(
            panels
                .lock()
                .expect("probe store")
                .notices
                .get(&workspace)
                .is_some_and(|notice| notice.held_since.is_some()),
            "resting the pointer on the toast holds it"
        );
        let dismiss =
            test_window.debug_bounds("beads-notice-dismiss").expect("every toast can be closed");
        test_window.simulate_click(dismiss.center(), gpui::Modifiers::default());
        assert!(
            panels.lock().expect("probe store").active_notice(workspace).is_none(),
            "the close mark took the toast down"
        );
    }

    /// A toast that lands on an open panel's corner stays on top of it. This
    /// section is narrow enough that the toast's close mark sits over the
    /// panel's own, so one press there shows which layer takes the pointer.
    /// It must be the toast: otherwise dismissing a notice would close the
    /// issue the reader was looking at.
    // @lat: [[test#Test Harness#GPUI Client Headless Suites#Beads notice toast placement]]
    #[gpui::test]
    fn a_toast_over_an_open_panel_takes_the_press(cx: &mut gpui::TestAppContext) {
        let workspace = WorkspaceId::new();
        let panels = Arc::new(Mutex::new(BeadsPanels::default()));
        {
            let mut store = panels.lock().expect("probe store");
            store.set_enabled(true);
            store.open(workspace, panel().card, panel().lane);
        }
        let failed = PanelNotice::at(
            NoticeTone::Error,
            "Couldn’t save the title",
            std::time::Instant::now(),
        )
        .saying("Forced nonzero write.");
        let mut test_window = notice_probe(cx, &panels, workspace, failed);

        let dismiss =
            test_window.debug_bounds("beads-notice-dismiss").expect("every toast can be closed");
        let covered = test_window
            .debug_bounds("beads-detail-close")
            .expect("the open panel painted its close mark");
        assert!(
            dismiss.contains(&covered.center()),
            "the toast's close mark {dismiss:?} no longer sits over the panel's {covered:?}"
        );
        test_window.simulate_click(covered.center(), gpui::Modifiers::default());
        let store = panels.lock().expect("probe store");
        assert!(
            store.visible(workspace).is_some(),
            "the press went through the toast and closed the panel beneath it"
        );
        assert!(
            store.active_notice(workspace).is_none(),
            "the toast on top of the panel never took the press"
        );
    }

    #[test]
    fn notice_slot_needs_room_under_the_board() {
        let region = Rect { x: 40.0, y: 0.0, width: 800.0, height: 600.0 };
        let board = Rect { x: 40.0, y: 0.0, width: 800.0, height: 197.0 };
        assert_eq!(notice_slot(region, board, 1.0), Some((488.0, 205.0, NOTICE_WIDTH)));
        let large = NOTICE_WIDTH * 1.6;
        assert_eq!(notice_slot(region, board, 1.6), Some((840.0 - 12.0 - large, 205.0, large)));
        let narrow = Rect { width: 260.0, ..region };
        assert_eq!(notice_slot(narrow, board, 1.0), Some((52.0, 205.0, 236.0)));
        assert_eq!(notice_slot(Rect { width: 240.0, ..region }, board, 1.0), None);
        let full_board = Rect { height: 590.0, ..board };
        assert_eq!(notice_slot(region, full_board, 1.0), None, "no room under a full board");
    }

    const REGION_WIDTH: f32 = 504.0;
    const REGION_HEIGHT: f32 = 739.0;

    fn region_layer(
        app: &mut gpui::App,
        panels: &Arc<Mutex<BeadsPanels>>,
        editor: &gpui::Entity<BeadsEditor>,
        workspace_id: WorkspaceId,
        x: f32,
    ) -> gpui::Entity<PanelLayer> {
        let focus = app.focus_handle();
        app.new(|_| PanelLayer {
            inputs: PanelLayerInputs {
                region: Rect { x, y: 0.0, width: REGION_WIDTH, height: REGION_HEIGHT },
                board: Rect { x, y: 0.0, width: REGION_WIDTH, height: 197.0 },
                workspace_id,
                state: Arc::clone(panels),
                editor: editor.clone(),
                terminal_focus: focus,
                write_enabled: true,
                scale: 1.0,
                colors: colors(),
                animations: AnimationSettings::resolve_with_env(false, None),
                panel: Some(panel()),
                notice: None,
            },
        })
    }
}
