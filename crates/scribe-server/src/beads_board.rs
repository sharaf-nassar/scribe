//! Lightweight workspace Beads-board snapshots backed by the installed `bd` CLI.
//!
//! Direction: Constellation. Dense five-column board, sharp geometry, quiet
//! terminal-native color, compact type, and state conveyed by labels plus color.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;
use tokio::sync::Mutex;

use scribe_common::protocol::{BeadsBoardItem, BeadsBoardSnapshot, BeadsBoardState};

const CACHE_TTL: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_ITEMS_PER_QUEUE: usize = 200;
const MAX_BLOCKERS_PER_ITEM: usize = 16;
const MAX_ID_CHARS: usize = 128;
const MAX_TITLE_CHARS: usize = 512;
const SYSTEM_BD_DIRS: [&str; 9] = [
    "/opt/homebrew/bin",
    "/opt/homebrew/opt/beads/bin",
    "/home/linuxbrew/.linuxbrew/bin",
    "/home/linuxbrew/.linuxbrew/opt/beads/bin",
    "/usr/local/bin",
    "/usr/local/opt/beads/bin",
    "/usr/bin",
    "/bin",
    "/opt/local/bin",
];

/// Server-owned, per-project stale-while-revalidate cache.
#[derive(Debug, Clone, Default)]
pub struct BeadsBoardCache {
    entries: Arc<Mutex<HashMap<PathBuf, CacheEntry>>>,
}

#[derive(Debug, Default)]
struct CacheEntry {
    last_good: Option<BeadsBoardSnapshot>,
    detected: Option<bool>,
    last_attempt: Option<Instant>,
    last_error: Option<String>,
    in_flight: bool,
}

/// Result of looking up a board. `refresh` is true for exactly one concurrent
/// caller, which must invoke [`BeadsBoardCache::refresh`].
pub struct CacheLookup {
    pub state: BeadsBoardState,
    pub refresh: bool,
    pub key: PathBuf,
}

impl BeadsBoardCache {
    /// Return paintable state immediately and reserve one background refresh
    /// when this project is missing or stale.
    pub async fn lookup(&self, project_root: &Path) -> CacheLookup {
        let key = project_root.canonicalize().unwrap_or_else(|_| project_root.to_path_buf());
        let now = Instant::now();
        let mut entries = self.entries.lock().await;
        let entry = entries.entry(key.clone()).or_default();
        let stale = entry.last_attempt.is_none_or(|at| now.duration_since(at) >= CACHE_TTL);
        let refresh = stale && !entry.in_flight;
        if refresh {
            entry.in_flight = true;
        }
        CacheLookup { state: entry.state(stale), refresh, key }
    }

    /// Refresh one project previously reserved by [`Self::lookup`].
    pub async fn refresh(&self, key: PathBuf) -> BeadsBoardState {
        let result = Box::pin(load_board(&key)).await;
        let mut entries = self.entries.lock().await;
        let entry = entries.entry(key.clone()).or_default();
        entry.in_flight = false;
        entry.last_attempt = Some(Instant::now());
        match result {
            Ok(LoadResult::NotDetected) => {
                entry.detected = Some(false);
                entry.last_good = None;
                entry.last_error = None;
            }
            Ok(LoadResult::Snapshot(snapshot)) => {
                entry.detected = Some(true);
                entry.last_good = Some(snapshot);
                entry.last_error = None;
            }
            Err(error) => {
                // The board's only failure signal: a stuck workspace paints
                // nothing, so an unexplained missing board is answered here.
                // Logged on change, since this retries every thirty seconds
                // for as long as the workspace stays broken.
                if entry.last_error.as_deref() != Some(error.as_str()) {
                    tracing::warn!(%error, root = %key.display(), "Beads board refresh failed");
                }
                entry.last_error = Some(error);
            }
        }
        entry.state(false)
    }
}

impl CacheEntry {
    fn state(&self, stale: bool) -> BeadsBoardState {
        if self.detected == Some(false) {
            return BeadsBoardState::NotDetected;
        }
        if let Some(snapshot) = &self.last_good {
            return BeadsBoardState::Ready {
                snapshot: snapshot.clone(),
                stale: stale || self.last_error.is_some(),
                refresh_error: self.last_error.clone(),
            };
        }
        if self.in_flight {
            return BeadsBoardState::Loading { cached: None };
        }
        BeadsBoardState::Unavailable {
            message: self.last_error.clone().unwrap_or_else(|| "Beads board is unavailable".into()),
        }
    }
}

enum LoadResult {
    NotDetected,
    Snapshot(BeadsBoardSnapshot),
}

async fn load_board(project_root: &Path) -> Result<LoadResult, String> {
    let bd = resolve_bd_executable()?;
    match Box::pin(run_bd(&bd, project_root, &["context"])).await {
        Ok(bytes) => {
            let context: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid bd context JSON: {error}"))?;
            if !context.is_object() {
                return Err("invalid bd context JSON: expected an object".into());
            }
        }
        Err(RunError::NoProject) => return Ok(LoadResult::NotDetected),
        Err(RunError::Failed(error)) => return Err(error),
    }

    let list =
        Box::pin(run_bd(&bd, project_root, &["list", "--all", "--limit", "0", "--skip-labels"]))
            .await
            .map_err(RunError::message)?;
    let ready = Box::pin(run_bd(&bd, project_root, &["ready", "--limit", "0"]))
        .await
        .map_err(RunError::message)?;
    let blocked =
        Box::pin(run_bd(&bd, project_root, &["blocked"])).await.map_err(RunError::message)?;

    classify_snapshot(&list, &ready, &blocked).map(LoadResult::Snapshot)
}

#[derive(Debug)]
enum RunError {
    NoProject,
    Failed(String),
}

impl RunError {
    fn message(self) -> String {
        match self {
            Self::NoProject => "no Beads project found".into(),
            Self::Failed(message) => message,
        }
    }
}

/// A discovered `bd`, plus the PATH its own child lookups should see.
struct Bd {
    exe: PathBuf,
    search_path: Option<OsString>,
}

fn resolve_bd_executable() -> Result<Bd, String> {
    resolve_bd_executable_from(std::env::var_os("PATH").as_deref(), dirs::home_dir().as_deref())
        .ok_or_else(|| {
            "bd is not installed or executable from PATH or a standard user install location".into()
        })
}

fn resolve_bd_executable_from(path: Option<&OsStr>, home: Option<&Path>) -> Option<Bd> {
    let dirs = bd_search_dirs(path, home);
    // Spawn the entry as found: a mise shim is a symlink to the mise binary
    // that dispatches on argv[0], so resolving it would run mise, not bd.
    let exe =
        dirs.iter().map(|dir| dir.join("bd")).find(|candidate| is_executable_file(candidate))?;
    Some(Bd { exe, search_path: std::env::join_paths(dirs).ok() })
}

/// Whether this user may execute `candidate` — the exec bits alone would accept
/// a `bd` owned by someone else, shadowing a usable one later in the search.
fn is_executable_file(candidate: &Path) -> bool {
    fs::metadata(candidate).is_ok_and(|metadata| metadata.is_file())
        && nix::unistd::access(candidate, nix::unistd::AccessFlags::X_OK).is_ok()
}

fn bd_search_dirs(path: Option<&OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute())
        .collect::<Vec<_>>();
    if let Some(home) = home {
        dirs.extend([
            home.join(".local/bin"),
            home.join(".local/share/mise/shims"),
            home.join("go/bin"),
            home.join(".cargo/bin"),
            home.join(".linuxbrew/bin"),
        ]);
    }
    dirs.extend(SYSTEM_BD_DIRS.map(PathBuf::from));
    let mut seen = HashSet::new();
    dirs.retain(|dir| seen.insert(dir.clone()));
    dirs
}

async fn run_bd(bd: &Bd, project_root: &Path, command_args: &[&str]) -> Result<Vec<u8>, RunError> {
    let mut command = Command::new(&bd.exe);
    command
        .args(["--readonly", "--json", "--no-color", "-C"])
        .arg(project_root)
        .args(command_args)
        // `-C` does not cover everything: `bd context` resolves the repository
        // through git in the process's own directory, and the server's is `/`.
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // bd's own lookups get the same search list bd was found on, not the
    // packaged service's minimal PATH.
    if let Some(search_path) = &bd.search_path {
        command.env("PATH", search_path);
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|error| {
        RunError::Failed(if error.kind() == std::io::ErrorKind::NotFound {
            "bd disappeared after it was discovered".into()
        } else {
            format!("could not start bd: {error}")
        })
    })?;
    let pid = child.id();
    let stdout_pipe =
        child.stdout.take().ok_or_else(|| RunError::Failed("bd stdout unavailable".into()))?;
    let stderr_pipe =
        child.stderr.take().ok_or_else(|| RunError::Failed("bd stderr unavailable".into()))?;
    let completed = Box::pin(tokio::time::timeout(COMMAND_TIMEOUT, async {
        tokio::join!(
            child.wait(),
            read_bounded(stdout_pipe, MAX_STDOUT_BYTES),
            read_bounded(stderr_pipe, MAX_STDERR_BYTES),
        )
    }))
    .await;
    let Ok((status_result, stdout_result, stderr_result)) = completed else {
        kill_process_group(pid);
        drop(child.kill().await);
        drop(child.wait().await);
        return Err(RunError::Failed("bd board query timed out".into()));
    };
    let status = status_result
        .map_err(|error| RunError::Failed(format!("could not wait for bd: {error}")))?;
    let stdout_bytes = stdout_result.map_err(RunError::Failed)?;
    let stderr_bytes = stderr_result.map_err(RunError::Failed)?;

    if status.success() {
        return Ok(stdout_bytes);
    }
    // A `--json` failure is reported as `{"error": …}` on stdout with nothing on
    // stderr, so reading only stderr turns every one of them into a bare status.
    let stderr_text = String::from_utf8_lossy(&stderr_bytes).trim().to_owned();
    let detail = if stderr_text.is_empty() { json_error(&stdout_bytes) } else { stderr_text };
    if detail.contains("no beads project found") {
        Err(RunError::NoProject)
    } else {
        Err(RunError::Failed(if detail.is_empty() {
            format!("bd exited with {status}")
        } else {
            format!("bd failed: {detail}")
        }))
    }
}

fn json_error(stdout: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .and_then(|value| value.get("error").and_then(serde_json::Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin, cap: usize) -> Result<Vec<u8>, String> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0_usize;
    loop {
        let read = reader.read(&mut buffer).await.map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        if kept.len() < cap {
            let keep = read.min(cap - kept.len());
            if let Some(chunk) = buffer.get(..keep) {
                kept.extend_from_slice(chunk);
            }
        }
    }
    if total > cap { Err(format!("bd output exceeded {cap} bytes")) } else { Ok(kept) }
}

#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok())
        && let Err(error) = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-pid),
            nix::sys::signal::Signal::SIGKILL,
        )
    {
        tracing::debug!(%error, "bd process group was already gone");
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>) {}

/// The three shapes `bd --json` answers a query with: `list`'s object today,
/// the bare array `ready` and `blocked` return today, and the
/// `{"data": …, "schema_version": …}` envelope bd 1.x announces on every run
/// as v2.0's default and already serves under `BD_JSON_ENVELOPE=1`. Accepting
/// the envelope now costs one variant and keeps a bd upgrade from silently
/// turning every board into a parse error.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IssueCollection {
    Envelope { issues: Vec<IssueJson> },
    Versioned { data: Vec<IssueJson> },
    Array(Vec<IssueJson>),
}

impl IssueCollection {
    fn into_issues(self) -> Vec<IssueJson> {
        match self {
            Self::Envelope { issues } | Self::Versioned { data: issues } | Self::Array(issues) => {
                issues
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct IssueJson {
    id: String,
    title: String,
    status: String,
    priority: u8,
    #[serde(default)]
    issue_type: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    dependencies: Vec<DependencyJson>,
    #[serde(default)]
    blocked_by: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DependencyJson {
    #[serde(default)]
    depends_on_id: String,
    #[serde(rename = "type", default)]
    dependency_type: String,
}

fn classify_snapshot(
    list_json: &[u8],
    ready_json: &[u8],
    blocked_json: &[u8],
) -> Result<BeadsBoardSnapshot, String> {
    let issues = parse_collection(list_json, "list")?;
    let ready = parse_collection(ready_json, "ready")?;
    let blocked = parse_collection(blocked_json, "blocked")?;
    let ready_ids: HashSet<&str> = ready.iter().map(|issue| issue.id.as_str()).collect();
    let blocked_by_id: HashMap<&str, &[String]> =
        blocked.iter().map(|issue| (issue.id.as_str(), issue.blocked_by.as_slice())).collect();
    let epic_names: HashMap<&str, &str> = issues
        .iter()
        .filter(|issue| issue.issue_type.as_deref() == Some("epic"))
        .map(|issue| (issue.id.as_str(), issue.title.as_str()))
        .collect();
    let mut snapshot = BeadsBoardSnapshot {
        refreshed_at_epoch_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        ..BeadsBoardSnapshot::default()
    };

    for issue in &issues {
        if issue.priority > 4 {
            return Err(format!("invalid priority {} for {}", issue.priority, issue.id));
        }
        let blocked_ids = blocked_by_id.get(issue.id.as_str()).copied().unwrap_or_default();
        let parent_id = issue.parent.as_deref().or_else(|| {
            issue
                .dependencies
                .iter()
                .find(|dependency| dependency.dependency_type == "parent-child")
                .map(|dependency| dependency.depends_on_id.as_str())
        });
        let item = BeadsBoardItem {
            id: truncate(&issue.id, MAX_ID_CHARS),
            title: truncate(&issue.title, MAX_TITLE_CHARS),
            priority: issue.priority,
            blocker_ids: blocked_ids
                .iter()
                .take(MAX_BLOCKERS_PER_ITEM)
                .map(|id| truncate(id, MAX_ID_CHARS))
                .collect(),
            parent_epic_name: parent_id
                .and_then(|id| epic_names.get(id).copied())
                .map(|name| truncate(name, MAX_TITLE_CHARS)),
        };

        let (queue, total) = if issue.status == "closed" {
            (&mut snapshot.done, &mut snapshot.done_total)
        } else if issue.status == "blocked" || blocked_by_id.contains_key(issue.id.as_str()) {
            (&mut snapshot.blocked, &mut snapshot.blocked_total)
        } else if issue.status == "in_progress" {
            (&mut snapshot.in_progress, &mut snapshot.in_progress_total)
        } else if ready_ids.contains(issue.id.as_str()) {
            (&mut snapshot.ready, &mut snapshot.ready_total)
        } else {
            (&mut snapshot.backlog, &mut snapshot.backlog_total)
        };
        *total = total.saturating_add(1);
        if queue.len() < MAX_ITEMS_PER_QUEUE {
            queue.push(item);
        }
    }
    Ok(snapshot)
}

fn parse_collection(bytes: &[u8], command: &str) -> Result<Vec<IssueJson>, String> {
    serde_json::from_slice::<IssueCollection>(bytes)
        .map(IssueCollection::into_issues)
        .map_err(|error| format!("invalid bd {command} JSON: {error}"))
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn bd_search_covers_linux_macos_and_user_installs() {
        let home = Path::new("/Users/tester");
        let dirs = bd_search_dirs(Some(OsStr::new("/custom/bin::relative")), Some(home));

        assert_eq!(dirs.first().map(PathBuf::as_path), Some(Path::new("/custom/bin")));
        for expected in [
            home.join(".local/bin"),
            home.join(".local/share/mise/shims"),
            home.join("go/bin"),
            home.join(".cargo/bin"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/opt/homebrew/opt/beads/bin"),
            PathBuf::from("/home/linuxbrew/.linuxbrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ] {
            assert!(dirs.contains(&expected), "missing {}", expected.display());
        }
        assert!(!dirs.iter().any(|dir| dir == Path::new("relative") || dir.as_os_str().is_empty()));
    }

    #[test]
    fn resolves_a_runnable_shim_without_following_it_to_its_target() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let scratch = std::env::temp_dir().join(format!("scribe-bd-resolver-{nonce}"));
        let unreadable = scratch.join("not-executable");
        let foreign = scratch.join("foreign");
        let shims = scratch.join("shims");
        for dir in [&unreadable, &foreign, &shims] {
            fs::create_dir_all(dir).expect("create bin dir");
        }
        fs::write(unreadable.join("bd"), "not executable").expect("write non-executable bd");
        // Root ignores the permission bits, so only a normal user sees this skipped.
        if !nix::unistd::Uid::effective().is_root() {
            let other = foreign.join("bd");
            fs::write(&other, "#!/bin/sh\n").expect("write group-executable bd");
            fs::set_permissions(&other, fs::Permissions::from_mode(0o011))
                .expect("chmod group-executable bd");
        }
        // A mise shim: a symlink to a multi-call binary that dispatches on argv[0].
        let multicall = scratch.join("mise");
        fs::write(&multicall, "#!/bin/sh\n").expect("write multicall binary");
        fs::set_permissions(&multicall, fs::Permissions::from_mode(0o755))
            .expect("chmod multicall binary");
        let shim = shims.join("bd");
        std::os::unix::fs::symlink(&multicall, &shim).expect("link shim to multicall binary");
        let path = std::env::join_paths([&unreadable, &foreign, &shims]).expect("join test PATH");

        let resolved = resolve_bd_executable_from(Some(&path), None).expect("resolve bd");

        assert_eq!(resolved.exe, shim, "resolved past the shim to its dispatch target");
        let search_path = resolved.search_path.expect("child search path");
        let search_dirs = std::env::split_paths(&search_path).collect::<Vec<_>>();
        assert!(search_dirs.contains(&shims), "child PATH dropped the dir bd came from");
        assert!(search_dirs.contains(&PathBuf::from("/usr/bin")), "child PATH lost system dirs");
        fs::remove_dir_all(scratch).expect("remove scratch dir");
    }

    #[tokio::test]
    async fn runs_bd_in_the_project_root_and_surfaces_its_stdout_error() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let scratch = std::env::temp_dir().join(format!("scribe-bd-run-{nonce}"));
        let root = scratch.join("project");
        fs::create_dir_all(&root).expect("create project dir");
        let fake = scratch.join("bd");
        // Stands in for a `--json` failure: the reason goes to stdout, not stderr.
        fs::write(&fake, "#!/bin/sh\nprintf '{\"error\":\"ran in %s\"}' \"$(pwd)\"\nexit 1\n")
            .expect("write fake bd");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod fake bd");
        let bd = Bd { exe: fake, search_path: None };

        let error = run_bd(&bd, &root, &["context"]).await.expect_err("fake bd exits 1");

        let expected = root.canonicalize().expect("canonical project root");
        assert_eq!(error.message(), format!("bd failed: ran in {}", expected.display()));
        fs::remove_dir_all(scratch).expect("remove scratch dir");
    }

    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn classifies_each_issue_once_with_board_precedence_and_metadata() {
        let list = br#"{"issues":[
          {"id":"epic","title":"Board epic","status":"open","priority":2,"issue_type":"epic"},
          {"id":"backlog","title":"Backlog","status":"deferred","priority":4},
          {"id":"ready","title":"Ready","status":"open","priority":1,"parent":"epic"},
          {"id":"doing","title":"Doing","status":"in_progress","priority":0},
          {"id":"blocked","title":"Blocked","status":"in_progress","priority":2},
          {"id":"done","title":"Done","status":"closed","priority":3}
        ]}"#;
        let ready = br#"[{"id":"ready","title":"Ready","status":"open","priority":1},{"id":"doing","title":"Doing","status":"in_progress","priority":0}]"#;
        let blocked = br#"[{"id":"blocked","title":"Blocked","status":"in_progress","priority":2,"blocked_by":["gate-1"]},{"id":"done","title":"Done","status":"closed","priority":3,"blocked_by":["old"]}]"#;

        let board = classify_snapshot(list, ready, blocked).expect("classify board");

        assert_eq!(
            board.backlog.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            ["epic", "backlog"]
        );
        assert_eq!(board.ready[0].id, "ready");
        assert_eq!(board.ready[0].parent_epic_name.as_deref(), Some("Board epic"));
        assert_eq!(board.in_progress[0].id, "doing");
        assert_eq!(board.blocked[0].id, "blocked");
        assert_eq!(board.blocked[0].blocker_ids, ["gate-1"]);
        assert_eq!(board.done[0].id, "done");
        assert_eq!(
            [
                board.backlog_total,
                board.ready_total,
                board.in_progress_total,
                board.blocked_total,
                board.done_total,
            ],
            [2, 1, 1, 1, 1]
        );
    }

    #[test]
    fn reads_issues_from_the_versioned_envelope_bd_v2_will_default_to() {
        let list =
            br#"{"data":[{"id":"a","title":"A","status":"open","priority":2}],"schema_version":1}"#;
        let board = classify_snapshot(list, b"[]", b"[]").expect("classify enveloped list");

        assert_eq!(board.backlog.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn rejects_malformed_json_and_non_native_priority() {
        assert!(classify_snapshot(b"{}", b"[]", b"[]").is_err());
        assert!(
            classify_snapshot(
                br#"[{"id":"bad","title":"Bad","status":"open","priority":5}]"#,
                b"[]",
                b"[]",
            )
            .is_err()
        );
    }

    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[tokio::test]
    async fn cache_deduplicates_refresh_and_retains_last_good_after_error() {
        let cache = BeadsBoardCache::default();
        let root = Path::new("/tmp/scribe-beads-cache-test");
        let first = cache.lookup(root).await;
        assert!(first.refresh);
        assert!(matches!(first.state, BeadsBoardState::Loading { .. }));
        assert!(!cache.lookup(root).await.refresh);

        let snapshot = BeadsBoardSnapshot { refreshed_at_epoch_ms: 7, ..Default::default() };
        {
            let mut entries = cache.entries.lock().await;
            let entry = entries.get_mut(&first.key).expect("cache entry");
            entry.in_flight = false;
            entry.detected = Some(true);
            entry.last_good = Some(snapshot.clone());
            entry.last_error = Some("bad JSON".into());
            entry.last_attempt = Some(Instant::now());
        }
        assert!(matches!(
            cache.lookup(root).await.state,
            BeadsBoardState::Ready {
                snapshot: cached,
                stale: true,
                refresh_error: Some(_),
            } if cached == snapshot
        ));
    }
}
