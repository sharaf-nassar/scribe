//! Persisted startup restore state and runtime launch bindings.

use std::path::PathBuf;
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

    #[allow(
        clippy::excessive_nesting,
        reason = "simple retry loop needs a stale-lock branch and stays local to index writes"
    )]
    fn acquire_index_lock(&self) -> Result<RestoreIndexLock, crate::window_state::StateError> {
        let path = self.lock_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    use std::io::Write as _;
                    writeln!(file, "{}", unix_time_ms())?;
                    return Ok(RestoreIndexLock { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = self.lock_is_stale(&path, unix_time_ms())?;
                    if stale {
                        drop(std::fs::remove_file(&path));
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    #[allow(clippy::unused_self, reason = "method for API consistency with write_toml_atomic")]
    fn read_toml<T: DeserializeOwned>(&self, path: Option<PathBuf>) -> std::io::Result<T> {
        let path = path.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing state dir")
        })?;
        let content = std::fs::read_to_string(&path)?;
        toml::from_str(&content)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    #[allow(clippy::unused_self, reason = "method for API consistency with read_toml")]
    fn write_toml_atomic<T: Serialize>(
        &self,
        path: Option<PathBuf>,
        value: &T,
    ) -> Result<(), crate::window_state::StateError> {
        let path = path.ok_or(crate::window_state::StateError::NoStateDir)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("tmp");
        let content = toml::to_string_pretty(value)?;
        std::fs::write(&tmp_path, content)?;
        std::fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    /// Load the persisted restore index, or return an empty one.
    #[allow(dead_code, reason = "used by clear_all for iterating windows")]
    pub fn load_index(&self) -> RestoreIndex {
        self.read_toml(self.index_path()).unwrap_or_else(|_| RestoreIndex {
            version: 1,
            updated_at_ms: 0,
            windows: Vec::new(),
        })
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
        self.read_toml(self.window_path(window_id)).ok()
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

        // Skip corrupted entries until we find a loadable window or exhaust
        // the index.
        while let Some(&window_id) = index.windows.first() {
            index.windows.remove(0);
            if let Some(state) = self.load_window(window_id) {
                self.remove_window(window_id);
                let remaining = index.windows.len();
                index.updated_at_ms = unix_time_ms();
                drop(self.save_index(&index));
                return Some((state, remaining));
            }
            // File missing or corrupted — clean up and try next.
            self.remove_window(window_id);
            tracing::warn!(%window_id, "skipping unreadable restore entry");
        }

        // All entries were corrupted — save the now-empty index.
        index.updated_at_ms = unix_time_ms();
        drop(self.save_index(&index));
        None
    }

    /// Remove all restore state: the index and every per-window file.
    #[allow(dead_code, reason = "available for full cleanup on quit-all")]
    pub fn clear_all(&self) {
        let index = self.load_index();
        for window_id in &index.windows {
            self.remove_window(*window_id);
        }
        if let Some(path) = self.index_path() {
            drop(std::fs::remove_file(path));
        }
    }

    /// Check whether a bootstrap lock file is old enough to be considered stale.
    #[allow(clippy::unused_self, reason = "method for API consistency with other store operations")]
    pub fn lock_is_stale(&self, path: &PathBuf, now_ms: u64) -> std::io::Result<bool> {
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

/// Current UNIX time in milliseconds.
pub fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
