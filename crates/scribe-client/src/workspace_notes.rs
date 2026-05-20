//! Non-durable client cache for server-owned workspace notes.

use std::collections::BTreeMap;

use scribe_common::ids::WorkspaceId;

pub use scribe_common::protocol::{
    ArchiveReason, WorkspaceNoteEntry, WorkspaceNotesCollection, WorkspaceNotesMutation,
};

#[derive(Debug, Clone)]
pub struct WorkspaceNoteSummary {
    pub note_id: String,
    pub text: String,
}

/// Per-workspace transient UI state for the hover-preview inline editor.
///
/// Created when the user clicks the preview's "+" affordance; lives on `App`
/// in a `BTreeMap<WorkspaceId, AddingNoteState>` so multiple workspaces can
/// hold independent editor state (FR-021). Logically a second view on the
/// workspace's saved draft buffer (FR-020); typing here writes back through
/// the existing `SaveDraft` debounce, and commit (Enter) consumes it via
/// `CreateActiveNote`.
#[derive(Clone, Debug)]
pub struct AddingNoteState {
    pub draft_text: String,
    pub draft_dirty: bool,
    pub caret_byte: usize,
    pub scroll_offset_rows: usize,
    pub last_server_error: Option<String>,
    pub committed_pending: bool,
}

impl AddingNoteState {
    pub fn new_from_saved_draft(text: String) -> Self {
        let caret_byte = text.len();
        Self {
            draft_text: text,
            draft_dirty: false,
            caret_byte,
            scroll_offset_rows: 0,
            last_server_error: None,
            committed_pending: false,
        }
    }

    pub fn insert_char(&mut self, ch: char) {
        self.draft_text.insert(self.caret_byte, ch);
        self.caret_byte += ch.len_utf8();
        self.draft_dirty = true;
        self.last_server_error = None;
    }

    pub fn backspace(&mut self) -> bool {
        if self.caret_byte == 0 {
            return false;
        }
        let mut prev = self.caret_byte - 1;
        while prev > 0 && !self.draft_text.is_char_boundary(prev) {
            prev -= 1;
        }
        self.draft_text.replace_range(prev..self.caret_byte, "");
        self.caret_byte = prev;
        self.draft_dirty = true;
        self.last_server_error = None;
        true
    }

    pub fn move_caret_left(&mut self) {
        if self.caret_byte == 0 {
            return;
        }
        let mut prev = self.caret_byte - 1;
        while prev > 0 && !self.draft_text.is_char_boundary(prev) {
            prev -= 1;
        }
        self.caret_byte = prev;
    }

    pub fn move_caret_right(&mut self) {
        if self.caret_byte >= self.draft_text.len() {
            return;
        }
        let mut next = self.caret_byte + 1;
        while next < self.draft_text.len() && !self.draft_text.is_char_boundary(next) {
            next += 1;
        }
        self.caret_byte = next;
    }

    pub fn move_caret_line_start(&mut self) {
        self.caret_byte = self.draft_text[..self.caret_byte].rfind('\n').map_or(0, |pos| pos + 1);
    }

    pub fn move_caret_line_end(&mut self) {
        self.caret_byte = self.draft_text[self.caret_byte..]
            .find('\n')
            .map_or(self.draft_text.len(), |rel| self.caret_byte + rel);
    }

    pub fn move_caret_up(&mut self) {
        let line_start = self.draft_text[..self.caret_byte].rfind('\n').map_or(0, |pos| pos + 1);
        if line_start == 0 {
            // Already on the first line — jump to start of buffer.
            self.caret_byte = 0;
            return;
        }
        // Column expressed as a CHAR count, not a byte offset (multi-byte safe).
        let col_chars = self.draft_text[line_start..self.caret_byte].chars().count();
        let prev_line_end = line_start - 1; // position of the '\n'
        let prev_line_start = self.draft_text[..prev_line_end].rfind('\n').map_or(0, |pos| pos + 1);
        let prev_line = &self.draft_text[prev_line_start..prev_line_end];
        let target_offset = byte_offset_of_nth_char(prev_line, col_chars);
        self.caret_byte = prev_line_start + target_offset;
    }

    pub fn move_caret_down(&mut self) {
        let line_start = self.draft_text[..self.caret_byte].rfind('\n').map_or(0, |pos| pos + 1);
        // Column expressed as a CHAR count, not a byte offset (multi-byte safe).
        let col_chars = self.draft_text[line_start..self.caret_byte].chars().count();
        let Some(rel_next) = self.draft_text[self.caret_byte..].find('\n') else {
            // Already on the last line — jump to end of buffer.
            self.caret_byte = self.draft_text.len();
            return;
        };
        let next_line_start = self.caret_byte + rel_next + 1;
        let next_line_end = self.draft_text[next_line_start..]
            .find('\n')
            .map_or(self.draft_text.len(), |rel| next_line_start + rel);
        let next_line = &self.draft_text[next_line_start..next_line_end];
        let target_offset = byte_offset_of_nth_char(next_line, col_chars);
        self.caret_byte = next_line_start + target_offset;
    }

    pub fn is_blank_trimmed(&self) -> bool {
        self.draft_text.trim().is_empty()
    }

    /// Snap the internal scroll offset so the caret stays visible within
    /// `editor_rows` visible rows wrapping at `content_cols` columns. Caller
    /// passes the same column/row budget the renderer will use. (FR-022 first
    /// input: caret-tracking auto-scroll.)
    pub fn clamp_scroll_to_caret(&mut self, content_cols: usize, editor_rows: usize) {
        if editor_rows == 0 {
            return;
        }
        let caret_line = visual_line_of(&self.draft_text, self.caret_byte, content_cols);
        if caret_line < self.scroll_offset_rows {
            self.scroll_offset_rows = caret_line;
        } else if caret_line >= self.scroll_offset_rows + editor_rows {
            self.scroll_offset_rows = caret_line + 1 - editor_rows;
        }
    }
}

/// Byte offset (within `s`) of the start of the n-th character, or `s.len()`
/// if `s` has fewer than `n` characters. Used to convert a column expressed in
/// characters into a byte offset that's safe to index with (always on a char
/// boundary).
fn byte_offset_of_nth_char(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

/// Visual line index (0-based) for `caret_byte` when `text` is wrapped at
/// `cols` columns. Mirrors `caret_line_index` in `workspace_notes_preview` so
/// both renderer and state can compute identical positions.
fn visual_line_of(text: &str, caret_byte: usize, cols: usize) -> usize {
    let cols = cols.max(1);
    let mut lines = 0usize;
    let mut col = 0usize;
    let mut bytes = 0usize;
    for ch in text.chars() {
        if bytes >= caret_byte {
            return lines;
        }
        if ch == '\n' {
            lines += 1;
            col = 0;
        } else {
            col += 1;
            if col >= cols {
                lines += 1;
                col = 0;
            }
        }
        bytes += ch.len_utf8();
    }
    lines
}

pub struct WorkspaceNotesStore {
    collections: BTreeMap<String, WorkspaceNotesCollection>,
    last_error: Option<String>,
}

impl WorkspaceNotesStore {
    pub fn load() -> Self {
        Self::new()
    }

    pub fn new() -> Self {
        Self { collections: BTreeMap::new(), last_error: None }
    }

    pub fn apply_collections(&mut self, collections: Vec<WorkspaceNotesCollection>) {
        for collection in collections {
            self.apply_collection(collection);
        }
    }

    pub fn apply_collection(&mut self, collection: WorkspaceNotesCollection) {
        self.collections.insert(workspace_key(collection.workspace_id), collection);
        self.last_error = None;
    }

    pub fn set_error(&mut self, message: String) {
        self.last_error = Some(message);
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn active_notes(&self, workspace_id: WorkspaceId) -> Vec<WorkspaceNoteEntry> {
        self.collection(workspace_id)
            .map_or_else(Vec::new, |collection| collection.active_notes.clone())
    }

    pub fn archived_notes(&self, workspace_id: WorkspaceId) -> Vec<WorkspaceNoteEntry> {
        self.collection(workspace_id)
            .map_or_else(Vec::new, |collection| collection.archived_notes.clone())
    }

    pub fn draft_text(&self, workspace_id: WorkspaceId) -> String {
        self.collection(workspace_id)
            .and_then(|collection| collection.draft.as_ref())
            .map_or_else(String::new, |draft| draft.text.clone())
    }

    pub fn hover_summaries(
        &self,
        workspace_id: WorkspaceId,
        max_entries: usize,
        max_chars: usize,
    ) -> (Vec<WorkspaceNoteSummary>, usize) {
        let active = self.active_notes(workspace_id);
        let total = active.len();
        let summaries = active
            .into_iter()
            .take(max_entries)
            .map(|entry| WorkspaceNoteSummary {
                note_id: entry.note_id,
                text: compact_summary(&entry.text, max_chars),
            })
            .collect();
        (summaries, total)
    }

    fn collection(&self, workspace_id: WorkspaceId) -> Option<&WorkspaceNotesCollection> {
        self.collections.get(&workspace_key(workspace_id))
    }
}

fn workspace_key(workspace_id: WorkspaceId) -> String {
    workspace_id.to_full_string()
}

fn compact_summary(text: &str, max_chars: usize) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    let mut truncated = false;
    for ch in flattened.chars() {
        if out.chars().count() >= max_chars {
            truncated = true;
            break;
        }
        out.push(ch);
    }
    if truncated {
        out.push_str("...");
    }
    out
}
