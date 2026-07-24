//! Non-durable client cache and inline-editor state for server-owned workspace
//! notes, ported from the winit client's `workspace_notes.rs`.
//!
//! The server owns the authoritative notes; this module holds the transient UI
//! projection the GPUI modal and hover preview render. [`WorkspaceNotesStore`]
//! caches the per-workspace [`WorkspaceNotesCollection`] pushed by the server,
//! and [`AddingNoteState`] is the per-workspace inline-editor buffer that shares
//! the workspace's saved draft (FR-020) and drives the caret geometry the
//! preview paints. The caret-motion, wrap, and scroll helpers are pure and byte
//! -for-byte ported so the GPUI preview positions the caret identically to the
//! winit client.

use std::collections::BTreeMap;

use scribe_common::ids::WorkspaceId;

pub use scribe_common::protocol::{
    ArchiveReason, WorkspaceNoteEntry, WorkspaceNotesCollection, WorkspaceNotesMutation,
};

/// A compacted single-line projection of one active note, used to fill the
/// hover-preview rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceNoteSummary {
    /// The server note id this summary projects.
    pub note_id: String,
    /// The whitespace-flattened, length-capped summary text.
    pub text: String,
}

/// Per-workspace transient UI state for the hover-preview inline editor.
///
/// Created when the user clicks the preview's "+" affordance; the shell keeps a
/// `BTreeMap<WorkspaceId, AddingNoteState>` so multiple workspaces can hold
/// independent editor state (FR-021). Logically a second view on the workspace's
/// saved draft buffer (FR-020): typing here writes back through the existing
/// `SaveDraft` debounce, and commit (Enter) consumes it via `CreateActiveNote`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddingNoteState {
    /// The editor's text buffer (shares the workspace's saved draft).
    pub draft_text: String,
    /// Whether the buffer has unwritten edits pending a `SaveDraft` flush.
    pub draft_dirty: bool,
    /// Caret position as a byte offset into `draft_text` (always a char
    /// boundary).
    pub caret_byte: usize,
    /// First visible wrapped row inside the editor viewport (FR-022).
    pub scroll_offset_rows: usize,
    /// The most recent server error surfaced under the editor row, if any.
    pub last_server_error: Option<String>,
    /// Whether an Enter-commit is awaiting the server's create acknowledgement.
    pub committed_pending: bool,
}

impl AddingNoteState {
    /// Seed a fresh editor with the workspace's saved draft, caret at the end.
    #[must_use]
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

    /// Insert `ch` at the caret, advancing it and marking the buffer dirty.
    pub fn insert_char(&mut self, ch: char) {
        self.draft_text.insert(self.caret_byte, ch);
        self.caret_byte += ch.len_utf8();
        self.draft_dirty = true;
        self.last_server_error = None;
    }

    /// Delete the char before the caret. Returns `false` at the buffer start.
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

    /// Move the caret one char left.
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

    /// Move the caret one char right.
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

    /// Move the caret to the start of its logical line.
    pub fn move_caret_line_start(&mut self) {
        self.caret_byte = self.draft_text[..self.caret_byte].rfind('\n').map_or(0, |pos| pos + 1);
    }

    /// Move the caret to the end of its logical line.
    pub fn move_caret_line_end(&mut self) {
        self.caret_byte = self.draft_text[self.caret_byte..]
            .find('\n')
            .map_or(self.draft_text.len(), |rel| self.caret_byte + rel);
    }

    /// Move the caret up one logical line, preserving the character column.
    pub fn move_caret_up(&mut self) {
        let line_start = self.draft_text[..self.caret_byte].rfind('\n').map_or(0, |pos| pos + 1);
        if line_start == 0 {
            self.caret_byte = 0;
            return;
        }
        let col_chars = self.draft_text[line_start..self.caret_byte].chars().count();
        let prev_line_end = line_start - 1;
        let prev_line_start = self.draft_text[..prev_line_end].rfind('\n').map_or(0, |pos| pos + 1);
        let prev_line = &self.draft_text[prev_line_start..prev_line_end];
        let target_offset = byte_offset_of_nth_char(prev_line, col_chars);
        self.caret_byte = prev_line_start + target_offset;
    }

    /// Move the caret down one logical line, preserving the character column.
    pub fn move_caret_down(&mut self) {
        let line_start = self.draft_text[..self.caret_byte].rfind('\n').map_or(0, |pos| pos + 1);
        let col_chars = self.draft_text[line_start..self.caret_byte].chars().count();
        let Some(rel_next) = self.draft_text[self.caret_byte..].find('\n') else {
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

    /// Whether the buffer is empty once trimmed (blocks a blank commit).
    #[must_use]
    pub fn is_blank_trimmed(&self) -> bool {
        self.draft_text.trim().is_empty()
    }

    /// Snap the scroll offset so the caret stays visible within `editor_rows`
    /// rows wrapping at `content_cols` columns (FR-022 caret-tracking scroll).
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

/// Byte offset of the start of the `n`-th character, or `s.len()` when `s` has
/// fewer than `n` characters.
fn byte_offset_of_nth_char(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

/// Visual line index (0-based) for `caret_byte` when `text` wraps at `cols`.
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

/// Client-side cache of server-owned workspace notes keyed by workspace id.
#[derive(Debug, Default)]
pub struct WorkspaceNotesStore {
    collections: BTreeMap<String, WorkspaceNotesCollection>,
    last_error: Option<String>,
}

impl WorkspaceNotesStore {
    /// Build an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self { collections: BTreeMap::new(), last_error: None }
    }

    /// Apply a batch of server collections, replacing any cached copies.
    pub fn apply_collections(&mut self, collections: Vec<WorkspaceNotesCollection>) {
        for collection in collections {
            self.apply_collection(collection);
        }
    }

    /// Apply one server collection, clearing any surfaced error.
    pub fn apply_collection(&mut self, collection: WorkspaceNotesCollection) {
        self.collections.insert(workspace_key(collection.workspace_id), collection);
        self.last_error = None;
    }

    /// Record a server error message for the modal footer.
    pub fn set_error(&mut self, message: String) {
        self.last_error = Some(message);
    }

    /// The most recent server error, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// The active notes cached for `workspace_id`.
    #[must_use]
    pub fn active_notes(&self, workspace_id: WorkspaceId) -> Vec<WorkspaceNoteEntry> {
        self.collection(workspace_id)
            .map_or_else(Vec::new, |collection| collection.active_notes.clone())
    }

    /// The archived notes cached for `workspace_id`.
    #[must_use]
    pub fn archived_notes(&self, workspace_id: WorkspaceId) -> Vec<WorkspaceNoteEntry> {
        self.collection(workspace_id)
            .map_or_else(Vec::new, |collection| collection.archived_notes.clone())
    }

    /// The saved draft text cached for `workspace_id`.
    #[must_use]
    pub fn draft_text(&self, workspace_id: WorkspaceId) -> String {
        self.collection(workspace_id)
            .and_then(|collection| collection.draft.as_ref())
            .map_or_else(String::new, |draft| draft.text.clone())
    }

    /// Build up to `max_entries` compacted active-note summaries for the hover
    /// preview, plus the total active count so overflow can be shown.
    #[must_use]
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

/// Flatten whitespace runs to single spaces and cap at `max_chars`, appending an
/// ellipsis when truncated.
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

#[cfg(test)]
mod tests {
    use scribe_common::protocol::WorkspaceNoteStatus;

    use super::*;

    fn entry(workspace_id: WorkspaceId, note_id: &str, text: &str) -> WorkspaceNoteEntry {
        WorkspaceNoteEntry {
            note_id: note_id.to_owned(),
            workspace_id,
            text: text.to_owned(),
            status: WorkspaceNoteStatus::Active,
            created_at_ms: 0,
            updated_at_ms: 0,
            archived_at_ms: None,
            archive_reason: None,
        }
    }

    // @lat: [[client#GPUI Workspace Notes#Inline editor caret motion]]
    #[test]
    fn insert_backspace_and_caret_motion_track_char_boundaries() {
        let mut state = AddingNoteState::new_from_saved_draft(String::new());
        for ch in "héllo".chars() {
            state.insert_char(ch);
        }
        assert!(state.draft_dirty);
        assert_eq!(state.caret_byte, "héllo".len());

        state.move_caret_left();
        state.move_caret_left();
        // Caret now sits before "lo"; the 'é' is multi-byte, so left/right must
        // land on char boundaries.
        assert!(state.draft_text.is_char_boundary(state.caret_byte));
        state.move_caret_line_start();
        assert_eq!(state.caret_byte, 0);
        state.move_caret_line_end();
        assert_eq!(state.caret_byte, "héllo".len());

        assert!(state.backspace());
        assert_eq!(state.draft_text, "héll");
    }

    // @lat: [[client#GPUI Workspace Notes#Inline editor caret motion]]
    #[test]
    fn vertical_caret_motion_preserves_character_column() {
        let mut state = AddingNoteState::new_from_saved_draft("abcd\nxy\nlongline".to_owned());
        // Caret at end (col 8 of "longline"). Up clamps to the shorter middle
        // line "xy" — landing at its end (col 2), byte offset 7.
        state.move_caret_up();
        assert_eq!(state.caret_byte, 7);
        let line_start = state.draft_text[..state.caret_byte].rfind('\n').map_or(0, |p| p + 1);
        assert_eq!(&state.draft_text[line_start..state.caret_byte], "xy");
        // Down preserves the character column (2), landing at col 2 of
        // "longline" — byte offset 10, not the end of the buffer.
        state.move_caret_down();
        assert_eq!(state.caret_byte, 10);
    }

    // @lat: [[client#GPUI Workspace Notes#Store projects summaries]]
    #[test]
    fn hover_summaries_flatten_and_cap_with_overflow_total() {
        let workspace_id = WorkspaceId::new();
        let mut store = WorkspaceNotesStore::new();
        store.apply_collection(WorkspaceNotesCollection {
            workspace_id,
            active_notes: vec![
                entry(workspace_id, "a", "first   note   body"),
                entry(workspace_id, "b", "second"),
                entry(workspace_id, "c", "third"),
            ],
            archived_notes: Vec::new(),
            draft: None,
            updated_at_ms: 0,
        });

        let (summaries, total) = store.hover_summaries(workspace_id, 2, 8);
        assert_eq!(total, 3);
        assert_eq!(summaries.len(), 2);
        assert_eq!(
            summaries[0],
            WorkspaceNoteSummary { note_id: "a".to_owned(), text: "first no...".to_owned() }
        );
    }
}
