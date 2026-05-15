//! GPU-rendered per-workspace notes modal.

use scribe_common::ids::WorkspaceId;
use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;
use crate::workspace_notes::WorkspaceNoteEntry;

type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

const MIN_MODAL_COLS: usize = 44;
const MAX_MODAL_COLS: usize = 82;
const MODAL_ROWS: usize = 31;
const MAX_MODAL_GRID_UNITS: usize = 65_535;
const PAD_COLS: usize = 3;
const HEADER_TITLE_ROW: usize = 1;
const NAV_ROW: usize = 3;
const HEADER_RULE_ROW: usize = 5;
const NOTE_LIST_TOP: usize = 7;
const NOTE_LIST_ROWS: usize = 8;
const ARCHIVE_ACTION_ROW: usize = 16;
const EDITOR_LABEL_ROW: usize = 18;
const EDITOR_INPUT_TOP: usize = 20;
const EDITOR_ROWS: usize = 5;
const EDITOR_CONTENT_TOP: usize = EDITOR_INPUT_TOP + 1;
const EDITOR_CONTENT_ROWS: usize = EDITOR_ROWS - 1;
const FOOTER_ROW: usize = 29;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceNotesView {
    Active,
    Archive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceNotesEditMode {
    Draft,
    ActiveNote { note_id: String },
    ArchivedNote { note_id: String },
    ArchiveBulk { note_ids: Vec<String> },
}

#[derive(Clone, Debug)]
pub enum WorkspaceNotesModalAction {
    Close,
    Save,
    CancelEdit,
    ShowActive,
    ShowArchive,
    EditActive(String),
    EditArchived(String),
    ArchiveDone(String),
    ArchiveRemoved(String),
    EditAllArchive,
}

#[derive(Clone)]
struct HitTarget {
    action: WorkspaceNotesModalAction,
    rect: Rect,
}

pub struct WorkspaceNotesModal {
    workspace_id: Option<WorkspaceId>,
    view: WorkspaceNotesView,
    draft_text: String,
    draft_dirty: bool,
    edit_text: String,
    edit_mode: WorkspaceNotesEditMode,
    scroll_offset: usize,
    hovered: Option<usize>,
    modal_rect: Option<Rect>,
    hit_targets: Vec<HitTarget>,
}

pub struct WorkspaceNotesModalBuildContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub workspace_rect: Rect,
    pub cell_size: (f32, f32),
    pub chrome: &'a ChromeColors,
    pub active_notes: &'a [WorkspaceNoteEntry],
    pub archived_notes: &'a [WorkspaceNoteEntry],
    pub error: Option<&'a str>,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

impl WorkspaceNotesModal {
    pub fn new() -> Self {
        Self {
            workspace_id: None,
            view: WorkspaceNotesView::Active,
            draft_text: String::new(),
            draft_dirty: false,
            edit_text: String::new(),
            edit_mode: WorkspaceNotesEditMode::Draft,
            scroll_offset: 0,
            hovered: None,
            modal_rect: None,
            hit_targets: Vec::new(),
        }
    }

    pub fn open(&mut self, workspace_id: WorkspaceId, draft_text: String) {
        self.workspace_id = Some(workspace_id);
        self.view = WorkspaceNotesView::Active;
        self.draft_text = draft_text;
        self.draft_dirty = false;
        self.edit_text.clear();
        self.edit_mode = WorkspaceNotesEditMode::Draft;
        self.scroll_offset = 0;
        self.hovered = None;
        self.modal_rect = None;
        self.hit_targets.clear();
    }

    pub fn close(&mut self) {
        self.workspace_id = None;
        self.view = WorkspaceNotesView::Active;
        self.draft_dirty = false;
        self.edit_text.clear();
        self.edit_mode = WorkspaceNotesEditMode::Draft;
        self.scroll_offset = 0;
        self.hovered = None;
        self.modal_rect = None;
        self.hit_targets.clear();
    }

    pub const fn is_open(&self) -> bool {
        self.workspace_id.is_some()
    }

    pub const fn workspace_id(&self) -> Option<WorkspaceId> {
        self.workspace_id
    }

    pub fn draft_text(&self) -> &str {
        &self.draft_text
    }

    pub const fn draft_dirty(&self) -> bool {
        self.draft_dirty
    }

    pub fn replace_pristine_draft(&mut self, text: String) {
        if self.edit_mode == WorkspaceNotesEditMode::Draft && !self.draft_dirty {
            self.draft_text = text;
        }
    }

    pub fn mark_draft_synced(&mut self) {
        if self.edit_mode == WorkspaceNotesEditMode::Draft {
            self.draft_dirty = false;
        }
    }

    pub fn edit_mode(&self) -> &WorkspaceNotesEditMode {
        &self.edit_mode
    }

    pub fn active_text(&self) -> &str {
        match self.edit_mode {
            WorkspaceNotesEditMode::Draft => &self.draft_text,
            WorkspaceNotesEditMode::ActiveNote { .. }
            | WorkspaceNotesEditMode::ArchivedNote { .. }
            | WorkspaceNotesEditMode::ArchiveBulk { .. } => &self.edit_text,
        }
    }

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

    pub fn set_view(&mut self, view: WorkspaceNotesView) {
        self.view = view;
        self.scroll_offset = 0;
        if !matches!(self.edit_mode, WorkspaceNotesEditMode::Draft) {
            self.cancel_edit();
        }
    }

    pub fn begin_active_edit(&mut self, note: &WorkspaceNoteEntry) {
        self.view = WorkspaceNotesView::Active;
        self.edit_text.clone_from(&note.text);
        self.edit_mode = WorkspaceNotesEditMode::ActiveNote { note_id: note.note_id.clone() };
    }

    pub fn begin_archived_edit(&mut self, note: &WorkspaceNoteEntry) {
        self.view = WorkspaceNotesView::Archive;
        self.edit_text.clone_from(&note.text);
        self.edit_mode = WorkspaceNotesEditMode::ArchivedNote { note_id: note.note_id.clone() };
    }

    pub fn begin_archive_bulk_edit(&mut self, notes: &[WorkspaceNoteEntry]) {
        self.view = WorkspaceNotesView::Archive;
        let note_ids = notes.iter().map(|note| note.note_id.clone()).collect();
        self.edit_text =
            notes.iter().map(|note| note.text.as_str()).collect::<Vec<_>>().join("\n---\n");
        self.edit_mode = WorkspaceNotesEditMode::ArchiveBulk { note_ids };
    }

    pub fn finish_edit(&mut self) {
        self.edit_text.clear();
        self.edit_mode = WorkspaceNotesEditMode::Draft;
    }

    pub fn cancel_edit(&mut self) {
        self.finish_edit();
    }

    pub fn push_char(&mut self, ch: char) {
        match self.edit_mode {
            WorkspaceNotesEditMode::Draft => {
                self.draft_text.push(ch);
                self.draft_dirty = true;
            }
            WorkspaceNotesEditMode::ActiveNote { .. }
            | WorkspaceNotesEditMode::ArchivedNote { .. }
            | WorkspaceNotesEditMode::ArchiveBulk { .. } => self.edit_text.push(ch),
        }
    }

    pub fn pop_char(&mut self) {
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
    }

    pub fn click(&self, x: f32, y: f32) -> Option<WorkspaceNotesModalAction> {
        self.hit_targets
            .iter()
            .find(|target| target.rect.contains(x, y))
            .map(|target| target.action.clone())
    }

    pub fn contains_point(&self, x: f32, y: f32) -> bool {
        self.modal_rect.is_some_and(|rect| rect.contains(x, y))
    }

    pub fn update_hover(&mut self, x: f32, y: f32) -> bool {
        let prev = self.hovered;
        self.hovered = self.hit_targets.iter().position(|target| target.rect.contains(x, y));
        self.hovered != prev
    }

    pub fn scroll_rows(&mut self, rows: i32, active_count: usize, archived_count: usize) -> bool {
        let note_count = match self.view {
            WorkspaceNotesView::Active => active_count,
            WorkspaceNotesView::Archive => archived_count,
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
        self.scroll_offset != previous
    }

    pub fn build_instances(&mut self, ctx: WorkspaceNotesModalBuildContext<'_>) {
        let WorkspaceNotesModalBuildContext {
            out,
            workspace_rect,
            cell_size,
            chrome,
            active_notes,
            archived_notes,
            error,
            resolve_glyph,
        } = ctx;
        let (cell_w, cell_h) = cell_size;
        if self.workspace_id.is_none() || cell_w <= 0.0 || cell_h <= 0.0 {
            self.modal_rect = None;
            return;
        }

        let Some(layout) = ModalLayout::new(workspace_rect, cell_size) else {
            self.modal_rect = None;
            return;
        };
        let colors = ModalColors::from_chrome(chrome);
        self.modal_rect = Some(layout.modal_rect);
        self.hit_targets.clear();

        let mut renderer = ModalRenderer { out, layout, colors, cell_size, resolve_glyph };
        renderer.push_solid_rect(workspace_rect, colors.backdrop);
        renderer.push_solid_rect(layout.modal_rect, colors.modal_bg);
        renderer.draw_frame();
        renderer.draw_rule(HEADER_RULE_ROW);
        renderer.emit_text(
            "Workspace notes",
            HEADER_TITLE_ROW,
            PAD_COLS,
            TextStyle::new(colors.title_fg, colors.modal_bg),
        );
        self.render_nav(&mut renderer);
        self.render_notes(&mut renderer, active_notes, archived_notes);
        self.render_editor(&mut renderer, error);
    }

    fn render_nav(&mut self, renderer: &mut ModalRenderer<'_, '_>) {
        let active_tone = if self.view == WorkspaceNotesView::Active {
            ButtonTone::Selected
        } else {
            ButtonTone::Normal
        };
        let archive_tone = if self.view == WorkspaceNotesView::Archive {
            ButtonTone::Selected
        } else {
            ButtonTone::Normal
        };
        let active_rect = renderer.button(NAV_ROW, PAD_COLS, " Active ", active_tone);
        self.hit_targets
            .push(HitTarget { action: WorkspaceNotesModalAction::ShowActive, rect: active_rect });
        let archive_col = PAD_COLS + 10;
        let archive_rect = renderer.button(NAV_ROW, archive_col, " Archive ", archive_tone);
        self.hit_targets
            .push(HitTarget { action: WorkspaceNotesModalAction::ShowArchive, rect: archive_rect });

        let close_col = renderer.layout.modal_cols.saturating_sub(10);
        let close_rect =
            renderer.button(HEADER_TITLE_ROW, close_col, " Close ", ButtonTone::Normal);
        self.hit_targets
            .push(HitTarget { action: WorkspaceNotesModalAction::Close, rect: close_rect });
    }

    fn render_notes(
        &mut self,
        renderer: &mut ModalRenderer<'_, '_>,
        active_notes: &[WorkspaceNoteEntry],
        archived_notes: &[WorkspaceNoteEntry],
    ) {
        let notes = match self.view {
            WorkspaceNotesView::Active => active_notes,
            WorkspaceNotesView::Archive => archived_notes,
        };
        let list_top = NOTE_LIST_TOP;
        let list_rows = NOTE_LIST_ROWS;
        renderer.draw_panel(list_top, list_rows + 1, renderer.colors.panel_bg);
        if notes.is_empty() {
            let empty = match self.view {
                WorkspaceNotesView::Active => "No active notes",
                WorkspaceNotesView::Archive => "No archived notes",
            };
            renderer.emit_text(
                empty,
                list_top + 1,
                PAD_COLS + 1,
                TextStyle::new(renderer.colors.muted_fg, renderer.colors.panel_bg),
            );
        }

        for (visible_idx, note) in notes.iter().skip(self.scroll_offset).take(list_rows).enumerate()
        {
            let row = list_top + 1 + visible_idx;
            renderer.draw_note_row(row);
            let marker = match self.view {
                WorkspaceNotesView::Active => "-",
                WorkspaceNotesView::Archive => "*",
            };
            let edit_col = renderer.layout.modal_cols.saturating_sub(match self.view {
                WorkspaceNotesView::Active => 25,
                WorkspaceNotesView::Archive => 10,
            });
            let summary_width = edit_col.saturating_sub(PAD_COLS + 5);
            let summary = single_line(&note.text, summary_width);
            renderer.emit_text(
                marker,
                row,
                PAD_COLS,
                TextStyle::new(renderer.colors.muted_fg, renderer.colors.row_bg),
            );
            renderer.emit_text(
                &summary,
                row,
                PAD_COLS + 2,
                TextStyle::new(renderer.colors.body_fg, renderer.colors.row_bg),
            );
            self.render_note_actions(renderer, note, row, edit_col);
        }

        if self.view == WorkspaceNotesView::Archive && !archived_notes.is_empty() {
            let rect = renderer.button(
                ARCHIVE_ACTION_ROW,
                PAD_COLS,
                " Edit all archived ",
                ButtonTone::Normal,
            );
            self.hit_targets
                .push(HitTarget { action: WorkspaceNotesModalAction::EditAllArchive, rect });
        }
    }

    fn render_note_actions(
        &mut self,
        renderer: &mut ModalRenderer<'_, '_>,
        note: &WorkspaceNoteEntry,
        row: usize,
        edit_col: usize,
    ) {
        let edit_rect = renderer.button(row, edit_col, " Edit ", ButtonTone::Normal);
        match self.view {
            WorkspaceNotesView::Active => {
                self.hit_targets.push(HitTarget {
                    action: WorkspaceNotesModalAction::EditActive(note.note_id.clone()),
                    rect: edit_rect,
                });
                let done_rect = renderer.button(row, edit_col + 7, " Done ", ButtonTone::Normal);
                self.hit_targets.push(HitTarget {
                    action: WorkspaceNotesModalAction::ArchiveDone(note.note_id.clone()),
                    rect: done_rect,
                });
                let remove_rect =
                    renderer.button(row, edit_col + 14, " Remove ", ButtonTone::Danger);
                self.hit_targets.push(HitTarget {
                    action: WorkspaceNotesModalAction::ArchiveRemoved(note.note_id.clone()),
                    rect: remove_rect,
                });
            }
            WorkspaceNotesView::Archive => {
                self.hit_targets.push(HitTarget {
                    action: WorkspaceNotesModalAction::EditArchived(note.note_id.clone()),
                    rect: edit_rect,
                });
            }
        }
    }

    fn render_editor(&mut self, renderer: &mut ModalRenderer<'_, '_>, error: Option<&str>) {
        let label = match self.edit_mode {
            WorkspaceNotesEditMode::Draft => "New",
            WorkspaceNotesEditMode::ActiveNote { .. } => "Edit active note",
            WorkspaceNotesEditMode::ArchivedNote { .. } => "Edit archived note",
            WorkspaceNotesEditMode::ArchiveBulk { .. } => "Edit archived notes",
        };
        renderer.emit_text(
            label,
            EDITOR_LABEL_ROW,
            PAD_COLS,
            TextStyle::new(renderer.colors.muted_fg, renderer.colors.modal_bg),
        );
        renderer.draw_input_box(EDITOR_INPUT_TOP, EDITOR_ROWS);
        let text = self.active_text().to_owned();
        let editor_text_width = renderer.layout.modal_cols.saturating_sub(8);
        for (idx, line) in text.lines().take(EDITOR_CONTENT_ROWS).enumerate() {
            renderer.emit_text(
                &single_line(line, editor_text_width),
                EDITOR_CONTENT_TOP + idx,
                PAD_COLS + 1,
                TextStyle::new(renderer.colors.body_fg, renderer.colors.input_bg),
            );
        }
        if text.is_empty() {
            renderer.emit_text(
                "Type note...",
                EDITOR_CONTENT_TOP,
                PAD_COLS + 2,
                TextStyle::new(renderer.colors.muted_fg, renderer.colors.input_bg),
            );
        }
        let (cursor_row, cursor_col) = editor_cursor_position(&text, editor_text_width);
        renderer.draw_cursor(EDITOR_CONTENT_TOP + cursor_row, PAD_COLS + 1 + cursor_col);

        let save_rect = renderer.button(FOOTER_ROW, PAD_COLS, " Save ", ButtonTone::Primary);
        self.hit_targets
            .push(HitTarget { action: WorkspaceNotesModalAction::Save, rect: save_rect });
        if !matches!(self.edit_mode, WorkspaceNotesEditMode::Draft) {
            let cancel_rect =
                renderer.button(FOOTER_ROW, PAD_COLS + 8, " Cancel ", ButtonTone::Normal);
            self.hit_targets.push(HitTarget {
                action: WorkspaceNotesModalAction::CancelEdit,
                rect: cancel_rect,
            });
        }
        if let Some(error) = error {
            renderer.emit_text(
                &single_line(error, renderer.layout.modal_cols.saturating_sub(PAD_COLS * 2)),
                FOOTER_ROW - 2,
                PAD_COLS,
                TextStyle::new(renderer.colors.button_danger_fg, renderer.colors.modal_bg),
            );
        }
        renderer.emit_text(
            "Enter newline, Ctrl+Enter save",
            FOOTER_ROW,
            renderer.layout.modal_cols.saturating_sub(34),
            TextStyle::new(renderer.colors.muted_fg, renderer.colors.modal_bg),
        );
    }
}

#[derive(Clone, Copy)]
struct ModalLayout {
    modal_rect: Rect,
    modal_cols: usize,
}

impl ModalLayout {
    fn new(workspace_rect: Rect, cell_size: (f32, f32)) -> Option<Self> {
        let (cell_w, cell_h) = cell_size;
        let available_cols = units_in_extent(workspace_rect.width * 0.86, cell_w);
        if available_cols < MIN_MODAL_COLS {
            return None;
        }
        let modal_cols = available_cols.min(MAX_MODAL_COLS);
        let modal_w = grid_width(modal_cols, cell_w);
        let modal_h = grid_height(MODAL_ROWS, cell_h);
        Some(Self {
            modal_rect: Rect {
                x: workspace_rect.x + (workspace_rect.width - modal_w) / 2.0,
                y: workspace_rect.y + (workspace_rect.height - modal_h) / 2.0,
                width: modal_w,
                height: modal_h,
            },
            modal_cols,
        })
    }
}

#[derive(Clone, Copy)]
struct ModalColors {
    backdrop: [f32; 4],
    modal_bg: [f32; 4],
    panel_bg: [f32; 4],
    row_bg: [f32; 4],
    border: [f32; 4],
    separator: [f32; 4],
    title_fg: [f32; 4],
    body_fg: [f32; 4],
    muted_fg: [f32; 4],
    button_fg: [f32; 4],
    button_bg: [f32; 4],
    button_selected_fg: [f32; 4],
    button_selected_bg: [f32; 4],
    button_primary_fg: [f32; 4],
    button_primary_bg: [f32; 4],
    button_danger_fg: [f32; 4],
    button_danger_bg: [f32; 4],
    input_bg: [f32; 4],
    input_border: [f32; 4],
    cursor: [f32; 4],
}

#[derive(Clone, Copy)]
enum ButtonTone {
    Normal,
    Selected,
    Primary,
    Danger,
}

#[derive(Clone, Copy)]
struct TextStyle {
    fg: [f32; 4],
    bg: [f32; 4],
}

impl TextStyle {
    const fn new(fg: [f32; 4], bg: [f32; 4]) -> Self {
        Self { fg, bg }
    }
}

impl ModalColors {
    fn from_chrome(chrome: &ChromeColors) -> Self {
        Self {
            backdrop: srgb_to_linear_rgba(with_alpha(chrome.tab_bar_bg, 0.46)),
            modal_bg: srgb_to_linear_rgba(lighten(chrome.tab_bar_bg, 0.012)),
            panel_bg: srgb_to_linear_rgba(lighten(chrome.tab_bar_bg, 0.024)),
            row_bg: srgb_to_linear_rgba(lighten(chrome.tab_bar_bg, 0.038)),
            border: srgb_to_linear_rgba(with_alpha(chrome.tab_separator, 0.92)),
            separator: srgb_to_linear_rgba(with_alpha(chrome.tab_text, 0.13)),
            title_fg: srgb_to_linear_rgba(chrome.tab_text_active),
            body_fg: srgb_to_linear_rgba(chrome.tab_text_active),
            muted_fg: srgb_to_linear_rgba(with_alpha(chrome.tab_text, 0.68)),
            button_fg: srgb_to_linear_rgba(chrome.tab_text),
            button_bg: srgb_to_linear_rgba(lighten(chrome.tab_bar_bg, 0.045)),
            button_selected_fg: srgb_to_linear_rgba(chrome.tab_text_active),
            button_selected_bg: srgb_to_linear_rgba(with_alpha(chrome.accent, 0.18)),
            button_primary_fg: srgb_to_linear_rgba(chrome.tab_bar_bg),
            button_primary_bg: srgb_to_linear_rgba(with_alpha(chrome.accent, 0.84)),
            button_danger_fg: srgb_to_linear_rgba(with_alpha(chrome.tab_text_active, 0.86)),
            button_danger_bg: srgb_to_linear_rgba(lighten(chrome.tab_bar_bg, 0.055)),
            input_bg: srgb_to_linear_rgba(lighten(chrome.tab_bar_bg, 0.034)),
            input_border: srgb_to_linear_rgba(with_alpha(chrome.tab_separator, 0.88)),
            cursor: srgb_to_linear_rgba(with_alpha(chrome.accent, 0.95)),
        }
    }
}

struct ModalRenderer<'a, 'b> {
    out: &'a mut Vec<CellInstance>,
    layout: ModalLayout,
    colors: ModalColors,
    cell_size: (f32, f32),
    resolve_glyph: &'a mut GlyphResolver<'b>,
}

impl ModalRenderer<'_, '_> {
    fn push_solid_rect(&mut self, rect: Rect, color: [f32; 4]) {
        self.out.push(CellInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: color,
            bg_color: color,
            corner_radius: 0.0,
        });
    }

    fn draw_frame(&mut self) {
        let rect = self.layout.modal_rect;
        self.push_solid_rect(
            Rect { x: rect.x, y: rect.y, width: rect.width, height: 1.0 },
            self.colors.border,
        );
        self.push_solid_rect(
            Rect { x: rect.x, y: rect.y + rect.height - 1.0, width: rect.width, height: 1.0 },
            self.colors.border,
        );
        self.push_solid_rect(
            Rect { x: rect.x, y: rect.y, width: 1.0, height: rect.height },
            self.colors.border,
        );
        self.push_solid_rect(
            Rect { x: rect.x + rect.width - 1.0, y: rect.y, width: 1.0, height: rect.height },
            self.colors.border,
        );
    }

    fn button(&mut self, row: usize, col: usize, label: &str, tone: ButtonTone) -> Rect {
        let width_cols = label.chars().count();
        let rect = Rect {
            x: grid_x(self.layout.modal_rect.x, col, self.cell_size.0),
            y: grid_y(self.layout.modal_rect.y, row, self.cell_size.1),
            width: grid_width(width_cols, self.cell_size.0),
            height: self.cell_size.1,
        };
        let (fg, bg) = match tone {
            ButtonTone::Normal => (self.colors.button_fg, self.colors.button_bg),
            ButtonTone::Selected => {
                (self.colors.button_selected_fg, self.colors.button_selected_bg)
            }
            ButtonTone::Primary => (self.colors.button_primary_fg, self.colors.button_primary_bg),
            ButtonTone::Danger => (self.colors.button_danger_fg, self.colors.button_danger_bg),
        };
        self.push_solid_rect(rect, bg);
        self.emit_text(label, row, col, TextStyle::new(fg, bg));
        rect
    }

    fn draw_panel(&mut self, row: usize, rows: usize, color: [f32; 4]) {
        let rect = Rect {
            x: grid_x(self.layout.modal_rect.x, PAD_COLS, self.cell_size.0),
            y: grid_y(self.layout.modal_rect.y, row, self.cell_size.1),
            width: grid_width(
                self.layout.modal_cols.saturating_sub(PAD_COLS * 2),
                self.cell_size.0,
            ),
            height: grid_height(rows, self.cell_size.1),
        };
        self.push_solid_rect(rect, color);
    }

    fn draw_note_row(&mut self, row: usize) {
        let rect = Rect {
            x: grid_x(self.layout.modal_rect.x, PAD_COLS, self.cell_size.0),
            y: grid_y(self.layout.modal_rect.y, row, self.cell_size.1),
            width: grid_width(
                self.layout.modal_cols.saturating_sub(PAD_COLS * 2),
                self.cell_size.0,
            ),
            height: self.cell_size.1,
        };
        self.push_solid_rect(rect, self.colors.row_bg);
    }

    fn draw_input_box(&mut self, row: usize, rows: usize) {
        let rect = Rect {
            x: grid_x(self.layout.modal_rect.x, PAD_COLS, self.cell_size.0),
            y: grid_y(self.layout.modal_rect.y, row, self.cell_size.1),
            width: grid_width(
                self.layout.modal_cols.saturating_sub(PAD_COLS * 2),
                self.cell_size.0,
            ),
            height: grid_height(rows, self.cell_size.1),
        };
        self.push_solid_rect(rect, self.colors.input_bg);
        self.push_solid_rect(
            Rect { x: rect.x, y: rect.y, width: rect.width, height: 1.0 },
            self.colors.input_border,
        );
    }

    fn draw_cursor(&mut self, row: usize, col: usize) {
        let max_col = self.layout.modal_cols.saturating_sub(PAD_COLS + 2);
        let rect = Rect {
            x: grid_x(self.layout.modal_rect.x, col.min(max_col), self.cell_size.0),
            y: grid_y(self.layout.modal_rect.y, row, self.cell_size.1),
            width: f32::max(2.0, self.cell_size.0 / 8.0),
            height: self.cell_size.1,
        };
        self.push_solid_rect(rect, self.colors.cursor);
    }

    fn draw_rule(&mut self, row: usize) {
        let rect = Rect {
            x: self.layout.modal_rect.x,
            y: grid_y(self.layout.modal_rect.y, row, self.cell_size.1),
            width: self.layout.modal_rect.width,
            height: 1.0,
        };
        self.push_solid_rect(rect, self.colors.separator);
    }

    fn emit_text(&mut self, text: &str, row: usize, start_col: usize, style: TextStyle) {
        let y = grid_y(self.layout.modal_rect.y, row, self.cell_size.1);
        for (idx, ch) in text.chars().enumerate() {
            let col = start_col + idx;
            if col >= self.layout.modal_cols {
                break;
            }
            let x = grid_x(self.layout.modal_rect.x, col, self.cell_size.0);
            let (uv_min, uv_max) = (self.resolve_glyph)(ch);
            self.out.push(CellInstance {
                pos: [x, y],
                size: [0.0, 0.0],
                uv_min,
                uv_max,
                fg_color: style.fg,
                bg_color: style.bg,
                corner_radius: 0.0,
            });
        }
    }
}

fn single_line(text: &str, max_chars: usize) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = flattened.chars().take(max_chars).collect::<String>();
    let truncated = flattened.chars().count() > max_chars;
    if truncated {
        out.push_str("...");
    }
    out
}

fn editor_cursor_position(text: &str, max_cols: usize) -> (usize, usize) {
    let mut row = 0usize;
    let mut col = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            row = row.saturating_add(1);
            col = 0;
        } else {
            col = col.saturating_add(1);
        }
    }
    (row.min(EDITOR_CONTENT_ROWS.saturating_sub(1)), col.min(max_cols))
}

fn render_grid_units(units: usize) -> u16 {
    u16::try_from(units.min(MAX_MODAL_GRID_UNITS)).unwrap_or(u16::MAX)
}

fn grid_x(origin: f32, col: usize, cell_w: f32) -> f32 {
    origin + f32::from(render_grid_units(col)) * cell_w
}

fn grid_y(origin: f32, row: usize, cell_h: f32) -> f32 {
    origin + f32::from(render_grid_units(row)) * cell_h
}

fn grid_width(cols: usize, cell_w: f32) -> f32 {
    f32::from(render_grid_units(cols)) * cell_w
}

fn grid_height(rows: usize, cell_h: f32) -> f32 {
    f32::from(render_grid_units(rows)) * cell_h
}

fn units_in_extent(extent: f32, unit: f32) -> usize {
    if unit <= 0.0 || !extent.is_finite() || extent <= 0.0 {
        return 0;
    }
    let mut low = 0usize;
    let mut high = 1usize;
    while high < MAX_MODAL_GRID_UNITS && grid_width(high, unit) <= extent {
        low = high;
        high = high.saturating_mul(2).min(MAX_MODAL_GRID_UNITS);
        if high == low {
            break;
        }
    }
    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if grid_width(mid, unit) <= extent {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    low
}

fn with_alpha(mut color: [f32; 4], alpha: f32) -> [f32; 4] {
    color[3] = alpha.clamp(0.0, 1.0);
    color
}

fn lighten(mut color: [f32; 4], amount: f32) -> [f32; 4] {
    color[0] = (color[0] + amount).min(1.0);
    color[1] = (color[1] + amount).min(1.0);
    color[2] = (color[2] + amount).min(1.0);
    color
}
