use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::{Config, Event, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher as _};
use thiserror::Error;
use tokio::sync::broadcast::{Receiver, Sender};
use tracing::{debug, warn};

const DEBOUNCE: Duration = Duration::from_millis(250);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The configured remote whose tracking ref moved to a local branch tip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteRepository {
    pub remote_name: String,
    pub push_url: String,
}

/// Server-internal gate for the later GitHub Actions polling window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushDetected {
    pub repository_root: PathBuf,
    pub head_sha: String,
    pub remote_repository: RemoteRepository,
}

#[derive(Debug, Error)]
pub enum GitRefWatchError {
    #[error("failed to run git {operation} in {cwd}")]
    GitSpawn {
        operation: &'static str,
        cwd: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git {operation} failed in {cwd}: {stderr}")]
    GitFailed { operation: &'static str, cwd: PathBuf, stderr: Box<str> },
    #[error("git {operation} returned non-UTF-8 output")]
    GitEncoding {
        operation: &'static str,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("git repository discovery returned incomplete paths for {0}")]
    IncompleteDiscovery(PathBuf),
    #[error("failed to watch {path}")]
    Watch {
        path: PathBuf,
        #[source]
        source: Box<notify::Error>,
    },
    #[error("failed to construct git ref watcher")]
    WatchSetup(#[source] Box<notify::Error>),
    #[error("failed to start git ref watcher worker")]
    WorkerSpawn(#[source] std::io::Error),
}

#[derive(Clone, Debug)]
struct Repository {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

impl Repository {
    fn discover(cwd: &Path) -> Result<Option<Self>, GitRefWatchError> {
        let output = execute_git(
            cwd,
            "repository discovery",
            [
                "rev-parse",
                "--show-toplevel",
                "--absolute-git-dir",
                "--path-format=absolute",
                "--git-common-dir",
            ],
        )?;
        if !output.status.success() {
            return Ok(None);
        }
        let output = decode_git_output(cwd, "repository discovery", output)?;
        let mut lines = output.lines();
        let Some(root) = lines.next().filter(|line| !line.is_empty()) else {
            return Err(GitRefWatchError::IncompleteDiscovery(cwd.to_path_buf()));
        };
        let Some(git_dir) = lines.next().filter(|line| !line.is_empty()) else {
            return Err(GitRefWatchError::IncompleteDiscovery(cwd.to_path_buf()));
        };
        let Some(common_dir) = lines.next().filter(|line| !line.is_empty()) else {
            return Err(GitRefWatchError::IncompleteDiscovery(cwd.to_path_buf()));
        };
        Ok(Some(Self::from_resolved_paths(root.into(), git_dir.into(), common_dir.into())))
    }

    fn from_resolved_paths(root: PathBuf, git_dir: PathBuf, common_dir: PathBuf) -> Self {
        Self { root, git_dir, common_dir }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RemoteRef {
    remote_name: String,
    ref_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RemoteTip {
    oid: String,
    push_url: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct LogicalSnapshot {
    local_heads: BTreeSet<String>,
    remote_refs: BTreeMap<RemoteRef, RemoteTip>,
}

fn logical_snapshot(repository: &Repository) -> Result<LogicalSnapshot, GitRefWatchError> {
    let local_heads = parse_refs(&git_output(
        &repository.root,
        "local branch snapshot",
        ["for-each-ref", "--format=%(refname)%00%(objectname)", "refs/heads"],
    )?)
    .into_values()
    .collect();

    let mut remote_refs = BTreeMap::new();
    let remote_names = git_output(&repository.root, "remote list", ["remote"])?;
    for remote_name in remote_names.lines().filter(|name| !name.is_empty()) {
        let key = format!("remote.{remote_name}.fetch");
        let fetch_specs = git_optional_output(
            &repository.root,
            "remote fetch refspecs",
            ["config", "--get-all", &key],
        )?;
        let patterns: Vec<String> =
            fetch_specs.lines().filter_map(fetch_destination_pattern).collect();
        if patterns.is_empty() {
            continue;
        }
        let push_url = git_output(
            &repository.root,
            "remote push URL",
            ["remote", "get-url", "--push", remote_name],
        )?
        .trim()
        .to_owned();
        let mut args =
            vec![String::from("for-each-ref"), String::from("--format=%(refname)%00%(objectname)")];
        args.extend(patterns);
        for (ref_name, oid) in
            parse_refs(&git_output(&repository.root, "remote ref snapshot", args)?)
        {
            remote_refs.insert(
                RemoteRef { remote_name: remote_name.to_owned(), ref_name },
                RemoteTip { oid, push_url: push_url.clone() },
            );
        }
    }
    Ok(LogicalSnapshot { local_heads, remote_refs })
}

fn fetch_destination_pattern(refspec: &str) -> Option<String> {
    let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
    if refspec.starts_with('^') {
        return None;
    }
    let (_, destination) = refspec.split_once(':')?;
    if destination.is_empty() {
        return None;
    }
    Some(destination.split_once('*').map_or(destination, |(prefix, _)| prefix).to_owned())
}

fn parse_refs(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.split_once('\0'))
        .map(|(name, oid)| (name.to_owned(), oid.to_owned()))
        .collect()
}

fn detect_pushes(
    repository: &Repository,
    before: &LogicalSnapshot,
    after: &LogicalSnapshot,
) -> Vec<PushDetected> {
    let mut seen = BTreeSet::new();
    after
        .remote_refs
        .iter()
        .filter(|(remote_ref, tip)| {
            before.remote_refs.get(*remote_ref).is_none_or(|old| old.oid != tip.oid)
                && after.local_heads.contains(&tip.oid)
        })
        .filter(|(remote_ref, tip)| {
            seen.insert((remote_ref.remote_name.clone(), tip.push_url.clone(), tip.oid.clone()))
        })
        .map(|(remote_ref, tip)| PushDetected {
            repository_root: repository.root.clone(),
            head_sha: tip.oid.clone(),
            remote_repository: RemoteRepository {
                remote_name: remote_ref.remote_name.clone(),
                push_url: tip.push_url.clone(),
            },
        })
        .collect()
}

fn watch_paths(repository: &Repository) -> Vec<(PathBuf, RecursiveMode)> {
    let mut paths = BTreeMap::new();
    paths.insert(repository.git_dir.clone(), RecursiveMode::NonRecursive);
    paths.insert(repository.common_dir.clone(), RecursiveMode::NonRecursive);
    let refs = repository.common_dir.join("refs");
    if refs.is_dir() {
        paths.insert(refs, RecursiveMode::Recursive);
    }
    let reftable = repository.common_dir.join("reftable");
    if reftable.is_dir() {
        paths.insert(reftable, RecursiveMode::NonRecursive);
    }
    paths.into_iter().collect()
}

enum WorkerEvent {
    Changed(PathBuf),
    Fallback { repository: PathBuf, reason: String },
    Stop,
}

enum WatchHandle {
    Native(RecommendedWatcher),
    Poll(PollWatcher),
}

struct WatchBackend {
    handle: WatchHandle,
    paths: BTreeMap<PathBuf, RecursiveMode>,
}

impl WatchBackend {
    fn native(
        repository: &Repository,
        worker: &std::sync::mpsc::Sender<WorkerEvent>,
    ) -> Result<Self, GitRefWatchError> {
        let key = repository.common_dir.clone();
        let mut watcher = notify::recommended_watcher(notify_handler(key, worker.clone()))
            .map_err(|source| GitRefWatchError::WatchSetup(Box::new(source)))?;
        let paths = watch_paths(repository);
        add_paths(&mut watcher, &paths)?;
        Ok(Self { handle: WatchHandle::Native(watcher), paths: paths.into_iter().collect() })
    }

    fn poll(
        repository: &Repository,
        git_dirs: &BTreeSet<PathBuf>,
        worker: &std::sync::mpsc::Sender<WorkerEvent>,
    ) -> Result<Self, GitRefWatchError> {
        let key = repository.common_dir.clone();
        let config = Config::default().with_poll_interval(POLL_INTERVAL);
        let mut watcher = PollWatcher::new(notify_handler(key, worker.clone()), config)
            .map_err(|source| GitRefWatchError::WatchSetup(Box::new(source)))?;
        let paths = all_watch_paths(repository, git_dirs);
        add_paths(&mut watcher, &paths)?;
        Ok(Self { handle: WatchHandle::Poll(watcher), paths: paths.into_iter().collect() })
    }

    fn is_native(&self) -> bool {
        matches!(self.handle, WatchHandle::Native(_))
    }

    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> Result<(), GitRefWatchError> {
        let result = match &mut self.handle {
            WatchHandle::Native(watcher) => watcher.watch(path, mode),
            WatchHandle::Poll(watcher) => watcher.watch(path, mode),
        };
        result.map_err(|source| GitRefWatchError::Watch {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        self.paths.insert(path.to_path_buf(), mode);
        Ok(())
    }
}

fn notify_handler(
    repository: PathBuf,
    worker: std::sync::mpsc::Sender<WorkerEvent>,
) -> impl FnMut(notify::Result<Event>) + Send + 'static {
    move |result| {
        let message = match result {
            Ok(event) if event.need_rescan() => WorkerEvent::Fallback {
                repository: repository.clone(),
                reason: String::from("native watcher requested a full rescan"),
            },
            Ok(_) => WorkerEvent::Changed(repository.clone()),
            Err(error) => {
                WorkerEvent::Fallback { repository: repository.clone(), reason: error.to_string() }
            }
        };
        drop(worker.send(message));
    }
}

fn add_paths<W: notify::Watcher>(
    watcher: &mut W,
    paths: &[(PathBuf, RecursiveMode)],
) -> Result<(), GitRefWatchError> {
    for (path, mode) in paths {
        watcher.watch(path, *mode).map_err(|source| GitRefWatchError::Watch {
            path: path.clone(),
            source: Box::new(source),
        })?;
    }
    Ok(())
}

fn all_watch_paths(
    repository: &Repository,
    git_dirs: &BTreeSet<PathBuf>,
) -> Vec<(PathBuf, RecursiveMode)> {
    let mut paths: BTreeMap<PathBuf, RecursiveMode> = watch_paths(repository).into_iter().collect();
    paths.extend(git_dirs.iter().cloned().map(|path| (path, RecursiveMode::NonRecursive)));
    paths.into_iter().collect()
}

struct WatchedRepository {
    repository: Repository,
    git_dirs: BTreeSet<PathBuf>,
    snapshot: LogicalSnapshot,
    backend: WatchBackend,
}

impl WatchedRepository {
    fn new(
        repository: Repository,
        snapshot: LogicalSnapshot,
        worker: &std::sync::mpsc::Sender<WorkerEvent>,
    ) -> Result<Self, GitRefWatchError> {
        let git_dirs = BTreeSet::from([repository.git_dir.clone()]);
        let backend = match WatchBackend::native(&repository, worker) {
            Ok(backend) => backend,
            Err(error) => {
                warn!(%error, repository = %repository.root.display(), "native git ref watcher unavailable; using polling fallback");
                WatchBackend::poll(&repository, &git_dirs, worker)?
            }
        };
        Ok(Self { repository, git_dirs, snapshot, backend })
    }

    fn add_git_dir(
        &mut self,
        git_dir: PathBuf,
        worker: &std::sync::mpsc::Sender<WorkerEvent>,
    ) -> Result<bool, GitRefWatchError> {
        let inserted = self.git_dirs.insert(git_dir);
        if inserted {
            self.ensure_paths(worker)?;
        }
        Ok(inserted)
    }

    fn ensure_paths(
        &mut self,
        worker: &std::sync::mpsc::Sender<WorkerEvent>,
    ) -> Result<(), GitRefWatchError> {
        let missing: Vec<_> = all_watch_paths(&self.repository, &self.git_dirs)
            .into_iter()
            .filter(|(path, _)| !self.backend.paths.contains_key(path))
            .collect();
        for (path, mode) in missing {
            if let Err(error) = self.backend.watch(&path, mode) {
                return self.recover_watch_error(error, worker);
            }
        }
        Ok(())
    }

    fn recover_watch_error(
        &mut self,
        error: GitRefWatchError,
        worker: &std::sync::mpsc::Sender<WorkerEvent>,
    ) -> Result<(), GitRefWatchError> {
        if !self.backend.is_native() {
            return Err(error);
        }
        warn!(%error, repository = %self.repository.root.display(), "native git ref watcher failed; using polling fallback");
        self.backend = WatchBackend::poll(&self.repository, &self.git_dirs, worker)?;
        Ok(())
    }

    fn use_polling(
        &mut self,
        worker: &std::sync::mpsc::Sender<WorkerEvent>,
    ) -> Result<(), GitRefWatchError> {
        if self.backend.is_native() {
            self.backend = WatchBackend::poll(&self.repository, &self.git_dirs, worker)?;
        }
        Ok(())
    }

    fn rescan(
        &mut self,
        worker: &std::sync::mpsc::Sender<WorkerEvent>,
    ) -> Result<Vec<PushDetected>, GitRefWatchError> {
        self.ensure_paths(worker)?;
        let next = logical_snapshot(&self.repository)?;
        let events = detect_pushes(&self.repository, &self.snapshot, &next);
        self.snapshot = next;
        Ok(events)
    }
}

type RepositoryMap = Arc<Mutex<HashMap<PathBuf, WatchedRepository>>>;

/// Owns Git ref-state watchers. Construction is absent when the feature is off.
pub struct GitRefWatcher {
    repositories: RepositoryMap,
    worker_tx: std::sync::mpsc::Sender<WorkerEvent>,
    worker: Option<JoinHandle<()>>,
}

pub type PushEventReceiver = Receiver<PushDetected>;
pub type StartedGitRefWatcher = (GitRefWatcher, PushEventReceiver);

struct RunningGitRefWatcher {
    watcher: GitRefWatcher,
    events: Option<PushEventReceiver>,
}

/// Server-owned live enable/disable handle for Git ref watching.
// @lat: [[server#Sessions#Git Push Detection]]
pub struct GitRefWatcherControl(Mutex<Option<RunningGitRefWatcher>>);

impl GitRefWatcherControl {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self(Mutex::new(Self::start(enabled)))
    }

    /// Reconcile the live watcher with the setting and report whether it changed.
    pub fn set_enabled(&self, enabled: bool) -> bool {
        let mut running = self.lock();
        if enabled == running.is_some() {
            return false;
        }
        *running = Self::start(enabled);
        true
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.lock().is_some()
    }

    /// Discover a session CWD only while the live setting is enabled.
    pub fn watch_cwd(&self, cwd: &Path) -> Result<bool, GitRefWatchError> {
        self.lock().as_ref().map_or(Ok(false), |running| running.watcher.watch_cwd(cwd))
    }

    /// Transfer the internal push stream to its single server-side consumer.
    pub fn take_event_receiver(&self) -> Option<PushEventReceiver> {
        self.lock().as_mut().and_then(|running| running.events.take())
    }

    fn start(enabled: bool) -> Option<RunningGitRefWatcher> {
        match GitRefWatcher::start(enabled) {
            Ok(Some((watcher, events))) => {
                Some(RunningGitRefWatcher { watcher, events: Some(events) })
            }
            Ok(None) => None,
            Err(error) => {
                warn!(%error, "Git ref watcher could not start; local push detection disabled");
                None
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<RunningGitRefWatcher>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl GitRefWatcher {
    /// Start the detector only when `enabled`; `false` allocates no watcher or worker.
    pub fn start(enabled: bool) -> Result<Option<StartedGitRefWatcher>, GitRefWatchError> {
        if !enabled {
            return Ok(None);
        }
        let repositories = RepositoryMap::default();
        let (worker_tx, worker_rx) = std::sync::mpsc::channel();
        let (push_tx, push_rx) = tokio::sync::broadcast::channel(16);
        let worker_repositories = Arc::clone(&repositories);
        let worker_sender = worker_tx.clone();
        let worker = std::thread::Builder::new()
            .name(String::from("scribe-git-ref-watcher"))
            .spawn(move || {
                worker_loop(&worker_rx, &worker_sender, &worker_repositories, &push_tx);
            })
            .map_err(GitRefWatchError::WorkerSpawn)?;
        Ok(Some((Self { repositories, worker_tx, worker: Some(worker) }, push_rx)))
    }

    /// Discover and watch the repository containing a session CWD.
    pub fn watch_cwd(&self, cwd: &Path) -> Result<bool, GitRefWatchError> {
        let Some(repository) = Repository::discover(cwd)? else {
            return Ok(false);
        };
        let key = repository.common_dir.clone();
        let mut repositories = lock_repositories(&self.repositories);
        if let Some(watched) = repositories.get_mut(&key) {
            return watched.add_git_dir(repository.git_dir, &self.worker_tx);
        }
        let snapshot = logical_snapshot(&repository)?;
        let watched = WatchedRepository::new(repository, snapshot, &self.worker_tx)?;
        repositories.insert(key, watched);
        Ok(true)
    }
}

impl Drop for GitRefWatcher {
    fn drop(&mut self) {
        lock_repositories(&self.repositories).clear();
        drop(self.worker_tx.send(WorkerEvent::Stop));
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            warn!("git ref watcher worker panicked during shutdown");
        }
    }
}

fn lock_repositories(
    repositories: &RepositoryMap,
) -> MutexGuard<'_, HashMap<PathBuf, WatchedRepository>> {
    repositories.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn worker_loop(
    receiver: &std::sync::mpsc::Receiver<WorkerEvent>,
    worker: &std::sync::mpsc::Sender<WorkerEvent>,
    repositories: &RepositoryMap,
    pushes: &Sender<PushDetected>,
) {
    let mut deadlines = HashMap::<PathBuf, Instant>::new();
    loop {
        let message = deadlines.values().min().map_or_else(
            || receiver.recv().ok(),
            |deadline| match receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(message) => Some(message),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Some(WorkerEvent::Stop),
            },
        );
        match message {
            Some(WorkerEvent::Changed(repository)) => {
                deadlines.insert(repository, Instant::now() + DEBOUNCE);
            }
            Some(WorkerEvent::Fallback { repository, reason }) => {
                warn!(repository = %repository.display(), %reason, "git ref watcher overflow or error; switching to polling fallback");
                if let Some(watched) = lock_repositories(repositories).get_mut(&repository)
                    && let Err(error) = watched.use_polling(worker)
                {
                    warn!(repository = %repository.display(), %error, "git ref polling fallback failed");
                }
                deadlines.insert(repository, Instant::now() + DEBOUNCE);
            }
            Some(WorkerEvent::Stop) => return,
            None => {}
        }

        let now = Instant::now();
        let due: Vec<_> = deadlines
            .iter()
            .filter_map(|(repository, deadline)| (*deadline <= now).then_some(repository.clone()))
            .collect();
        for repository in due {
            deadlines.remove(&repository);
            let result = lock_repositories(repositories)
                .get_mut(&repository)
                .map(|watched| watched.rescan(worker));
            match result {
                Some(Ok(events)) => {
                    send_push_events(pushes, events);
                }
                Some(Err(error)) => {
                    warn!(repository = %repository.display(), %error, "git ref rescan failed");
                }
                None => {
                    debug!(repository = %repository.display(), "git ref event for unregistered repository");
                }
            }
        }
    }
}

fn send_push_events(pushes: &Sender<PushDetected>, events: Vec<PushDetected>) {
    for event in events {
        drop(pushes.send(event));
    }
}

fn git_output<I, S>(
    cwd: &Path,
    operation: &'static str,
    args: I,
) -> Result<String, GitRefWatchError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    decode_git_output(cwd, operation, execute_git(cwd, operation, args)?)
}

fn git_optional_output<I, S>(
    cwd: &Path,
    operation: &'static str,
    args: I,
) -> Result<String, GitRefWatchError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = execute_git(cwd, operation, args)?;
    if output.status.code() == Some(1) {
        return Ok(String::new());
    }
    decode_git_output(cwd, operation, output)
}

fn execute_git<I, S>(
    cwd: &Path,
    operation: &'static str,
    args: I,
) -> Result<Output, GitRefWatchError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_command()
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|source| GitRefWatchError::GitSpawn { operation, cwd: cwd.to_path_buf(), source })
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(name);
    }
    command
}

fn decode_git_output(
    cwd: &Path,
    operation: &'static str,
    output: Output,
) -> Result<String, GitRefWatchError> {
    if !output.status.success() {
        return Err(GitRefWatchError::GitFailed {
            operation,
            cwd: cwd.to_path_buf(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().into(),
        });
    }
    String::from_utf8(output.stdout)
        .map_err(|source| GitRefWatchError::GitEncoding { operation, source })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use notify::RecursiveMode;

    use super::{
        GitRefWatcher, GitRefWatcherControl, Repository, detect_pushes, git_command,
        logical_snapshot, watch_paths,
    };

    struct Fixture {
        base: PathBuf,
        work: PathBuf,
        remote: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let base = std::env::temp_dir()
                .join(format!("scribe-git-ref-watcher-{}", uuid::Uuid::new_v4()));
            let work = base.join("work");
            let remote = base.join("remote.git");
            fs::create_dir_all(&base).expect("create fixture root");
            run(&base, ["init", "--bare", remote.to_str().expect("utf-8 temp path")]);
            run(&base, ["init", work.to_str().expect("utf-8 temp path")]);
            run(&work, ["config", "user.email", "scribe@example.com"]);
            run(&work, ["config", "user.name", "Scribe Test"]);
            run(&work, ["config", "core.hooksPath", ".git-hooks-disabled"]);
            fs::write(work.join("tracked.txt"), "one\n").expect("write first revision");
            run(&work, ["add", "tracked.txt"]);
            run(&work, ["commit", "-m", "first"]);
            run(&work, ["branch", "-M", "main"]);
            run(&work, ["remote", "add", "origin", remote.to_str().expect("utf-8 temp path")]);
            run(&work, ["push", "-u", "origin", "main"]);
            Self { base, work, remote }
        }

        fn commit(&self, contents: &str) -> String {
            fs::write(self.work.join("tracked.txt"), contents).expect("write revision");
            run(&self.work, ["add", "tracked.txt"]);
            run(&self.work, ["commit", "-m", "next"]);
            run(&self.work, ["rev-parse", "HEAD"])
        }

        fn push(&self) {
            run(&self.work, ["push", "origin", "main"]);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.base).expect("remove fixture");
        }
    }

    fn run<I, S>(cwd: &Path, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = git_command().arg("-C").arg(cwd).args(args).output().expect("run git");
        assert!(
            output.status.success(),
            "git failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output is utf-8").trim().to_owned()
    }

    fn detected_after_push(fixture: &Fixture) -> Vec<super::PushDetected> {
        let repo = Repository::discover(&fixture.work)
            .expect("discover repository")
            .expect("fixture is a repository");
        let before = logical_snapshot(&repo).expect("snapshot before push");
        let head = fixture.commit("two\n");
        fixture.push();
        let after = logical_snapshot(&repo).expect("snapshot after push");
        let detected = detect_pushes(&repo, &before, &after);
        assert_eq!(detected[0].head_sha, head);
        detected
    }

    // @lat: [[test#GitHub CI Tracking#Git ref-state detection#Disabled construction creates no watcher]]
    #[test]
    fn disabled_feature_constructs_no_watcher_or_worker() {
        assert!(GitRefWatcher::start(false).expect("disabled start").is_none());
    }

    // @lat: [[test#GitHub CI Tracking#Git ref-state detection#Non-repository CWD is ignored]]
    #[test]
    fn non_repository_cwd_is_ignored() {
        let base = std::env::temp_dir()
            .join(format!("scribe-git-ref-watcher-non-repo-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&base).expect("create non-repository directory");
        let (watcher, _events) =
            GitRefWatcher::start(true).expect("start watcher").expect("enabled watcher");

        assert!(!watcher.watch_cwd(&base).expect("ignore non-repository"));
        fs::remove_dir_all(base).expect("remove non-repository directory");
    }

    // @lat: [[test#GitHub CI Tracking#Git ref-state detection#Live disable stops ref watching]]
    #[tokio::test]
    async fn live_disable_stops_ref_watching() {
        let fixture = Fixture::new();
        let control = GitRefWatcherControl::new(true);
        assert!(control.is_running());
        assert!(control.watch_cwd(&fixture.work).expect("watch repository"));
        let mut events = control.take_event_receiver().expect("push event receiver");

        assert!(control.set_enabled(false));
        assert!(!control.is_running());
        fixture.commit("two\n");
        fixture.push();
        assert!(events.recv().await.is_err(), "disabled watcher emitted a push event");
    }

    // @lat: [[test#GitHub CI Tracking#Git ref-state detection#Loose refs emit pushed head]]
    #[test]
    fn loose_remote_tracking_ref_change_emits_pushed_head_and_target() {
        let fixture = Fixture::new();
        let detected = detected_after_push(&fixture);

        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].repository_root, fixture.work);
        assert_eq!(detected[0].remote_repository.remote_name, "origin");
        assert_eq!(detected[0].remote_repository.push_url, fixture.remote.to_string_lossy());
    }

    // @lat: [[test#GitHub CI Tracking#Git ref-state detection#Packed refs use logical state]]
    #[test]
    fn packed_remote_tracking_ref_change_uses_git_logical_state() {
        let fixture = Fixture::new();
        run(&fixture.work, ["pack-refs", "--all"]);
        assert!(fixture.work.join(".git/packed-refs").is_file());
        assert!(!fixture.work.join(".git/refs/remotes/origin/main").exists());

        let detected = detected_after_push(&fixture);
        assert_eq!(detected.len(), 1);
    }

    // @lat: [[test#GitHub CI Tracking#Git ref-state detection#Packed ref rewrites do not mimic pushes]]
    #[test]
    fn packing_refs_without_an_oid_change_emits_nothing() {
        let fixture = Fixture::new();
        let repo = Repository::discover(&fixture.work)
            .expect("discover repository")
            .expect("fixture is a repository");
        let before = logical_snapshot(&repo).expect("snapshot before pack");
        run(&fixture.work, ["pack-refs", "--all"]);
        let after = logical_snapshot(&repo).expect("snapshot after pack");

        assert!(detect_pushes(&repo, &before, &after).is_empty());
    }

    // @lat: [[test#GitHub CI Tracking#Git ref-state detection#Linked worktrees resolve indirection]]
    #[test]
    fn linked_worktree_gitfile_resolves_private_and_common_dirs() {
        let fixture = Fixture::new();
        let linked = fixture.base.join("linked");
        run(
            &fixture.work,
            ["worktree", "add", "-b", "linked", linked.to_str().expect("utf-8 temp path")],
        );
        let repo = Repository::discover(&linked)
            .expect("discover linked worktree")
            .expect("fixture is a linked worktree");

        assert!(linked.join(".git").is_file());
        assert_ne!(repo.git_dir, repo.common_dir);
        assert_eq!(repo.common_dir, fixture.work.join(".git"));
        assert!(watch_paths(&repo).contains(&(repo.git_dir.clone(), RecursiveMode::NonRecursive)));

        let before = logical_snapshot(&repo).expect("snapshot linked worktree");
        fs::write(linked.join("linked.txt"), "linked\n").expect("write linked revision");
        run(&linked, ["add", "linked.txt"]);
        run(&linked, ["commit", "-m", "linked"]);
        let head = run(&linked, ["rev-parse", "HEAD"]);
        run(&linked, ["push", "origin", "HEAD:main"]);
        let after = logical_snapshot(&repo).expect("snapshot linked push");
        let detected = detect_pushes(&repo, &before, &after);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].repository_root, linked);
        assert_eq!(detected[0].head_sha, head);
        assert_eq!(detected[0].remote_repository.remote_name, "origin");
    }

    // Host Git 2.43 cannot create reftable repositories. This synthetic fixture
    // verifies the researched storage path while logical reads remain Git-owned.
    // @lat: [[test#GitHub CI Tracking#Git ref-state detection#Reftable path fixture]]
    #[test]
    fn reftable_path_is_watched_when_host_git_cannot_create_a_real_fixture() {
        let base = std::env::temp_dir()
            .join(format!("scribe-reftable-watch-path-{}", uuid::Uuid::new_v4()));
        let git_dir = base.join("git-dir");
        let common_dir = base.join("common-dir");
        fs::create_dir_all(common_dir.join("refs")).expect("create refs");
        fs::create_dir_all(common_dir.join("reftable")).expect("create reftable");
        fs::create_dir_all(&git_dir).expect("create git dir");
        let repo =
            Repository::from_resolved_paths(base.clone(), git_dir.clone(), common_dir.clone());

        let paths = watch_paths(&repo);
        assert!(paths.contains(&(git_dir, RecursiveMode::NonRecursive)));
        assert!(paths.contains(&(common_dir.clone(), RecursiveMode::NonRecursive)));
        assert!(paths.contains(&(common_dir.join("refs"), RecursiveMode::Recursive)));
        assert!(paths.contains(&(common_dir.join("reftable"), RecursiveMode::NonRecursive)));
        fs::remove_dir_all(base).expect("remove synthetic fixture");
    }

    // @lat: [[test#GitHub CI Tracking#Git ref-state detection#Watcher debounces push events]]
    #[tokio::test]
    async fn watcher_debounces_ref_burst_into_one_push_event() {
        let fixture = Fixture::new();
        let (watcher, mut events) =
            GitRefWatcher::start(true).expect("start watcher").expect("enabled watcher");
        assert!(watcher.watch_cwd(&fixture.work).expect("watch repository"));

        let head = fixture.commit("two\n");
        fixture.push();
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("push event timeout")
            .expect("watcher event channel closed");
        assert_eq!(event.repository_root, fixture.work);
        assert_eq!(event.head_sha, head);
        assert_eq!(event.remote_repository.remote_name, "origin");
        assert!(
            tokio::time::timeout(Duration::from_millis(600), events.recv()).await.is_err(),
            "one push burst emitted more than one event"
        );
    }
}
