//! Crash-recovery dump of the server's session and workspace state.
//!
//! The server's authoritative state — PTY scrollback, session metadata, and
//! per-window workspace trees — otherwise lives only in RAM plus the
//! `--upgrade` handoff socket, so a crash or an ordinary `systemctl stop`
//! loses every terminal's contents even though the client's own snapshots can
//! rebuild the layout. This module persists the same [`HandoffState`] the hot
//! handoff sends (minus the process-bound fds) to
//! disk on a dirty-gated interval and once more at graceful shutdown, and
//! loads it back on the next cold start so replayed panes come up with their
//! pre-crash scrollback instead of blank grids. (The dump drops only the fds,
//! which a live `SCM_RIGHTS` transfer alone can carry.)
//!
//! Recovery is content-only and self-gating: the loaded sessions are parked in
//! a map keyed by env-envelope id (== the client's launch id), and an entry is
//! consumed only when a cold-restart replay re-creates that exact launch
//! ([`crate::ipc_server`]'s `CreateSession` path). A window the user killed
//! has no snapshot, replays nothing, and its recovered content is never shown.
//! Workspace and window state in the dump is deliberately ignored on load —
//! the client's replay re-reports the layout — so the dump cannot fight the
//! client over topology.
//!
//! Terminal contents are as sensitive as env values (`cat ~/.ssh/id_rsa`), so
//! the dump takes the same at-rest posture as the env store: the `MessagePack`
//! payload is AEAD-sealed via [`crate::env_store::envelope`] with a dedicated
//! DEK in the OS keystore, and keystore failure stops dumping rather than
//! falling back to plaintext.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use scribe_common::app::current_state_dir;
use scribe_common::screen_replay::SessionReplay;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::env_store::envelope;
use crate::env_store::keystore::{self, Dek};
use crate::github_ci::GithubCiTrackerHandle;
use crate::handoff::{self, HandoffState};
use crate::ipc_server::LiveSessionRegistry;
use crate::workspace_manager::WorkspaceManager;

/// How often the dump task samples the dirty generation.
///
/// Every tick that finds new activity re-snapshots and re-encodes every live
/// session, so this is a full-state checkpoint, not an incremental log; 30 s
/// keeps that work off any hot path while bounding crash loss to well under a
/// scrollback's worth of context.
// ponytail: full-state dump per dirty interval; switch to per-session dirty
// tracking with cached replays if profiling ever shows the 30 s tick mattering.
pub const DUMP_INTERVAL: Duration = Duration::from_secs(30);

/// Serialized dumps above this are skipped, mirroring the handoff receiver's
/// 256 MiB `MAX_STATE_SIZE` so a dump can never persist what a handoff would
/// refuse to accept.
const MAX_DUMP_BYTES: usize = 256 * 1024 * 1024;

/// Fixed keystore account for the dump's data-encryption key. One per install
/// flavor via [`keystore::service_identifier`]'s flavored service name.
const DUMP_DEK_ACCOUNT: &str = "state-dump-key";

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Monotonic generation bumped by every state mutation worth persisting.
static DUMP_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Whether this process has dumped at least one live session. Gates the
/// zero-session file removal: a process that owned sessions clears its dump
/// when they are all gone (content hygiene), while a fresh idle server — whose
/// zero-session state says nothing about the previous process's dump — must
/// not delete a crash's recovery file before any client has replayed it.
static DUMPED_LIVE_SESSIONS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Note that server state changed since the last dump. Called from the PTY
/// feed funnels, hook ingress, the session lifecycle funnels, and the
/// workspace-manager mutators; a relaxed increment so the hot paths pay one
/// uncontended atomic.
pub fn mark_dirty() {
    DUMP_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Recovered per-session content from a previous server process, keyed by
/// env-envelope id (the client's launch id). Shared with the `CreateSession`
/// path, which consumes entries as cold-restart replays re-create launches.
pub type RecoveredSessions = Arc<std::sync::Mutex<HashMap<String, SessionReplay>>>;

/// The dump file under the flavor's state dir.
fn dump_path() -> Option<PathBuf> {
    current_state_dir().map(|dir| dir.join("recovery").join("server-state.envz"))
}

/// Write one dump of the current server state; returns whether it succeeded.
///
/// A state with zero sessions removes the file instead: there is nothing to
/// recover, and stale ciphertext should not outlive the sessions it described.
pub async fn dump_now(
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    github_ci_tracker: &GithubCiTrackerHandle,
) -> bool {
    let Some(path) = dump_path() else {
        return false;
    };
    let (state, _fds) =
        handoff::serialize_state(live_sessions, workspace_manager, github_ci_tracker).await;
    if state.sessions.is_empty() {
        if DUMPED_LIVE_SESSIONS.load(Ordering::Relaxed) {
            remove_dump_file(&path);
        }
        return true;
    }
    DUMPED_LIVE_SESSIONS.store(true, Ordering::Relaxed);
    let session_count = state.sessions.len();
    let plaintext = match rmp_serde::to_vec_named(&state) {
        Ok(bytes) => bytes,
        Err(error) => {
            warn!(%error, "state dump encode failed");
            return false;
        }
    };
    if plaintext.len() > MAX_DUMP_BYTES {
        warn!(bytes = plaintext.len(), "state dump exceeds the handoff size cap; skipping");
        return false;
    }
    let Some(dek) = dump_dek_for_write().await else {
        return false;
    };
    let bytes = plaintext.len();
    let written = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let sealed = envelope::seal_bytes(&plaintext, &dek)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_private_atomic(&path, &sealed)
    })
    .await;
    match written {
        Ok(Ok(())) => {
            debug!(sessions = session_count, bytes, "state dump written");
            true
        }
        Ok(Err(error)) => {
            warn!(%error, "state dump write failed");
            false
        }
        Err(error) => {
            warn!(%error, "state dump task panicked");
            false
        }
    }
}

/// Run the dirty-gated dump loop until the server exits.
pub fn spawn_dump_task(
    live_sessions: LiveSessionRegistry,
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
    github_ci_tracker: GithubCiTrackerHandle,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // One behind whatever the current generation is, so a server that
        // came up with restored (handoff) sessions writes its first dump on
        // the first tick even before new output arrives.
        let mut dumped_generation = DUMP_GENERATION.load(Ordering::Relaxed).wrapping_sub(1);
        loop {
            tokio::time::sleep(DUMP_INTERVAL).await;
            // Sampled before collection: a mutation landing mid-dump keeps the
            // generation ahead, so the next tick re-dumps rather than losing it.
            let generation = DUMP_GENERATION.load(Ordering::Relaxed);
            if generation == dumped_generation {
                continue;
            }
            if dump_now(&live_sessions, &workspace_manager, &github_ci_tracker).await {
                dumped_generation = generation;
            }
        }
    })
}

/// Load the previous process's dump into the recovery map.
///
/// Every failure path degrades to an empty map: recovery is best-effort by
/// definition, and a server must never refuse to start over it. The file is
/// left in place — the next dirty dump supersedes it, and keeping it means a
/// server that crashes again before its first dump still recovers the same
/// content on the following start.
pub async fn load_recovered_sessions() -> HashMap<String, SessionReplay> {
    let Some(path) = dump_path() else {
        return HashMap::new();
    };
    let sealed = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(error) => {
            warn!(%error, "state dump unreadable; skipping recovery");
            return HashMap::new();
        }
    };
    let dek = match keystore::get_named_dek(DUMP_DEK_ACCOUNT).await {
        Ok(dek) => dek,
        Err(error) => {
            warn!(%error, "state dump key unavailable; skipping recovery");
            return HashMap::new();
        }
    };
    let state = match decode_dump(&sealed, &dek) {
        Ok(state) => state,
        Err(reason) => {
            warn!(reason, "state dump rejected; discarding it");
            remove_dump_file(&path);
            return HashMap::new();
        }
    };
    let map = recovered_map_from_state(state);
    info!(sessions = map.len(), "recovered session content from the previous server");
    map
}

/// Open and decode a sealed dump, gating the payload's declared handoff
/// version exactly as an upgrade receiver would.
fn decode_dump(sealed: &[u8], dek: &Dek) -> Result<HandoffState, &'static str> {
    let plaintext = envelope::open_bytes(sealed, dek).map_err(|_| "envelope open failed")?;
    let state: HandoffState =
        rmp_serde::from_slice(&plaintext).map_err(|_| "handoff state decode failed")?;
    if !handoff::handoff_version_accepted(state.version, handoff::HANDOFF_VERSION) {
        return Err("handoff version outside the supported range");
    }
    Ok(state)
}

/// Reduce a decoded dump to the per-launch replay map.
///
/// Only sessions carrying both an envelope id and a replay payload are
/// recoverable: the id is the only key a cold-restart replay presents, and
/// without a replay there is nothing to show.
fn recovered_map_from_state(state: HandoffState) -> HashMap<String, SessionReplay> {
    state
        .sessions
        .into_iter()
        .filter_map(|session| Some((session.env_envelope_id?, session.session_replay?)))
        .collect()
}

/// Get the dump DEK, minting and storing a fresh one on first use.
async fn dump_dek_for_write() -> Option<Dek> {
    match keystore::get_named_dek(DUMP_DEK_ACCOUNT).await {
        Ok(dek) => Some(dek),
        Err(keystore::KeystoreError::NotFound) => {
            let dek = keystore::generate_dek();
            match keystore::set_named_dek(DUMP_DEK_ACCOUNT, &dek).await {
                Ok(()) => Some(dek),
                Err(error) => {
                    warn!(%error, "state dump key mint failed; skipping dump");
                    None
                }
            }
        }
        Err(error) => {
            warn!(%error, "state dump key unavailable; skipping dump");
            None
        }
    }
}

fn remove_dump_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warn!(%error, "failed to remove the state dump"),
    }
}

/// Write `bytes` to `path` via a same-directory 0600 temp file, fsync, and
/// atomic rename, creating the 0700 parent first. Same pattern as the env
/// store's envelope writes; duplicated intentionally to keep this module free
/// of the env store's per-window path layout.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "dump path has no dir"))?;
    std::fs::create_dir_all(parent)?;
    set_private_dir_permissions(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("server-state"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(PRIVATE_FILE_MODE);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        drop(std::fs::remove_file(&tmp));
    }
    result
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dek() -> Dek {
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::try_from(i).unwrap_or(u8::MAX);
        }
        key
    }

    fn session(envelope_id: Option<&str>, replay: bool) -> crate::handoff::HandoffSession {
        crate::handoff::HandoffSession {
            session_id: scribe_common::ids::SessionId::new(),
            workspace_id: scribe_common::ids::WorkspaceId::new(),
            child_pid: 1,
            child_identity: None,
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 16,
            snapshot: None,
            session_replay: replay.then(|| SessionReplay {
                cols: 80,
                rows: 24,
                scrollback_rows: 0,
                cursor_col: 0,
                cursor_row: 0,
                cursor_style: scribe_common::screen::CursorStyle::Block,
                cursor_visible: true,
                alt_screen: false,
                replay_zstd: vec![1, 2, 3],
            }),
            title: None,
            icon_title: None,
            shell_name: String::from("bash"),
            task_label: None,
            codex_task_label: None,
            cwd: None,
            context: None,
            ai_state: None,
            ai_provider_hint: None,
            shell_tool: None,
            prompt_state: None,
            env_window_id: None,
            env_envelope_id: envelope_id.map(str::to_owned),
            image_state: None,
        }
    }

    fn state(sessions: Vec<crate::handoff::HandoffSession>) -> HandoffState {
        let version = crate::handoff::handoff_state_version(&sessions);
        HandoffState {
            version,
            sessions,
            workspaces: Vec::new(),
            workspace_tree: None,
            windows: Vec::new(),
            ci_windows: Vec::new(),
        }
    }

    // @lat: [[server#Server#Crash Recovery Dump#Dump round-trips through the sealed envelope]]
    #[test]
    fn dump_round_trips_through_the_sealed_envelope() {
        let dek = sample_dek();
        let original = state(vec![session(Some("launch-a"), true), session(None, true)]);
        let plaintext = rmp_serde::to_vec_named(&original).expect("encode");
        let sealed = envelope::seal_bytes(&plaintext, &dek).expect("seal");

        let decoded = decode_dump(&sealed, &dek).expect("decode");
        assert_eq!(decoded.version, original.version);
        assert_eq!(decoded.sessions.len(), 2);

        // Only the session with both an envelope id and a replay is
        // recoverable; the id-less one has no key a replay could present.
        let map = recovered_map_from_state(decoded);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("launch-a").map(|r| r.replay_zstd.clone()), Some(vec![1, 2, 3]));
    }

    // @lat: [[server#Server#Crash Recovery Dump#Dump rejects foreign versions and keys]]
    #[test]
    fn dump_rejects_foreign_versions_and_keys() {
        let dek = sample_dek();
        let mut future = state(vec![session(Some("launch-a"), true)]);
        future.version = crate::handoff::HANDOFF_VERSION + 1;
        let sealed = envelope::seal_bytes(&rmp_serde::to_vec_named(&future).expect("encode"), &dek)
            .expect("seal");
        assert!(decode_dump(&sealed, &dek).is_err(), "a future version must be refused");

        let mut wrong_dek = sample_dek();
        wrong_dek[0] ^= 1;
        let current = state(vec![session(Some("launch-a"), true)]);
        let sealed_current =
            envelope::seal_bytes(&rmp_serde::to_vec_named(&current).expect("encode"), &dek)
                .expect("seal");
        assert!(decode_dump(&sealed_current, &wrong_dek).is_err(), "a foreign key must fail AEAD");
    }

    // @lat: [[server#Server#Crash Recovery Dump#Sessions without a replay or launch id are dropped]]
    #[test]
    fn sessions_without_a_replay_or_launch_id_are_dropped() {
        let decoded = state(vec![
            session(Some("launch-a"), false),
            session(None, false),
            session(Some("launch-b"), true),
        ]);
        let map = recovered_map_from_state(decoded);
        assert_eq!(map.into_keys().collect::<Vec<_>>(), vec![String::from("launch-b")]);
    }
}
