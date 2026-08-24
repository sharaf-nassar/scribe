//! On-disk envelope I/O — path layout, atomic write-temp + rename,
//! permissions, lifecycle (create / update / read / delete).
//!
//! Each envelope lives at:
//!   `$XDG_STATE_HOME/<flavor>/restore/env/<window_id>/<launch_id>.envz`
//!
//! The `<flavor>` segment comes from
//! [`scribe_common::app::current_state_dir`], the same helper that backs
//! [`crate::env_store`]'s sibling `restore` tree in
//! `scribe-client::restore_state`. That helper resolves to
//! `dirs::state_dir().join(<slug>)` where `<slug>` is `scribe` or
//! `scribe-dev` per [`scribe_common::app::AppIdentity::slug`], so on
//! Linux it lands under `$XDG_STATE_HOME` and on macOS under the
//! platform-appropriate state directory.
//!
//! Layout invariants:
//!   * `0o700` on each enclosing directory.
//!   * `0o600` on each `.envz` file.
//!   * Atomic via private-mode temp file + rename (same pattern as
//!     `scribe-client/src/restore_state.rs`; the helpers are duplicated
//!     here intentionally to keep server-only ownership of `env_store`).
//!   * Retained on crash; deleted on clean session close, clean Scribe
//!     quit, or feature-disable.

use std::io;
use std::path::{Path, PathBuf};

use scribe_common::app::current_state_dir;
use scribe_common::ids::WindowId;

use super::delta::TerminalEnvDelta;
use super::envelope::{EnvelopeError, open as envelope_open, seal as envelope_seal};
use super::keystore::{self, Dek, KeystoreError};

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Maximum attempts to create a unique private temp file before giving up.
const TEMP_FILE_ATTEMPTS: u32 = 16;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("envelope error: {0}")]
    Envelope(#[from] EnvelopeError),
    #[error("keystore error: {0}")]
    Keystore(#[from] KeystoreError),
    #[error("missing state dir (no XDG_STATE_HOME and no fallback)")]
    NoStateDir,
}

/// Returns the per-window env-envelope directory:
/// `<state_dir>/restore/env/<window_id>/`.
///
/// `<state_dir>` is the flavor-aware path returned by
/// [`scribe_common::app::current_state_dir`] — the same root that
/// `scribe-client::restore_state::RestoreStore` uses for its `restore/`
/// subtree. Returns [`StoreError::NoStateDir`] when no state directory
/// can be resolved.
///
/// Does NOT create the directory — call [`ensure_env_dir`] for that.
pub fn env_dir_for(window_id: WindowId) -> Result<PathBuf, StoreError> {
    Ok(env_root()?.join(window_id.to_full_string()))
}

/// Returns the root every per-window env dir hangs off:
/// `<state_dir>/restore/env/`.
///
/// Only [`crate::env_store::gc`] walks it — every other caller addresses a
/// single window. Does NOT create the directory.
pub fn env_root() -> Result<PathBuf, StoreError> {
    let state = current_state_dir().ok_or(StoreError::NoStateDir)?;
    Ok(state.join("restore").join("env"))
}

/// Path to one envelope: `env_dir_for(window_id).join("<launch_id>.envz")`.
pub fn envelope_path(window_id: WindowId, launch_id: &str) -> Result<PathBuf, StoreError> {
    Ok(env_dir_for(window_id)?.join(format!("{launch_id}.envz")))
}

/// Ensures the per-window envelope directory exists with 0o700 perms.
/// Idempotent.
///
/// Creates `<state_dir>/restore/env/<window_id>/` and (re)applies
/// private-mode perms on the leaf. Parent directories already created by
/// `std::fs::create_dir_all` are left with their existing perms — they
/// either pre-exist with their own controls or are sibling-private to
/// the rest of the `restore/` tree.
pub async fn ensure_env_dir(window_id: WindowId) -> Result<PathBuf, StoreError> {
    let dir = env_dir_for(window_id)?;
    let dir_for_task = dir.clone();
    tokio::task::spawn_blocking(move || -> Result<(), StoreError> {
        std::fs::create_dir_all(&dir_for_task)?;
        set_private_dir_perms(&dir_for_task)?;
        Ok(())
    })
    .await
    .map_err(|e| StoreError::Io(io::Error::other(format!("blocking panic: {e}"))))??;
    Ok(dir)
}

/// Read + decrypt an envelope. Fetches the DEK from the keystore, then
/// AEAD-opens the on-disk file. Returns `Ok(None)` if the file is absent
/// (a normal "no envelope yet" state — not an error).
pub async fn read_envelope(
    window_id: WindowId,
    launch_id: &str,
) -> Result<Option<TerminalEnvDelta>, StoreError> {
    let path = envelope_path(window_id, launch_id)?;
    let launch_id_owned = launch_id.to_owned();

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StoreError::Io(e)),
    };

    let dek = keystore::get_dek(window_id, &launch_id_owned).await?;
    let delta = envelope_open(&bytes, &dek)?;
    Ok(Some(delta))
}

/// Encrypt + atomically write an envelope. Fetches the DEK (creating one
/// if absent), seals the delta, and writes to disk via write-temp + rename
/// with 0o600 perms. Idempotent: callers may invoke on every persist tick.
pub async fn write_envelope(
    window_id: WindowId,
    launch_id: &str,
    delta: &TerminalEnvDelta,
) -> Result<(), StoreError> {
    let dir = ensure_env_dir(window_id).await?;
    let final_path = dir.join(format!("{launch_id}.envz"));
    let launch_id_owned = launch_id.to_owned();

    // Get-or-create DEK.
    let dek: Dek = match keystore::get_dek(window_id, &launch_id_owned).await {
        Ok(k) => k,
        Err(KeystoreError::NotFound) => {
            let fresh = keystore::generate_dek();
            keystore::set_dek(window_id, &launch_id_owned, &fresh).await?;
            fresh
        }
        Err(e) => return Err(StoreError::Keystore(e)),
    };

    let envelope_bytes = envelope_seal(delta, &dek)?;

    // Atomic write-temp + rename, on a blocking thread so the async
    // runtime is not held by `fsync`.
    tokio::task::spawn_blocking(move || write_private_atomic_file(&final_path, &envelope_bytes))
        .await
        .map_err(|e| StoreError::Io(io::Error::other(format!("blocking panic: {e}"))))?
}

/// Stage one envelope's workspace-transfer re-bind: copy its DEK and sealed
/// file from `source_window`'s coordinates to `target_window`'s, without
/// decrypting and without touching the source. Returns whether a file was
/// staged.
///
/// The file gates everything — a session that never persisted has nothing to
/// re-bind and costs zero keystore I/O. A file whose DEK is already missing
/// is copied alone: restore at the new coordinates fails exactly as it would
/// have at the old ones. Any real keystore or filesystem failure propagates
/// so the transfer can refuse with `EnvironmentRebindFailed`; the staged
/// target copy is then collectable garbage (`delete_envelope` on the target
/// coordinates), never a broken restore.
pub async fn stage_envelope_transfer(
    source_window: WindowId,
    target_window: WindowId,
    launch_id: &str,
) -> Result<bool, StoreError> {
    let source_path = envelope_path(source_window, launch_id)?;
    let sealed = match tokio::fs::read(&source_path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(StoreError::Io(e)),
    };

    match keystore::get_dek(source_window, launch_id).await {
        Ok(dek) => keystore::set_dek(target_window, launch_id, &dek).await?,
        Err(KeystoreError::NotFound) => {}
        Err(e) => return Err(StoreError::Keystore(e)),
    }

    let target_dir = ensure_env_dir(target_window).await?;
    let target_path = target_dir.join(format!("{launch_id}.envz"));
    tokio::task::spawn_blocking(move || write_private_atomic_file(&target_path, &sealed))
        .await
        .map_err(|e| StoreError::Io(io::Error::other(format!("blocking panic: {e}"))))??;
    Ok(true)
}

/// Write `content` to `final_path` through a private temp file and same-dir
/// rename. The temp file is removed if the rename fails.
fn write_private_atomic_file(final_path: &Path, content: &[u8]) -> Result<(), StoreError> {
    let tmp = write_private_temp_file(final_path, content)?;
    if let Err(e) = std::fs::rename(&tmp, final_path) {
        // Best-effort cleanup of the orphaned temp on rename failure.
        drop(std::fs::remove_file(&tmp));
        return Err(StoreError::Io(e));
    }
    set_private_file_perms(final_path)?;
    Ok(())
}

/// Delete an envelope + its DEK. Idempotent; missing entries are not errors.
pub async fn delete_envelope(window_id: WindowId, launch_id: &str) -> Result<(), StoreError> {
    let path = envelope_path(window_id, launch_id)?;
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(StoreError::Io(e)),
    }
    // Best-effort DEK delete — log on failure, don't propagate (the disk
    // entry is gone, which is the user-visible state).
    let launch_id_owned = launch_id.to_owned();
    if let Err(e) = keystore::delete_dek(window_id, &launch_id_owned).await
        && !matches!(e, KeystoreError::NotFound)
    {
        tracing::warn!(
            target: "scribe_server::env_store::store",
            error = ?e,
            window_id = ?window_id,
            launch_id = %launch_id_owned,
            "delete_dek failed during envelope deletion"
        );
    }
    Ok(())
}

/// Delete every envelope under a window's env dir. Used on clean window
/// close and on the feature-disable transition.
pub async fn delete_window_envelopes(window_id: WindowId) -> Result<(), StoreError> {
    let dir = env_dir_for(window_id)?;
    // List, delete each (so DEKs come with).
    let entries: Vec<PathBuf> = match tokio::fs::read_dir(&dir).await {
        Ok(mut rd) => {
            let mut out = Vec::new();
            while let Ok(Some(e)) = rd.next_entry().await {
                out.push(e.path());
            }
            out
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(StoreError::Io(e)),
    };
    for path in entries {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            // Filename is "<launch_id>.envz" — stem == launch_id.
            _ = delete_envelope(window_id, stem).await;
        }
    }
    // Best-effort: remove the now-empty dir.
    _ = tokio::fs::remove_dir(&dir).await;
    Ok(())
}

// ---- Private filesystem helpers (mirroring restore_state.rs) ----

#[cfg(unix)]
fn set_private_dir_perms(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut p = std::fs::metadata(dir)?.permissions();
    p.set_mode(PRIVATE_DIR_MODE);
    std::fs::set_permissions(dir, p)
}

#[cfg(not(unix))]
fn set_private_dir_perms(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_perms(file: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut p = std::fs::metadata(file)?.permissions();
    p.set_mode(PRIVATE_FILE_MODE);
    std::fs::set_permissions(file, p)
}

#[cfg(not(unix))]
fn set_private_file_perms(_file: &Path) -> io::Result<()> {
    Ok(())
}

/// Create a private-mode temp file alongside `final_path`, fsync the
/// content to disk, and return the temp path on success. The caller is
/// responsible for renaming it onto `final_path`.
fn write_private_temp_file(final_path: &Path, content: &[u8]) -> io::Result<PathBuf> {
    use std::io::Write as _;
    let parent =
        final_path.parent().ok_or_else(|| io::Error::other("envelope path has no parent"))?;
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());

    for attempt in 0u32..TEMP_FILE_ATTEMPTS {
        let stem = final_path.file_name().and_then(|s| s.to_str()).unwrap_or("envelope");
        let tmp_name = format!(".{stem}.tmp.{pid}.{nanos}.{attempt}");
        let tmp_path = parent.join(tmp_name);

        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(PRIVATE_FILE_MODE);
        }
        match opts.open(&tmp_path) {
            Ok(mut f) => {
                f.write_all(content)?;
                f.sync_all()?;
                return Ok(tmp_path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("could not create private temp file after 16 attempts"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("scribe-env-transfer-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    // @lat: [[server#Workspace Transfer#Staged env re-bind]]
    #[test]
    fn private_atomic_write_leaves_no_temp() {
        let dir = scratch_dir("copy");
        let target = dir.join("launch-a.envz");
        write_private_atomic_file(&target, b"sealed-bytes").expect("write");
        assert_eq!(std::fs::read(&target).expect("read staged copy"), b"sealed-bytes");
        let leftovers = std::fs::read_dir(&dir)
            .expect("list dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.path() != target)
            .count();
        assert_eq!(leftovers, 0, "no temp file survives a successful copy");
    }

    // @lat: [[server#Workspace Transfer#Staged env re-bind]]
    #[test]
    fn private_atomic_write_failure_leaves_no_partial_target() {
        let dir = scratch_dir("fail");
        let target = dir.join("missing-subdir").join("launch-a.envz");
        assert!(write_private_atomic_file(&target, b"sealed-bytes").is_err());
        assert!(!target.exists(), "no partial file at the target coordinates");
    }
}
