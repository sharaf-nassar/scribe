use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::net::IpAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use scribe_common::protocol::{
    CiRunConclusion, CiRunDelta, CiRunState, CiRunStatus, CiWorkflowRun, CiWorkflowStatus,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::git_ref_watcher::PushDetected;
use crate::git_ref_watcher::{GitRefWatcherControl, PushEventReceiver};
use crate::ipc_server::{CiDismissals, WindowShares, publish_ci_run_delta};
use crate::workspace_manager::WorkspaceManager;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const DISCOVERY_WINDOW: Duration = Duration::from_mins(2);
const MAX_ATTEMPTS_PER_HOUR: usize = 720;
const MAX_WORKFLOWS: usize = 100;
const MAX_REPOSITORY_ROOTS: usize = 64;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Live GitHub CI eligibility. Enabling it performs no process or network I/O;
/// a later qualifying push owns prerequisite checks and any GitHub request.
static GITHUB_CI_ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether GitHub CI tracking is currently eligible to react to a local push.
#[must_use]
pub fn github_ci_enabled() -> bool {
    GITHUB_CI_ENABLED.load(Ordering::Relaxed)
}

/// Apply `github_ci.enabled` live, returning its previous value.
pub fn set_github_ci_enabled(enabled: bool) -> bool {
    GITHUB_CI_ENABLED.swap(enabled, Ordering::Relaxed)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct GithubRepository {
    owner: String,
    name: String,
}

impl GithubRepository {
    fn new(owner: &str, name: &str) -> Option<Self> {
        valid_repo_part(owner).then_some(())?;
        valid_repo_part(name).then_some(())?;
        Some(Self { owner: owner.to_owned(), name: name.to_owned() })
    }

    fn from_push_url(push_url: &str) -> Option<Self> {
        let path = if push_url.contains("://") {
            let url = reqwest::Url::parse(push_url).ok()?;
            if !matches!(url.scheme(), "https" | "ssh")
                || !url.host_str()?.eq_ignore_ascii_case("github.com")
            {
                return None;
            }
            url.path().trim_start_matches('/').to_owned()
        } else {
            let (host, path) = push_url.split_once(':')?;
            if !host
                .rsplit_once('@')
                .map_or(host, |(_, hostname)| hostname)
                .eq_ignore_ascii_case("github.com")
            {
                return None;
            }
            path.to_owned()
        };
        let path = path.trim_end_matches('/').strip_suffix(".git").unwrap_or(&path);
        let mut parts = path.split('/');
        let repository = Self::new(parts.next()?, parts.next()?)?;
        parts.next().is_none().then_some(repository)
    }

    fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

fn valid_repo_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 100
        && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

#[must_use]
pub fn aggregate_status(workflows: &[CiWorkflowRun]) -> CiRunStatus {
    if workflows.iter().any(|run| run.status == CiWorkflowStatus::InProgress) {
        return CiRunStatus::Running;
    }
    if workflows.iter().any(|run| run.status == CiWorkflowStatus::Queued) {
        return CiRunStatus::Queued;
    }
    if workflows
        .iter()
        .any(|run| run.conclusion.is_none_or(|value| value == CiRunConclusion::Failure))
    {
        return CiRunStatus::Failure;
    }
    if workflows.iter().any(|run| run.conclusion == Some(CiRunConclusion::Cancelled)) {
        return CiRunStatus::Cancelled;
    }
    CiRunStatus::Success
}

#[derive(Default)]
pub struct PollScheduler {
    attempts: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

impl PollScheduler {
    fn ready(&mut self, now: Instant) -> bool {
        self.discard_old_attempts(now);
        self.ready_at().is_none_or(|ready| ready <= now)
    }

    fn record(&mut self, now: Instant) {
        self.discard_old_attempts(now);
        self.attempts.push_back(now);
    }

    fn ready_at(&self) -> Option<Instant> {
        let cadence = self.attempts.back().and_then(|last| last.checked_add(POLL_INTERVAL));
        let hourly = (self.attempts.len() >= MAX_ATTEMPTS_PER_HOUR)
            .then(|| {
                self.attempts.front().and_then(|first| first.checked_add(Duration::from_hours(1)))
            })
            .flatten();
        [cadence, hourly, self.blocked_until].into_iter().flatten().max()
    }

    fn discard_old_attempts(&mut self, now: Instant) {
        while self.attempts.front().is_some_and(|attempt| {
            now.saturating_duration_since(*attempt) >= Duration::from_hours(1)
        }) {
            self.attempts.pop_front();
        }
    }
}

pub struct SecretToken(String);

impl SecretToken {
    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

pub struct ApiRequest {
    url: reqwest::Url,
    head_sha: String,
}

impl ApiRequest {
    #[cfg(test)]
    fn for_test(repository: &GithubRepository, head_sha: &str) -> Self {
        let url = reqwest::Url::parse(&format!(
            "https://api.github.com/repos/{}/actions/runs",
            repository.full_name()
        ))
        .expect("test URL");
        Self { url, head_sha: head_sha.to_owned() }
    }
}

pub enum ApiResponse {
    NotModified,
    Runs { etag: Option<String>, branch: String, workflows: Vec<CiWorkflowRun> },
}

struct RunsObservation {
    at: Instant,
    epoch_secs: u64,
    etag: Option<String>,
    branch: String,
    workflows: Vec<CiWorkflowRun>,
}

#[derive(Debug)]
pub enum ApiError {
    Authentication,
    Permission,
    Transient,
    RateLimited(Duration),
    UntrustedEndpoint,
    InvalidPush,
}

pub trait GithubApi: Send + Sync {
    fn prepare(
        &self,
        repository: &GithubRepository,
        head_sha: &str,
    ) -> Result<ApiRequest, ApiError>;

    fn authenticate(&self) -> BoxFuture<'_, Result<SecretToken, ApiError>>;

    fn fetch<'a>(
        &'a self,
        request: &'a ApiRequest,
        token: &'a SecretToken,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ApiResponse, ApiError>>;
}

pub struct HttpGithubApi {
    http: reqwest::Client,
    override_base: Option<String>,
}

impl HttpGithubApi {
    fn new() -> Self {
        Self {
            http: crate::updater::http_client().clone(),
            override_base: std::env::var("SCRIBE_GITHUB_API_URL").ok(),
        }
    }

    #[cfg(test)]
    fn with_override(override_base: Option<String>) -> Self {
        Self { http: reqwest::Client::new(), override_base }
    }

    fn request_url(
        &self,
        repository: &GithubRepository,
        head_sha: &str,
    ) -> Result<reqwest::Url, ApiError> {
        if head_sha.is_empty()
            || head_sha.len() > 128
            || !head_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ApiError::InvalidPush);
        }
        let mut url = match self.override_base.as_deref() {
            None => reqwest::Url::parse("https://api.github.com")
                .map_err(|_| ApiError::UntrustedEndpoint)?,
            Some(base) => {
                let url = reqwest::Url::parse(base).map_err(|_| ApiError::UntrustedEndpoint)?;
                let loopback = url
                    .host_str()
                    .and_then(|host| host.parse::<IpAddr>().ok())
                    .is_some_and(|address| address.is_loopback());
                if !loopback
                    || !matches!(url.scheme(), "http" | "https")
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || !matches!(url.path(), "" | "/")
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err(ApiError::UntrustedEndpoint);
                }
                url
            }
        };
        url.set_path(&format!("/repos/{}/{}/actions/runs", repository.owner, repository.name));
        url.query_pairs_mut()
            .append_pair("head_sha", head_sha)
            .append_pair("event", "push")
            .append_pair("per_page", "100");
        Ok(url)
    }
}

impl GithubApi for HttpGithubApi {
    fn prepare(
        &self,
        repository: &GithubRepository,
        head_sha: &str,
    ) -> Result<ApiRequest, ApiError> {
        Ok(ApiRequest {
            url: self.request_url(repository, head_sha)?,
            head_sha: head_sha.to_owned(),
        })
    }

    fn authenticate(&self) -> BoxFuture<'_, Result<SecretToken, ApiError>> {
        Box::pin(async {
            let output = tokio::process::Command::new("gh")
                .args(["auth", "token", "--hostname", "github.com"])
                .env("GH_PROMPT_DISABLED", "1")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|_| ApiError::Authentication)?;
            if !output.status.success() {
                return Err(ApiError::Authentication);
            }
            let token = String::from_utf8(output.stdout).map_err(|_| ApiError::Authentication)?;
            let token = token.trim();
            if token.is_empty() || token.contains(['\r', '\n']) {
                return Err(ApiError::Authentication);
            }
            Ok(SecretToken::new(token))
        })
    }

    fn fetch<'a>(
        &'a self,
        request: &'a ApiRequest,
        token: &'a SecretToken,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ApiResponse, ApiError>> {
        Box::pin(async move {
            let mut pending = self
                .http
                .get(request.url.clone())
                .bearer_auth(&token.0)
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            if let Some(etag) = etag {
                pending = pending.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            let response = pending.send().await.map_err(|_| ApiError::Transient)?;
            let status = response.status();
            if status == reqwest::StatusCode::NOT_MODIFIED {
                return Ok(ApiResponse::NotModified);
            }
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(ApiError::Authentication);
            }
            if !status.is_success()
                && let Some(delay) = retry_delay(response.headers(), epoch_secs())
            {
                return Err(ApiError::RateLimited(delay));
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(ApiError::RateLimited(Duration::from_mins(1)));
            }
            if matches!(status, reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND) {
                return Err(ApiError::Permission);
            }
            if !status.is_success() {
                return Err(ApiError::Transient);
            }
            let response_etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.len() <= 1_024)
                .map(str::to_owned);
            let body: GithubRunsResponse =
                response.json().await.map_err(|_| ApiError::Transient)?;
            Ok(body.into_response(&request.head_sha, response_etag))
        })
    }
}

#[derive(Deserialize)]
struct GithubRunsResponse {
    workflow_runs: Vec<GithubWorkflowRun>,
}

#[derive(Deserialize)]
struct GithubWorkflowRun {
    id: u64,
    name: String,
    head_sha: String,
    #[serde(default)]
    head_branch: String,
    status: String,
    conclusion: Option<String>,
}

impl GithubRunsResponse {
    fn into_response(self, head_sha: &str, etag: Option<String>) -> ApiResponse {
        let mut branch = String::new();
        let workflows = self
            .workflow_runs
            .into_iter()
            .filter(|run| run.head_sha == head_sha)
            .take(MAX_WORKFLOWS)
            .map(|run| {
                if branch.is_empty() {
                    branch = truncate_text(&run.head_branch, 256);
                }
                CiWorkflowRun {
                    run_id: run.id,
                    name: truncate_text(&run.name, 256),
                    status: match run.status.as_str() {
                        "completed" => CiWorkflowStatus::Completed,
                        "in_progress" => CiWorkflowStatus::InProgress,
                        _ => CiWorkflowStatus::Queued,
                    },
                    conclusion: run.conclusion.as_deref().map(|conclusion| match conclusion {
                        "success" | "neutral" | "skipped" => CiRunConclusion::Success,
                        "cancelled" => CiRunConclusion::Cancelled,
                        _ => CiRunConclusion::Failure,
                    }),
                    started_at_epoch_secs: None,
                    updated_at_epoch_secs: None,
                }
            })
            .collect();
        ApiResponse::Runs { etag, branch, workflows }
    }
}

fn truncate_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn retry_delay(headers: &reqwest::header::HeaderMap, now_epoch_secs: u64) -> Option<Duration> {
    if let Some(seconds) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Some(Duration::from_secs(seconds));
    }
    headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|reset| Duration::from_secs(reset.saturating_sub(now_epoch_secs)))
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

struct PollWindow {
    repository: GithubRepository,
    roots: BTreeSet<PathBuf>,
    head_sha: String,
    discovery_deadline: Option<Instant>,
    next_attempt: Instant,
    etag: Option<String>,
    observed: Option<CiRunState>,
    offline_backoff: Duration,
    transient_logged: bool,
}

impl PollWindow {
    fn new(repository: GithubRepository, root: PathBuf, head_sha: String, now: Instant) -> Self {
        Self {
            repository,
            roots: BTreeSet::from([root]),
            head_sha,
            discovery_deadline: now.checked_add(DISCOVERY_WINDOW),
            next_attempt: now,
            etag: None,
            observed: None,
            offline_backoff: POLL_INTERVAL,
            transient_logged: false,
        }
    }
}

/// Minimal active tracker state carried across a hot upgrade. It contains no
/// credential and the successor re-polls each descriptor before normal cadence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HandoffCiWindow {
    repository: GithubRepository,
    roots: Vec<PathBuf>,
    head_sha: String,
    discovery_remaining_secs: Option<u64>,
    last_state: Option<CiRunState>,
}

#[derive(Clone, Default)]
pub struct GithubCiTrackerHandle {
    active: std::sync::Arc<std::sync::Mutex<Vec<HandoffCiWindow>>>,
}

impl GithubCiTrackerHandle {
    #[must_use]
    pub fn handoff_windows(&self) -> Vec<HandoffCiWindow> {
        self.active.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn replace(&self, windows: Vec<HandoffCiWindow>) {
        *self.active.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = windows;
    }
}

struct PushResult {
    #[cfg(test)]
    new_head: bool,
    publications: Vec<(PathBuf, CiRunDelta)>,
}

enum PollStep {
    Config(Result<(), tokio::sync::watch::error::RecvError>),
    Push(Option<Result<PushDetected, tokio::sync::broadcast::error::RecvError>>),
    Complete(Vec<(PathBuf, CiRunDelta)>),
}

#[derive(Default)]
pub struct GithubCiTracker {
    windows: HashMap<GithubRepository, PollWindow>,
    scheduler: PollScheduler,
    token: Option<SecretToken>,
}

impl GithubCiTracker {
    fn accept_push(&mut self, push: PushDetected, now: Instant) -> Option<PushResult> {
        let Some(repository) = GithubRepository::from_push_url(&push.remote_repository.push_url)
        else {
            debug!(
                repository_root = %push.repository_root.display(),
                remote = %push.remote_repository.remote_name,
                "ignoring local push whose target is not github.com"
            );
            return None;
        };
        if let Some(window) = self.windows.get_mut(&repository)
            && window.head_sha == push.head_sha
        {
            if window.roots.len() < MAX_REPOSITORY_ROOTS {
                window.roots.insert(push.repository_root);
            }
            return Some(PushResult {
                #[cfg(test)]
                new_head: false,
                publications: Vec::new(),
            });
        }
        let mut roots = BTreeSet::from([push.repository_root.clone()]);
        let publications = self.windows.remove(&repository).map_or_else(Vec::new, |previous| {
            roots.extend(previous.roots.iter().take(MAX_REPOSITORY_ROOTS).cloned());
            previous.observed.map_or_else(Vec::new, |state| {
                publications(&previous.roots, &CiRunDelta::Cleared { head_sha: state.head_sha })
            })
        });
        self.windows.insert(
            repository.clone(),
            PollWindow {
                roots,
                ..PollWindow::new(repository, push.repository_root, push.head_sha, now)
            },
        );
        Some(PushResult {
            #[cfg(test)]
            new_head: true,
            publications,
        })
    }

    async fn poll_one(
        &mut self,
        now: Instant,
        observed_epoch_secs: u64,
        api: &dyn GithubApi,
    ) -> Vec<(PathBuf, CiRunDelta)> {
        self.expire_discovery(now);
        if !self.scheduler.ready(now) {
            return Vec::new();
        }
        let Some(key) = self
            .windows
            .iter()
            .filter(|(_, window)| window.next_attempt <= now)
            .min_by_key(|(_, window)| window.next_attempt)
            .map(|(key, _)| key.clone())
        else {
            return Vec::new();
        };

        let request = {
            let Some(window) = self.windows.get(&key) else {
                return Vec::new();
            };
            match api.prepare(&window.repository, &window.head_sha) {
                Ok(request) => request,
                Err(error) => {
                    let observed = window.observed.is_some();
                    warn_tracker_error(&key, &error, observed);
                    return self.stop_failed_window(&key);
                }
            }
        };
        if self.token.is_none() {
            match api.authenticate().await {
                Ok(token) => self.token = Some(token),
                Err(error) => {
                    let observed =
                        self.windows.get(&key).is_some_and(|window| window.observed.is_some());
                    warn_tracker_error(&key, &error, observed);
                    return self.stop_failed_window(&key);
                }
            }
        }
        self.scheduler.record(now);
        let Some(token) = self.token.as_ref() else {
            return Vec::new();
        };
        let etag = self.windows.get(&key).and_then(|window| window.etag.as_deref());
        match api.fetch(&request, token, etag).await {
            Ok(ApiResponse::NotModified) => {
                if let Some(window) = self.windows.get_mut(&key) {
                    window.next_attempt = now + POLL_INTERVAL;
                }
                Vec::new()
            }
            Ok(ApiResponse::Runs { etag, branch, workflows }) => self.apply_runs(
                &key,
                RunsObservation {
                    at: now,
                    epoch_secs: observed_epoch_secs,
                    etag,
                    branch,
                    workflows,
                },
            ),
            Err(ApiError::Authentication) => {
                warn!(repository = %key.full_name(), "GitHub CI polling lost authentication; stopping window");
                self.token = None;
                self.stop_failed_window(&key)
            }
            Err(ApiError::Permission) => {
                warn!(repository = %key.full_name(), "GitHub CI account cannot read repository Actions; stopping window");
                self.stop_failed_window(&key)
            }
            Err(ApiError::RateLimited(delay)) => {
                self.scheduler.blocked_until = now.checked_add(delay);
                self.mark_stale_or_backoff(&key, now)
            }
            Err(ApiError::Transient) => self.mark_stale_or_backoff(&key, now),
            Err(ApiError::UntrustedEndpoint | ApiError::InvalidPush) => {
                self.windows.remove(&key);
                Vec::new()
            }
        }
    }

    fn apply_runs(
        &mut self,
        key: &GithubRepository,
        mut observation: RunsObservation,
    ) -> Vec<(PathBuf, CiRunDelta)> {
        let Some(window) = self.windows.get_mut(key) else {
            return Vec::new();
        };
        window.etag = observation.etag;
        window.next_attempt = observation.at + POLL_INTERVAL;
        window.offline_backoff = POLL_INTERVAL;
        window.transient_logged = false;
        if observation.workflows.is_empty() {
            return Vec::new();
        }
        observation.workflows.truncate(MAX_WORKFLOWS);
        let first_seen = window.observed.as_ref().map(|state| {
            state
                .workflows
                .iter()
                .map(|workflow| (workflow.run_id, workflow.started_at_epoch_secs))
                .collect::<HashMap<_, _>>()
        });
        for workflow in &mut observation.workflows {
            workflow.started_at_epoch_secs = first_seen
                .as_ref()
                .and_then(|seen| seen.get(&workflow.run_id).copied().flatten())
                .or(Some(observation.epoch_secs));
            workflow.updated_at_epoch_secs = Some(observation.epoch_secs);
        }
        let state = CiRunState {
            repository: key.full_name(),
            head_sha: window.head_sha.clone(),
            branch: observation.branch,
            rollup: aggregate_status(&observation.workflows),
            workflows: observation.workflows,
            stale: false,
        };
        window.discovery_deadline = None;
        window.observed = Some(state.clone());
        let publications = publications(&window.roots, &CiRunDelta::Set(state.clone()));
        if matches!(
            state.rollup,
            CiRunStatus::Success | CiRunStatus::Failure | CiRunStatus::Cancelled
        ) {
            self.windows.remove(key);
        }
        publications
    }

    fn mark_stale_or_backoff(
        &mut self,
        key: &GithubRepository,
        now: Instant,
    ) -> Vec<(PathBuf, CiRunDelta)> {
        let Some(window) = self.windows.get_mut(key) else {
            return Vec::new();
        };
        window.next_attempt = now + window.offline_backoff;
        window.offline_backoff = (window.offline_backoff * 2).min(Duration::from_secs(30));
        if !window.transient_logged {
            warn!(repository = %key.full_name(), "GitHub CI request failed; retrying with bounded backoff");
            window.transient_logged = true;
        }
        let Some(mut state) = window.observed.clone() else {
            return Vec::new();
        };
        if state.stale {
            return Vec::new();
        }
        state.stale = true;
        window.observed = Some(state.clone());
        publications(&window.roots, &CiRunDelta::Set(state))
    }

    fn stop_failed_window(&mut self, key: &GithubRepository) -> Vec<(PathBuf, CiRunDelta)> {
        let Some(window) = self.windows.remove(key) else {
            return Vec::new();
        };
        let Some(mut state) = window.observed else {
            return Vec::new();
        };
        state.stale = true;
        publications(&window.roots, &CiRunDelta::Set(state))
    }

    fn expire_discovery(&mut self, now: Instant) {
        self.windows
            .retain(|_, window| window.discovery_deadline.is_none_or(|deadline| deadline > now));
    }

    fn clear(&mut self) -> Vec<(PathBuf, CiRunDelta)> {
        self.token = None;
        self.windows
            .drain()
            .flat_map(|(_, window)| {
                window.observed.map_or_else(Vec::new, |state| {
                    publications(&window.roots, &CiRunDelta::Cleared { head_sha: state.head_sha })
                })
            })
            .collect()
    }

    fn next_wake(&self, now: Instant) -> Option<Instant> {
        let next_poll = self
            .windows
            .values()
            .map(|window| window.next_attempt)
            .min()
            .map(|due| self.scheduler.ready_at().map_or(due, |ready| due.max(ready)));
        let expiry = self.windows.values().filter_map(|window| window.discovery_deadline).min();
        [next_poll, expiry].into_iter().flatten().min().map(|wake| wake.max(now))
    }

    fn handoff_windows(&self, now: Instant) -> Vec<HandoffCiWindow> {
        self.windows
            .values()
            .map(|window| HandoffCiWindow {
                repository: window.repository.clone(),
                roots: window.roots.iter().cloned().collect(),
                head_sha: window.head_sha.clone(),
                discovery_remaining_secs: window
                    .discovery_deadline
                    .map(|deadline| deadline.saturating_duration_since(now).as_secs()),
                last_state: window.observed.clone(),
            })
            .collect()
    }

    fn restore(windows: Vec<HandoffCiWindow>, now: Instant) -> Self {
        let mut tracker = Self::default();
        for saved in windows {
            if saved.roots.is_empty()
                || saved.repository.owner.is_empty()
                || saved.repository.name.is_empty()
                || saved.head_sha.is_empty()
            {
                continue;
            }
            let Some(root) = saved.roots.first().cloned() else {
                continue;
            };
            let mut window = PollWindow::new(
                saved.repository.clone(),
                root,
                saved.head_sha,
                now + Duration::from_secs(1),
            );
            window.roots = saved.roots.into_iter().take(MAX_REPOSITORY_ROOTS).collect();
            window.next_attempt = now + Duration::from_secs(1);
            window.discovery_deadline = saved.discovery_remaining_secs.and_then(|remaining| {
                now.checked_add(Duration::from_secs(remaining.min(DISCOVERY_WINDOW.as_secs())))
            });
            window.observed = saved.last_state.map(|mut state| {
                state.stale = false;
                state
            });
            tracker.windows.insert(saved.repository, window);
        }
        tracker
    }

    #[cfg(test)]
    fn window_count(&self) -> usize {
        self.windows.len()
    }

    #[cfg(test)]
    fn roots_for(&self, full_name: &str) -> Vec<PathBuf> {
        self.windows
            .iter()
            .find(|(repository, _)| repository.full_name() == full_name)
            .map_or_else(Vec::new, |(_, window)| window.roots.iter().cloned().collect())
    }

    #[cfg(test)]
    fn head_for(&self, full_name: &str) -> Option<&str> {
        self.windows
            .iter()
            .find(|(repository, _)| repository.full_name() == full_name)
            .map(|(_, window)| window.head_sha.as_str())
    }
}

fn warn_tracker_error(repository: &GithubRepository, error: &ApiError, observed: bool) {
    let reason = match error {
        ApiError::Authentication => "gh is unavailable or not authenticated for github.com",
        ApiError::Permission => "the active GitHub account cannot read this repository's Actions",
        ApiError::UntrustedEndpoint => "SCRIBE_GITHUB_API_URL is not a loopback URL",
        ApiError::InvalidPush => "the pushed Git object ID is invalid",
        ApiError::Transient => "the GitHub API is temporarily unavailable",
        ApiError::RateLimited(_) => "GitHub API rate limit delayed polling",
    };
    warn!(repository = %repository.full_name(), observed, reason, "GitHub CI window stopped");
}

/// Start the dormant push-gated tracker. Construction performs no subprocess
/// or HTTP work; both begin only after a qualifying watcher event.
pub fn spawn_tracker(
    git_ref_watcher: std::sync::Arc<GitRefWatcherControl>,
    workspace_manager: std::sync::Arc<RwLock<WorkspaceManager>>,
    window_shares: WindowShares,
    dismissals: CiDismissals,
    restored: Vec<HandoffCiWindow>,
) -> GithubCiTrackerHandle {
    let handle = GithubCiTrackerHandle::default();
    let task_handle = handle.clone();
    tokio::spawn(async move {
        run_tracker(
            TrackerRuntime {
                git_ref_watcher,
                workspace_manager,
                window_shares,
                dismissals,
                handle: task_handle,
            },
            restored,
            HttpGithubApi::new(),
        )
        .await;
    });
    handle
}

struct TrackerRuntime {
    git_ref_watcher: std::sync::Arc<GitRefWatcherControl>,
    workspace_manager: std::sync::Arc<RwLock<WorkspaceManager>>,
    window_shares: WindowShares,
    dismissals: CiDismissals,
    handle: GithubCiTrackerHandle,
}

async fn run_tracker(runtime: TrackerRuntime, restored: Vec<HandoffCiWindow>, api: impl GithubApi) {
    let TrackerRuntime { git_ref_watcher, workspace_manager, window_shares, dismissals, handle } =
        runtime;
    let mut tracker = GithubCiTracker::restore(restored, Instant::now());
    let mut config_rx = git_ref_watcher.subscribe_changes();
    let mut pushes = None;
    loop {
        if !github_ci_enabled() || !git_ref_watcher.is_running() {
            pushes = None;
            let publications = tracker.clear();
            handle.replace(tracker.handoff_windows(Instant::now()));
            publish_all(&workspace_manager, &window_shares, &dismissals, publications).await;
        } else if pushes.is_none() {
            pushes = git_ref_watcher.take_event_receiver();
        }
        handle.replace(tracker.handoff_windows(Instant::now()));

        let wake = tracker.next_wake(Instant::now());
        tokio::select! {
            biased;
            transition = config_rx.changed() => {
                if transition.is_err() {
                    return;
                }
            }
            event = receive_push(&mut pushes) => {
                match event {
                    Some(Ok(push)) => {
                        if let Some(result) = tracker.accept_push(push, Instant::now()) {
                            handle.replace(tracker.handoff_windows(Instant::now()));
                            publish_all(&workspace_manager, &window_shares, &dismissals, result.publications).await;
                        }
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                        warn!(skipped, "GitHub CI push receiver lagged");
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => pushes = None,
                    None => {}
                }
            }
            () = sleep_until(wake) => {
                let step = {
                    let polling = tracker.poll_one(Instant::now(), epoch_secs(), &api);
                    tokio::pin!(polling);
                    tokio::select! {
                        biased;
                        transition = config_rx.changed() => PollStep::Config(transition),
                        event = receive_push(&mut pushes) => PollStep::Push(event),
                        publications = &mut polling => PollStep::Complete(publications),
                    }
                };
                match step {
                    PollStep::Config(Err(_)) => return,
                    PollStep::Config(Ok(())) | PollStep::Push(None) => {}
                    PollStep::Push(Some(Ok(push))) => {
                        if let Some(result) = tracker.accept_push(push, Instant::now()) {
                            handle.replace(tracker.handoff_windows(Instant::now()));
                            publish_all(&workspace_manager, &window_shares, &dismissals, result.publications).await;
                        }
                    }
                    PollStep::Push(Some(Err(tokio::sync::broadcast::error::RecvError::Closed))) => {
                        pushes = None;
                    }
                    PollStep::Push(Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)))) => {
                        warn!(skipped, "GitHub CI push receiver lagged");
                    }
                    PollStep::Complete(publications) => {
                        handle.replace(tracker.handoff_windows(Instant::now()));
                        publish_all(&workspace_manager, &window_shares, &dismissals, publications).await;
                    }
                }
            }
        }
    }
}

async fn publish_all(
    workspace_manager: &std::sync::Arc<RwLock<WorkspaceManager>>,
    window_shares: &WindowShares,
    dismissals: &CiDismissals,
    publications: Vec<(PathBuf, CiRunDelta)>,
) {
    for (root, delta) in publications {
        publish_ci_run_delta(workspace_manager, window_shares, dismissals, &root, delta).await;
    }
}

async fn receive_push(
    receiver: &mut Option<PushEventReceiver>,
) -> Option<Result<PushDetected, tokio::sync::broadcast::error::RecvError>> {
    match receiver {
        Some(receiver) => Some(receiver.recv().await),
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

fn publications(roots: &BTreeSet<PathBuf>, delta: &CiRunDelta) -> Vec<(PathBuf, CiRunDelta)> {
    roots.iter().cloned().map(|root| (root, delta.clone())).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use scribe_common::protocol::{
        CiRunConclusion, CiRunDelta, CiRunStatus, CiWorkflowRun, CiWorkflowStatus,
    };

    use super::{
        ApiError, ApiRequest, ApiResponse, BoxFuture, GithubApi, GithubCiTracker, GithubRepository,
        HttpGithubApi, PollScheduler, github_ci_enabled, set_github_ci_enabled,
    };
    use crate::git_ref_watcher::{PushDetected, RemoteRepository};

    fn push(url: &str, root: &str, head: &str) -> PushDetected {
        PushDetected {
            repository_root: root.into(),
            head_sha: head.into(),
            remote_repository: RemoteRepository {
                remote_name: "upstream".into(),
                push_url: url.into(),
            },
        }
    }

    fn run(
        id: u64,
        status: CiWorkflowStatus,
        conclusion: Option<CiRunConclusion>,
    ) -> CiWorkflowRun {
        CiWorkflowRun {
            run_id: id,
            name: format!("workflow-{id}"),
            status,
            conclusion,
            started_at_epoch_secs: None,
            updated_at_epoch_secs: None,
        }
    }

    // @lat: [[test#GitHub CI Opt-in#Live projection]]
    #[test]
    fn live_projection_follows_config_changes() {
        set_github_ci_enabled(false);
        assert!(!github_ci_enabled());

        set_github_ci_enabled(true);
        assert!(github_ci_enabled());

        set_github_ci_enabled(false);
    }

    // @lat: [[test#GitHub CI Tracking#Push-target repository resolution]]
    #[test]
    fn canonicalizes_supported_github_push_urls_and_rejects_other_hosts() {
        let expected = GithubRepository::new("acme", "widget").unwrap();
        for url in [
            "https://github.com/acme/widget.git",
            "ssh://git@github.com/acme/widget.git",
            "git@github.com:acme/widget.git",
        ] {
            assert_eq!(GithubRepository::from_push_url(url), Some(expected.clone()), "{url}");
        }
        assert_eq!(
            GithubRepository::from_push_url("git@github.com:fork-owner/widget.git"),
            GithubRepository::new("fork-owner", "widget")
        );
        assert_eq!(GithubRepository::from_push_url("https://gitlab.com/acme/widget.git"), None);
        assert_eq!(GithubRepository::from_push_url("https://github.com/acme/widget/extra"), None);
    }

    // @lat: [[test#GitHub CI Tracking#Workflow rollup]]
    #[test]
    fn rollup_waits_for_all_runs_then_uses_worst_terminal_result() {
        assert_eq!(
            super::aggregate_status(&[
                run(1, CiWorkflowStatus::Completed, Some(CiRunConclusion::Failure)),
                run(2, CiWorkflowStatus::InProgress, None),
            ]),
            CiRunStatus::Running
        );
        assert_eq!(
            super::aggregate_status(&[
                run(1, CiWorkflowStatus::Completed, Some(CiRunConclusion::Success)),
                run(2, CiWorkflowStatus::Completed, Some(CiRunConclusion::Cancelled)),
                run(3, CiWorkflowStatus::Completed, Some(CiRunConclusion::Failure)),
            ]),
            CiRunStatus::Failure
        );
        assert_eq!(
            super::aggregate_status(&[
                run(1, CiWorkflowStatus::Completed, Some(CiRunConclusion::Success)),
                run(2, CiWorkflowStatus::Completed, Some(CiRunConclusion::Cancelled)),
            ]),
            CiRunStatus::Cancelled
        );
        assert_eq!(
            super::aggregate_status(&[run(1, CiWorkflowStatus::Completed, None)]),
            CiRunStatus::Failure
        );
    }

    // @lat: [[test#GitHub CI Tracking#Shared latest-head window]]
    #[test]
    fn same_remote_repo_deduplicates_roots_and_latest_head_replaces_old_window() {
        let now = Instant::now();
        let mut tracker = GithubCiTracker::default();
        assert!(
            tracker
                .accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), now)
                .unwrap()
                .new_head
        );
        assert!(
            !tracker
                .accept_push(push("https://github.com/acme/widget.git", "/b", "head-a"), now,)
                .unwrap()
                .new_head
        );
        assert_eq!(tracker.window_count(), 1);
        assert_eq!(
            tracker.roots_for("acme/widget"),
            vec![std::path::PathBuf::from("/a"), std::path::PathBuf::from("/b")]
        );

        assert!(
            tracker
                .accept_push(
                    push("ssh://git@github.com/acme/widget", "/b", "head-b"),
                    now + Duration::from_secs(1),
                )
                .unwrap()
                .new_head
        );
        assert_eq!(tracker.head_for("acme/widget"), Some("head-b"));
    }

    // @lat: [[test#GitHub CI Tracking#Server-wide request budget]]
    #[test]
    fn scheduler_enforces_five_seconds_and_rolling_hour_ceiling() {
        let start = Instant::now();
        let mut scheduler = PollScheduler::default();
        assert!(scheduler.ready(start));
        scheduler.record(start);
        assert!(!scheduler.ready(start + Duration::from_millis(4_999)));
        assert!(scheduler.ready(start + Duration::from_secs(5)));

        for attempt in 1..720 {
            scheduler.record(start + Duration::from_secs(attempt * 5));
        }
        assert!(!scheduler.ready(start + Duration::from_secs(3_599)));
        assert!(scheduler.ready(start + Duration::from_hours(1)));
    }

    #[derive(Default)]
    struct CountingApi {
        auth_calls: AtomicUsize,
        http_calls: AtomicUsize,
        fail_auth: bool,
    }

    impl GithubApi for CountingApi {
        fn prepare(
            &self,
            repository: &GithubRepository,
            head_sha: &str,
        ) -> Result<ApiRequest, ApiError> {
            Ok(ApiRequest::for_test(repository, head_sha))
        }

        fn authenticate(&self) -> BoxFuture<'_, Result<super::SecretToken, ApiError>> {
            self.auth_calls.fetch_add(1, Ordering::Relaxed);
            let result = (!self.fail_auth)
                .then(|| super::SecretToken::new("test-token"))
                .ok_or(ApiError::Authentication);
            Box::pin(async move { result })
        }

        fn fetch<'a>(
            &'a self,
            _request: &'a ApiRequest,
            _token: &'a super::SecretToken,
            _etag: Option<&'a str>,
        ) -> BoxFuture<'a, Result<ApiResponse, ApiError>> {
            self.http_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(ApiResponse::NotModified) })
        }
    }

    // @lat: [[test#GitHub CI Tracking#Zero-idle and auth-failure boundary]]
    #[tokio::test]
    async fn idle_and_auth_failure_never_reach_http() {
        let now = Instant::now();
        let api = Arc::new(CountingApi::default());
        let mut idle = GithubCiTracker::default();
        assert!(idle.poll_one(now, 100, &*api).await.is_empty());
        assert_eq!(api.auth_calls.load(Ordering::Relaxed), 0);
        assert_eq!(api.http_calls.load(Ordering::Relaxed), 0);

        let failing = CountingApi { fail_auth: true, ..CountingApi::default() };
        let mut tracker = GithubCiTracker::default();
        tracker.accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), now);
        assert!(tracker.poll_one(now, 100, &failing).await.is_empty());
        assert_eq!(failing.auth_calls.load(Ordering::Relaxed), 1);
        assert_eq!(failing.http_calls.load(Ordering::Relaxed), 0);
        assert_eq!(tracker.window_count(), 0);
    }

    // @lat: [[test#GitHub CI Tracking#Trusted API request]]
    #[test]
    fn api_request_is_exact_and_override_is_loopback_only() {
        let repository = GithubRepository::new("acme", "widget").unwrap();
        let production = HttpGithubApi::with_override(None);
        assert_eq!(
            production.prepare(&repository, "abc123").unwrap().url.as_str(),
            "https://api.github.com/repos/acme/widget/actions/runs?head_sha=abc123&event=push&per_page=100"
        );
        let fixture = HttpGithubApi::with_override(Some("http://127.0.0.1:8098".into()));
        assert_eq!(
            fixture.prepare(&repository, "abc123").unwrap().url.as_str(),
            "http://127.0.0.1:8098/repos/acme/widget/actions/runs?head_sha=abc123&event=push&per_page=100"
        );
        let attacker = HttpGithubApi::with_override(Some("https://example.com".into()));
        assert!(matches!(
            attacker.prepare(&repository, "abc123"),
            Err(ApiError::UntrustedEndpoint)
        ));
    }

    #[derive(Default)]
    struct RunsApi {
        auth_calls: AtomicUsize,
        http_calls: AtomicUsize,
        responses: std::sync::Mutex<VecDeque<ApiResponse>>,
        etags: std::sync::Mutex<Vec<Option<String>>>,
    }

    impl RunsApi {
        fn with_responses(responses: Vec<ApiResponse>) -> Self {
            Self { responses: std::sync::Mutex::new(responses.into()), ..Self::default() }
        }
    }

    impl GithubApi for RunsApi {
        fn prepare(
            &self,
            repository: &GithubRepository,
            head_sha: &str,
        ) -> Result<ApiRequest, ApiError> {
            Ok(ApiRequest::for_test(repository, head_sha))
        }

        fn authenticate(&self) -> BoxFuture<'_, Result<super::SecretToken, ApiError>> {
            self.auth_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(super::SecretToken::new("test-token")) })
        }

        fn fetch<'a>(
            &'a self,
            _request: &'a ApiRequest,
            _token: &'a super::SecretToken,
            etag: Option<&'a str>,
        ) -> BoxFuture<'a, Result<ApiResponse, ApiError>> {
            self.http_calls.fetch_add(1, Ordering::Relaxed);
            self.etags.lock().unwrap().push(etag.map(str::to_owned));
            Box::pin(async move {
                self.responses.lock().unwrap().pop_front().ok_or(ApiError::Transient)
            })
        }
    }

    fn runs_response(status: CiWorkflowStatus, conclusion: Option<CiRunConclusion>) -> ApiResponse {
        ApiResponse::Runs {
            etag: Some("etag".into()),
            branch: "main".into(),
            workflows: vec![run(1, status, conclusion)],
        }
    }

    // @lat: [[test#GitHub CI Tracking#Observation timestamps and terminal stop]]
    #[tokio::test]
    async fn observations_keep_first_seen_time_and_stop_on_terminal() {
        let start = Instant::now();
        let api = RunsApi::with_responses(vec![
            runs_response(CiWorkflowStatus::Queued, None),
            runs_response(CiWorkflowStatus::Completed, Some(CiRunConclusion::Success)),
        ]);
        let mut tracker = GithubCiTracker::default();
        tracker.accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), start);

        let first = tracker.poll_one(start, 1_000, &api).await;
        let CiRunDelta::Set(first) = &first[0].1 else { panic!("expected state") };
        assert_eq!(first.workflows[0].started_at_epoch_secs, Some(1_000));
        assert_eq!(first.workflows[0].updated_at_epoch_secs, Some(1_000));

        let second = tracker.poll_one(start + Duration::from_secs(5), 1_005, &api).await;
        let CiRunDelta::Set(second) = &second[0].1 else { panic!("expected state") };
        assert_eq!(second.workflows[0].started_at_epoch_secs, Some(1_000));
        assert_eq!(second.workflows[0].updated_at_epoch_secs, Some(1_005));
        assert_eq!(second.rollup, CiRunStatus::Success);
        assert_eq!(api.etags.lock().unwrap().as_slice(), &[None, Some("etag".into())]);
        assert_eq!(tracker.window_count(), 0);
    }

    // @lat: [[test#GitHub CI Tracking#Rate-limit delay]]
    #[test]
    fn retry_after_precedes_rate_reset_without_a_quota_probe() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1060".parse().unwrap());
        assert_eq!(super::retry_delay(&headers, 1_000), Some(Duration::from_secs(30)));

        headers.remove(reqwest::header::RETRY_AFTER);
        assert_eq!(super::retry_delay(&headers, 1_000), Some(Duration::from_mins(1)));
    }

    // @lat: [[test#GitHub CI Tracking#No-run discovery expiry]]
    #[tokio::test]
    async fn no_run_window_makes_at_most_twenty_four_requests() {
        let start = Instant::now();
        let empty =
            || ApiResponse::Runs { etag: None, branch: String::new(), workflows: Vec::new() };
        let api = RunsApi::with_responses((0..24).map(|_| empty()).collect());
        let mut tracker = GithubCiTracker::default();
        tracker.accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), start);
        for tick in 0..=24 {
            tracker.poll_one(start + Duration::from_secs(tick * 5), 1_000 + tick * 5, &api).await;
        }
        assert_eq!(api.http_calls.load(Ordering::Relaxed), 24);
        assert_eq!(tracker.window_count(), 0);
    }

    // @lat: [[test#GitHub CI Tracking#Hot-upgrade active window]]
    #[tokio::test]
    async fn handoff_carries_active_state_without_token_and_repolls_it() {
        let start = Instant::now();
        let api = RunsApi::with_responses(vec![runs_response(CiWorkflowStatus::InProgress, None)]);
        let mut tracker = GithubCiTracker::default();
        tracker.accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), start);
        assert_eq!(tracker.poll_one(start, 1_000, &api).await.len(), 1);

        let saved = tracker.handoff_windows(start + Duration::from_secs(1));
        let bytes = rmp_serde::to_vec_named(&saved).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("test-token"));

        let takeover = start + Duration::from_secs(2);
        let mut restored = GithubCiTracker::restore(saved, takeover);
        assert_eq!(restored.head_for("acme/widget"), Some("head-a"));
        let unauthenticated = CountingApi { fail_auth: true, ..CountingApi::default() };
        let publications =
            restored.poll_one(takeover + Duration::from_secs(1), 1_003, &unauthenticated).await;
        let CiRunDelta::Set(state) = &publications[0].1 else { panic!("expected stale state") };
        assert!(state.stale);
    }
}
