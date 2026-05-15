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
