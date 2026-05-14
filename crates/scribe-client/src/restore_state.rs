//! Persisted startup restore state and runtime launch bindings.

use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use scribe_common::ai_state::AiProvider;
use scribe_common::app::current_state_dir;
use scribe_common::ids::{WindowId, WorkspaceId};
use scribe_common::protocol::LayoutDirection;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Persisted list of windows that should be reopened on the next cold start.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreIndex {
    pub version: u32,
    pub updated_at_ms: u64,
    pub windows: Vec<WindowId>,
}

/// Persisted logical state for one client window.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowRestoreState {
    pub version: u32,
    pub window_id: WindowId,
    pub focused_workspace_id: WorkspaceId,
    pub root: WorkspaceLayoutSnapshot,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub launches: Vec<LaunchRecord>,
}

impl WindowRestoreState {
    pub fn is_replayable(&self) -> bool {
        !self.launches.is_empty()
            && self.workspaces.iter().any(|workspace| !workspace.tabs.is_empty())
    }
}

/// Snapshot of the workspace split tree.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkspaceLayoutSnapshot {
    Leaf {
        workspace_id: WorkspaceId,
    },
    Split {
        direction: LayoutDirection,
        ratio: f32,
        first: Box<WorkspaceLayoutSnapshot>,
        second: Box<WorkspaceLayoutSnapshot>,
    },
}

/// Snapshot of one workspace and its tab stack.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub workspace_id: WorkspaceId,
    pub name: Option<String>,
    pub accent_color: [f32; 4],
    pub active_tab_index: usize,
    pub tabs: Vec<TabSnapshot>,
}

/// Snapshot of one tab and its pane tree.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub focused_launch_id: String,
    pub pane_tree: PaneSnapshot,
}

/// Snapshot of the pane split tree within a tab.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaneSnapshot {
    Leaf {
        launch_id: String,
    },
    Split {
        direction: LayoutDirection,
        ratio: f32,
        first: Box<PaneSnapshot>,
        second: Box<PaneSnapshot>,
    },
}

/// Persisted record for one launchable session.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub launch_id: String,
    pub cwd: Option<PathBuf>,
    pub kind: LaunchKind,
    #[serde(default)]
    pub first_prompt: Option<String>,
    #[serde(default)]
    pub latest_prompt: Option<String>,
    /// Wall-clock time the most recent prompt was received, encoded as
    /// Unix-epoch seconds. Used by the prompt bar's elapsed-time counter
    /// to keep counting up across cold restarts. `None` for snapshots
    /// written by older clients that predate this field.
    #[serde(default)]
    pub latest_prompt_at: Option<u64>,
    /// Wall-clock time the LLM finished responding to the most recent
    /// prompt, encoded as Unix-epoch seconds. When `Some`, the timer
    /// stays frozen at this instant across cold restarts so the displayed
    /// elapsed value continues to reflect response duration rather than
    /// being recomputed from `latest_prompt_at` plus downtime. `None`
    /// when the LLM was still processing (or in older snapshots).
    #[serde(default)]
    pub latest_prompt_finished_at: Option<u64>,
    #[serde(default)]
    pub prompt_count: u32,
}

/// Launch type recorded for restore replay.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchKind {
    Shell,
    CustomCommand { argv: Vec<String> },
    Ai { provider: AiProvider, resume_mode: AiResumeMode, conversation_id: Option<String> },
}

/// Whether an AI launch was newly created or resumed.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AiResumeMode {
    New,
    Resume,
}

/// Runtime binding kept on each pane so restore snapshots can refer to a
/// stable launch ID even before replay logic exists.

#[derive(Debug, Clone)]
pub struct LaunchBinding {
    pub launch_id: String,
    pub kind: LaunchKind,
    pub fallback_cwd: Option<PathBuf>,
}

/// Client-side restore store rooted under the current state directory.
pub struct RestoreStore {
    root: Option<PathBuf>,
}

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

struct RestoreIndexLock {
    path: PathBuf,
}

impl Drop for RestoreIndexLock {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.path));
    }
}

impl RestoreStore {
    /// Create a new store rooted at `$XDG_STATE_HOME/scribe/restore`.
    pub fn new() -> Self {
        Self { root: current_state_dir().map(|dir| dir.join("restore")) }
    }

    fn index_path(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|root| root.join("index.toml"))
    }

    fn window_path(&self, window_id: WindowId) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|root| root.join("windows").join(format!("{}.toml", window_id.to_full_string())))
    }

    fn lock_path(&self) -> std::io::Result<PathBuf> {
        self.root
            .as_ref()
            .map(|root| root.join("bootstrap.lock"))
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "missing state dir"))
    }

    fn acquire_index_lock(&self) -> Result<RestoreIndexLock, crate::window_state::StateError> {
        let path = self.lock_path()?;
        self.ensure_restore_parent(&path)?;
        loop {
            if let Some(lock) = Self::try_create_index_lock(&path)? {
                return Ok(lock);
            }

            if Self::remove_stale_lock_if_needed(&path, unix_time_ms())? {
                continue;
            }

            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn read_toml<T: DeserializeOwned>(path: Option<PathBuf>) -> std::io::Result<T> {
        let path = path.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing state dir")
        })?;
        let content = std::fs::read_to_string(&path)?;
        toml::from_str(&content)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    fn write_toml_atomic<T: Serialize>(
        &self,
        path: Option<PathBuf>,
        value: &T,
    ) -> Result<(), crate::window_state::StateError> {
        let path = path.ok_or(crate::window_state::StateError::NoStateDir)?;
        self.ensure_restore_parent(&path)?;
        let content = toml::to_string_pretty(value)?;
        let tmp_path = Self::write_private_temp_file(&path, content.as_bytes())?;
        if let Err(error) = std::fs::rename(&tmp_path, &path) {
            drop(std::fs::remove_file(&tmp_path));
            return Err(error.into());
        }
        set_private_file_permissions(&path)?;
        Ok(())
    }

    fn ensure_restore_parent(&self, path: &Path) -> Result<(), crate::window_state::StateError> {
        let root = self.root.as_ref().ok_or(crate::window_state::StateError::NoStateDir)?;
        ensure_private_dir(root)?;
        if let Some(parent) = path.parent() {
            if parent != root {
                ensure_private_dir(parent)?;
            }
        }
        Ok(())
    }

    fn write_private_temp_file(
        path: &Path,
        content: &[u8],
    ) -> Result<PathBuf, crate::window_state::StateError> {
        let mut last_exists = None;
        for attempt in 0..16 {
            let tmp_path = private_temp_path(path, attempt);
            match create_private_file(&tmp_path) {
                Ok(mut file) => {
                    file.write_all(content)?;
                    return Ok(tmp_path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_exists = Some(error);
                }
                Err(error) => return Err(error.into()),
            }
        }

        Err(last_exists
            .unwrap_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "could not allocate restore temp file",
                )
            })
            .into())
    }

    /// Save the restore index to disk.
    pub fn save_index(&self, index: &RestoreIndex) -> Result<(), crate::window_state::StateError> {
        self.write_toml_atomic(self.index_path(), index)
    }

    /// Insert or refresh a window entry in the restore index.
    pub fn upsert_index(&self, window_id: WindowId) -> Result<(), crate::window_state::StateError> {
        let _lock = self.acquire_index_lock()?;
        let mut index = self.read_index_for_update()?;
        if !index.windows.contains(&window_id) {
            index.windows.push(window_id);
        }
        index.updated_at_ms = unix_time_ms();
        self.save_index(&index)
    }

    /// Remove a window entry from the restore index.
    pub fn remove_from_index(
        &self,
        window_id: WindowId,
    ) -> Result<(), crate::window_state::StateError> {
        let _lock = self.acquire_index_lock()?;
        let mut index = self.read_index_for_update()?;
        index.windows.retain(|id| *id != window_id);
        index.updated_at_ms = unix_time_ms();
        self.save_index(&index)
    }

    /// Load the persisted logical state for a single window.
    pub fn load_window(&self, window_id: WindowId) -> Option<WindowRestoreState> {
        Self::read_toml(self.window_path(window_id)).ok()
    }

    /// Save one window's logical state to disk.
    pub fn save_window(
        &self,
        state: &WindowRestoreState,
    ) -> Result<(), crate::window_state::StateError> {
        self.write_toml_atomic(self.window_path(state.window_id), state)
    }

    /// Remove a window's persisted logical state.
    pub fn remove_window(&self, window_id: WindowId) {
        let Some(path) = self.window_path(window_id) else { return };
        let result = std::fs::remove_file(path);
        if let Err(error) = result {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%window_id, error = %error, "failed to remove restore window state");
            }
        }
    }

    /// Atomically claim the first valid window from the restore index for cold
    /// restart replay.  Returns the claimed window's state and the number of
    /// remaining unclaimed windows (so the caller can spawn additional client
    /// processes).  Corrupted entries are skipped and removed.  The claimed
    /// entry and its on-disk file are cleaned up.
    pub fn claim_first_window(&self) -> Option<(WindowRestoreState, usize)> {
        let _lock = self.acquire_index_lock().ok()?;
        let mut index = self.read_index_for_update().ok()?;
        let mut claimed: Option<WindowRestoreState> = None;
        let mut remaining_valid = Vec::with_capacity(index.windows.len());

        for window_id in index.windows.drain(..) {
            match self.load_window(window_id) {
                Some(state) if !state.is_replayable() => {
                    self.remove_window(window_id);
                    tracing::warn!(
                        %window_id,
                        launches = state.launches.len(),
                        "skipping non-replayable restore entry"
                    );
                }
                Some(state) if claimed.is_none() => {
                    self.remove_window(window_id);
                    claimed = Some(state);
                }
                Some(_) => {
                    remaining_valid.push(window_id);
                }
                None => {
                    // File missing or corrupted — clean up and drop the stale index entry.
                    self.remove_window(window_id);
                    tracing::warn!(%window_id, "skipping unreadable restore entry");
                }
            }
        }

        index.windows = remaining_valid;
        index.updated_at_ms = unix_time_ms();
        drop(self.save_index(&index));

        claimed.map(|state| (state, index.windows.len()))
    }

    /// Check whether a bootstrap lock file is old enough to be considered stale.
    pub fn lock_is_stale(path: &PathBuf, now_ms: u64) -> std::io::Result<bool> {
        let created_ms = match std::fs::read_to_string(path) {
            Ok(raw) => raw.trim().parse::<u64>().ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        }
        .or_else(|| {
            std::fs::metadata(path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        })
        .unwrap_or(now_ms);
        Ok(now_ms.saturating_sub(created_ms) > 30_000)
    }

    fn try_create_index_lock(
        path: &Path,
    ) -> Result<Option<RestoreIndexLock>, crate::window_state::StateError> {
        match create_private_file(path) {
            Ok(mut file) => {
                writeln!(file, "{}", unix_time_ms())?;
                Ok(Some(RestoreIndexLock { path: path.to_path_buf() }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn remove_stale_lock_if_needed(
        path: &PathBuf,
        now_ms: u64,
    ) -> Result<bool, crate::window_state::StateError> {
        if !Self::lock_is_stale(path, now_ms)? {
            return Ok(false);
        }

        drop(std::fs::remove_file(path));
        Ok(true)
    }

    fn read_index_for_update(&self) -> Result<RestoreIndex, crate::window_state::StateError> {
        let Some(path) = self.index_path() else {
            return Err(crate::window_state::StateError::NoStateDir);
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let index = toml::from_str(&content)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                Ok(index)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(RestoreIndex { version: 1, updated_at_ms: 0, windows: Vec::new() })
            }
            Err(error) => Err(error.into()),
        }
    }
}

fn ensure_private_dir(path: &Path) -> Result<(), crate::window_state::StateError> {
    std::fs::create_dir_all(path)?;
    set_private_dir_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(PRIVATE_FILE_MODE);
    }
    let file = options.open(path)?;
    set_private_file_permissions(path)?;
    Ok(file)
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn private_temp_path(path: &Path, attempt: u32) -> PathBuf {
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("restore");
    let tmp_name =
        format!(".{file_name}.{}.{}.{}.tmp", std::process::id(), unix_time_ms(), attempt);
    path.with_file_name(tmp_name)
}

/// Current UNIX time in milliseconds.
pub fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
