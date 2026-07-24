//! Per-workspace notes modal for the GPUI client rebuild.
//!
//! The winit client drew this modal as hand-placed `CellInstance` quads over a
//! grid ([`workspace_notes_modal.rs`](../../scribe-client/src/workspace_notes_modal.rs)),
//! with the active/archive/editor state machine folded into the painter. This
//! port keeps that state machine verbatim — the [`WorkspaceNotesView`] toggle,
//! the [`WorkspaceNotesEditMode`] editor target, the draft dirty flag, and the
//! `\n---\n` bulk-archive splitter — while painting the surfaces with GPUI
//! elements (rounded panel, nav buttons, note rows with per-row actions, and an
//! input box with a caret). Clicking a control emits a
//! [`WorkspaceNotesModalAction`]; the shell routes it and, for Save/archive,
//! turns the current state into a frozen [`WorkspaceNotesMutation`] via
//! [`WorkspaceNotesModalView::save_mutation`] /
//! [`WorkspaceNotesModalView::archive_mutation`].

use gpui::{Context, EventEmitter, FocusHandle, Rgba, div, prelude::*, px};
use scribe_common::ids::WorkspaceId;
use scribe_common::protocol::WorkspaceNotesMutation;
use scribe_common::theme::ChromeColors;

use crate::tab_bar::srgba;
use crate::workspace_notes::{ArchiveReason, WorkspaceNoteEntry};

/// Maximum active/archive rows shown before the list scrolls.
pub const NOTE_LIST_ROWS: usize = 8;

/// Which note list the modal is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceNotesView {
    /// The active-notes list.
    Active,
    /// The archived-notes list.
    Archive,
}

/// What the editor pane is currently editing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceNotesEditMode {
    /// The new-note draft buffer (the default).
    Draft,
    /// Editing an existing active note.
    ActiveNote {
        /// The note being edited.
        note_id: String,
    },
    /// Editing a single archived note.
    ArchivedNote {
        /// The archived note being edited.
        note_id: String,
    },
    /// Bulk-editing every archived note, joined by `\n---\n`.
    ArchiveBulk {
        /// The archived note ids in join order.
        note_ids: Vec<String>,
    },
}

/// A control the user activated in the modal, routed by the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceNotesModalAction {
    /// Close the modal.
    Close,
    /// Save the current editor buffer.
    Save,
    /// Cancel the current edit and return to the draft.
    CancelEdit,
    /// Switch to the active-notes view.
    ShowActive,
    /// Switch to the archived-notes view.
    ShowArchive,
    /// Begin editing the named active note.
    EditActive(String),
    /// Begin editing the named archived note.
    EditArchived(String),
    /// Archive the named active note as done.
    ArchiveDone(String),
    /// Archive the named active note as removed.
    ArchiveRemoved(String),
    /// Begin the bulk edit of every archived note.
    EditAllArchive,
}

impl EventEmitter<WorkspaceNotesModalAction> for WorkspaceNotesModalView {}

/// Resolved GPUI colours for the modal.
#[derive(Clone, Copy)]
pub struct WorkspaceNotesModalColors {
    backdrop: Rgba,
    modal_bg: Rgba,
    panel_bg: Rgba,
    row_bg: Rgba,
    border: Rgba,
    title_fg: Rgba,
    body_fg: Rgba,
    muted_fg: Rgba,
    button_fg: Rgba,
    button_bg: Rgba,
    selected_fg: Rgba,
    selected_bg: Rgba,
    primary_fg: Rgba,
    primary_bg: Rgba,
    danger_fg: Rgba,
    input_bg: Rgba,
    caret: Rgba,
}

impl From<&ChromeColors> for WorkspaceNotesModalColors {
    fn from(chrome: &ChromeColors) -> Self {
        Self {
            backdrop: with_alpha(srgba(chrome.tab_bar_bg), 0.46),
            modal_bg: srgba(lighten(chrome.tab_bar_bg, 0.012)),
            panel_bg: srgba(lighten(chrome.tab_bar_bg, 0.024)),
            row_bg: srgba(lighten(chrome.tab_bar_bg, 0.038)),
            border: with_alpha(srgba(chrome.tab_separator), 0.92),
            title_fg: srgba(chrome.tab_text_active),
            body_fg: srgba(chrome.tab_text_active),
            muted_fg: with_alpha(srgba(chrome.tab_text), 0.68),
            button_fg: srgba(chrome.tab_text),
            button_bg: srgba(lighten(chrome.tab_bar_bg, 0.045)),
            selected_fg: srgba(chrome.tab_text_active),
            selected_bg: with_alpha(srgba(chrome.accent), 0.18),
            primary_fg: srgba(chrome.tab_bar_bg),
            primary_bg: with_alpha(srgba(chrome.accent), 0.84),
            danger_fg: with_alpha(srgba(chrome.tab_text_active), 0.86),
            input_bg: srgba(lighten(chrome.tab_bar_bg, 0.034)),
            caret: with_alpha(srgba(chrome.accent), 0.95),
        }
    }
}

/// Visual weight of a modal button.
#[derive(Clone, Copy)]
enum ButtonTone {
    Normal,
    Selected,
    Primary,
    Danger,
}

/// The inputs describing one modal button, grouped so [`WorkspaceNotesModalView::button`]
/// stays a two-argument call.
struct ButtonSpec<'a> {
    /// Stable element id disambiguating this button from its siblings.
    id: usize,
    /// The button's label text.
    label: &'a str,
    /// The button's visual weight.
    tone: ButtonTone,
    /// The action emitted when the button is clicked.
    action: WorkspaceNotesModalAction,
}

/// The workspace-notes modal view.
///
/// Holds the ported active/archive/editor state machine plus the cached note
/// lists and error the shell feeds it, and paints them as GPUI elements.
pub struct WorkspaceNotesModalView {
    colors: WorkspaceNotesModalColors,
    workspace_id: Option<WorkspaceId>,
    view: WorkspaceNotesView,
    draft_text: String,
    draft_dirty: bool,
    edit_text: String,
    edit_mode: WorkspaceNotesEditMode,
    scroll_offset: usize,
    active_notes: Vec<WorkspaceNoteEntry>,
    archived_notes: Vec<WorkspaceNoteEntry>,
    error: Option<String>,
    focus_handle: FocusHandle,
}

impl WorkspaceNotesModalView {
    /// Build a closed modal.
    pub fn new(colors: &WorkspaceNotesModalColors, cx: &mut Context<Self>) -> Self {
        Self {
            colors: *colors,
            workspace_id: None,
            view: WorkspaceNotesView::Active,
            draft_text: String::new(),
            draft_dirty: false,
            edit_text: String::new(),
            edit_mode: WorkspaceNotesEditMode::Draft,
            scroll_offset: 0,
            active_notes: Vec::new(),
            archived_notes: Vec::new(),
            error: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Open the modal for `workspace_id`, seeded with its saved draft.
    pub fn open(&mut self, workspace_id: WorkspaceId, draft_text: String, cx: &mut Context<Self>) {
        self.workspace_id = Some(workspace_id);
        self.view = WorkspaceNotesView::Active;
        self.draft_text = draft_text;
        self.draft_dirty = false;
        self.edit_text.clear();
        self.edit_mode = WorkspaceNotesEditMode::Draft;
        self.scroll_offset = 0;
        self.error = None;
        cx.notify();
    }

    /// Close the modal, resetting its editor state.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.workspace_id = None;
        self.view = WorkspaceNotesView::Active;
        self.draft_dirty = false;
        self.edit_text.clear();
        self.edit_mode = WorkspaceNotesEditMode::Draft;
        self.scroll_offset = 0;
        cx.notify();
    }

    /// Whether the modal is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.workspace_id.is_some()
    }

    /// The workspace the modal is bound to, if open.
    #[must_use]
    pub const fn workspace_id(&self) -> Option<WorkspaceId> {
        self.workspace_id
    }

    /// The current draft text.
    #[must_use]
    pub fn draft_text(&self) -> &str {
        &self.draft_text
    }

    /// Whether the draft has unwritten edits.
    #[must_use]
    pub const fn draft_dirty(&self) -> bool {
        self.draft_dirty
    }

    /// Replace the draft with a fresh server copy only when it is pristine.
    pub fn replace_pristine_draft(&mut self, text: String, cx: &mut Context<Self>) {
        if self.edit_mode == WorkspaceNotesEditMode::Draft && !self.draft_dirty {
            self.draft_text = text;
            cx.notify();
        }
    }

    /// Clear the draft-dirty flag after a `SaveDraft` flush.
    pub fn mark_draft_synced(&mut self) {
        if self.edit_mode == WorkspaceNotesEditMode::Draft {
            self.draft_dirty = false;
        }
    }

    /// The current active/archive view.
    #[must_use]
    pub const fn view(&self) -> WorkspaceNotesView {
        self.view
    }

    /// The editor's current target.
    #[must_use]
    pub const fn edit_mode(&self) -> &WorkspaceNotesEditMode {
        &self.edit_mode
    }

    /// The buffer the editor is currently showing (draft or edit text).
    #[must_use]
    pub fn active_text(&self) -> &str {
        match self.edit_mode {
            WorkspaceNotesEditMode::Draft => &self.draft_text,
            WorkspaceNotesEditMode::ActiveNote { .. }
            | WorkspaceNotesEditMode::ArchivedNote { .. }
            | WorkspaceNotesEditMode::ArchiveBulk { .. } => &self.edit_text,
        }
    }

    /// Split the bulk-edit buffer back into `(note_id, text)` updates.
    #[must_use]
    pub fn archive_bulk_updates(&self) -> Vec<(String, String)> {
        let WorkspaceNotesEditMode::ArchiveBulk { note_ids } = &self.edit_mode else {
            return Vec::new();
        };
        let mut parts = self.edit_text.split("\n---\n");
        let mut updates = Vec::new();
        for note_id in note_ids {
            if let Some(text) = parts.next() {
                updates.push((note_id.clone(), text.to_owned()));
            }
        }
        updates
    }

    /// Feed the current note lists (from the client store) to the view.
    pub fn set_notes(
        &mut self,
        active_notes: Vec<WorkspaceNoteEntry>,
        archived_notes: Vec<WorkspaceNoteEntry>,
        cx: &mut Context<Self>,
    ) {
        self.active_notes = active_notes;
        self.archived_notes = archived_notes;
        cx.notify();
    }

    /// Set (or clear) the server error surfaced in the footer.
    pub fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.error = error;
        cx.notify();
    }

    /// Switch the active/archive view, cancelling any non-draft edit.
    pub fn set_view(&mut self, view: WorkspaceNotesView, cx: &mut Context<Self>) {
        self.view = view;
        self.scroll_offset = 0;
        if !matches!(self.edit_mode, WorkspaceNotesEditMode::Draft) {
            self.cancel_edit();
        }
        cx.notify();
    }

    /// Begin editing an active note.
    pub fn begin_active_edit(&mut self, note: &WorkspaceNoteEntry, cx: &mut Context<Self>) {
        self.view = WorkspaceNotesView::Active;
        self.edit_text.clone_from(&note.text);
        self.edit_mode = WorkspaceNotesEditMode::ActiveNote { note_id: note.note_id.clone() };
        cx.notify();
    }

    /// Begin editing a single archived note.
    pub fn begin_archived_edit(&mut self, note: &WorkspaceNoteEntry, cx: &mut Context<Self>) {
        self.view = WorkspaceNotesView::Archive;
        self.edit_text.clone_from(&note.text);
        self.edit_mode = WorkspaceNotesEditMode::ArchivedNote { note_id: note.note_id.clone() };
        cx.notify();
    }

    /// Begin the bulk edit of every archived note.
    pub fn begin_archive_bulk_edit(
        &mut self,
        notes: &[WorkspaceNoteEntry],
        cx: &mut Context<Self>,
    ) {
        self.view = WorkspaceNotesView::Archive;
        let note_ids = notes.iter().map(|note| note.note_id.clone()).collect();
        self.edit_text =
            notes.iter().map(|note| note.text.as_str()).collect::<Vec<_>>().join("\n---\n");
        self.edit_mode = WorkspaceNotesEditMode::ArchiveBulk { note_ids };
        cx.notify();
    }

    /// Begin editing the cached active note with `note_id`, if present. Lets the
    /// shell route an [`WorkspaceNotesModalAction::EditActive`] without holding
    /// its own copy of the note list.
    pub fn begin_active_edit_by_id(&mut self, note_id: &str, cx: &mut Context<Self>) {
        if let Some(note) = self.active_notes.iter().find(|n| n.note_id == note_id).cloned() {
            self.begin_active_edit(&note, cx);
        }
    }

    /// Begin editing the cached archived note with `note_id`, if present.
    pub fn begin_archived_edit_by_id(&mut self, note_id: &str, cx: &mut Context<Self>) {
        if let Some(note) = self.archived_notes.iter().find(|n| n.note_id == note_id).cloned() {
            self.begin_archived_edit(&note, cx);
        }
    }

    /// Begin the bulk edit over every cached archived note.
    pub fn begin_archive_bulk_edit_all(&mut self, cx: &mut Context<Self>) {
        if !self.archived_notes.is_empty() {
            let notes = self.archived_notes.clone();
            self.begin_archive_bulk_edit(&notes, cx);
        }
    }

    /// Return the editor to the draft buffer.
    pub fn finish_edit(&mut self) {
        self.edit_text.clear();
        self.edit_mode = WorkspaceNotesEditMode::Draft;
    }

    /// Cancel the current edit (same as [`Self::finish_edit`]).
    pub fn cancel_edit(&mut self) {
        self.finish_edit();
    }

    /// Append `ch` to the active buffer, marking the draft dirty when editing it.
    pub fn push_char(&mut self, ch: char, cx: &mut Context<Self>) {
        match self.edit_mode {
            WorkspaceNotesEditMode::Draft => {
                self.draft_text.push(ch);
                self.draft_dirty = true;
            }
            WorkspaceNotesEditMode::ActiveNote { .. }
            | WorkspaceNotesEditMode::ArchivedNote { .. }
            | WorkspaceNotesEditMode::ArchiveBulk { .. } => self.edit_text.push(ch),
        }
        cx.notify();
    }

    /// Delete the last char of the active buffer.
    pub fn pop_char(&mut self, cx: &mut Context<Self>) {
        match self.edit_mode {
            WorkspaceNotesEditMode::Draft => {
                self.draft_text.pop();
                self.draft_dirty = true;
            }
            WorkspaceNotesEditMode::ActiveNote { .. }
            | WorkspaceNotesEditMode::ArchivedNote { .. }
            | WorkspaceNotesEditMode::ArchiveBulk { .. } => {
                self.edit_text.pop();
            }
        }
        cx.notify();
    }

    /// Scroll the current note list by `rows`, returning whether the offset
    /// moved.
    pub fn scroll_rows(&mut self, rows: i32, cx: &mut Context<Self>) -> bool {
        let note_count = match self.view {
            WorkspaceNotesView::Active => self.active_notes.len(),
            WorkspaceNotesView::Archive => self.archived_notes.len(),
        };
        let max_offset = note_count.saturating_sub(NOTE_LIST_ROWS);
        let previous = self.scroll_offset;
        if rows > 0 {
            self.scroll_offset = self
                .scroll_offset
                .saturating_add(usize::try_from(rows).unwrap_or(usize::MAX))
                .min(max_offset);
        } else if rows < 0 {
            self.scroll_offset = self
                .scroll_offset
                .saturating_sub(usize::try_from(rows.unsigned_abs()).unwrap_or(usize::MAX));
        }
        let moved = self.scroll_offset != previous;
        if moved {
            cx.notify();
        }
        moved
    }

    /// Map the current editor state to the [`WorkspaceNotesMutation`] a Save
    /// should send, or `None` when the buffer is blank (matching the winit
    /// trim guards). Ported from the winit `save_workspace_notes_modal`.
    #[must_use]
    pub fn save_mutation(&self) -> Option<WorkspaceNotesMutation> {
        let workspace_id = self.workspace_id?;
        match &self.edit_mode {
            WorkspaceNotesEditMode::Draft => {
                let text = self.draft_text.clone();
                if text.trim().is_empty() {
                    return None;
                }
                Some(WorkspaceNotesMutation::CreateActiveNote { workspace_id, text })
            }
            WorkspaceNotesEditMode::ActiveNote { note_id }
            | WorkspaceNotesEditMode::ArchivedNote { note_id } => {
                let text = self.edit_text.clone();
                if text.trim().is_empty() {
                    return None;
                }
                Some(WorkspaceNotesMutation::EditNote {
                    workspace_id,
                    note_id: note_id.clone(),
                    text,
                })
            }
            WorkspaceNotesEditMode::ArchiveBulk { .. } => {
                let updates = self.archive_bulk_updates();
                if updates.iter().any(|(_, text)| text.trim().is_empty()) {
                    return None;
                }
                Some(WorkspaceNotesMutation::BulkEditArchived { workspace_id, updates })
            }
        }
    }

    /// Build the `ArchiveNote` mutation for a Done/Removed control.
    #[must_use]
    pub fn archive_mutation(
        &self,
        note_id: &str,
        reason: ArchiveReason,
    ) -> Option<WorkspaceNotesMutation> {
        let workspace_id = self.workspace_id?;
        Some(WorkspaceNotesMutation::ArchiveNote {
            workspace_id,
            note_id: note_id.to_owned(),
            reason,
        })
    }

    fn button(&self, spec: ButtonSpec<'_>, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ButtonSpec { id, label, tone, action } = spec;
        let colors = self.colors;
        let (fg, bg) = match tone {
            ButtonTone::Normal => (colors.button_fg, colors.button_bg),
            ButtonTone::Selected => (colors.selected_fg, colors.selected_bg),
            ButtonTone::Primary => (colors.primary_fg, colors.primary_bg),
            ButtonTone::Danger => (colors.danger_fg, colors.button_bg),
        };
        div()
            .id(("wn-button", id))
            .px_2()
            .py_0p5()
            .rounded_sm()
            .text_sm()
            .text_color(fg)
            .bg(bg)
            .hover(move |s| s.bg(colors.selected_bg))
            .child(label.to_owned())
            .on_click(cx.listener(move |_, _, _win, ctx| {
                ctx.stop_propagation();
                ctx.emit(action.clone());
            }))
            .into_any_element()
    }

    fn render_note_row(
        &self,
        index: usize,
        note: &WorkspaceNoteEntry,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let marker = match self.view {
            WorkspaceNotesView::Active => "-",
            WorkspaceNotesView::Archive => "*",
        };
        let summary = single_line(&note.text, 60);
        let note_id = note.note_id.clone();
        let mut row = div()
            .id(("wn-note", index))
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_0p5()
            .rounded_sm()
            .bg(colors.row_bg)
            .child(div().text_sm().text_color(colors.muted_fg).child(marker))
            .child(div().flex_1().text_sm().text_color(colors.body_fg).child(summary));
        match self.view {
            WorkspaceNotesView::Active => {
                row = row
                    .child(self.button(
                        ButtonSpec {
                            id: index * 10,
                            label: "Edit",
                            tone: ButtonTone::Normal,
                            action: WorkspaceNotesModalAction::EditActive(note_id.clone()),
                        },
                        cx,
                    ))
                    .child(self.button(
                        ButtonSpec {
                            id: index * 10 + 1,
                            label: "Done",
                            tone: ButtonTone::Normal,
                            action: WorkspaceNotesModalAction::ArchiveDone(note_id.clone()),
                        },
                        cx,
                    ))
                    .child(self.button(
                        ButtonSpec {
                            id: index * 10 + 2,
                            label: "Remove",
                            tone: ButtonTone::Danger,
                            action: WorkspaceNotesModalAction::ArchiveRemoved(note_id),
                        },
                        cx,
                    ));
            }
            WorkspaceNotesView::Archive => {
                row = row.child(self.button(
                    ButtonSpec {
                        id: index * 10,
                        label: "Edit",
                        tone: ButtonTone::Normal,
                        action: WorkspaceNotesModalAction::EditArchived(note_id),
                    },
                    cx,
                ));
            }
        }
        row.into_any_element()
    }

    fn render_notes(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let notes = match self.view {
            WorkspaceNotesView::Active => &self.active_notes,
            WorkspaceNotesView::Archive => &self.archived_notes,
        };
        let mut list =
            div().w_full().flex().flex_col().gap_0p5().p_2().rounded_sm().bg(colors.panel_bg);
        if notes.is_empty() {
            let empty = match self.view {
                WorkspaceNotesView::Active => "No active notes",
                WorkspaceNotesView::Archive => "No archived notes",
            };
            list = list.child(div().text_sm().text_color(colors.muted_fg).child(empty));
        } else {
            for (visible_idx, note) in
                notes.iter().skip(self.scroll_offset).take(NOTE_LIST_ROWS).enumerate()
            {
                list = list.child(self.render_note_row(visible_idx, note, cx));
            }
        }
        if self.view == WorkspaceNotesView::Archive && !self.archived_notes.is_empty() {
            list = list.child(self.button(
                ButtonSpec {
                    id: 900,
                    label: "Edit all archived",
                    tone: ButtonTone::Normal,
                    action: WorkspaceNotesModalAction::EditAllArchive,
                },
                cx,
            ));
        }
        list.into_any_element()
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let colors = self.colors;
        let label = match self.edit_mode {
            WorkspaceNotesEditMode::Draft => "New",
            WorkspaceNotesEditMode::ActiveNote { .. } => "Edit active note",
            WorkspaceNotesEditMode::ArchivedNote { .. } => "Edit archived note",
            WorkspaceNotesEditMode::ArchiveBulk { .. } => "Edit archived notes",
        };
        let text = self.active_text();
        let editor_body: gpui::AnyElement = if text.is_empty() {
            div().text_sm().text_color(colors.muted_fg).child("Type note...").into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .children(
                    text.lines()
                        .map(|line| {
                            div()
                                .text_sm()
                                .text_color(colors.body_fg)
                                .child(line.to_owned())
                                .into_any_element()
                        })
                        .collect::<Vec<_>>(),
                )
                .into_any_element()
        };
        // A slim caret bar mirrors the winit editor's block caret.
        let caret = div().w(px(2.0)).h(px(14.0)).bg(colors.caret);

        let mut footer = div().flex().items_center().gap_2().child(self.button(
            ButtonSpec {
                id: 800,
                label: "Save",
                tone: ButtonTone::Primary,
                action: WorkspaceNotesModalAction::Save,
            },
            cx,
        ));
        if !matches!(self.edit_mode, WorkspaceNotesEditMode::Draft) {
            footer = footer.child(self.button(
                ButtonSpec {
                    id: 801,
                    label: "Cancel",
                    tone: ButtonTone::Normal,
                    action: WorkspaceNotesModalAction::CancelEdit,
                },
                cx,
            ));
        }
        footer = footer.child(
            div()
                .flex_1()
                .text_xs()
                .text_color(colors.muted_fg)
                .child("Enter save, Ctrl+Enter newline"),
        );

        let mut section = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_xs().text_color(colors.muted_fg).child(label))
            .child(
                div()
                    .w_full()
                    .min_h(px(72.0))
                    .p_2()
                    .rounded_sm()
                    .bg(colors.input_bg)
                    .flex()
                    .items_start()
                    .gap_1()
                    .child(editor_body)
                    .child(caret),
            );
        if let Some(error) = &self.error {
            section = section
                .child(div().text_xs().text_color(colors.danger_fg).child(single_line(error, 76)));
        }
        section.child(footer).into_any_element()
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let tone = |active: bool| if active { ButtonTone::Selected } else { ButtonTone::Normal };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(self.button(
                ButtonSpec {
                    id: 100,
                    label: " Active ",
                    tone: tone(self.view == WorkspaceNotesView::Active),
                    action: WorkspaceNotesModalAction::ShowActive,
                },
                cx,
            ))
            .child(self.button(
                ButtonSpec {
                    id: 101,
                    label: " Archive ",
                    tone: tone(self.view == WorkspaceNotesView::Archive),
                    action: WorkspaceNotesModalAction::ShowArchive,
                },
                cx,
            ))
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().text_sm().text_color(self.colors.title_fg).child("Workspace notes"))
            .child(self.button(
                ButtonSpec {
                    id: 102,
                    label: " Close ",
                    tone: ButtonTone::Normal,
                    action: WorkspaceNotesModalAction::Close,
                },
                cx,
            ))
            .into_any_element()
    }
}

impl Render for WorkspaceNotesModalView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        if !self.is_open() {
            return div();
        }
        let header = self.render_header(cx);
        let nav = self.render_nav(cx);
        let notes = self.render_notes(cx);
        let editor = self.render_editor(cx);

        div()
            .track_focus(&self.focus_handle)
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_center()
            .bg(colors.backdrop)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|_, _, _win, ctx| {
                    ctx.emit(WorkspaceNotesModalAction::Close);
                }),
            )
            .child(
                div()
                    .w(px(560.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .bg(colors.modal_bg)
                    .border_1()
                    .border_color(colors.border)
                    .rounded_lg()
                    .shadow_lg()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
                    .child(header)
                    .child(nav)
                    .child(notes)
                    .child(editor),
            )
    }
}

/// Flatten whitespace runs and cap at `max_chars`, appending an ellipsis when
/// truncated.
fn single_line(text: &str, max_chars: usize) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = flattened.chars().take(max_chars).collect::<String>();
    if flattened.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    Rgba { a: alpha.clamp(0.0, 1.0), ..color }
}

fn lighten(color: [f32; 4], amount: f32) -> [f32; 4] {
    [
        (color[0] + amount).min(1.0),
        (color[1] + amount).min(1.0),
        (color[2] + amount).min(1.0),
        color[3],
    ]
}

/// The delay between the last draft keystroke and the coalesced `SaveDraft`
/// flush, ported verbatim from the winit client's `WORKSPACE_NOTES_DEBOUNCE`.
pub const WORKSPACE_NOTES_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

/// Emitted when the debounce window elapses and the shell should flush the
/// pending draft through a `SaveDraft` mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftDebounceEvent {
    /// The debounce elapsed (or was forced); flush the current draft now.
    Flush,
}

/// Coalescing timer for draft-text persistence.
///
/// Ports the winit client's `workspace_notes_save_pending` Instant that gated
/// `flush_workspace_notes_if_due`: each keystroke restarts a
/// [`WORKSPACE_NOTES_DEBOUNCE`] timer, so a burst of typing produces exactly one
/// [`DraftDebounceEvent::Flush`] once the user pauses, rather than one write per
/// character. Higher-priority handoff and window-close paths force an immediate
/// flush with [`Self::flush_now`].
pub struct DraftDebounce {
    /// Monotonically increasing marker so a superseded timer never fires.
    generation: u64,
    /// The in-flight timer task; dropping it cancels the pending flush.
    pending: Option<gpui::Task<()>>,
}

impl EventEmitter<DraftDebounceEvent> for DraftDebounce {}

impl Default for DraftDebounce {
    fn default() -> Self {
        Self::new()
    }
}

impl DraftDebounce {
    /// Build an idle debounce with no pending flush.
    #[must_use]
    pub const fn new() -> Self {
        Self { generation: 0, pending: None }
    }

    /// Whether a flush is currently scheduled.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Record a draft edit, (re)starting the debounce window. Any previously
    /// scheduled flush is cancelled, so continuous typing collapses to one
    /// flush after the user pauses for [`WORKSPACE_NOTES_DEBOUNCE`].
    pub fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        // Assigning a new task drops the previous one, cancelling its timer.
        self.pending = Some(cx.spawn(async move |this, app| {
            app.background_executor().timer(WORKSPACE_NOTES_DEBOUNCE).await;
            this.update(app, |this, ecx| this.fire_if_current(generation, ecx)).ok();
        }));
    }

    /// Emit a flush only when the timer that woke us is still the current one —
    /// a later [`Self::mark_dirty`] supersedes an in-flight timer.
    fn fire_if_current(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.generation == generation {
            self.pending = None;
            cx.emit(DraftDebounceEvent::Flush);
        }
    }

    /// Force an immediate flush when one is pending, cancelling the timer.
    /// Used by the workspace-switch, overlay-handoff, and window-close paths
    /// that must persist a draft before yielding.
    pub fn flush_now(&mut self, cx: &mut Context<Self>) {
        if self.pending.take().is_some() {
            self.generation = self.generation.wrapping_add(1);
            cx.emit(DraftDebounceEvent::Flush);
        }
    }
}

#[cfg(test)]
mod tests;
