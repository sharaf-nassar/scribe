//! Startup garbage collection for orphaned env envelopes.
//!
//! Every create path now mints its own launch id, so an envelope outlives the
//! launch that wrote it whenever the owning window never closes cleanly — a
//! crash, a SIGKILL, a cold restart that replays under a fresh window id. Those
//! files are encrypted and small, but they accumulate forever and each one
//! keeps a keystore DEK alive with it.
//!
//! One sweep runs at server startup. An envelope is deleted only when BOTH
//! hold: no window snapshot on disk still names its launch id, and the file
//! has not been touched for [`ORPHAN_RETENTION`]. The age gate is the safety
//! net — a window whose snapshot has not been written yet, or a client that
//! writes snapshots on a slower cadence than the server starts, must never
//! lose an envelope to a race.
//!
//! The reference set comes from the client's own restore snapshots
//! (`<state_dir>/restore/windows/*.toml`), the same tree
//! `scribe-client::restore_state` writes. Only `launch_id` is read out of
//! them, and a snapshot the server cannot parse suppresses the whole sweep:
//! deleting on an incomplete reference set is worse than deleting nothing. The
//! client drops unreadable snapshots on its next cold start, so that state
//! resolves itself.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use scribe_common::app::current_state_dir;
use scribe_common::ids::WindowId;

use super::store;

/// How long an unreferenced envelope is kept before the startup sweep deletes
/// it. Long enough that no plausible client-restart or snapshot-write cadence
/// can lose live state to the sweep, short enough that a machine that crashes
/// regularly does not carry a year of dead DEKs.
pub const ORPHAN_RETENTION: Duration = Duration::from_hours(24 * 30);

/// The parts of a sweep that stay fixed across every window directory it
/// walks. Bundled so the per-directory pass reads as "this tree, under these
/// rules" instead of a six-argument call.
struct SweepRules<'a> {
    referenced: &'a HashSet<String>,
    retention: Duration,
    now: SystemTime,
}

/// One envelope the sweep decided to delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub window_id: WindowId,
    pub launch_id: String,
}

/// Only the part of a persisted window snapshot the sweep cares about. Serde
/// ignores every other field, so this deliberately does not track
/// `scribe-client::restore_state::WindowRestoreState`.
#[derive(serde::Deserialize)]
struct WindowSnapshotRefs {
    #[serde(default)]
    launches: Vec<LaunchRef>,
}

#[derive(serde::Deserialize)]
struct LaunchRef {
    launch_id: String,
}

/// Run one startup sweep against the real state directory.
///
/// Never fails the caller: a machine with no state dir, no env tree, or an
/// unreadable snapshot simply sweeps nothing. Deletion goes through
/// [`store::delete_envelope`] so each removed file takes its keystore DEK with
/// it.
pub async fn sweep_orphaned_envelopes(retention: Duration) {
    let Some(state) = current_state_dir() else {
        return;
    };
    let Ok(env_root) = store::env_root() else {
        return;
    };
    let windows_dir = state.join("restore").join("windows");

    let scan = tokio::task::spawn_blocking(move || {
        collect_orphans(&env_root, &windows_dir, retention, SystemTime::now())
    })
    .await;
    let orphans = match scan {
        Ok(orphans) => orphans,
        Err(error) => {
            tracing::warn!(
                target: "scribe_server::env_store::gc",
                %error,
                "env-envelope GC scan panicked; skipping the sweep"
            );
            return;
        }
    };
    if orphans.is_empty() {
        return;
    }

    let mut deleted = 0_usize;
    for orphan in &orphans {
        match store::delete_envelope(orphan.window_id, &orphan.launch_id).await {
            Ok(()) => deleted += 1,
            Err(error) => tracing::warn!(
                target: "scribe_server::env_store::gc",
                ?error,
                window_id = ?orphan.window_id,
                launch_id = %orphan.launch_id,
                "orphaned env envelope could not be deleted"
            ),
        }
    }
    prune_empty_window_dirs(&orphans).await;
    tracing::info!(
        target: "scribe_server::env_store::gc",
        deleted,
        considered = orphans.len(),
        retention_days = retention.as_secs() / 86_400,
        "swept orphaned env envelopes"
    );
}

/// Remove the per-window directories the sweep just emptied. Best-effort:
/// `remove_dir` refuses a non-empty directory, which is exactly the guard we
/// want against deleting a window that still holds a live envelope.
async fn prune_empty_window_dirs(orphans: &[Orphan]) {
    let mut seen: HashSet<String> = HashSet::new();
    for orphan in orphans {
        if !seen.insert(orphan.window_id.to_full_string()) {
            continue;
        }
        if let Ok(dir) = store::env_dir_for(orphan.window_id) {
            drop(tokio::fs::remove_dir(dir).await);
        }
    }
}

/// Decide which envelopes under `env_root` are orphaned, given the window
/// snapshots in `windows_dir` and a clock reading.
///
/// Pure over the filesystem so a test can stage a tree and assert the exact
/// selection without a keystore. Returns an empty selection — never a partial
/// one — for any condition that makes the reference set untrustworthy.
fn collect_orphans(
    env_root: &Path,
    windows_dir: &Path,
    retention: Duration,
    now: SystemTime,
) -> Vec<Orphan> {
    let Some(referenced) = referenced_launch_ids(windows_dir) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(env_root) else {
        // No env tree yet: nothing has ever been persisted on this machine.
        return Vec::new();
    };

    let rules = SweepRules { referenced: &referenced, retention, now };
    let mut orphans = Vec::new();
    for window_dir in entries.flatten() {
        let name = window_dir.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(window_id) = name.parse::<WindowId>() else {
            // Not a window dir the store wrote; leave foreign files alone.
            continue;
        };
        collect_window_orphans(&window_dir.path(), window_id, &rules, &mut orphans);
    }
    orphans.sort_by(|a, b| {
        (a.window_id.to_full_string(), &a.launch_id)
            .cmp(&(b.window_id.to_full_string(), &b.launch_id))
    });
    orphans
}

/// Append every orphaned envelope in one window's directory.
fn collect_window_orphans(
    dir: &Path,
    window_id: WindowId,
    rules: &SweepRules<'_>,
    out: &mut Vec<Orphan>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "envz") {
            continue;
        }
        let Some(launch_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if rules.referenced.contains(launch_id) {
            continue;
        }
        if !is_older_than(&path, rules.retention, rules.now) {
            continue;
        }
        out.push(Orphan { window_id, launch_id: launch_id.to_owned() });
    }
}

/// Whether a file's last modification is at least `retention` in the past.
///
/// Anything unreadable, or stamped in the future by a skewed clock, counts as
/// young: the sweep only ever deletes on positive evidence of age.
fn is_older_than(path: &Path, retention: Duration, now: SystemTime) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|meta| meta.modified()) else {
        return false;
    };
    now.duration_since(modified).is_ok_and(|age| age >= retention)
}

/// Every launch id any window snapshot still names, or `None` when the set
/// cannot be trusted (a snapshot that does not parse).
fn referenced_launch_ids(windows_dir: &Path) -> Option<HashSet<String>> {
    let mut referenced = HashSet::new();
    let entries = match std::fs::read_dir(windows_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(referenced),
        Err(error) => {
            tracing::warn!(
                target: "scribe_server::env_store::gc",
                %error,
                "restore/windows unreadable; skipping env-envelope GC"
            );
            return None;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let snapshot = parse_snapshot_refs(&path)?;
        referenced.extend(snapshot.launches.into_iter().map(|launch| launch.launch_id));
    }
    Some(referenced)
}

fn parse_snapshot_refs(path: &Path) -> Option<WindowSnapshotRefs> {
    let text = std::fs::read_to_string(path)
        .inspect_err(|error| {
            tracing::warn!(
                target: "scribe_server::env_store::gc",
                %error,
                path = %path.display(),
                "window snapshot unreadable; skipping env-envelope GC"
            );
        })
        .ok()?;
    toml::from_str::<WindowSnapshotRefs>(&text)
        .inspect_err(|error| {
            tracing::warn!(
                target: "scribe_server::env_store::gc",
                %error,
                path = %path.display(),
                "window snapshot unparsable; skipping env-envelope GC"
            );
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("scribe-env-gc-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(dir.join("env")).expect("create env root");
        std::fs::create_dir_all(dir.join("windows")).expect("create windows dir");
        dir
    }

    /// Stage `<env>/<window>/<launch>.envz` with an mtime `age_days` in the
    /// past, so the retention gate can be exercised without waiting.
    fn write_envelope(root: &Path, window_id: WindowId, launch_id: &str, age_days: u64) {
        let dir = root.join("env").join(window_id.to_full_string());
        std::fs::create_dir_all(&dir).expect("create window env dir");
        let path = dir.join(format!("{launch_id}.envz"));
        std::fs::write(&path, b"sealed").expect("write envelope");
        let file = std::fs::File::options().write(true).open(&path).expect("reopen envelope");
        let modified = SystemTime::now() - Duration::from_secs(age_days * 86_400);
        file.set_times(std::fs::FileTimes::new().set_modified(modified)).expect("set mtime");
    }

    fn write_snapshot(root: &Path, window_id: WindowId, launch_ids: &[&str]) {
        let mut text = format!("version = 1\nwindow_id = \"{}\"\n", window_id.to_full_string());
        for launch_id in launch_ids {
            write!(text, "\n[[launches]]\nlaunch_id = \"{launch_id}\"\nkind = \"shell\"\n")
                .expect("format snapshot");
        }
        std::fs::write(
            root.join("windows").join(format!("{}.toml", window_id.to_full_string())),
            text,
        )
        .expect("write snapshot");
    }

    fn sweep(root: &Path) -> Vec<Orphan> {
        collect_orphans(
            &root.join("env"),
            &root.join("windows"),
            ORPHAN_RETENTION,
            SystemTime::now(),
        )
    }

    // @lat: [[server#Server#Env Persistence#Orphaned Envelope GC]]
    #[test]
    fn deletes_only_unreferenced_envelopes_past_retention() {
        let root = scratch_dir("selection");
        let window = WindowId::new();
        let live = WindowId::new();

        write_envelope(&root, window, "orphan-old", 31);
        write_envelope(&root, window, "orphan-young", 3);
        write_envelope(&root, live, "still-referenced", 400);
        write_snapshot(&root, live, &["still-referenced"]);

        let orphans = sweep(&root);

        assert_eq!(orphans, vec![Orphan { window_id: window, launch_id: "orphan-old".to_owned() }]);
        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn a_launch_id_referenced_from_any_window_is_kept() {
        let root = scratch_dir("cross-window");
        // A cold restart replays a saved launch under a brand-new window id,
        // so the envelope the old window wrote must survive on the strength of
        // the new window's snapshot naming the same launch.
        let old_window = WindowId::new();
        let new_window = WindowId::new();

        write_envelope(&root, old_window, "shared-launch", 90);
        write_snapshot(&root, new_window, &["shared-launch"]);

        assert!(sweep(&root).is_empty());
        drop(std::fs::remove_dir_all(&root));
    }

    #[test]
    fn an_unparsable_snapshot_suppresses_the_whole_sweep() {
        let root = scratch_dir("corrupt");
        let window = WindowId::new();

        write_envelope(&root, window, "orphan-old", 90);
        std::fs::write(root.join("windows").join("broken.toml"), "this is not = = toml")
            .expect("write corrupt snapshot");

        assert!(sweep(&root).is_empty());
        drop(std::fs::remove_dir_all(&root));
    }
}
