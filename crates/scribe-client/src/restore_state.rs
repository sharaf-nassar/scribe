//! Persisted cold-restart restore state for the GPUI client.
//!
//! Ported byte-for-byte from the legacy client's `restore_state.rs`. The
//! [`RestoreStore`] persists one TOML snapshot per window under
//! `$XDG_STATE_HOME/scribe/restore/windows/<window_id>.toml` plus a shared
//! `index.toml`, all hardened to `0700`/`0600` because launch bindings can
//! carry prompt text and provider conversation IDs. A bootstrap lock file
//! serialises multi-process index mutations; stale locks (>30 s) are reclaimed.
//! [`RestoreStore::claim_first_window`] atomically claims the first replayable
//! entry for cold-restart replay and reports how many windows remain so the
//! caller can fan out `--restore-child` processes.

use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use scribe_common::ai_state::AiProvider;
use scribe_common::app::current_state_dir;
use scribe_common::ids::{WindowId, WorkspaceId};
pub use scribe_common::protocol::AiResumeMode;
use scribe_common::protocol::{LayoutDirection, SessionPromptState, ShellTool};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Persisted list of windows that should be reopened on the next cold start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreIndex {
    pub version: u32,
    pub updated_at_ms: u64,
    pub windows: Vec<WindowId>,
    /// Windows whose snapshot has been claimed for replay but not yet
    /// superseded by a fresh snapshot. A claimed entry keeps its file on disk
    /// — the last good layout must survive until a replacement is durably
    /// written — and is not claimable again while the claim is fresh, so a
    /// crash loop cannot replay the same snapshot more than once per
    /// [`CLAIM_TTL_MS`]. Absent in indexes written by older clients.
    #[serde(default)]
    pub claimed: Vec<WindowId>,
    /// Claim timestamps (Unix ms) parallel to `claimed`. A claim older than
    /// [`CLAIM_TTL_MS`] whose window never flushed a fresh snapshot is moved
    /// back to `windows` at the next claim scan: without the expiry, a client
    /// that crashed mid-replay — after claiming, before its first post-claim
    /// flush — parked that layout in `claimed` forever and it was never
    /// restorable again. Entries missing here (an older client rewrote the
    /// index) are re-stamped "now" on the next scan, restarting their TTL
    /// rather than reclaiming immediately.
    #[serde(default)]
    pub claimed_at_ms: Vec<u64>,
}

/// How long a claim shields its snapshot from being claimed again.
///
/// A live claimant with a working server flushes a fresh snapshot (which
/// supersedes the claim) within seconds of replay, and a claimant against a
/// dead server never replays at all — so anything still claimed after this
/// window is a crashed or wedged claimant, and its layout should become
/// claimable again. Long enough that a slow-but-alive claimant cannot race a
/// second launch into a duplicate replay; short enough that one mid-replay
/// crash costs the user a single restart, not the layout.
pub const CLAIM_TTL_MS: u64 = 15 * 60 * 1000;

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
    /// A snapshot is replayable only when it has at least one launch record and
    /// at least one workspace with a tab; a blank window is never replayed.
    #[must_use]
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
    /// Prompt-bar history for the pane, the same record the server retains and
    /// the client renders from. Flattened, so the five prompt fields keep the
    /// top-level names older snapshots were written with, and each defaults
    /// individually for snapshots that predate it.
    #[serde(flatten)]
    pub prompts: SessionPromptState,
}

/// Launch type recorded for restore replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchKind {
    Shell,
    CustomCommand {
        argv: Vec<String>,
    },
    Ai {
        provider: AiProvider,
        resume_mode: AiResumeMode,
        conversation_id: Option<String>,
    },
    /// A launch-only tool tab (see [`ShellTool`]). Replay re-runs the tool in a
    /// fresh plain-tab shell; there is no conversation to target.
    ShellTool {
        tool: ShellTool,
    },
}

/// Runtime binding kept on each pane so restore snapshots can refer to a stable
/// launch ID even before replay logic exists.
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

impl Default for RestoreStore {
    fn default() -> Self {
        Self::new()
    }
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
    #[must_use]
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
        if let Some(parent) = path.parent()
            && parent != root
        {
            ensure_private_dir(parent)?;
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
    ///
    /// # Errors
    /// Returns [`StateError`](crate::window_state::StateError) when the state
    /// directory is unavailable or the file cannot be written.
    pub fn save_index(&self, index: &RestoreIndex) -> Result<(), crate::window_state::StateError> {
        self.write_toml_atomic(self.index_path(), index)
    }

    /// Insert or refresh a window entry in the restore index.
    ///
    /// # Errors
    /// Returns [`StateError`](crate::window_state::StateError) when the index
    /// lock cannot be acquired or the index cannot be persisted.
    pub fn upsert_index(&self, window_id: WindowId) -> Result<(), crate::window_state::StateError> {
        let _lock = self.acquire_index_lock()?;
        let mut index = self.read_index_for_update()?;
        if !index.windows.contains(&window_id) {
            index.windows.push(window_id);
        }
        // A fresh snapshot supersedes any earlier claim of the same window.
        remove_claim(&mut index, window_id);
        index.updated_at_ms = unix_time_ms();
        self.save_index(&index)
    }

    /// Remove a window entry from the restore index.
    ///
    /// # Errors
    /// Returns [`StateError`](crate::window_state::StateError) when the index
    /// lock cannot be acquired or the index cannot be persisted.
    pub fn remove_from_index(
        &self,
        window_id: WindowId,
    ) -> Result<(), crate::window_state::StateError> {
        let _lock = self.acquire_index_lock()?;
        let mut index = self.read_index_for_update()?;
        index.windows.retain(|id| *id != window_id);
        remove_claim(&mut index, window_id);
        index.updated_at_ms = unix_time_ms();
        self.save_index(&index)
    }

    /// Load the persisted logical state for a single window.
    #[must_use]
    pub fn load_window(&self, window_id: WindowId) -> Option<WindowRestoreState> {
        Self::read_toml(self.window_path(window_id)).ok()
    }

    /// Save one window's logical state to disk.
    ///
    /// # Errors
    /// Returns [`StateError`](crate::window_state::StateError) when the state
    /// directory is unavailable or the file cannot be written.
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
        if let Err(error) = result
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%window_id, error = %error, "failed to remove restore window state");
        }
    }

    /// Atomically claim the first valid window from the restore index for
    /// cold-restart replay. Returns the claimed window's state and the number
    /// of remaining unclaimed windows (so the caller can spawn additional
    /// client processes). Corrupted entries are skipped and removed. The
    /// claimed entry moves to the index's `claimed` list — never claimable
    /// again, so a crash loop cannot double-replay it — but its on-disk file
    /// survives until a fresh snapshot supersedes it via [`Self::upsert_index`]
    /// or the claimant discards it via [`Self::remove_from_index`].
    #[must_use]
    pub fn claim_first_window(&self) -> Option<(WindowRestoreState, usize)> {
        let _lock = self.acquire_index_lock().ok()?;
        let mut index = self.read_index_for_update().ok()?;
        reclaim_stale_claims(&mut index, unix_time_ms());
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
                    // The file stays on disk: it is the last good layout for
                    // this window and only a durably written replacement may
                    // retire it.
                    index.claimed.push(window_id);
                    index.claimed_at_ms.push(unix_time_ms());
                    claimed = Some(state);
                }
                Some(_) => {
                    remaining_valid.push(window_id);
                }
                None => {
                    // File missing or corrupted — clean up and drop the stale
                    // index entry.
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
    ///
    /// # Errors
    /// Returns the underlying I/O error when the lock file cannot be read for a
    /// reason other than it being absent.
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RestoreIndex {
                version: 1,
                updated_at_ms: 0,
                windows: Vec::new(),
                claimed: Vec::new(),
                claimed_at_ms: Vec::new(),
            }),
            Err(error) => Err(error.into()),
        }
    }
}

/// Drop `window_id`'s claim, keeping the timestamp vec aligned.
fn remove_claim(index: &mut RestoreIndex, window_id: WindowId) {
    normalize_claim_stamps(index, unix_time_ms());
    let mut stamps = index.claimed_at_ms.iter().copied();
    let mut kept_stamps = Vec::with_capacity(index.claimed_at_ms.len());
    index.claimed.retain(|id| {
        let stamp = stamps.next().unwrap_or_else(unix_time_ms);
        let keep = *id != window_id;
        if keep {
            kept_stamps.push(stamp);
        }
        keep
    });
    index.claimed_at_ms = kept_stamps;
}

/// Re-stamp claims an older client's index rewrite left timestamp-less, so a
/// missing stamp restarts its TTL instead of reading as immediately stale.
fn normalize_claim_stamps(index: &mut RestoreIndex, now_ms: u64) {
    index.claimed_at_ms.truncate(index.claimed.len());
    while index.claimed_at_ms.len() < index.claimed.len() {
        index.claimed_at_ms.push(now_ms);
    }
}

/// Move claims older than [`CLAIM_TTL_MS`] back to the front of the claimable
/// list, preserving their original priority over younger windows.
fn reclaim_stale_claims(index: &mut RestoreIndex, now_ms: u64) {
    normalize_claim_stamps(index, now_ms);
    let mut reclaimed = Vec::new();
    let mut kept_ids = Vec::with_capacity(index.claimed.len());
    let mut kept_stamps = Vec::with_capacity(index.claimed.len());
    for (id, stamp) in index.claimed.iter().copied().zip(index.claimed_at_ms.iter().copied()) {
        if now_ms.saturating_sub(stamp) > CLAIM_TTL_MS {
            reclaimed.push(id);
        } else {
            kept_ids.push(id);
            kept_stamps.push(stamp);
        }
    }
    if reclaimed.is_empty() {
        return;
    }
    tracing::warn!(
        count = reclaimed.len(),
        "reclaiming stale restore claims whose claimant never flushed a snapshot"
    );
    index.claimed = kept_ids;
    index.claimed_at_ms = kept_stamps;
    reclaimed.append(&mut index.windows);
    index.windows = reclaimed;
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
#[must_use]
pub fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_at(root: &Path) -> RestoreStore {
        RestoreStore { root: Some(root.join("restore")) }
    }

    fn leaf_snapshot(window_id: WindowId, workspace_id: WorkspaceId) -> WindowRestoreState {
        WindowRestoreState {
            version: 1,
            window_id,
            focused_workspace_id: workspace_id,
            root: WorkspaceLayoutSnapshot::Leaf { workspace_id },
            workspaces: vec![WorkspaceSnapshot {
                workspace_id,
                name: Some("proj".to_owned()),
                accent_color: [0.1, 0.2, 0.3, 1.0],
                active_tab_index: 0,
                tabs: vec![TabSnapshot {
                    focused_launch_id: "launch-a".to_owned(),
                    pane_tree: PaneSnapshot::Leaf { launch_id: "launch-a".to_owned() },
                }],
            }],
            launches: vec![LaunchRecord {
                launch_id: "launch-a".to_owned(),
                cwd: Some(PathBuf::from("/tmp/proj")),
                kind: LaunchKind::Shell,
                prompts: SessionPromptState::default(),
            }],
        }
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Snapshot round-trips through disk]]
    #[test]
    fn snapshot_round_trips_through_disk() {
        let dir = tempdir();
        let store = store_at(&dir);
        let window_id = WindowId::new();
        let workspace_id = WorkspaceId::new();
        let snapshot = leaf_snapshot(window_id, workspace_id);

        store.save_window(&snapshot).expect("save window snapshot");
        let loaded = store.load_window(window_id).expect("load window snapshot");

        assert_eq!(loaded.window_id, window_id);
        assert_eq!(loaded.focused_workspace_id, workspace_id);
        assert_eq!(loaded.workspaces[0].name.as_deref(), Some("proj"));
        assert_eq!(loaded.launches[0].launch_id, "launch-a");
        assert!(loaded.is_replayable());
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#AI resume variant names stay stable]]
    #[test]
    fn ai_resume_mode_toml_round_trip_preserves_variant_names() {
        for (resume_mode, variant_name) in
            [(AiResumeMode::New, "New"), (AiResumeMode::Resume, "Resume")]
        {
            let original = LaunchKind::Ai {
                provider: AiProvider::ClaudeCode,
                resume_mode,
                conversation_id: None,
            };
            let encoded = toml::to_string(&original).expect("serialize AI launch kind");
            assert_eq!(
                encoded,
                format!(
                    "kind = \"ai\"\nprovider = \"claude_code\"\nresume_mode = \"{variant_name}\"\n"
                )
            );

            let decoded: LaunchKind = toml::from_str(&encoded).expect("deserialize AI launch kind");
            assert!(matches!(
                decoded,
                LaunchKind::Ai {
                    provider: AiProvider::ClaudeCode,
                    resume_mode: decoded_mode,
                    conversation_id: None,
                } if decoded_mode == resume_mode
            ));
        }
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Tool launch kind round-trips]]
    #[test]
    fn shell_tool_toml_round_trip_preserves_the_tool() {
        let original = LaunchKind::ShellTool { tool: ShellTool::Pi };
        let encoded = toml::to_string(&original).expect("serialize tool launch kind");
        assert_eq!(encoded, "kind = \"shell_tool\"\ntool = \"pi\"\n");

        let decoded: LaunchKind = toml::from_str(&encoded).expect("deserialize tool launch kind");
        assert!(matches!(decoded, LaunchKind::ShellTool { tool: ShellTool::Pi }));
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Claim skips non-replayable and remaining count]]
    #[test]
    fn claim_first_window_skips_non_replayable_and_reports_remaining() {
        let dir = tempdir();
        let store = store_at(&dir);

        let win_blank = WindowId::new();
        let mut blank = leaf_snapshot(win_blank, WorkspaceId::new());
        blank.launches.clear();
        blank.workspaces[0].tabs.clear();
        assert!(!blank.is_replayable());

        let win_a = WindowId::new();
        let win_b = WindowId::new();
        let snap_a = leaf_snapshot(win_a, WorkspaceId::new());
        let snap_b = leaf_snapshot(win_b, WorkspaceId::new());

        store.save_window(&blank).expect("save blank");
        store.save_window(&snap_a).expect("save a");
        store.save_window(&snap_b).expect("save b");
        store.upsert_index(win_blank).expect("index blank");
        store.upsert_index(win_a).expect("index a");
        store.upsert_index(win_b).expect("index b");

        let (claimed, remaining) = store.claim_first_window().expect("claim first");
        // The blank entry is dropped, the first replayable (win_a) is claimed,
        // and win_b remains for a --restore-child fan-out.
        assert_eq!(claimed.window_id, win_a);
        assert_eq!(remaining, 1);
        // The claimed window's file survives the claim: it is the last good
        // layout until a replacement snapshot is durably written.
        assert!(store.load_window(win_a).is_some());
        // The blank (non-replayable) window's file is still pruned.
        assert!(store.load_window(win_blank).is_none());

        // A second claim skips the already-claimed win_a — a crash loop must
        // never replay the same snapshot twice — and takes win_b instead.
        let (claimed_b, remaining_b) = store.claim_first_window().expect("claim second");
        assert_eq!(claimed_b.window_id, win_b);
        assert_eq!(remaining_b, 0);
        assert!(store.claim_first_window().is_none());

        // Writing a fresh snapshot supersedes the claim: the id becomes
        // claimable again and later claims replay the new file.
        store.save_window(&snap_a).expect("save replacement");
        store.upsert_index(win_a).expect("re-index a");
        let (reclaimed, _) = store.claim_first_window().expect("claim replacement");
        assert_eq!(reclaimed.window_id, win_a);
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Stale claim reclaimed]]
    #[test]
    fn stale_claim_becomes_claimable_again() {
        let dir = tempdir();
        let store = store_at(&dir);
        let window_id = WindowId::new();
        let snapshot = leaf_snapshot(window_id, WorkspaceId::new());
        store.save_window(&snapshot).expect("save window");
        store.upsert_index(window_id).expect("index window");

        let (first, _) = store.claim_first_window().expect("first claim");
        assert_eq!(first.window_id, window_id);
        // A fresh claim shields the snapshot: nothing further is claimable.
        assert!(store.claim_first_window().is_none());

        // Age the claim past the TTL by rewriting its stamp, as if the
        // claimant crashed mid-replay fifteen-plus minutes ago.
        let index_path = dir.join("restore").join("index.toml");
        let mut index: RestoreIndex =
            toml::from_str(&std::fs::read_to_string(&index_path).expect("read index"))
                .expect("parse index");
        assert_eq!(index.claimed, vec![window_id]);
        index.claimed_at_ms = vec![0];
        std::fs::write(&index_path, toml::to_string(&index).expect("encode index"))
            .expect("rewrite index");

        // The stale claim is reclaimed and the same snapshot replays again.
        let (reclaimed, remaining) = store.claim_first_window().expect("reclaim");
        assert_eq!(reclaimed.window_id, window_id);
        assert_eq!(remaining, 0);
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Stale claim reclaimed]]
    #[test]
    fn missing_claim_stamp_restarts_its_ttl() {
        // An index rewritten by an older client keeps `claimed` but drops the
        // stamps; the claim must NOT read as immediately stale.
        let mut index = RestoreIndex {
            version: 1,
            updated_at_ms: 0,
            windows: Vec::new(),
            claimed: vec![WindowId::new()],
            claimed_at_ms: Vec::new(),
        };
        reclaim_stale_claims(&mut index, 500_000);
        assert_eq!(index.claimed.len(), 1, "stampless claim keeps shielding its snapshot");
        assert_eq!(index.claimed_at_ms, vec![500_000], "stamp restarts at the scan instant");
        assert!(index.windows.is_empty());

        // Only once that restarted TTL expires is the claim reclaimed, ahead
        // of any younger unclaimed window.
        let younger = WindowId::new();
        index.windows.push(younger);
        reclaim_stale_claims(&mut index, 500_000 + CLAIM_TTL_MS + 1);
        assert!(index.claimed.is_empty());
        assert_eq!(index.windows.len(), 2);
        assert_eq!(index.windows[1], younger, "reclaimed window regains front priority");
    }

    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Stale lock reclaimed]]
    #[test]
    fn stale_lock_is_reclaimed() {
        let dir = tempdir();
        let lock = dir.join("bootstrap.lock");
        std::fs::write(&lock, "0").expect("seed lock");
        // now_ms far past the 30 s window makes the lock stale.
        assert!(RestoreStore::lock_is_stale(&lock, 60_000).expect("stale check"));
        // A freshly-stamped lock is not stale.
        std::fs::write(&lock, "59_000".replace('_', "")).expect("restamp lock");
        assert!(!RestoreStore::lock_is_stale(&lock, 60_000).expect("fresh check"));
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "scribe-gpui-restore-{}-{}",
            std::process::id(),
            unix_time_ms()
        ));
        std::fs::create_dir_all(&base).expect("create tempdir");
        base
    }
}
