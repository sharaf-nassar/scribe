//! Lightweight workspace Beads-board snapshots backed by the installed `bd` CLI.
//!
//! Direction: Constellation. Dense five-column board, sharp geometry, quiet
//! terminal-native color, compact type, and state conveyed by labels plus color.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;
use tokio::sync::Mutex;

use scribe_common::protocol::{
    BeadsBoardItem, BeadsBoardSnapshot, BeadsBoardState, BeadsEpicGraph, BeadsEpicGraphOutcome,
    BeadsEpicGraphRefusal, BeadsGraphEdge, BeadsGraphNode, BeadsIssueComment, BeadsIssueDetail,
    BeadsIssueLink, BeadsIssueQueue, BeadsIssueQueueBasis, BeadsIssueWrite, BeadsIssueWriteGuards,
    BeadsIssueWriteResult,
};

const CACHE_TTL: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_ITEMS_PER_QUEUE: usize = 200;
const MAX_FLOW_NODES: usize = 200;
const MAX_FLOW_EDGES_PER_NODE: usize = 16;
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
    last_good: Option<CachedBoard>,
    detected: Option<bool>,
    last_attempt: Option<Instant>,
    last_error: Option<String>,
    in_flight: bool,
    generation: u64,
    /// Generation that produced `last_good`'s retained list source. A write
    /// advances `generation` before its authoritative refresh, fencing Flow
    /// requests off the pre-write graph during that interval.
    source_generation: Option<u64>,
}

pub struct BeadsIssueWriteOutcome {
    pub result: BeadsIssueWriteResult,
    pub lock: Option<File>,
}

struct WriteIssueRequest<'a> {
    bd: &'a Bd,
    lock_tmp: &'a Path,
    canonical_root: &'a Path,
    issue_id: &'a str,
    verb: &'a BeadsIssueWrite,
    guards: &'a BeadsIssueWriteGuards,
    timeout: Duration,
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
        let generation = self.entries.lock().await.entry(key.clone()).or_default().generation;
        let result = Box::pin(load_board(&key)).await;
        let mut entries = self.entries.lock().await;
        let entry = entries.entry(key.clone()).or_default();
        entry.in_flight = false;
        apply_refresh_if_current(entry, generation, result, &key);
        entry.state(false)
    }

    /// Return the current cache generation and full parsed list for Flow graph
    /// assembly. It is deliberately a cache read, never a second `bd` query.
    pub async fn graph_source(
        &self,
        project_root: &Path,
    ) -> Option<(u64, Vec<BeadsGraphSourceIssue>)> {
        let key = project_root.canonicalize().unwrap_or_else(|_| project_root.to_path_buf());
        let entries = self.entries.lock().await;
        let entry = entries.get(&key)?;
        if entry.source_generation != Some(entry.generation) {
            return None;
        }
        let board = entry.last_good.as_ref()?;
        let source =
            board.issues.iter().map(|issue| graph_source_issue(issue, &board.ready_ids)).collect();
        Some((entry.generation, source))
    }

    /// Assemble one admitted Flow graph from the cached list, never another `bd` query.
    pub async fn epic_graph(&self, project_root: &Path, epic_id: &str) -> BeadsEpicGraphOutcome {
        let Some((_, source)) = self.graph_source(project_root).await else {
            return BeadsEpicGraphOutcome::Unavailable {
                message: "Beads board data is not available yet".into(),
            };
        };
        assemble_epic_graph(&source, epic_id)
    }

    /// Report whether an official bd executable is installed for this process.
    pub fn write_available() -> bool {
        write_available_from(std::env::var_os("PATH").as_deref(), dirs::home_dir().as_deref())
    }

    /// Execute one typed write and advance this root's generation only after
    /// bd confirms a versioned success envelope.
    // @lat: [[client#Client#Beads Board CLI Data Source#Guarded issue writes]]
    pub async fn write_issue(
        &self,
        project_root: &Path,
        issue_id: &str,
        verb: &BeadsIssueWrite,
        guards: &BeadsIssueWriteGuards,
    ) -> BeadsIssueWriteOutcome {
        let started = Instant::now();
        let canonical_root = match project_root.canonicalize() {
            Ok(root) => root,
            Err(error) => {
                return BeadsIssueWriteOutcome {
                    result: BeadsIssueWriteResult::Failed {
                        reason: format!("could not resolve Beads project root: {error}"),
                    },
                    lock: None,
                };
            }
        };
        let outcome = match resolve_bd_executable() {
            Ok(bd) => {
                self.write_issue_with_bd(WriteIssueRequest {
                    bd: &bd,
                    lock_tmp: Path::new("/tmp"),
                    canonical_root: &canonical_root,
                    issue_id,
                    verb,
                    guards,
                    timeout: WRITE_TIMEOUT,
                })
                .await
            }
            Err(reason) => BeadsIssueWriteOutcome {
                result: BeadsIssueWriteResult::Failed { reason },
                lock: None,
            },
        };
        let outcome_name = match &outcome.result {
            BeadsIssueWriteResult::Applied { .. } => "applied",
            BeadsIssueWriteResult::PreconditionFailed => "precondition_failed",
            BeadsIssueWriteResult::Failed { .. } => "failed",
        };
        let generation = match &outcome.result {
            BeadsIssueWriteResult::Applied { generation } => Some(*generation),
            _ => None,
        };
        tracing::info!(
            root = %canonical_root.display(),
            %issue_id,
            verb = write_verb_name(verb),
            ?generation,
            outcome = outcome_name,
            elapsed_ms = started.elapsed().as_millis(),
            "Beads issue write finished"
        );
        outcome
    }

    async fn write_issue_with_bd(&self, request: WriteIssueRequest<'_>) -> BeadsIssueWriteOutcome {
        let argv = match compose_write_argv(request.issue_id, request.verb) {
            Ok(argv) => argv,
            Err(reason) => {
                return BeadsIssueWriteOutcome {
                    result: BeadsIssueWriteResult::Failed { reason },
                    lock: None,
                };
            }
        };
        let lock = match acquire_project_write_lock_at(
            request.lock_tmp,
            scribe_common::socket::current_uid(),
            request.canonical_root,
        )
        .await
        {
            Ok(lock) => lock,
            Err(reason) => {
                return BeadsIssueWriteOutcome {
                    result: BeadsIssueWriteResult::Failed { reason },
                    lock: None,
                };
            }
        };
        let result = match fresh_issue_matches_guards(
            request.bd,
            request.canonical_root,
            request.issue_id,
            request.guards,
        )
        .await
        {
            Ok(true) => {
                run_bd_write(request.bd, request.canonical_root, &argv, request.timeout).await
            }
            Ok(false) => Err(WriteError::PreconditionFailed),
            Err(reason) => Err(WriteError::Failed(reason)),
        };
        match result {
            Ok(()) => {
                let mut entries = self.entries.lock().await;
                let entry = entries.entry(request.canonical_root.to_path_buf()).or_default();
                entry.generation = entry.generation.wrapping_add(1);
                BeadsIssueWriteOutcome {
                    result: BeadsIssueWriteResult::Applied { generation: entry.generation },
                    lock: Some(lock),
                }
            }
            Err(WriteError::PreconditionFailed) => BeadsIssueWriteOutcome {
                result: BeadsIssueWriteResult::PreconditionFailed,
                lock: None,
            },
            Err(WriteError::Failed(reason)) => BeadsIssueWriteOutcome {
                result: BeadsIssueWriteResult::Failed { reason },
                lock: None,
            },
        }
    }

    /// Force the authoritative post-write board load. A newer committed write
    /// fences this result out before it can replace the cache's last-good data.
    pub async fn refresh_after_write(
        &self,
        key: PathBuf,
        generation: u64,
        _lock: File,
    ) -> BeadsBoardState {
        let result = Box::pin(load_board(&key)).await;
        let mut entries = self.entries.lock().await;
        let entry = entries.entry(key.clone()).or_default();
        apply_refresh_if_current(entry, generation, result, &key);
        entry.state(false)
    }
}

fn apply_refresh_if_current(
    entry: &mut CacheEntry,
    generation: u64,
    result: Result<LoadResult, String>,
    key: &Path,
) -> bool {
    if entry.generation != generation {
        return false;
    }
    let refreshed_source = matches!(&result, Ok(LoadResult::Snapshot(_)));
    apply_refresh_result(entry, result, key);
    if refreshed_source {
        entry.source_generation = Some(generation);
    }
    true
}

fn apply_refresh_result(entry: &mut CacheEntry, result: Result<LoadResult, String>, key: &Path) {
    entry.last_attempt = Some(Instant::now());
    match result {
        Ok(LoadResult::NotDetected) => {
            entry.detected = Some(false);
            entry.last_good = None;
            entry.last_error = None;
            entry.source_generation = None;
        }
        Ok(LoadResult::Snapshot(board)) => {
            entry.detected = Some(true);
            entry.last_good = Some(*board);
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
}

impl CacheEntry {
    fn state(&self, stale: bool) -> BeadsBoardState {
        if self.detected == Some(false) {
            return BeadsBoardState::NotDetected;
        }
        if let Some(board) = &self.last_good {
            return BeadsBoardState::Ready {
                snapshot: board.snapshot.clone(),
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
    Snapshot(Box<CachedBoard>),
}

/// One coherent board refresh: paintable queues plus every list issue needed
/// to assemble a Flow graph without another `bd` invocation.
#[derive(Debug)]
struct CachedBoard {
    snapshot: BeadsBoardSnapshot,
    issues: Vec<IssueJson>,
    ready_ids: HashSet<String>,
}

/// Raw list data retained alongside a board snapshot for Flow assembly.
///
/// This remains server-local rather than crossing the protocol; assembly applies
/// its own graph bounds before producing the smaller `BeadsEpicGraph` wire type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeadsGraphSourceIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: u8,
    pub issue_type: Option<String>,
    pub parent: Option<String>,
    pub dependencies: Vec<BeadsGraphSourceDependency>,
    pub assignee: Option<String>,
    pub updated_at: String,
    /// The same `bd ready` membership that classifies an unblocked open card.
    pub ready: bool,
}

/// One typed dependency from the cached `bd list` issue record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeadsGraphSourceDependency {
    pub depends_on_id: String,
    pub dependency_type: String,
}

/// Assemble a complete epic graph from the retained list source.
///
/// The board's paint cap never participates here. A refusal is intentional and
/// typed: Flow has no partial or degenerate rendering path.
pub fn assemble_epic_graph(
    issues: &[BeadsGraphSourceIssue],
    epic_id: &str,
) -> BeadsEpicGraphOutcome {
    let Some(epic) = issues
        .iter()
        .find(|issue| issue.id == epic_id && issue.issue_type.as_deref() == Some("epic"))
    else {
        return refused_graph(BeadsEpicGraphRefusal::NoEpic);
    };
    let members = epic_members(issues, epic_id);
    if members.is_empty() {
        return refused_graph(BeadsEpicGraphRefusal::NoEpic);
    }
    if members.len() > MAX_FLOW_NODES {
        return refused_graph(BeadsEpicGraphRefusal::TooLarge);
    }

    let member_ids = members.iter().map(|issue| issue.id.as_str()).collect::<HashSet<_>>();
    let edges = match epic_edges(&members, &member_ids) {
        Ok(edges) => edges,
        Err(reason) => return refused_graph(reason),
    };
    if graph_has_cycle(&member_ids, &edges) {
        return refused_graph(BeadsEpicGraphRefusal::Cycle);
    }
    if graph_is_disconnected(&member_ids, &edges) {
        return refused_graph(BeadsEpicGraphRefusal::Disconnected);
    }

    let by_id = issues.iter().map(|issue| (issue.id.as_str(), issue)).collect::<HashMap<_, _>>();
    BeadsEpicGraphOutcome::Graph(Box::new(BeadsEpicGraph {
        epic_id: truncate(&epic.id, MAX_ID_CHARS),
        epic_title: truncate(&epic.title, MAX_TITLE_CHARS),
        closed: members
            .iter()
            .filter(|issue| issue.status == "closed")
            .count()
            .try_into()
            .unwrap_or(u32::MAX),
        total: members.len().try_into().unwrap_or(u32::MAX),
        nodes: members.iter().map(|issue| graph_node(issue, &by_id)).collect(),
        edges: edges
            .into_iter()
            .map(|edge| BeadsGraphEdge {
                from: truncate(&edge.from, MAX_ID_CHARS),
                to: truncate(&edge.to, MAX_ID_CHARS),
            })
            .collect(),
    }))
}

fn refused_graph(reason: BeadsEpicGraphRefusal) -> BeadsEpicGraphOutcome {
    BeadsEpicGraphOutcome::NoGraph { reason }
}

fn epic_members<'a>(
    issues: &'a [BeadsGraphSourceIssue],
    epic_id: &str,
) -> Vec<&'a BeadsGraphSourceIssue> {
    issues.iter().filter(|issue| is_epic_member(issue, epic_id)).collect()
}

fn is_epic_member(issue: &BeadsGraphSourceIssue, epic_id: &str) -> bool {
    issue.parent.as_deref() == Some(epic_id)
        || issue.dependencies.iter().any(|dependency| {
            dependency.dependency_type == "parent-child" && dependency.depends_on_id == epic_id
        })
}

fn epic_edges(
    members: &[&BeadsGraphSourceIssue],
    member_ids: &HashSet<&str>,
) -> Result<Vec<BeadsGraphEdge>, BeadsEpicGraphRefusal> {
    let mut edges = Vec::new();
    let mut seen = HashSet::new();
    for member in members {
        let blockers = member
            .dependencies
            .iter()
            .filter(|dependency| dependency.dependency_type == "blocks")
            .collect::<Vec<_>>();
        if blockers.len() > MAX_FLOW_EDGES_PER_NODE {
            return Err(BeadsEpicGraphRefusal::TooLarge);
        }
        for blocker in blockers {
            if !member_ids.contains(blocker.depends_on_id.as_str()) {
                return Err(BeadsEpicGraphRefusal::ExternalBlocker);
            }
            if seen.insert((blocker.depends_on_id.as_str(), member.id.as_str())) {
                edges.push(BeadsGraphEdge {
                    from: blocker.depends_on_id.clone(),
                    to: member.id.clone(),
                });
            }
        }
    }
    Ok(edges)
}

fn graph_is_disconnected(member_ids: &HashSet<&str>, edges: &[BeadsGraphEdge]) -> bool {
    let Some(first) = member_ids.iter().next().copied() else {
        return true;
    };
    let mut connected = HashSet::from([first]);
    let mut frontier = vec![first];
    while let Some(id) = frontier.pop() {
        for edge in edges {
            let neighbor = if edge.from == id {
                Some(edge.to.as_str())
            } else if edge.to == id {
                Some(edge.from.as_str())
            } else {
                None
            };
            if let Some(neighbor) = neighbor
                && connected.insert(neighbor)
            {
                frontier.push(neighbor);
            }
        }
    }
    connected.len() != member_ids.len() || member_ids.len() < 2
}

fn graph_has_cycle(member_ids: &HashSet<&str>, edges: &[BeadsGraphEdge]) -> bool {
    let mut indegree = member_ids.iter().map(|id| (*id, 0_usize)).collect::<HashMap<_, _>>();
    for edge in edges {
        if let Some(degree) = indegree.get_mut(edge.to.as_str()) {
            *degree += 1;
        }
    }
    let mut ready = indegree
        .iter()
        .filter_map(|(&id, &degree)| (degree == 0).then_some(id))
        .collect::<Vec<_>>();
    let mut visited = 0_usize;
    while let Some(id) = ready.pop() {
        visited += 1;
        for edge in edges.iter().filter(|edge| edge.from == id) {
            let Some(degree) = indegree.get_mut(edge.to.as_str()) else {
                return true;
            };
            *degree -= 1;
            if *degree == 0 {
                ready.push(edge.to.as_str());
            }
        }
    }
    visited != member_ids.len()
}

fn graph_node(
    issue: &BeadsGraphSourceIssue,
    by_id: &HashMap<&str, &BeadsGraphSourceIssue>,
) -> BeadsGraphNode {
    let has_open_blocker = issue.dependencies.iter().any(|dependency| {
        dependency.dependency_type == "blocks"
            && by_id
                .get(dependency.depends_on_id.as_str())
                .is_some_and(|blocker| blocker.status != "closed")
    });
    let (queue, _) = classify_issue(
        &issue.status,
        QueueSignals { has_open_blockers: has_open_blocker, ready: issue.ready },
    );
    BeadsGraphNode {
        id: truncate(&issue.id, MAX_ID_CHARS),
        title: truncate(&issue.title, MAX_TITLE_CHARS),
        priority: issue.priority,
        status: truncate(&issue.status, MAX_ID_CHARS),
        queue,
        assignee: issue.assignee.as_deref().map(|value| truncate(value, MAX_TITLE_CHARS)),
        updated_at: truncate(&issue.updated_at, MAX_TITLE_CHARS),
    }
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

    let list = Box::pin(run_bd(
        &bd,
        project_root,
        &["list", "--all", "--limit", "0", "--skip-labels", "--sort", "created"],
    ))
    .await
    .map_err(RunError::message)?;
    let ready = Box::pin(run_bd(&bd, project_root, &["ready", "--limit", "0"]))
        .await
        .map_err(RunError::message)?;
    let blocked =
        Box::pin(run_bd(&bd, project_root, &["blocked"])).await.map_err(RunError::message)?;

    parse_board_snapshot(&list, &ready, &blocked).map(|board| LoadResult::Snapshot(Box::new(board)))
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

#[derive(Debug)]
enum WriteError {
    PreconditionFailed,
    Failed(String),
}

#[derive(Debug)]
struct BdOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn resolve_bd_executable() -> Result<Bd, String> {
    resolve_bd_executable_from(std::env::var_os("PATH").as_deref(), dirs::home_dir().as_deref())
        .ok_or_else(|| {
            "bd is not installed or executable from PATH or a standard user install location".into()
        })
}

fn write_available_from(path: Option<&OsStr>, home: Option<&Path>) -> bool {
    resolve_bd_executable_from(path, home).is_some()
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

fn compose_write_argv(issue_id: &str, verb: &BeadsIssueWrite) -> Result<Vec<OsString>, String> {
    if issue_id.is_empty() || issue_id.starts_with('-') || issue_id.contains('\0') {
        return Err("invalid Beads issue id".into());
    }
    let mut argv = Vec::new();
    match verb {
        BeadsIssueWrite::SetTitle { title } => {
            update_arg(&mut argv, issue_id, "--title", title);
        }
        BeadsIssueWrite::SetDescription { description } => {
            update_arg(&mut argv, issue_id, "--description", description);
        }
        BeadsIssueWrite::SetAcceptance { acceptance } => {
            update_arg(&mut argv, issue_id, "--acceptance", acceptance);
        }
        BeadsIssueWrite::SetNotes { notes } => {
            update_arg(&mut argv, issue_id, "--notes", notes);
        }
        BeadsIssueWrite::SetDesign { design } => {
            update_arg(&mut argv, issue_id, "--design", design);
        }
        BeadsIssueWrite::SetSpecId { spec_id } => {
            update_arg(&mut argv, issue_id, "--spec-id", spec_id.as_deref().unwrap_or(""));
        }
        BeadsIssueWrite::SetPriority { priority } => {
            if *priority > 4 {
                return Err(format!("invalid Beads priority {priority}; expected 0 through 4"));
            }
            update_arg(&mut argv, issue_id, "--priority", &priority.to_string());
        }
        BeadsIssueWrite::SetType { issue_type } => {
            update_arg(&mut argv, issue_id, "--type", issue_type);
        }
        BeadsIssueWrite::SetLabels { labels } => {
            update_arg(&mut argv, issue_id, "--set-labels", &labels.join(","));
        }
        BeadsIssueWrite::SetStatus { status, clear_defer } => {
            if !matches!(status.as_str(), "open" | "in_progress" | "closed") {
                return Err(format!("unsupported Beads status {status:?}"));
            }
            update_arg(&mut argv, issue_id, "--status", status);
            if *clear_defer {
                argv.extend([OsString::from("--defer"), OsString::new()]);
            }
        }
        BeadsIssueWrite::Claim => {
            argv.extend(["update".into(), issue_id.into(), "--claim".into()]);
        }
        BeadsIssueWrite::CloseIssue => {
            argv.extend(["close".into(), issue_id.into()]);
        }
        BeadsIssueWrite::UndoClose => {
            argv.extend(["reopen".into(), issue_id.into()]);
        }
        BeadsIssueWrite::AddComment { body } => {
            argv.extend([
                "comments".into(),
                "add".into(),
                issue_id.into(),
                truncate_bytes(body, MAX_DETAIL_FIELD_BYTES).into(),
            ]);
        }
    }
    Ok(argv)
}

fn update_arg(argv: &mut Vec<OsString>, issue_id: &str, flag: &str, value: &str) {
    argv.extend(["update".into(), issue_id.into(), flag.into(), value.into()]);
}

fn write_verb_name(verb: &BeadsIssueWrite) -> &'static str {
    match verb {
        BeadsIssueWrite::SetTitle { .. } => "set_title",
        BeadsIssueWrite::SetDescription { .. } => "set_description",
        BeadsIssueWrite::SetAcceptance { .. } => "set_acceptance",
        BeadsIssueWrite::SetNotes { .. } => "set_notes",
        BeadsIssueWrite::SetDesign { .. } => "set_design",
        BeadsIssueWrite::SetSpecId { .. } => "set_spec_id",
        BeadsIssueWrite::SetPriority { .. } => "set_priority",
        BeadsIssueWrite::SetType { .. } => "set_type",
        BeadsIssueWrite::SetLabels { .. } => "set_labels",
        BeadsIssueWrite::SetStatus { .. } => "set_status",
        BeadsIssueWrite::Claim => "claim",
        BeadsIssueWrite::CloseIssue => "close",
        BeadsIssueWrite::UndoClose => "undo_close",
        BeadsIssueWrite::AddComment { .. } => "add_comment",
    }
}

async fn fresh_issue_matches_guards(
    bd: &Bd,
    project_root: &Path,
    issue_id: &str,
    guards: &BeadsIssueWriteGuards,
) -> Result<bool, String> {
    let bytes = run_bd(bd, project_root, &["show", issue_id]).await.map_err(RunError::message)?;
    let issue = parse_envelope::<IssueDetailPayload>(&bytes, "show")?.into_issue()?;
    Ok(guards.if_status.as_ref().is_none_or(|status| status == &issue.status)
        && guards
            .if_assignee
            .as_deref()
            .is_none_or(|assignee| assignee == issue.assignee.as_deref().unwrap_or("")))
}

async fn run_bd_write(
    bd: &Bd,
    project_root: &Path,
    argv: &[OsString],
    timeout: Duration,
) -> Result<(), WriteError> {
    let output = invoke_bd(bd, project_root, argv, timeout, "issue write")
        .await
        .map_err(WriteError::Failed)?;
    if !output.status.success() {
        return Err(WriteError::Failed(format!("bd failed: {}", failure_detail(&output))));
    }
    parse_envelope::<serde_json::Value>(&output.stdout, "write")
        .map(|_| ())
        .map_err(WriteError::Failed)
}

fn project_write_lock_path(tmp: &Path, uid: u32, canonical_root: &Path) -> PathBuf {
    let digest = Sha256::digest(canonical_root.as_os_str().as_encoded_bytes());
    let name = format!("{digest:x}");
    tmp.join(format!("scribe-beads-writes-{uid}")).join(format!("{name}.lock"))
}

async fn acquire_project_write_lock_at(
    tmp: &Path,
    uid: u32,
    canonical_root: &Path,
) -> Result<File, String> {
    let path = project_write_lock_path(tmp, uid, canonical_root);
    tokio::task::spawn_blocking(move || open_and_lock_project_file(&path, uid))
        .await
        .map_err(|error| format!("Beads write-lock task failed: {error}"))?
}

fn open_and_lock_project_file(path: &Path, uid: u32) -> Result<File, String> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};

    let directory = path.parent().ok_or_else(|| "Beads write lock has no parent".to_owned())?;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("could not create Beads write-lock directory: {error}")),
    }
    let directory_metadata = fs::symlink_metadata(directory)
        .map_err(|error| format!("could not inspect Beads write-lock directory: {error}"))?;
    if !directory_metadata.is_dir() || directory_metadata.uid() != uid {
        return Err("Beads write-lock directory is not a uid-owned directory".into());
    }
    fs::set_permissions(directory, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .map_err(|error| format!("could not secure Beads write-lock directory: {error}"))?;

    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).mode(0o600);
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file =
        options.open(path).map_err(|error| format!("could not open Beads write lock: {error}"))?;
    let file_metadata =
        file.metadata().map_err(|error| format!("could not inspect Beads write lock: {error}"))?;
    if !file_metadata.is_file() || file_metadata.uid() != uid {
        return Err("Beads write lock is not a uid-owned file".into());
    }
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|error| format!("could not secure Beads write lock: {error}"))?;
    file.lock().map_err(|error| format!("could not acquire Beads write lock: {error}"))?;
    Ok(file)
}

async fn invoke_bd(
    bd: &Bd,
    project_root: &Path,
    argv: &[OsString],
    timeout: Duration,
    operation: &str,
) -> Result<BdOutput, String> {
    let mut command = Command::new(&bd.exe);
    command
        .args(["--json", "-C"])
        .arg(project_root)
        .current_dir(project_root)
        .args(argv)
        .env("BD_JSON_ENVELOPE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(search_path) = &bd.search_path {
        command.env("PATH", search_path);
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "bd disappeared after it was discovered".into()
        } else {
            format!("could not start bd: {error}")
        }
    })?;
    let pid = child.id();
    let stdout_pipe = child.stdout.take().ok_or_else(|| "bd stdout unavailable".to_owned())?;
    let stderr_pipe = child.stderr.take().ok_or_else(|| "bd stderr unavailable".to_owned())?;
    let completed = Box::pin(tokio::time::timeout(timeout, async {
        tokio::join!(
            child.wait(),
            read_bounded(stdout_pipe, MAX_STDOUT_BYTES),
            read_bounded(stderr_pipe, MAX_STDERR_BYTES),
        )
    }))
    .await;
    let Ok((status, stdout, stderr)) = completed else {
        kill_process_group(pid);
        drop(child.kill().await);
        drop(child.wait().await);
        return Err(format!("bd {operation} timed out"));
    };
    Ok(BdOutput {
        status: status.map_err(|error| format!("could not wait for bd: {error}"))?,
        stdout: stdout?,
        stderr: stderr?,
    })
}

fn failure_detail(output: &BdOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !stderr.is_empty() {
        return stderr;
    }
    let json = json_error(&output.stdout);
    if json.is_empty() { format!("exited with {}", output.status) } else { json }
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

#[derive(Debug, Clone, Deserialize)]
struct IssueJson {
    id: String,
    title: String,
    status: String,
    priority: u8,
    #[serde(default)]
    issue_type: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    /// Typed list edges include already-satisfied `blocks` relations; Flow
    /// must show dependency history, not only what `bd blocked` reports now.
    #[serde(default)]
    dependencies: Vec<DependencyJson>,
    #[serde(default)]
    blocked_by: Vec<String>,
    #[serde(default)]
    assignee: Option<String>,
    /// Kept verbatim so a malformed tracker timestamp cannot break a board
    /// refresh; the client owns relative-time presentation.
    #[serde(default)]
    updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DependencyJson {
    #[serde(default)]
    depends_on_id: String,
    #[serde(rename = "type", default)]
    dependency_type: String,
}

fn graph_source_issue(issue: &IssueJson, ready_ids: &HashSet<String>) -> BeadsGraphSourceIssue {
    BeadsGraphSourceIssue {
        id: issue.id.clone(),
        title: issue.title.clone(),
        status: issue.status.clone(),
        priority: issue.priority,
        issue_type: issue.issue_type.clone(),
        parent: issue.parent.clone(),
        dependencies: issue
            .dependencies
            .iter()
            .map(|dependency| BeadsGraphSourceDependency {
                depends_on_id: dependency.depends_on_id.clone(),
                dependency_type: dependency.dependency_type.clone(),
            })
            .collect(),
        assignee: issue.assignee.clone(),
        updated_at: issue.updated_at.clone(),
        ready: ready_ids.contains(&issue.id),
    }
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
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    created_by: Option<String>,
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
    let parent_epic_name = issue
        .dependencies
        .iter()
        .find(|dependency| dependency.dependency_type == "parent-child")
        .and_then(|dependency| {
            (!dependency.title.is_empty())
                .then(|| truncate_bytes(&dependency.title, MAX_TITLE_CHARS))
        });

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
        parent_epic_name,
        assignee: bounded_option(issue.assignee, MAX_TITLE_CHARS),
        owner: bounded_option(issue.owner.or(issue.created_by), MAX_TITLE_CHARS),
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

#[cfg(test)]
fn classify_snapshot(
    list_json: &[u8],
    ready_json: &[u8],
    blocked_json: &[u8],
) -> Result<BeadsBoardSnapshot, String> {
    parse_board_snapshot(list_json, ready_json, blocked_json).map(|board| board.snapshot)
}

fn parse_board_snapshot(
    list_json: &[u8],
    ready_json: &[u8],
    blocked_json: &[u8],
) -> Result<CachedBoard, String> {
    let issues = parse_collection(list_json, "list")?;
    let ready = parse_collection(ready_json, "ready")?;
    let blocked = parse_collection(blocked_json, "blocked")?;
    let snapshot = classify_issues(&issues, &ready, &blocked)?;
    let ready_ids = ready.into_iter().map(|issue| issue.id).collect();
    Ok(CachedBoard { snapshot, issues, ready_ids })
}

fn classify_issues(
    issues: &[IssueJson],
    ready: &[IssueJson],
    blocked: &[IssueJson],
) -> Result<BeadsBoardSnapshot, String> {
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

    for issue in issues {
        if issue.issue_type.as_deref() == Some("epic") {
            continue;
        }
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
            parent_epic_id: parent_id.map(|id| truncate(id, MAX_ID_CHARS)),
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

    use scribe_common::protocol::{
        BeadsEpicGraphOutcome, BeadsEpicGraphRefusal, BeadsIssueQueue, BeadsIssueQueueBasis,
    };

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
            "parent": "epic",
            "dependencies": [
                {
                    "depends_on_id": "gate",
                    "title": "Gate",
                    "status": "closed",
                    "dependency_type": "blocks"
                },
                {
                    "depends_on_id": "epic",
                    "title": "Card detail epic",
                    "status": "open",
                    "dependency_type": "parent-child"
                }
            ],
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
            assert_eq!(detail.parent_epic_name.as_deref(), Some("Card detail epic"));
            assert_eq!(detail.dependents[0].id, "child");
        }
    }

    #[test]
    fn reads_real_bd_owner_alongside_created_by() {
        let mut issue = detail_issue();
        issue["created_by"] = serde_json::Value::String("creator".into());

        let detail = parse_issue_detail(&detail_envelope(&serde_json::json!([issue])), false)
            .expect("parse real bd ownership fields");

        assert_eq!(detail.owner.as_deref(), Some("owner"));
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
        let fake =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bd-detail-not-found.sh");
        let bd = Bd { exe: fake, search_path: None };

        let result = load_issue_detail_with(&bd, &scratch, "gone").await.expect("typed result");

        assert!(matches!(result, DetailLoadResult::NotFound));
        fs::remove_dir_all(scratch).expect("remove scratch root");
    }

    #[tokio::test]
    async fn detail_reads_bypass_snapshot_cache_and_use_exact_argv() {
        let scratch = beads_test_scratch_path("detail-uncached");
        fs::create_dir_all(&scratch).expect("create scratch root");
        let fake =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bd-detail-uncached.sh");
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
        let fake = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/bd-detail-ready-membership.sh");
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
            ["backlog"]
        );
        assert_eq!(board.ready[0].id, "ready");
        assert_eq!(board.ready[0].parent_epic_name.as_deref(), Some("Board epic"));
        assert_eq!(board.ready[0].parent_epic_id.as_deref(), Some("epic"));
        assert_eq!(board.backlog[0].parent_epic_id, None);
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
            [1, 1, 1, 1, 1]
        );
    }

    #[tokio::test]
    async fn retains_typed_list_edges_and_node_metadata_for_flow() {
        let list = br#"{"data":{"issues":[
          {"id":"epic","title":"Flow epic","status":"open","priority":2,"issue_type":"epic"},
          {"id":"closed-gate","title":"Closed gate","status":"closed","priority":1},
          {"id":"child","title":"Child","status":"open","priority":2,"parent":"epic","assignee":"agent-1","updated_at":"not-an-iso-timestamp","dependencies":[{"type":"blocks","depends_on_id":"closed-gate"},{"type":"parent-child","depends_on_id":"epic"}]},
          {"id":"standalone","title":"Standalone","status":"open","priority":3,"assignee":null}
        ]},"schema_version":1}"#;
        let ready = br#"{"data":[{"id":"child","title":"Child","status":"open","priority":2}],"schema_version":1}"#;
        let empty = br#"{"data":[],"schema_version":1}"#;

        let board = parse_board_snapshot(list, ready, empty).expect("parse board");
        let scratch = beads_test_scratch_path("flow-source");
        fs::create_dir_all(&scratch).expect("create cache root");
        let cache = BeadsBoardCache::default();
        let lookup = cache.lookup(&scratch).await;
        {
            let mut entries = cache.entries.lock().await;
            let entry = entries.get_mut(&lookup.key).expect("cache entry");
            entry.last_good = Some(board);
            entry.detected = Some(true);
            entry.last_attempt = Some(Instant::now());
            entry.source_generation = Some(entry.generation);
        }
        let (generation, source) = cache.graph_source(&scratch).await.expect("cached source");
        let child = source.iter().find(|issue| issue.id == "child").expect("child");
        let closed_edge = child
            .dependencies
            .iter()
            .find(|edge| edge.dependency_type == "blocks")
            .expect("closed blocks edge retained");
        let standalone = source.iter().find(|issue| issue.id == "standalone").expect("standalone");
        let BeadsBoardState::Ready { snapshot, .. } = cache.lookup(&scratch).await.state else {
            panic!("cached board must remain paintable");
        };

        assert_eq!(generation, 0);
        assert_eq!(closed_edge.depends_on_id, "closed-gate");
        assert_eq!(child.assignee.as_deref(), Some("agent-1"));
        assert_eq!(child.updated_at, "not-an-iso-timestamp");
        assert_eq!(standalone.assignee, None);
        assert!(standalone.updated_at.is_empty(), "absent timestamp defaults empty");
        assert_eq!(snapshot.ready[0].parent_epic_id.as_deref(), Some("epic"));
        assert_eq!(snapshot.backlog[0].parent_epic_id, None);
        assert_eq!(snapshot.done[0].id, "closed-gate");
        assert_eq!(
            [
                snapshot.backlog_total,
                snapshot.ready_total,
                snapshot.in_progress_total,
                snapshot.blocked_total,
                snapshot.done_total,
            ],
            [1, 1, 0, 0, 1]
        );
        fs::remove_dir_all(scratch).expect("remove cache root");
    }

    fn cached_admitted_flow_board() -> CachedBoard {
        let source_issue = |id: &str, status: &str, parent: Option<&str>, dependencies| IssueJson {
            id: id.into(),
            title: format!("{id} title"),
            status: status.into(),
            priority: 2,
            issue_type: None,
            parent: parent.map(str::to_owned),
            dependencies,
            blocked_by: vec![],
            assignee: None,
            updated_at: String::new(),
        };
        CachedBoard {
            snapshot: BeadsBoardSnapshot::default(),
            issues: vec![
                IssueJson {
                    issue_type: Some("epic".into()),
                    ..source_issue("epic", "open", None, vec![])
                },
                source_issue("foundation", "closed", Some("epic"), vec![]),
                source_issue(
                    "ship",
                    "open",
                    Some("epic"),
                    vec![DependencyJson {
                        depends_on_id: "foundation".into(),
                        dependency_type: "blocks".into(),
                    }],
                ),
            ],
            ready_ids: HashSet::from(["ship".into()]),
        }
    }

    // @lat: [[server#Server#Beads Flow source cache#Flow graph admission]]
    #[tokio::test]
    async fn flow_source_generation_fences_the_pre_write_graph() {
        let scratch = beads_test_scratch_path("flow-generation");
        fs::create_dir_all(&scratch).expect("create cache root");
        let cache = BeadsBoardCache::default();
        let lookup = cache.lookup(&scratch).await;
        {
            let mut entries = cache.entries.lock().await;
            let entry = entries.get_mut(&lookup.key).expect("cache entry");
            entry.last_good = Some(cached_admitted_flow_board());
            entry.detected = Some(true);
            entry.source_generation = Some(entry.generation);
        }
        assert!(matches!(
            cache.epic_graph(&scratch, "epic").await,
            BeadsEpicGraphOutcome::Graph(_)
        ));
        cache.entries.lock().await.get_mut(&lookup.key).expect("cache entry").generation += 1;
        assert!(matches!(
            cache.epic_graph(&scratch, "epic").await,
            BeadsEpicGraphOutcome::Unavailable { .. }
        ));
        fs::remove_dir_all(scratch).expect("remove cache root");
    }

    #[test]
    fn epics_never_enter_queues_totals_or_the_item_cap() {
        let mut issues = (0..MAX_ITEMS_PER_QUEUE)
            .map(|index| {
                serde_json::json!({
                    "id": format!("ready-epic-{index}"),
                    "title": "Ready epic",
                    "status": "open",
                    "priority": 2,
                    "issue_type": "epic",
                })
            })
            .collect::<Vec<_>>();
        let mut ready = issues.clone();
        issues.extend([
            serde_json::json!({"id":"doing-epic","title":"Doing epic","status":"in_progress","priority":2,"issue_type":"epic"}),
            serde_json::json!({"id":"blocked-epic","title":"Blocked epic","status":"blocked","priority":2,"issue_type":"epic"}),
            serde_json::json!({"id":"done-epic","title":"Done epic","status":"closed","priority":2,"issue_type":"epic"}),
            serde_json::json!({"id":"backlog-epic","title":"Backlog epic","status":"deferred","priority":5,"issue_type":"epic"}),
            serde_json::json!({"id":"task","title":"Task","status":"open","priority":1,"issue_type":"task"}),
        ]);
        ready.push(serde_json::json!({
            "id": "task",
            "title": "Task",
            "status": "open",
            "priority": 1,
        }));
        let list =
            serde_json::to_vec(&serde_json::json!({"data":{"issues":issues},"schema_version":1}))
                .expect("serialize list");
        let ready = serde_json::to_vec(&serde_json::json!({"data":ready,"schema_version":1}))
            .expect("serialize ready");
        let empty = br#"{"data":[],"schema_version":1}"#;

        let board = classify_snapshot(&list, &ready, empty).expect("classify board");

        assert_eq!(board.ready.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), ["task"]);
        assert!(board.backlog.is_empty());
        assert!(board.in_progress.is_empty());
        assert!(board.blocked.is_empty());
        assert!(board.done.is_empty());
        assert_eq!(
            [
                board.backlog_total,
                board.ready_total,
                board.in_progress_total,
                board.blocked_total,
                board.done_total,
            ],
            [0, 1, 0, 0, 0]
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

    // @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Beads Write Executor Unit Contract]]
    #[test]
    fn composes_every_write_verb_for_the_official_cli_without_private_guards() {
        let cases = [
            (
                BeadsIssueWrite::SetTitle { title: "Title".into() },
                vec!["update", "id", "--title", "Title"],
            ),
            (
                BeadsIssueWrite::SetDescription { description: "Desc".into() },
                vec!["update", "id", "--description", "Desc"],
            ),
            (
                BeadsIssueWrite::SetAcceptance { acceptance: "Accept".into() },
                vec!["update", "id", "--acceptance", "Accept"],
            ),
            (
                BeadsIssueWrite::SetNotes { notes: "Notes".into() },
                vec!["update", "id", "--notes", "Notes"],
            ),
            (
                BeadsIssueWrite::SetDesign { design: "Design".into() },
                vec!["update", "id", "--design", "Design"],
            ),
            (BeadsIssueWrite::SetSpecId { spec_id: None }, vec!["update", "id", "--spec-id", ""]),
            (
                BeadsIssueWrite::AddComment { body: "Comment".into() },
                vec!["comments", "add", "id", "Comment"],
            ),
        ];

        for (verb, expected) in cases {
            let argv = compose_write_argv("id", &verb).expect("compose argv");
            assert_eq!(argv, expected.iter().map(OsString::from).collect::<Vec<_>>());
        }

        let lifecycle_cases = [
            (BeadsIssueWrite::SetPriority { priority: 1 }, vec!["update", "id", "--priority", "1"]),
            (
                BeadsIssueWrite::SetType { issue_type: "bug".into() },
                vec!["update", "id", "--type", "bug"],
            ),
            (
                BeadsIssueWrite::SetLabels { labels: vec!["one".into(), "two".into()] },
                vec!["update", "id", "--set-labels", "one,two"],
            ),
            (
                BeadsIssueWrite::SetStatus { status: "open".into(), clear_defer: true },
                vec!["update", "id", "--status", "open", "--defer", ""],
            ),
            (BeadsIssueWrite::Claim, vec!["update", "id", "--claim"]),
            (BeadsIssueWrite::CloseIssue, vec!["close", "id"]),
            (BeadsIssueWrite::UndoClose, vec!["reopen", "id"]),
        ];
        for (verb, expected) in lifecycle_cases {
            let argv = compose_write_argv("id", &verb).expect("compose argv");
            assert_eq!(argv, expected.iter().map(OsString::from).collect::<Vec<_>>());
        }
    }

    #[test]
    fn official_bd_on_path_enables_writes_without_a_version_probe() {
        let scratch = beads_test_scratch_path("official-capability");
        fs::create_dir_all(&scratch).expect("create bin dir");
        let bd = scratch.join("bd");
        fs::write(&bd, "#!/bin/sh\nexit 99\n").expect("write official bd stand-in");
        fs::set_permissions(&bd, fs::Permissions::from_mode(0o755)).expect("chmod bd stand-in");
        let path = std::env::join_paths([&scratch]).expect("join test PATH");

        assert!(write_available_from(Some(&path), None));

        fs::remove_dir_all(scratch).expect("remove bin dir");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn project_write_lock_serializes_one_root_without_blocking_another() {
        let scratch = beads_test_scratch_path("write-locks");
        let locks = scratch.join("tmp");
        let first_root = scratch.join("first");
        let second_root = scratch.join("second");
        for path in [&locks, &first_root, &second_root] {
            fs::create_dir_all(path).expect("create lock fixture directory");
        }
        let first_root = first_root.canonicalize().expect("canonical first root");
        let second_root = second_root.canonicalize().expect("canonical second root");
        let uid = scribe_common::socket::current_uid();
        let first = acquire_project_write_lock_at(&locks, uid, &first_root)
            .await
            .expect("take first-root lock");
        let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
        let same_locks = locks.clone();
        let same_root = first_root.clone();
        tokio::spawn(async move {
            let lock = acquire_project_write_lock_at(&same_locks, uid, &same_root)
                .await
                .expect("take queued first-root lock");
            drop(acquired_tx.send(lock));
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut acquired_rx).await.is_err(),
            "a second writer entered the same root"
        );
        let other = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_project_write_lock_at(&locks, uid, &second_root),
        )
        .await
        .expect("different-root lock blocked")
        .expect("take different-root lock");
        drop(other);
        drop(first);
        let queued = tokio::time::timeout(Duration::from_secs(1), &mut acquired_rx)
            .await
            .expect("same-root lock stayed blocked")
            .expect("same-root lock task ended");
        drop(queued);
        fs::remove_dir_all(scratch).expect("remove lock fixture");
    }

    #[tokio::test]
    async fn project_write_lock_has_a_deterministic_private_safe_path() {
        let scratch = beads_test_scratch_path("write-lock-path");
        let locks = scratch.join("tmp");
        let root = scratch.join("project with spaces");
        fs::create_dir_all(&locks).expect("create temporary root");
        fs::create_dir_all(&root).expect("create project root");
        let root = root.canonicalize().expect("canonical project root");
        let uid = scribe_common::socket::current_uid();
        let expected = project_write_lock_path(&locks, uid, &root);

        let lock = acquire_project_write_lock_at(&locks, uid, &root).await.expect("take lock");

        assert_eq!(project_write_lock_path(&locks, uid, &root), expected);
        assert_eq!(
            expected
                .parent()
                .expect("lock parent")
                .metadata()
                .expect("lock dir metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(expected.metadata().expect("lock metadata").permissions().mode() & 0o777, 0o600);
        let name = expected.file_name().and_then(OsStr::to_str).expect("ASCII lock name");
        assert_eq!(Path::new(name).extension().and_then(OsStr::to_str), Some("lock"));
        assert!(name[..name.len() - 5].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!name.contains("project"));
        drop(lock);
        fs::remove_dir_all(scratch).expect("remove lock fixture");
    }

    fn serialized_write_fixture(label: &str, status: &str, assignee: &str) -> (PathBuf, Bd) {
        let scratch = beads_test_scratch_path(label);
        fs::create_dir_all(&scratch).expect("create serialized write root");
        fs::write(scratch.join("status"), status).expect("write fixture status");
        fs::write(scratch.join("assignee"), assignee).expect("write fixture assignee");
        let fake =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bd-write-serialized.sh");
        (scratch, Bd { exe: fake, search_path: None })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_guards_are_checked_only_after_entering_the_project_lock() {
        let (root, bd) = serialized_write_fixture("fresh-guard", "open", "");
        let root = root.canonicalize().expect("canonical project root");
        let locks = root.join("locks");
        fs::create_dir_all(&locks).expect("create lock base");
        let uid = scribe_common::socket::current_uid();
        let held = acquire_project_write_lock_at(&locks, uid, &root).await.expect("hold root lock");
        let cache = BeadsBoardCache::default();
        let queued_root = root.clone();
        let queued_locks = locks.clone();
        let queued = tokio::spawn(async move {
            cache
                .write_issue_with_bd(WriteIssueRequest {
                    bd: &bd,
                    lock_tmp: &queued_locks,
                    canonical_root: &queued_root,
                    issue_id: "issue",
                    verb: &BeadsIssueWrite::AddComment { body: "must not land".into() },
                    guards: &BeadsIssueWriteGuards {
                        if_status: Some("open".into()),
                        if_assignee: Some(String::new()),
                    },
                    timeout: Duration::from_millis(250),
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        fs::write(root.join("status"), "in_progress").expect("advance fixture status");
        drop(held);

        let outcome = queued.await.expect("join queued write");

        assert!(matches!(outcome.result, BeadsIssueWriteResult::PreconditionFailed));
        assert!(!root.join("writes").exists(), "stale guarded write reached bd");
        fs::remove_dir_all(root).expect("remove serialized write root");
    }

    #[tokio::test]
    async fn applied_result_retains_the_project_lock_for_authoritative_refresh() {
        let (root, bd) = serialized_write_fixture("refresh-lock", "open", "");
        let root = root.canonicalize().expect("canonical project root");
        let locks = root.join("locks");
        fs::create_dir_all(&locks).expect("create lock base");
        let cache = BeadsBoardCache::default();

        let outcome = cache
            .write_issue_with_bd(WriteIssueRequest {
                bd: &bd,
                lock_tmp: &locks,
                canonical_root: &root,
                issue_id: "issue",
                verb: &BeadsIssueWrite::SetTitle { title: "Applied".into() },
                guards: &BeadsIssueWriteGuards::default(),
                timeout: Duration::from_millis(250),
            })
            .await;

        assert!(matches!(outcome.result, BeadsIssueWriteResult::Applied { .. }));
        let lock = outcome.lock.expect("applied result must retain its lock");
        {
            let blocked =
                acquire_project_write_lock_at(&locks, scribe_common::socket::current_uid(), &root);
            tokio::pin!(blocked);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut blocked).await.is_err(),
                "next same-root writer entered before authoritative refresh"
            );
            drop(lock);
            tokio::time::timeout(Duration::from_secs(1), &mut blocked)
                .await
                .expect("same-root writer stayed blocked")
                .expect("take released lock");
        }
        fs::remove_dir_all(root).expect("remove serialized write root");
    }

    #[tokio::test]
    async fn timed_out_write_releases_the_project_lock() {
        let (root, bd) = serialized_write_fixture("timeout-lock", "open", "");
        fs::write(root.join("mode"), "timeout").expect("enable timeout mode");
        let root = root.canonicalize().expect("canonical project root");
        let locks = root.join("locks");
        fs::create_dir_all(&locks).expect("create lock base");
        let cache = BeadsBoardCache::default();

        let outcome = cache
            .write_issue_with_bd(WriteIssueRequest {
                bd: &bd,
                lock_tmp: &locks,
                canonical_root: &root,
                issue_id: "issue",
                verb: &BeadsIssueWrite::SetTitle { title: "Never applied".into() },
                guards: &BeadsIssueWriteGuards::default(),
                timeout: Duration::from_millis(100),
            })
            .await;

        assert!(matches!(
            outcome.result,
            BeadsIssueWriteResult::Failed { ref reason } if reason == "bd issue write timed out"
        ));
        assert!(outcome.lock.is_none());
        tokio::time::timeout(
            Duration::from_secs(1),
            acquire_project_write_lock_at(&locks, scribe_common::socket::current_uid(), &root),
        )
        .await
        .expect("timeout leaked the project lock")
        .expect("take released project lock");
        fs::remove_dir_all(root).expect("remove serialized write root");
    }

    #[test]
    fn generation_fence_discards_refreshes_started_before_a_write() {
        let mut entry = CacheEntry { generation: 2, ..CacheEntry::default() };
        let old = BeadsBoardSnapshot { refreshed_at_epoch_ms: 1, ..Default::default() };
        let current = BeadsBoardSnapshot { refreshed_at_epoch_ms: 2, ..Default::default() };

        assert!(!apply_refresh_if_current(
            &mut entry,
            1,
            Ok(LoadResult::Snapshot(Box::new(CachedBoard {
                snapshot: old,
                issues: vec![],
                ready_ids: HashSet::new()
            }))),
            Path::new("/tmp/fenced"),
        ));
        assert!(entry.last_good.is_none());
        assert_eq!(entry.source_generation, None);
        assert!(apply_refresh_if_current(
            &mut entry,
            2,
            Ok(LoadResult::Snapshot(Box::new(CachedBoard {
                snapshot: current.clone(),
                issues: vec![],
                ready_ids: HashSet::new()
            }))),
            Path::new("/tmp/fenced"),
        ));
        assert_eq!(entry.last_good.as_ref().map(|board| &board.snapshot), Some(&current));
        assert_eq!(entry.source_generation, Some(2));
        entry.generation = 3;
        assert_ne!(entry.source_generation, Some(entry.generation));
    }

    #[tokio::test]
    async fn write_timeout_kills_the_whole_bd_process_group() {
        let scratch = beads_test_scratch_path("write-timeout");
        fs::create_dir_all(&scratch).expect("create scratch root");
        let child_pid = scratch.join("child.pid");
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bd-write-timeout.sh");
        let bd = Bd { exe: fake, search_path: None };

        let error =
            invoke_bd(&bd, &scratch, &["update".into()], Duration::from_millis(100), "issue write")
                .await
                .expect_err("fake bd must time out");

        assert_eq!(error, "bd issue write timed out");
        let pid_text = fs::read_to_string(&child_pid).expect("read child pid");
        let pid = pid_text.trim().parse::<i32>().expect("parse child pid");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "bd child survived timeout");
        fs::remove_dir_all(scratch).expect("remove scratch root");
    }

    fn flow_source_issue(
        id: &str,
        parent: Option<&str>,
        status: &str,
        blockers: &[&str],
    ) -> BeadsGraphSourceIssue {
        BeadsGraphSourceIssue {
            id: id.into(),
            title: format!("{id} title"),
            status: status.into(),
            priority: 2,
            issue_type: None,
            parent: parent.map(str::to_owned),
            dependencies: blockers
                .iter()
                .map(|blocker_id| BeadsGraphSourceDependency {
                    depends_on_id: (*blocker_id).into(),
                    dependency_type: "blocks".into(),
                })
                .collect(),
            assignee: None,
            updated_at: "2026-08-18T00:00:00Z".into(),
            ready: true,
        }
    }

    fn flow_epic(id: &str) -> BeadsGraphSourceIssue {
        BeadsGraphSourceIssue {
            issue_type: Some("epic".into()),
            parent: None,
            dependencies: vec![],
            ..flow_source_issue(id, None, "open", &[])
        }
    }

    fn refusal(outcome: &BeadsEpicGraphOutcome) -> BeadsEpicGraphRefusal {
        let BeadsEpicGraphOutcome::NoGraph { reason } = outcome else {
            panic!("expected graph refusal");
        };
        *reason
    }

    // @lat: [[server#Server#Beads Flow source cache#Flow graph admission]]
    #[test]
    fn flow_admission_returns_a_complete_graph_or_a_typed_refusal() {
        let admitted = vec![
            flow_epic("epic"),
            flow_source_issue("foundation", Some("epic"), "closed", &[]),
            flow_source_issue("ship", Some("epic"), "open", &["foundation"]),
        ];
        let BeadsEpicGraphOutcome::Graph(graph) = assemble_epic_graph(&admitted, "epic") else {
            panic!("expected admitted graph");
        };
        assert_eq!(graph.total, 2);
        assert_eq!(graph.closed, 1);
        assert_eq!(graph.edges, [BeadsGraphEdge { from: "foundation".into(), to: "ship".into() }]);
        assert!(matches!(
            graph.nodes.iter().find(|node| node.id == "ship").map(|node| node.queue),
            Some(BeadsIssueQueue::Ready)
        ));

        let external = vec![
            flow_epic("epic"),
            flow_source_issue("inside", Some("epic"), "open", &["outside"]),
        ];
        assert_eq!(
            refusal(&assemble_epic_graph(&external, "epic")),
            BeadsEpicGraphRefusal::ExternalBlocker
        );
        let disconnected = vec![
            flow_epic("epic"),
            flow_source_issue("a", Some("epic"), "open", &[]),
            flow_source_issue("b", Some("epic"), "open", &[]),
        ];
        assert_eq!(
            refusal(&assemble_epic_graph(&disconnected, "epic")),
            BeadsEpicGraphRefusal::Disconnected
        );
    }

    // @lat: [[server#Server#Beads Flow source cache#Flow graph admission]]
    #[test]
    fn flow_graph_keeps_closed_members_past_the_board_done_cap() {
        let mut issues = (0..MAX_ITEMS_PER_QUEUE)
            .map(|index| {
                serde_json::json!({
                    "id": format!("unrelated-{index}"),
                    "title": "Unrelated closed",
                    "status": "closed",
                    "priority": 2,
                })
            })
            .collect::<Vec<_>>();
        issues.extend([
            serde_json::json!({"id":"epic","title":"Epic","status":"open","priority":2,"issue_type":"epic"}),
            serde_json::json!({"id":"foundation","title":"Foundation","status":"closed","priority":2,"parent":"epic"}),
            serde_json::json!({"id":"ship","title":"Ship","status":"open","priority":2,"parent":"epic","dependencies":[{"type":"blocks","depends_on_id":"foundation"}]}),
        ]);
        let list = serde_json::to_vec(&serde_json::json!({
            "data": {"issues": issues},
            "schema_version": BD_JSON_SCHEMA_VERSION,
        }))
        .expect("serialize list");
        let ready = br#"{"data":[{"id":"ship","title":"Ship","status":"open","priority":2}],"schema_version":1}"#;
        let board = parse_board_snapshot(&list, ready, br#"{"data":[],"schema_version":1}"#)
            .expect("parse board");
        let source = board
            .issues
            .iter()
            .map(|issue| graph_source_issue(issue, &board.ready_ids))
            .collect::<Vec<_>>();

        assert_eq!(board.snapshot.done.len(), MAX_ITEMS_PER_QUEUE);
        assert!(board.snapshot.done.iter().all(|item| item.id != "foundation"));
        let BeadsEpicGraphOutcome::Graph(graph) = assemble_epic_graph(&source, "epic") else {
            panic!("expected graph after board cap");
        };
        assert_eq!(
            graph.nodes.iter().map(|node| node.id.as_str()).collect::<HashSet<_>>(),
            HashSet::from(["foundation", "ship"])
        );
    }

    // bd refuses cycles, so this follows the actual admission boundary with an
    // in-memory source rather than adding a test-only tracker corruption seam.
    #[test]
    fn flow_admission_rejects_cycles_and_independent_bounds() {
        let cycle = vec![
            flow_epic("epic"),
            flow_source_issue("a", Some("epic"), "open", &["b"]),
            flow_source_issue("b", Some("epic"), "open", &["a"]),
        ];
        assert_eq!(refusal(&assemble_epic_graph(&cycle, "epic")), BeadsEpicGraphRefusal::Cycle);

        let mut too_large = vec![flow_epic("epic")];
        too_large.extend(
            (0..=MAX_FLOW_NODES).map(|index| {
                flow_source_issue(&format!("node-{index}"), Some("epic"), "open", &[])
            }),
        );
        assert_eq!(
            refusal(&assemble_epic_graph(&too_large, "epic")),
            BeadsEpicGraphRefusal::TooLarge
        );

        let mut edge_bound = vec![flow_epic("epic")];
        edge_bound.extend((0..=MAX_FLOW_EDGES_PER_NODE).map(|index| {
            flow_source_issue(&format!("blocker-{index}"), Some("epic"), "closed", &[])
        }));
        let blockers = (0..=MAX_FLOW_EDGES_PER_NODE)
            .map(|index| format!("blocker-{index}"))
            .collect::<Vec<_>>();
        let blocker_refs = blockers.iter().map(String::as_str).collect::<Vec<_>>();
        edge_bound.push(flow_source_issue("dependent", Some("epic"), "open", &blocker_refs));
        assert_eq!(
            refusal(&assemble_epic_graph(&edge_bound, "epic")),
            BeadsEpicGraphRefusal::TooLarge
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
            entry.last_good = Some(CachedBoard {
                snapshot: snapshot.clone(),
                issues: vec![],
                ready_ids: HashSet::new(),
            });
            entry.source_generation = Some(entry.generation);
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
