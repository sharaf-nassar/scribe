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

use serde::{Deserialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;
use tokio::sync::Mutex;

use scribe_common::protocol::{
    BeadsBoardItem, BeadsBoardSnapshot, BeadsBoardState, BeadsIssueComment, BeadsIssueDetail,
    BeadsIssueLink, BeadsIssueQueue, BeadsIssueQueueBasis,
};

const CACHE_TTL: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_ITEMS_PER_QUEUE: usize = 200;
const MAX_BLOCKERS_PER_ITEM: usize = 16;
const MAX_ID_CHARS: usize = 128;
const MAX_TITLE_CHARS: usize = 512;
const MAX_DETAIL_FIELD_BYTES: usize = 64 * 1024;
const MAX_DETAIL_COMMENTS: usize = 50;
const MAX_DETAIL_COLLECTION_ITEMS: usize = 200;
const BD_JSON_SCHEMA_VERSION: u64 = 1;
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
            let context: serde_json::Value = parse_envelope(&bytes, "context")?;
            if !context.is_object() {
                return Err("invalid bd context JSON: expected an object".into());
            }
        }
        Err(RunError::NoProject) => return Ok(LoadResult::NotDetected),
        Err(error) => return Err(error.message()),
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

/// Result of a fresh issue-detail query. Missing issues stay distinct from
/// subprocess and schema failures on the typed wire response.
pub enum DetailLoadResult {
    NotFound,
    Found(Box<BeadsIssueDetail>),
}

/// Read one issue directly from `bd`; board snapshots never participate.
pub async fn load_issue_detail(
    project_root: &Path,
    issue_id: &str,
) -> Result<DetailLoadResult, String> {
    let bd = resolve_bd_executable()?;
    load_issue_detail_with(&bd, project_root, issue_id).await
}

async fn load_issue_detail_with(
    bd: &Bd,
    project_root: &Path,
    issue_id: &str,
) -> Result<DetailLoadResult, String> {
    let canonical_root = project_root
        .canonicalize()
        .map_err(|error| format!("could not resolve Beads project root: {error}"))?;
    let detail = match run_bd(
        bd,
        &canonical_root,
        &["show", issue_id, "--include-comments", "--include-dependents"],
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(RunError::IssueNotFound) => return Ok(DetailLoadResult::NotFound),
        Err(error) => return Err(error.message()),
    };
    let ready =
        run_bd(bd, &canonical_root, &["ready", "--limit", "0"]).await.map_err(RunError::message)?;
    let is_ready = parse_collection(&ready, "ready")?.iter().any(|issue| issue.id == issue_id);
    parse_issue_detail(&detail, is_ready).map(Box::new).map(DetailLoadResult::Found)
}

#[derive(Debug)]
enum RunError {
    NoProject,
    IssueNotFound,
    Failed(String),
}

impl RunError {
    fn message(self) -> String {
        match self {
            Self::NoProject => "no Beads project found".into(),
            Self::IssueNotFound => "Beads issue not found".into(),
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
        .args(["--readonly", "--json", "-C"])
        .arg(project_root)
        .args(command_args)
        // `-C` does not cover everything: `bd context` resolves the repository
        // through git in the process's own directory, and the server's is `/`.
        .current_dir(project_root)
        .env("BD_JSON_ENVELOPE", "1")
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
    let lowercase = detail.to_ascii_lowercase();
    if lowercase.contains("no beads project found") {
        Err(RunError::NoProject)
    } else if lowercase.contains("issue") && lowercase.contains("not found") {
        Err(RunError::IssueNotFound)
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

#[derive(Debug, Deserialize)]
struct JsonEnvelope {
    data: serde_json::Value,
    schema_version: u64,
}

/// The two collection payloads bd 1.1 returns under `BD_JSON_ENVELOPE=1`:
/// `list` nests its issues, while `ready` and `blocked` return arrays.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IssueCollection {
    List { issues: Vec<IssueJson> },
    Array(Vec<IssueJson>),
}

impl IssueCollection {
    fn into_issues(self) -> Vec<IssueJson> {
        match self {
            Self::List { issues } | Self::Array(issues) => issues,
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

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IssueDetailPayload {
    Issue(Box<IssueDetailJson>),
    List { issues: Vec<IssueDetailJson> },
    Array(Vec<IssueDetailJson>),
}

impl IssueDetailPayload {
    fn into_issue(self) -> Result<IssueDetailJson, String> {
        match self {
            Self::Issue(issue) => Ok(*issue),
            Self::List { issues } | Self::Array(issues) => one_detail_issue(issues),
        }
    }
}

fn one_detail_issue(mut issues: Vec<IssueDetailJson>) -> Result<IssueDetailJson, String> {
    if issues.len() != 1 {
        return Err(format!("invalid bd show JSON: expected one issue, got {}", issues.len()));
    }
    issues.pop().ok_or_else(|| "invalid bd show JSON: issue missing".into())
}

#[derive(Debug, Deserialize)]
struct IssueDetailJson {
    id: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    acceptance_criteria: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    design: String,
    #[serde(default)]
    spec_id: Option<String>,
    status: String,
    priority: u8,
    issue_type: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default, alias = "created_by")]
    owner: Option<String>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    closed_at: Option<String>,
    #[serde(default)]
    close_reason: Option<String>,
    #[serde(default)]
    defer_until: Option<String>,
    #[serde(default, alias = "due")]
    due_at: Option<String>,
    #[serde(default, alias = "estimate")]
    estimated_minutes: Option<u32>,
    #[serde(default)]
    external_ref: Option<String>,
    #[serde(default)]
    dependencies: Vec<DetailRelationJson>,
    #[serde(default)]
    blocked_by: Vec<DetailRelationValue>,
    #[serde(default)]
    dependents: Vec<DetailRelationJson>,
    #[serde(default)]
    comments: Vec<DetailCommentJson>,
}

#[derive(Debug, Deserialize)]
struct DetailRelationJson {
    #[serde(default, alias = "depends_on_id", alias = "issue_id")]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, alias = "type")]
    dependency_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DetailRelationValue {
    Id(String),
    Relation(DetailRelationJson),
}

impl DetailRelationValue {
    fn into_relation(self) -> DetailRelationJson {
        match self {
            Self::Id(id) => DetailRelationJson {
                id: id.clone(),
                title: id,
                status: None,
                dependency_type: "blocks".into(),
            },
            Self::Relation(relation) => relation,
        }
    }
}

#[derive(Debug, Deserialize)]
struct DetailCommentJson {
    #[serde(default)]
    author: String,
    #[serde(default)]
    created_at: String,
    #[serde(default, alias = "body")]
    text: String,
}

fn parse_issue_detail(bytes: &[u8], ready: bool) -> Result<BeadsIssueDetail, String> {
    let issue = parse_envelope::<IssueDetailPayload>(bytes, "show")?.into_issue()?;
    if issue.priority > 4 {
        return Err(format!("invalid priority {} for {}", issue.priority, issue.id));
    }

    let has_open_blockers = !issue.blocked_by.is_empty()
        || issue.dependencies.iter().any(|dependency| {
            dependency.dependency_type == "blocks" && dependency.status.as_deref() != Some("closed")
        });
    let (queue, queue_basis) =
        classify_issue(&issue.status, QueueSignals { has_open_blockers, ready });

    let blockers = bounded_links(
        issue
            .dependencies
            .into_iter()
            .filter(|dependency| dependency.dependency_type == "blocks")
            .chain(issue.blocked_by.into_iter().map(DetailRelationValue::into_relation)),
    );
    let dependents = bounded_links(issue.dependents.into_iter().filter(|dependent| {
        dependent.dependency_type.is_empty() || dependent.dependency_type == "blocks"
    }));
    let hidden_comment_count = issue.comments.len().saturating_sub(MAX_DETAIL_COMMENTS);
    let comments = issue
        .comments
        .into_iter()
        .rev()
        .take(MAX_DETAIL_COMMENTS)
        .map(|comment| BeadsIssueComment {
            author: truncate_bytes(&comment.author, MAX_TITLE_CHARS),
            created_at: truncate_bytes(&comment.created_at, MAX_TITLE_CHARS),
            body: truncate_bytes(&comment.text, MAX_DETAIL_FIELD_BYTES),
        })
        .collect();

    Ok(BeadsIssueDetail {
        id: truncate_bytes(&issue.id, MAX_ID_CHARS),
        title: truncate_bytes(&issue.title, MAX_TITLE_CHARS),
        description: truncate_bytes(&issue.description, MAX_DETAIL_FIELD_BYTES),
        acceptance_criteria: truncate_bytes(&issue.acceptance_criteria, MAX_DETAIL_FIELD_BYTES),
        notes: truncate_bytes(&issue.notes, MAX_DETAIL_FIELD_BYTES),
        design: truncate_bytes(&issue.design, MAX_DETAIL_FIELD_BYTES),
        spec_id: bounded_option(issue.spec_id, MAX_TITLE_CHARS),
        status: truncate_bytes(&issue.status, MAX_ID_CHARS),
        priority: issue.priority,
        issue_type: truncate_bytes(&issue.issue_type, MAX_ID_CHARS),
        labels: issue
            .labels
            .into_iter()
            .take(MAX_DETAIL_COLLECTION_ITEMS)
            .map(|label| truncate_bytes(&label, MAX_TITLE_CHARS))
            .collect(),
        assignee: bounded_option(issue.assignee, MAX_TITLE_CHARS),
        owner: bounded_option(issue.owner, MAX_TITLE_CHARS),
        created_at: truncate_bytes(&issue.created_at, MAX_TITLE_CHARS),
        updated_at: truncate_bytes(&issue.updated_at, MAX_TITLE_CHARS),
        closed_at: bounded_option(issue.closed_at, MAX_TITLE_CHARS),
        close_reason: bounded_option(issue.close_reason, MAX_DETAIL_FIELD_BYTES),
        defer_until: bounded_option(issue.defer_until, MAX_TITLE_CHARS),
        due_at: bounded_option(issue.due_at, MAX_TITLE_CHARS),
        estimated_minutes: issue.estimated_minutes,
        external_ref: bounded_option(issue.external_ref, MAX_DETAIL_FIELD_BYTES),
        blockers,
        dependents,
        comments,
        hidden_comment_count: hidden_comment_count.try_into().unwrap_or(u32::MAX),
        queue,
        queue_basis,
    })
}

fn bounded_links(relations: impl Iterator<Item = DetailRelationJson>) -> Vec<BeadsIssueLink> {
    let mut seen = HashSet::new();
    relations
        .filter_map(|relation| {
            let id = truncate_bytes(&relation.id, MAX_ID_CHARS);
            if id.is_empty() || !seen.insert(id.clone()) {
                return None;
            }
            let title = if relation.title.is_empty() { id.clone() } else { relation.title };
            Some(BeadsIssueLink { id, title: truncate_bytes(&title, MAX_TITLE_CHARS) })
        })
        .take(MAX_DETAIL_COLLECTION_ITEMS)
        .collect()
}

fn bounded_option(value: Option<String>, cap: usize) -> Option<String> {
    value.map(|value| truncate_bytes(&value, cap))
}

#[derive(Clone, Copy)]
struct QueueSignals {
    has_open_blockers: bool,
    ready: bool,
}

fn classify_issue(status: &str, signals: QueueSignals) -> (BeadsIssueQueue, BeadsIssueQueueBasis) {
    if status == "closed" {
        (BeadsIssueQueue::Done, BeadsIssueQueueBasis::ClosedStatus)
    } else if status == "blocked" {
        (BeadsIssueQueue::Blocked, BeadsIssueQueueBasis::BlockedStatus)
    } else if signals.has_open_blockers {
        (BeadsIssueQueue::Blocked, BeadsIssueQueueBasis::OpenBlockers)
    } else if status == "in_progress" {
        (BeadsIssueQueue::InProgress, BeadsIssueQueueBasis::InProgressStatus)
    } else if signals.ready {
        (BeadsIssueQueue::Ready, BeadsIssueQueueBasis::ReadySet)
    } else {
        (BeadsIssueQueue::Backlog, BeadsIssueQueueBasis::BacklogFallback)
    }
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

        let (queue, total) = match classify_issue(
            &issue.status,
            QueueSignals {
                has_open_blockers: blocked_by_id.contains_key(issue.id.as_str()),
                ready: ready_ids.contains(issue.id.as_str()),
            },
        )
        .0
        {
            BeadsIssueQueue::Done => (&mut snapshot.done, &mut snapshot.done_total),
            BeadsIssueQueue::Blocked => (&mut snapshot.blocked, &mut snapshot.blocked_total),
            BeadsIssueQueue::InProgress => {
                (&mut snapshot.in_progress, &mut snapshot.in_progress_total)
            }
            BeadsIssueQueue::Ready => (&mut snapshot.ready, &mut snapshot.ready_total),
            BeadsIssueQueue::Backlog => (&mut snapshot.backlog, &mut snapshot.backlog_total),
        };
        *total = total.saturating_add(1);
        if queue.len() < MAX_ITEMS_PER_QUEUE {
            queue.push(item);
        }
    }
    Ok(snapshot)
}

fn parse_collection(bytes: &[u8], command: &str) -> Result<Vec<IssueJson>, String> {
    parse_envelope::<IssueCollection>(bytes, command).map(IssueCollection::into_issues)
}

fn parse_envelope<T: DeserializeOwned>(bytes: &[u8], command: &str) -> Result<T, String> {
    let envelope = serde_json::from_slice::<JsonEnvelope>(bytes)
        .map_err(|error| format!("invalid bd {command} JSON: {error}"))?;
    if envelope.schema_version != BD_JSON_SCHEMA_VERSION {
        return Err(format!(
            "unsupported bd {command} JSON schema version {} (expected {BD_JSON_SCHEMA_VERSION})",
            envelope.schema_version
        ));
    }
    serde_json::from_value(envelope.data)
        .map_err(|error| format!("invalid bd {command} JSON: {error}"))
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    use scribe_common::protocol::{BeadsIssueQueue, BeadsIssueQueueBasis};

    use super::*;

    static NEXT_SCRATCH_ID: AtomicU64 = AtomicU64::new(0);

    fn beads_test_scratch_path(label: &str) -> PathBuf {
        let id = NEXT_SCRATCH_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("scribe-bd-{label}-{}-{id}", std::process::id()))
    }

    #[test]
    fn bd_test_scratch_paths_are_unique_within_a_process() {
        assert_ne!(beads_test_scratch_path("unique"), beads_test_scratch_path("unique"));
    }

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
        let scratch = beads_test_scratch_path("resolver");
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
    async fn runs_bd_in_the_project_root_with_versioned_json_and_surfaces_its_stdout_error() {
        // Stands in for a `--json` failure: the reason goes to stdout, not stderr.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let fake = root.join("tests/fixtures/bd-stdout-error.sh");
        let bd = Bd { exe: fake, search_path: None };

        let error = run_bd(&bd, root, &["context"]).await.expect_err("fake bd exits 1");

        let expected = root.canonicalize().expect("canonical project root");
        assert_eq!(
            error.message(),
            format!(
                "bd failed: envelope 1, ran in {}, args --readonly --json -C {} context",
                expected.display(),
                root.display()
            )
        );
    }

    #[tokio::test]
    async fn recognizes_the_current_no_project_error() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let fake = root.join("tests/fixtures/bd-no-project.sh");
        let bd = Bd { exe: fake, search_path: None };

        let error = run_bd(&bd, root, &["context"]).await.expect_err("fake bd exits 1");

        assert!(matches!(error, RunError::NoProject));
    }

    fn detail_envelope(data: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "data": data,
            "schema_version": 1,
        }))
        .expect("serialize detail fixture")
    }

    fn detail_issue() -> serde_json::Value {
        serde_json::json!({
            "id": "scribe-42",
            "title": "Issue detail",
            "description": "Description",
            "acceptance_criteria": "Acceptance",
            "notes": "Notes",
            "design": "Design",
            "spec_id": "024",
            "status": "open",
            "priority": 2,
            "issue_type": "task",
            "labels": ["server"],
            "assignee": "mamba",
            "owner": "owner",
            "created_at": "2026-08-14T10:00:00Z",
            "updated_at": "2026-08-15T10:00:00Z",
            "dependencies": [{
                "depends_on_id": "gate",
                "title": "Gate",
                "status": "closed",
                "dependency_type": "blocks"
            }],
            "dependents": [{"id": "child", "title": "Child"}],
            "comments": []
        })
    }

    #[test]
    fn reads_detail_from_each_supported_envelope_shape() {
        let issue = detail_issue();
        let shapes = [
            issue.clone(),
            serde_json::json!([issue.clone()]),
            serde_json::json!({"issues": [issue]}),
        ];

        for data in shapes {
            let detail = parse_issue_detail(&detail_envelope(&data), false).expect("parse detail");
            assert_eq!(detail.id, "scribe-42");
            assert_eq!(detail.dependents[0].id, "child");
        }
    }

    #[test]
    fn bounds_fields_and_keeps_the_newest_fifty_comments() {
        let comments = (0..52)
            .map(|index| {
                serde_json::json!({
                    "author": format!("author-{index}"),
                    "created_at": format!("2026-08-15T10:00:{index:02}Z"),
                    "text": if index == 51 { "é".repeat(40_000) } else { format!("body-{index}") },
                })
            })
            .collect::<Vec<_>>();
        let mut issue = detail_issue();
        issue["description"] = serde_json::Value::String("d".repeat(70_000));
        issue["comments"] = serde_json::Value::Array(comments);

        let detail =
            parse_issue_detail(&detail_envelope(&issue), false).expect("parse bounded detail");

        assert_eq!(detail.comments.len(), 50);
        assert_eq!(detail.hidden_comment_count, 2);
        assert_eq!(detail.comments[0].author, "author-51");
        assert_eq!(detail.comments[49].author, "author-2");
        assert!(detail.description.len() <= 64 * 1024);
        assert!(detail.comments[0].body.len() <= 64 * 1024);
        assert!(detail.comments[0].body.chars().all(|character| character == 'é'));
    }

    #[test]
    fn derives_every_queue_with_snapshot_precedence() {
        let cases = [
            ("closed", true, true, (BeadsIssueQueue::Done, BeadsIssueQueueBasis::ClosedStatus)),
            (
                "blocked",
                false,
                false,
                (BeadsIssueQueue::Blocked, BeadsIssueQueueBasis::BlockedStatus),
            ),
            ("open", true, true, (BeadsIssueQueue::Blocked, BeadsIssueQueueBasis::OpenBlockers)),
            (
                "in_progress",
                false,
                true,
                (BeadsIssueQueue::InProgress, BeadsIssueQueueBasis::InProgressStatus),
            ),
            ("open", false, true, (BeadsIssueQueue::Ready, BeadsIssueQueueBasis::ReadySet)),
            (
                "deferred",
                false,
                false,
                (BeadsIssueQueue::Backlog, BeadsIssueQueueBasis::BacklogFallback),
            ),
        ];

        for (status, open_blockers, ready, expected) in cases {
            assert_eq!(
                classify_issue(status, QueueSignals { has_open_blockers: open_blockers, ready }),
                expected
            );
        }
    }

    #[tokio::test]
    async fn maps_a_missing_issue_to_typed_not_found() {
        let scratch = beads_test_scratch_path("detail-not-found");
        fs::create_dir_all(&scratch).expect("create scratch root");
        let fake = scratch.join("bd");
        fs::write(&fake, "#!/bin/sh\nprintf '%s' '{\"error\":\"issue gone not found\"}'\nexit 1\n")
            .expect("write fake bd");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod fake bd");
        let bd = Bd { exe: fake, search_path: None };

        let result = load_issue_detail_with(&bd, &scratch, "gone").await.expect("typed result");

        assert!(matches!(result, DetailLoadResult::NotFound));
        fs::remove_dir_all(scratch).expect("remove scratch root");
    }

    #[tokio::test]
    async fn detail_reads_bypass_snapshot_cache_and_use_exact_argv() {
        let scratch = beads_test_scratch_path("detail-uncached");
        fs::create_dir_all(&scratch).expect("create scratch root");
        let fake = scratch.join("bd");
        fs::write(
            &fake,
            r#"#!/bin/sh
if [ "$*" = "--readonly --json -C $PWD ready --limit 0" ]; then
  printf '%s' '{"data":[{"id":"issue","title":"Issue","status":"open","priority":2}],"schema_version":1}'
  exit
fi
expected="--readonly --json -C $PWD show issue --include-comments --include-dependents"
[ "$*" = "$expected" ] || { printf '%s' '{"error":"wrong argv"}'; exit 1; }
calls=0
[ ! -f calls ] || calls=$(sed -n '1p' calls)
calls=$((calls + 1))
printf '%s\n' "$calls" > calls
printf '{"data":{"id":"issue","title":"call %s","status":"open","priority":2,"issue_type":"task","created_at":"now","updated_at":"now"},"schema_version":1}' "$calls"
"#,
        )
        .expect("write fake bd");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod fake bd");
        let bd = Bd { exe: fake, search_path: None };

        let first = load_issue_detail_with(&bd, &scratch, "issue").await.expect("first detail");
        let second = load_issue_detail_with(&bd, &scratch, "issue").await.expect("second detail");

        let DetailLoadResult::Found(first) = first else { panic!("first issue missing") };
        let DetailLoadResult::Found(second) = second else { panic!("second issue missing") };
        assert_eq!(first.title, "call 1");
        assert_eq!(second.title, "call 2", "detail was reused from a cache");
        fs::remove_dir_all(scratch).expect("remove scratch root");
    }

    #[tokio::test]
    async fn detail_queue_uses_fresh_ready_membership_for_open_p4_issues() {
        let scratch = beads_test_scratch_path("detail-ready-membership");
        fs::create_dir_all(&scratch).expect("create scratch root");
        let fake = scratch.join("bd");
        fs::write(
            &fake,
            r#"#!/bin/sh
case "$*" in
  "--readonly --json -C $PWD show backlog --include-comments --include-dependents")
    printf '%s' '{"data":{"id":"backlog","title":"Backlog","status":"open","priority":4,"issue_type":"task","created_at":"now","updated_at":"now"},"schema_version":1}' ;;
  "--readonly --json -C $PWD show ready --include-comments --include-dependents")
    printf '%s' '{"data":{"id":"ready","title":"Ready","status":"open","priority":4,"issue_type":"task","created_at":"now","updated_at":"now"},"schema_version":1}' ;;
  "--readonly --json -C $PWD ready --limit 0")
    printf '%s' '{"data":[{"id":"ready","title":"Ready","status":"open","priority":4}],"schema_version":1}' ;;
  *) printf '%s' '{"error":"wrong argv"}'; exit 1 ;;
esac
"#,
        )
        .expect("write fake bd");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod fake bd");
        let bd = Bd { exe: fake, search_path: None };

        let backlog =
            load_issue_detail_with(&bd, &scratch, "backlog").await.expect("backlog detail");
        let ready = load_issue_detail_with(&bd, &scratch, "ready").await.expect("ready detail");

        let DetailLoadResult::Found(backlog) = backlog else { panic!("backlog issue missing") };
        let DetailLoadResult::Found(ready) = ready else { panic!("ready issue missing") };
        assert_eq!(backlog.queue, BeadsIssueQueue::Backlog);
        assert_eq!(backlog.queue_basis, BeadsIssueQueueBasis::BacklogFallback);
        assert_eq!(ready.queue, BeadsIssueQueue::Ready);
        assert_eq!(ready.queue_basis, BeadsIssueQueueBasis::ReadySet);
        fs::remove_dir_all(scratch).expect("remove scratch root");
    }

    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn classifies_each_issue_once_with_board_precedence_and_metadata() {
        let list = br#"{"data":{"issues":[
          {"id":"epic","title":"Board epic","status":"open","priority":2,"issue_type":"epic"},
          {"id":"backlog","title":"Backlog","status":"deferred","priority":4},
          {"id":"ready","title":"Ready","status":"open","priority":1,"parent":"epic"},
          {"id":"doing","title":"Doing","status":"in_progress","priority":0},
          {"id":"blocked","title":"Blocked","status":"in_progress","priority":2},
          {"id":"done","title":"Done","status":"closed","priority":3}
        ]},"schema_version":1}"#;
        let ready = br#"{"data":[{"id":"ready","title":"Ready","status":"open","priority":1},{"id":"doing","title":"Doing","status":"in_progress","priority":0}],"schema_version":1}"#;
        let blocked = br#"{"data":[{"id":"blocked","title":"Blocked","status":"in_progress","priority":2,"blocked_by":["gate-1"]},{"id":"done","title":"Done","status":"closed","priority":3,"blocked_by":["old"]}],"schema_version":1}"#;

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
    fn reads_both_collection_shapes_from_the_versioned_envelope() {
        let list = br#"{"data":{"issues":[{"id":"a","title":"A","status":"open","priority":2}]},"schema_version":1}"#;
        let empty = br#"{"data":[],"schema_version":1}"#;
        let board = classify_snapshot(list, empty, empty).expect("classify enveloped list");

        assert_eq!(board.backlog.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn rejects_unknown_schema_malformed_json_and_non_native_priority() {
        let empty = br#"{"data":[],"schema_version":1}"#;
        let error = classify_snapshot(
            br#"{"data":{"renamed_in_future":[]},"schema_version":2}"#,
            empty,
            empty,
        )
        .expect_err("future schema must fail explicitly");
        assert_eq!(error, "unsupported bd list JSON schema version 2 (expected 1)");
        assert!(classify_snapshot(b"{}", empty, empty).is_err());
        assert!(
            classify_snapshot(
                br#"{"data":{"issues":[{"id":"bad","title":"Bad","status":"open","priority":5}]},"schema_version":1}"#,
                empty,
                empty,
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
