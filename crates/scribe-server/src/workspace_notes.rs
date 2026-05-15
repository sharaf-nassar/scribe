//! Server-owned per-workspace notes state and write-through persistence.

use std::collections::BTreeMap;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use scribe_common::app::current_state_dir;
use scribe_common::ids::WorkspaceId;
use scribe_common::protocol::{
    ArchiveReason, WorkspaceNoteDraft, WorkspaceNoteEntry, WorkspaceNoteStatus,
    WorkspaceNotesCollection, WorkspaceNotesMutation,
};
use serde::{Deserialize, Serialize};

const STORE_VERSION: u32 = 1;
const STORE_OWNER: &str = "server";
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceNotesError {
    #[error("could not determine XDG state directory")]
    NoStateDir,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TOML serialize error: {0}")]
    Serialize(String),
    #[error("TOML parse error: {0}")]
    Parse(String),
    #[error("note text cannot be empty")]
    EmptyNote,
    #[error("note not found")]
    NoteNotFound,
    #[error("note is already archived")]
    AlreadyArchived,
    #[error("unsupported workspace notes version {0}")]
    UnsupportedVersion(u32),
    #[error("workspace notes store owner is not server-owned")]
    NonServerOwnedStore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedWorkspaceNotes {
    pub version: u32,
    #[serde(default)]
    pub owner: String,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceNotesCollection>,
}

impl Default for PersistedWorkspaceNotes {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            owner: STORE_OWNER.to_owned(),
            updated_at_ms: 0,
            workspaces: BTreeMap::new(),
        }
    }
}

pub struct WorkspaceNotesStore {
    path: Option<PathBuf>,
    data: PersistedWorkspaceNotes,
}

impl WorkspaceNotesStore {
    pub fn load() -> Self {
        let path = current_state_dir().map(|dir| dir.join("workspace_notes.toml"));
        let data = match Self::read(path.as_deref()) {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(%error, "failed to load workspace notes; using empty store");
                PersistedWorkspaceNotes::default()
            }
        };
        Self { path, data }
    }

    fn read(path: Option<&Path>) -> Result<PersistedWorkspaceNotes, WorkspaceNotesError> {
        let Some(path) = path else {
            return Ok(PersistedWorkspaceNotes::default());
        };
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedWorkspaceNotes::default());
            }
            Err(error) => return Err(error.into()),
        };
        let data: PersistedWorkspaceNotes = toml::from_str(&content)
            .map_err(|error| WorkspaceNotesError::Parse(error.to_string()))?;
        if data.version != STORE_VERSION {
            return Err(WorkspaceNotesError::UnsupportedVersion(data.version));
        }
        if data.owner != STORE_OWNER {
            return Err(WorkspaceNotesError::NonServerOwnedStore);
        }
        Ok(data)
    }

    pub fn collections_for(&self, workspace_ids: &[WorkspaceId]) -> Vec<WorkspaceNotesCollection> {
        if workspace_ids.is_empty() {
            return self.data.workspaces.values().cloned().collect();
        }
        workspace_ids
            .iter()
            .copied()
            .map(|workspace_id| self.collection_or_empty(workspace_id))
            .collect()
    }

    pub fn apply_mutation(
        &mut self,
        mutation: WorkspaceNotesMutation,
    ) -> Result<WorkspaceNotesCollection, WorkspaceNotesError> {
        let workspace_id = mutation.workspace_id();
        let mut next = self.data.clone();
        apply_mutation_to_data(&mut next, mutation)?;
        self.persist_next(next, workspace_id)
    }

    fn persist_next(
        &mut self,
        next: PersistedWorkspaceNotes,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceNotesCollection, WorkspaceNotesError> {
        let path = self.path.as_deref().ok_or(WorkspaceNotesError::NoStateDir)?;
        write_toml_atomic(path, &next)?;
        let collection = next
            .workspaces
            .get(&workspace_key(workspace_id))
            .cloned()
            .unwrap_or_else(|| empty_collection(workspace_id));
        self.data = next;
        Ok(collection)
    }

    fn collection_or_empty(&self, workspace_id: WorkspaceId) -> WorkspaceNotesCollection {
        self.data
            .workspaces
            .get(&workspace_key(workspace_id))
            .cloned()
            .unwrap_or_else(|| empty_collection(workspace_id))
    }
}

trait WorkspaceNotesMutationExt {
    fn workspace_id(&self) -> WorkspaceId;
}

impl WorkspaceNotesMutationExt for WorkspaceNotesMutation {
    fn workspace_id(&self) -> WorkspaceId {
        match self {
            Self::SaveDraft { workspace_id, .. }
            | Self::CreateActiveNote { workspace_id, .. }
            | Self::EditNote { workspace_id, .. }
            | Self::ArchiveNote { workspace_id, .. }
            | Self::BulkEditArchived { workspace_id, .. } => *workspace_id,
        }
    }
}

fn apply_mutation_to_data(
    data: &mut PersistedWorkspaceNotes,
    mutation: WorkspaceNotesMutation,
) -> Result<(), WorkspaceNotesError> {
    match mutation {
        WorkspaceNotesMutation::SaveDraft { workspace_id, text } => {
            save_draft(data, workspace_id, text);
            Ok(())
        }
        WorkspaceNotesMutation::CreateActiveNote { workspace_id, text } => {
            create_active_note(data, workspace_id, text).map(drop)
        }
        WorkspaceNotesMutation::EditNote { workspace_id, note_id, text } => {
            edit_note(data, workspace_id, &note_id, text)
        }
        WorkspaceNotesMutation::ArchiveNote { workspace_id, note_id, reason } => {
            archive_note(data, workspace_id, &note_id, reason)
        }
        WorkspaceNotesMutation::BulkEditArchived { workspace_id, updates } => {
            bulk_edit_archived(data, workspace_id, &updates)
        }
    }
}

fn save_draft(data: &mut PersistedWorkspaceNotes, workspace_id: WorkspaceId, text: String) {
    let now = unix_time_ms();
    if text.is_empty() {
        let Some(collection) = data.workspaces.get_mut(&workspace_key(workspace_id)) else {
            return;
        };
        if collection.draft.take().is_none() {
            return;
        }
        collection.updated_at_ms = now;
        data.updated_at_ms = now;
        return;
    }
    let collection = collection_mut(data, workspace_id, now);
    collection.draft =
        Some(WorkspaceNoteDraft { workspace_id, text, updated_at_ms: now, dirty: true });
    collection.updated_at_ms = now;
    data.updated_at_ms = now;
}

fn create_active_note(
    data: &mut PersistedWorkspaceNotes,
    workspace_id: WorkspaceId,
    text: String,
) -> Result<String, WorkspaceNotesError> {
    if text.trim().is_empty() {
        return Err(WorkspaceNotesError::EmptyNote);
    }
    let now = unix_time_ms();
    let next_seq = data
        .workspaces
        .get(&workspace_key(workspace_id))
        .map_or(0, |collection| collection.active_notes.len() + collection.archived_notes.len())
        .saturating_add(1);
    let note_id = format!("note-{}-{now}-{next_seq}", workspace_id.to_full_string());
    let note = WorkspaceNoteEntry {
        note_id: note_id.clone(),
        workspace_id,
        text,
        status: WorkspaceNoteStatus::Active,
        created_at_ms: now,
        updated_at_ms: now,
        archived_at_ms: None,
        archive_reason: None,
    };
    let collection = collection_mut(data, workspace_id, now);
    collection.active_notes.push(note);
    collection.draft = None;
    collection.updated_at_ms = now;
    data.updated_at_ms = now;
    Ok(note_id)
}

fn edit_note(
    data: &mut PersistedWorkspaceNotes,
    workspace_id: WorkspaceId,
    note_id: &str,
    text: String,
) -> Result<(), WorkspaceNotesError> {
    if text.trim().is_empty() {
        return Err(WorkspaceNotesError::EmptyNote);
    }
    let now = unix_time_ms();
    let Some(collection) = data.workspaces.get_mut(&workspace_key(workspace_id)) else {
        return Err(WorkspaceNotesError::NoteNotFound);
    };
    if let Some(note) = collection.active_notes.iter_mut().find(|note| note.note_id == note_id) {
        note.text = text;
        note.updated_at_ms = now;
        collection.updated_at_ms = now;
        data.updated_at_ms = now;
        return Ok(());
    }
    if let Some(note) = collection.archived_notes.iter_mut().find(|note| note.note_id == note_id) {
        note.text = text;
        note.updated_at_ms = now;
        collection.updated_at_ms = now;
        data.updated_at_ms = now;
        return Ok(());
    }
    Err(WorkspaceNotesError::NoteNotFound)
}

fn archive_note(
    data: &mut PersistedWorkspaceNotes,
    workspace_id: WorkspaceId,
    note_id: &str,
    reason: ArchiveReason,
) -> Result<(), WorkspaceNotesError> {
    let now = unix_time_ms();
    let Some(collection) = data.workspaces.get_mut(&workspace_key(workspace_id)) else {
        return Err(WorkspaceNotesError::NoteNotFound);
    };
    if collection.archived_notes.iter().any(|note| note.note_id == note_id) {
        return Err(WorkspaceNotesError::AlreadyArchived);
    }
    let Some(index) = collection.active_notes.iter().position(|note| note.note_id == note_id)
    else {
        return Err(WorkspaceNotesError::NoteNotFound);
    };
    let mut note = collection.active_notes.remove(index);
    note.status = WorkspaceNoteStatus::Archived;
    note.updated_at_ms = now;
    note.archived_at_ms = Some(now);
    note.archive_reason = Some(reason);
    collection.archived_notes.push(note);
    collection.updated_at_ms = now;
    data.updated_at_ms = now;
    Ok(())
}

fn bulk_edit_archived(
    data: &mut PersistedWorkspaceNotes,
    workspace_id: WorkspaceId,
    updates: &[(String, String)],
) -> Result<(), WorkspaceNotesError> {
    if updates.iter().any(|(_, text)| text.trim().is_empty()) {
        return Err(WorkspaceNotesError::EmptyNote);
    }
    let Some(existing_collection) = data.workspaces.get(&workspace_key(workspace_id)) else {
        return Err(WorkspaceNotesError::NoteNotFound);
    };
    if updates.iter().any(|(note_id, _)| {
        !existing_collection.archived_notes.iter().any(|note| note.note_id == *note_id)
    }) {
        return Err(WorkspaceNotesError::NoteNotFound);
    }

    let now = unix_time_ms();
    let Some(mut_collection) = data.workspaces.get_mut(&workspace_key(workspace_id)) else {
        return Err(WorkspaceNotesError::NoteNotFound);
    };
    for (note_id, text) in updates {
        if let Some(note) =
            mut_collection.archived_notes.iter_mut().find(|note| note.note_id == *note_id)
        {
            note.text.clone_from(text);
            note.updated_at_ms = now;
        }
    }
    mut_collection.updated_at_ms = now;
    data.updated_at_ms = now;
    Ok(())
}

fn collection_mut(
    data: &mut PersistedWorkspaceNotes,
    workspace_id: WorkspaceId,
    now: u64,
) -> &mut WorkspaceNotesCollection {
    data.workspaces.entry(workspace_key(workspace_id)).or_insert_with(|| WorkspaceNotesCollection {
        workspace_id,
        active_notes: Vec::new(),
        archived_notes: Vec::new(),
        draft: None,
        updated_at_ms: now,
    })
}

fn empty_collection(workspace_id: WorkspaceId) -> WorkspaceNotesCollection {
    WorkspaceNotesCollection {
        workspace_id,
        active_notes: Vec::new(),
        archived_notes: Vec::new(),
        draft: None,
        updated_at_ms: 0,
    }
}

fn workspace_key(workspace_id: WorkspaceId) -> String {
    workspace_id.to_full_string()
}

fn write_toml_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), WorkspaceNotesError> {
    ensure_private_parent(path)?;
    let content = toml::to_string_pretty(value)
        .map_err(|error| WorkspaceNotesError::Serialize(error.to_string()))?;
    let tmp_path = private_temp_path(path);
    {
        let mut file = create_private_file(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    if let Err(error) = std::fs::rename(&tmp_path, path) {
        drop(std::fs::remove_file(&tmp_path));
        return Err(error.into());
    }
    set_private_file_permissions(path)?;
    Ok(())
}

fn ensure_private_parent(path: &Path) -> Result<(), WorkspaceNotesError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_private_dir_permissions(parent)?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        options.mode(PRIVATE_FILE_MODE);
    }
    let file = options.open(path)?;
    set_private_file_permissions(path)?;
    Ok(file)
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn private_temp_path(path: &Path) -> PathBuf {
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("workspace_notes");
    path.with_file_name(format!(".{file_name}.{}.{}.tmp", std::process::id(), unix_time_ms()))
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
