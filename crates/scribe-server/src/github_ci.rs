use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::net::IpAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use scribe_common::protocol::{
    CiJob, CiJobStep, CiRunConclusion, CiRunDelta, CiRunState, CiRunStatus, CiWorkflowRun,
    CiWorkflowStatus, MAX_CI_TRACKED_HEADS,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

use crate::git_ref_watcher::PushDetected;
use crate::git_ref_watcher::{GitRefWatcherControl, PushEventReceiver};
use crate::ipc_server::{
    CiDismissals, SharedWriter, WindowShares, publish_ci_run_delta, send_message,
};
use crate::workspace_manager::WorkspaceManager;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const DISCOVERY_WINDOW: Duration = Duration::from_mins(2);
const MAX_ATTEMPTS_PER_HOUR: usize = 720;
const MAX_WORKFLOWS: usize = 100;
const MAX_REPOSITORY_ROOTS: usize = 64;
/// Bound on a workflow run's triggering event, kept well above the longest
/// event GitHub documents so two distinct events cannot dedup into one key.
const MAX_EVENT_BYTES: usize = 64;

/// One tracked head of one repository. Windows are per head, so branches that
/// run at the same time each keep their own bar.
type WindowKey = (GithubRepository, String);

/// Dedup identity of a workflow run at one head: the workflow file plus the
/// event that triggered it, so a `push` and a `pull_request` run of the same
/// file both survive while a retag's superseded run does not.
type WorkflowKey = (u64, String);

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

/// Keeps only the highest-id run for each workflow at this head, so a same-OID
/// generation (retag) collapses onto its newest run while distinct concurrent
/// workflows at the same head all survive into the published rollup.
fn newest_per_workflow(runs: Vec<(WorkflowKey, CiWorkflowRun)>) -> Vec<CiWorkflowRun> {
    let mut newest: HashMap<WorkflowKey, CiWorkflowRun> = HashMap::new();
    for (key, run) in runs {
        let keep = newest.get(&key).is_none_or(|existing| run.run_id > existing.run_id);
        if keep {
            newest.insert(key, run);
        }
    }
    let mut workflows: Vec<CiWorkflowRun> = newest.into_values().collect();
    workflows.sort_by_key(|run| run.run_id);
    workflows
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
    kind: ApiRequestKind,
}

enum ApiRequestKind {
    Runs { head_sha: String },
    Jobs { run_id: u64 },
}

impl ApiRequest {
    #[cfg(test)]
    fn for_test(repository: &GithubRepository, head_sha: &str) -> Self {
        let url = reqwest::Url::parse(&format!(
            "https://api.github.com/repos/{}/actions/runs",
            repository.full_name()
        ))
        .expect("test URL");
        Self { url, kind: ApiRequestKind::Runs { head_sha: head_sha.to_owned() } }
    }
}

pub enum ApiResponse {
    NotModified,
    Runs { etag: Option<String>, branch: String, workflows: Vec<(WorkflowKey, CiWorkflowRun)> },
    Jobs { etag: Option<String>, run_id: u64, jobs: Vec<CiJob> },
}

struct RunsObservation {
    at: Instant,
    epoch_secs: u64,
    etag: Option<String>,
    branch: String,
    workflows: Vec<(WorkflowKey, CiWorkflowRun)>,
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

    fn prepare_jobs(
        &self,
        _repository: &GithubRepository,
        _run_id: u64,
    ) -> Result<ApiRequest, ApiError> {
        Err(ApiError::InvalidPush)
    }

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
        url.query_pairs_mut().append_pair("head_sha", head_sha).append_pair("per_page", "100");
        Ok(url)
    }

    fn jobs_url(
        &self,
        repository: &GithubRepository,
        run_id: u64,
    ) -> Result<reqwest::Url, ApiError> {
        let mut url = self.request_url(repository, "0")?;
        url.set_path(&format!(
            "/repos/{}/{}/actions/runs/{run_id}/jobs",
            repository.owner, repository.name
        ));
        url.set_query(None);
        url.query_pairs_mut().append_pair("filter", "latest").append_pair("per_page", "100");
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
            kind: ApiRequestKind::Runs { head_sha: head_sha.to_owned() },
        })
    }

    fn prepare_jobs(
        &self,
        repository: &GithubRepository,
        run_id: u64,
    ) -> Result<ApiRequest, ApiError> {
        Ok(ApiRequest {
            url: self.jobs_url(repository, run_id)?,
            kind: ApiRequestKind::Jobs { run_id },
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
            match &request.kind {
                ApiRequestKind::Runs { head_sha } => {
                    let body: GithubRunsResponse =
                        response.json().await.map_err(|_| ApiError::Transient)?;
                    Ok(body.into_response(head_sha, response_etag))
                }
                ApiRequestKind::Jobs { run_id } => {
                    let body: GithubJobsResponse =
                        response.json().await.map_err(|_| ApiError::Transient)?;
                    Ok(ApiResponse::Jobs {
                        etag: response_etag,
                        run_id: *run_id,
                        jobs: body.into_jobs(*run_id, ""),
                    })
                }
            }
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
    workflow_id: u64,
    name: String,
    head_sha: String,
    #[serde(default)]
    head_branch: String,
    #[serde(default)]
    event: String,
    status: String,
    conclusion: Option<String>,
}

#[derive(Deserialize)]
struct GithubJobsResponse {
    jobs: Vec<GithubJob>,
}

#[derive(Deserialize)]
struct GithubJob {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    #[serde(default)]
    steps: Vec<GithubJobStep>,
}

#[derive(Deserialize)]
struct GithubJobStep {
    name: String,
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
                (
                    (run.workflow_id, truncate_text(&run.event, MAX_EVENT_BYTES)),
                    CiWorkflowRun {
                        run_id: run.id,
                        name: truncate_text(&run.name, 256),
                        status: github_status(&run.status),
                        conclusion: github_conclusion(run.conclusion.as_deref()),
                        started_at_epoch_secs: None,
                        updated_at_epoch_secs: None,
                    },
                )
            })
            .collect();
        ApiResponse::Runs { etag, branch, workflows }
    }
}

impl GithubJobsResponse {
    fn into_jobs(self, run_id: u64, workflow_name: &str) -> Vec<CiJob> {
        self.jobs
            .into_iter()
            .take(100)
            .map(|job| CiJob {
                job_id: job.id,
                workflow_run_id: run_id,
                workflow_name: truncate_text(workflow_name, 256),
                name: truncate_text(&job.name, 256),
                status: github_status(&job.status),
                conclusion: github_conclusion(job.conclusion.as_deref()),
                started_at_epoch_secs: job.started_at.as_deref().and_then(parse_github_time),
                completed_at_epoch_secs: job.completed_at.as_deref().and_then(parse_github_time),
                steps: job
                    .steps
                    .into_iter()
                    .take(100)
                    .map(|step| CiJobStep {
                        name: truncate_text(&step.name, 256),
                        status: github_status(&step.status),
                        conclusion: github_conclusion(step.conclusion.as_deref()),
                    })
                    .collect(),
            })
            .collect()
    }
}

fn github_status(status: &str) -> CiWorkflowStatus {
    match status {
        "completed" => CiWorkflowStatus::Completed,
        "in_progress" => CiWorkflowStatus::InProgress,
        _ => CiWorkflowStatus::Queued,
    }
}

fn github_conclusion(conclusion: Option<&str>) -> Option<CiRunConclusion> {
    conclusion.map(|conclusion| match conclusion {
        "success" | "neutral" | "skipped" => CiRunConclusion::Success,
        "cancelled" => CiRunConclusion::Cancelled,
        _ => CiRunConclusion::Failure,
    })
}

fn parse_github_time(value: &str) -> Option<u64> {
    let (date, time) = value.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.split('-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    if date.next().is_some() {
        return None;
    }
    let mut time = time.split(':').map(str::parse::<i64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if time.next().is_some()
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let shifted_year = year - i64::from(month <= 2);
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days.checked_mul(86_400)?.checked_add(hour * 3_600 + minute * 60 + second)?).ok()
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year.rem_euclid(4) == 0
            && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0) =>
        {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
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

/// One run as the last published state described it.
struct ObservedRun {
    status: CiWorkflowStatus,
    conclusion: Option<CiRunConclusion>,
    started_at_epoch_secs: Option<u64>,
    updated_at_epoch_secs: Option<u64>,
}

struct PollWindow {
    repository: GithubRepository,
    roots: BTreeSet<PathBuf>,
    head_sha: String,
    opened_at: Instant,
    discovery_deadline: Option<Instant>,
    next_attempt: Instant,
    etag: Option<String>,
    observed: Option<CiRunState>,
    offline_backoff: Duration,
    transient_logged: bool,
    /// When this head reached a terminal rollup. It then schedules no request,
    /// but stays reopenable by a same-OID generation until the next sweep past
    /// its discovery window retires it.
    settled_at: Option<Instant>,
    /// Highest run id this head had already published when a same-OID
    /// generation reopened it. Until GitHub returns a run above it, responses
    /// carry only the previous generation and must not settle the window.
    generation_baseline: Option<u64>,
}

impl PollWindow {
    fn new(repository: GithubRepository, root: PathBuf, head_sha: String, now: Instant) -> Self {
        Self {
            repository,
            roots: BTreeSet::from([root]),
            head_sha,
            opened_at: now,
            discovery_deadline: now.checked_add(DISCOVERY_WINDOW),
            next_attempt: now,
            etag: None,
            observed: None,
            offline_backoff: POLL_INTERVAL,
            transient_logged: false,
            settled_at: None,
            generation_baseline: None,
        }
    }

    /// Highest run id in the last published state.
    fn newest_observed_run(&self) -> Option<u64> {
        self.observed.as_ref().and_then(|state| state.workflows.iter().map(|run| run.run_id).max())
    }

    /// A terminal head schedules no further request.
    fn settled(&self) -> bool {
        self.settled_at.is_some()
    }

    /// What the bar already shows for each run at this head, so a
    /// re-observation keeps the clocks it derived from.
    fn observed_runs(&self) -> HashMap<u64, ObservedRun> {
        self.observed
            .as_ref()
            .map(|state| {
                state
                    .workflows
                    .iter()
                    .map(|run| {
                        (
                            run.run_id,
                            ObservedRun {
                                status: run.status,
                                conclusion: run.conclusion,
                                started_at_epoch_secs: run.started_at_epoch_secs,
                                updated_at_epoch_secs: run.updated_at_epoch_secs,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
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
    detail_tx: Option<mpsc::UnboundedSender<DetailCommand>>,
}

impl GithubCiTrackerHandle {
    #[must_use]
    pub fn handoff_windows(&self) -> Vec<HandoffCiWindow> {
        self.active.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn replace(&self, windows: Vec<HandoffCiWindow>) {
        *self.active.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = windows;
    }

    pub fn set_detail_interest(&self, interest: DetailInterest) {
        if let Some(tx) = &self.detail_tx {
            tx.send(DetailCommand::Set(interest)).ok();
        }
    }

    pub fn drop_detail_writer(&self, writer: SharedWriter) {
        if let Some(tx) = &self.detail_tx {
            tx.send(DetailCommand::DropWriter(writer)).ok();
        }
    }
}

pub struct DetailInterest {
    pub repo_root: PathBuf,
    pub head_sha: String,
    pub writer: SharedWriter,
    pub interested: bool,
}

enum DetailCommand {
    Set(DetailInterest),
    DropWriter(SharedWriter),
}

struct DetailTrace {
    repository: GithubRepository,
    workflows: Vec<(u64, String)>,
    subscribers: Vec<SharedWriter>,
    jobs: HashMap<u64, Vec<CiJob>>,
    etags: HashMap<u64, String>,
    next_workflow: usize,
    next_attempt: Instant,
}

struct PushResult {
    #[cfg(test)]
    new_head: bool,
    publications: Vec<(PathBuf, CiRunDelta)>,
}

enum PollStep {
    Config(Result<(), tokio::sync::watch::error::RecvError>),
    Push(Option<Result<PushDetected, tokio::sync::broadcast::error::RecvError>>),
    Detail(Option<DetailCommand>),
    Complete(Vec<(PathBuf, CiRunDelta)>),
}

#[derive(Default)]
pub struct GithubCiTracker {
    windows: HashMap<WindowKey, PollWindow>,
    details: HashMap<(PathBuf, String), DetailTrace>,
    scheduler: PollScheduler,
    token: Option<SecretToken>,
    prefer_details: bool,
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
        let key = (repository, push.head_sha.clone());
        if let Some(window) = self.windows.get_mut(&key) {
            if window.roots.len() < MAX_REPOSITORY_ROOTS {
                window.roots.insert(push.repository_root);
            }
            if push.same_oid_generation {
                // A same-OID generation (retag) at this window's unchanged head: reopen
                // in place instead of clearing, so a still-active workflow the window
                // already observed survives alongside whatever the new generation adds.
                // `observed` and `roots` are carried forward untouched above; only the
                // poll timers restart, mirroring a fresh window's initial values. A
                // settled head reopens the same way, and the baseline keeps the first
                // old-only response from settling it again before the new run appears.
                window.discovery_deadline = now.checked_add(DISCOVERY_WINDOW);
                window.next_attempt = now;
                window.generation_baseline = window.newest_observed_run();
                window.settled_at = None;
            }
            return Some(PushResult {
                #[cfg(test)]
                new_head: false,
                publications: Vec::new(),
            });
        }
        // A concurrent head keeps its own window, so several branches or
        // worktrees can run at once. Only the head cap retires one.
        self.windows.insert(
            key.clone(),
            PollWindow::new(key.0.clone(), push.repository_root, push.head_sha, now),
        );
        let publications = self.retire_oldest_heads(&key.0);
        Some(PushResult {
            #[cfg(test)]
            new_head: true,
            publications,
        })
    }

    /// Keep at most [`MAX_CI_TRACKED_HEADS`] windows per repository, retiring
    /// the heads opened first and clearing whatever they published.
    fn retire_oldest_heads(&mut self, repository: &GithubRepository) -> Vec<(PathBuf, CiRunDelta)> {
        let mut opened = self
            .windows
            .iter()
            .filter(|((tracked, _), _)| tracked == repository)
            .map(|(key, window)| (window.opened_at, key.clone()))
            .collect::<Vec<_>>();
        if opened.len() <= MAX_CI_TRACKED_HEADS {
            return Vec::new();
        }
        opened.sort_unstable_by_key(|(opened_at, _)| *opened_at);
        opened
            .drain(..opened.len() - MAX_CI_TRACKED_HEADS)
            .filter_map(|(_, key)| self.windows.remove(&key).map(|window| (key, window)))
            .flat_map(|(key, window)| {
                // Roots as well as head: a fork and its upstream are separate
                // repositories that share commit SHAs.
                self.details
                    .retain(|(root, head), _| head != &key.1 || !window.roots.contains(root));
                publications(&window.roots, &CiRunDelta::Cleared { head_sha: key.1 })
            })
            .collect()
    }

    // @lat: [[server#Sessions#GitHub Actions Tracking]]
    fn set_detail_interest(&mut self, interest: DetailInterest, now: Instant) {
        let key = (interest.repo_root.clone(), interest.head_sha.clone());
        if !interest.interested {
            let empty = self.details.get_mut(&key).is_some_and(|trace| {
                trace
                    .subscribers
                    .retain(|writer| !std::sync::Arc::ptr_eq(writer, &interest.writer));
                trace.subscribers.is_empty()
            });
            if empty {
                self.details.remove(&key);
            }
            return;
        }
        let Some(window) = self.windows.values().find(|window| {
            window.head_sha == interest.head_sha && window.roots.contains(&interest.repo_root)
        }) else {
            return;
        };
        let Some(state) = window.observed.as_ref() else { return };
        let repository = window.repository.clone();
        let workflows = state
            .workflows
            .iter()
            .map(|workflow| (workflow.run_id, workflow.name.clone()))
            .collect::<Vec<_>>();
        if workflows.is_empty() {
            return;
        }
        let trace = self.details.entry(key).or_insert_with(|| DetailTrace {
            repository,
            workflows,
            subscribers: Vec::new(),
            jobs: HashMap::new(),
            etags: HashMap::new(),
            next_workflow: 0,
            next_attempt: now,
        });
        if !trace.subscribers.iter().any(|writer| std::sync::Arc::ptr_eq(writer, &interest.writer))
        {
            trace.subscribers.push(interest.writer);
        }
        self.prefer_details = true;
    }

    fn drop_detail_writer(&mut self, writer: &SharedWriter) {
        self.details.retain(|_, trace| {
            trace.subscribers.retain(|subscriber| !std::sync::Arc::ptr_eq(subscriber, writer));
            !trace.subscribers.is_empty()
        });
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
        let detail_ready = self
            .details
            .iter()
            .filter(|(_, trace)| trace.next_attempt <= now)
            .min_by_key(|(_, trace)| trace.next_attempt)
            .map(|(key, _)| key.clone());
        let run_ready = self
            .windows
            .iter()
            .filter(|(_, window)| !window.settled() && window.next_attempt <= now)
            .min_by_key(|(_, window)| window.next_attempt)
            .map(|(key, _)| key.clone());
        if let Some(detail_key) = detail_ready.clone()
            && (self.prefer_details || run_ready.is_none())
        {
            self.prefer_details = false;
            self.poll_detail(detail_key, now, api).await;
            return Vec::new();
        }
        self.prefer_details = detail_ready.is_some();
        let Some(key) = run_ready else { return Vec::new() };
        self.poll_run(key, now, observed_epoch_secs, api).await
    }

    async fn poll_run(
        &mut self,
        key: WindowKey,
        now: Instant,
        observed_epoch_secs: u64,
        api: &dyn GithubApi,
    ) -> Vec<(PathBuf, CiRunDelta)> {
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
            Ok(ApiResponse::Jobs { .. }) => Vec::new(),
            Err(ApiError::Authentication) => {
                warn!(repository = %key.0.full_name(), head = %key.1, "GitHub CI polling lost authentication; stopping window");
                self.token = None;
                self.stop_failed_window(&key)
            }
            Err(ApiError::Permission) => {
                warn!(repository = %key.0.full_name(), head = %key.1, "GitHub CI account cannot read repository Actions; stopping window");
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

    async fn poll_detail(&mut self, key: (PathBuf, String), now: Instant, api: &dyn GithubApi) {
        let Some((repository, run_id, workflow_name)) = self.details.get(&key).and_then(|trace| {
            trace
                .workflows
                .get(trace.next_workflow)
                .map(|(run_id, name)| (trace.repository.clone(), *run_id, name.clone()))
        }) else {
            self.details.remove(&key);
            return;
        };
        let Ok(request) = api.prepare_jobs(&repository, run_id) else {
            self.details.remove(&key);
            return;
        };
        if self.token.is_none() {
            let Ok(token) = api.authenticate().await else {
                self.details.remove(&key);
                return;
            };
            self.token = Some(token);
        }
        self.scheduler.record(now);
        let Some(token) = self.token.as_ref() else { return };
        let etag =
            self.details.get(&key).and_then(|trace| trace.etags.get(&run_id)).map(String::as_str);
        match api.fetch(&request, token, etag).await {
            Ok(ApiResponse::Jobs { etag, run_id: response_run_id, mut jobs }) => {
                for job in &mut jobs {
                    job.workflow_name.clone_from(&workflow_name);
                }
                if let Some(trace) = self.details.get_mut(&key) {
                    trace.etags.extend(etag.map(|etag| (response_run_id, etag)));
                    trace.jobs.insert(response_run_id, jobs);
                }
                self.publish_detail(&key).await;
                self.advance_detail(&key, now);
            }
            Ok(ApiResponse::NotModified | ApiResponse::Runs { .. }) => {
                self.advance_detail(&key, now);
            }
            Err(ApiError::Authentication) => {
                self.token = None;
                self.details.remove(&key);
            }
            Err(ApiError::Permission | ApiError::InvalidPush | ApiError::UntrustedEndpoint) => {
                self.details.remove(&key);
            }
            Err(ApiError::RateLimited(delay)) => {
                self.scheduler.blocked_until = now.checked_add(delay);
                self.defer_detail(&key, now, delay);
            }
            Err(ApiError::Transient) => self.defer_detail(&key, now, POLL_INTERVAL),
        }
    }

    fn advance_detail(&mut self, key: &(PathBuf, String), now: Instant) {
        if let Some(trace) = self.details.get_mut(key) {
            trace.next_workflow = (trace.next_workflow + 1) % trace.workflows.len();
            trace.next_attempt = now + POLL_INTERVAL;
        }
    }

    fn defer_detail(&mut self, key: &(PathBuf, String), now: Instant, delay: Duration) {
        if let Some(trace) = self.details.get_mut(key) {
            trace.next_attempt = now + delay.max(POLL_INTERVAL);
        }
    }

    async fn publish_detail(&self, key: &(PathBuf, String)) {
        let Some(trace) = self.details.get(key) else { return };
        let jobs = trace
            .workflows
            .iter()
            .filter_map(|(run_id, _)| trace.jobs.get(run_id))
            .flatten()
            .cloned()
            .collect();
        let message = scribe_common::protocol::ServerMessage::CiRunDetails {
            repo_root: key.0.clone(),
            details: scribe_common::protocol::CiRunDetails { head_sha: key.1.clone(), jobs },
        };
        for writer in &trace.subscribers {
            send_message(writer, &message).await;
        }
    }

    fn apply_runs(
        &mut self,
        key: &WindowKey,
        observation: RunsObservation,
    ) -> Vec<(PathBuf, CiRunDelta)> {
        let Some(window) = self.windows.get_mut(key) else {
            return Vec::new();
        };
        window.etag = observation.etag;
        window.next_attempt = observation.at + POLL_INTERVAL;
        window.offline_backoff = POLL_INTERVAL;
        window.transient_logged = false;
        let mut workflows = newest_per_workflow(observation.workflows);
        if workflows.is_empty() {
            return Vec::new();
        }
        workflows.truncate(MAX_WORKFLOWS);
        let seen = window.observed_runs();
        // A reopened generation that has produced nothing newer than the run
        // it already published keeps polling: GitHub often serves the previous
        // generation's runs for a few seconds before the new one appears.
        let newest_run = workflows.iter().map(|run| run.run_id).max().unwrap_or_default();
        let awaiting_generation =
            window.generation_baseline.is_some_and(|baseline| newest_run <= baseline);
        if !awaiting_generation {
            window.generation_baseline = None;
        }
        for workflow in &mut workflows {
            let previous = seen.get(&workflow.run_id);
            workflow.started_at_epoch_secs =
                previous.and_then(|run| run.started_at_epoch_secs).or(Some(observation.epoch_secs));
            // While a generation is still awaited, every returned run is one
            // already published; re-stamping them would drift the elapsed
            // clock of a bar whose runs reported no news.
            workflow.updated_at_epoch_secs = previous
                .filter(|run| {
                    awaiting_generation
                        && run.status == workflow.status
                        && run.conclusion == workflow.conclusion
                })
                .and_then(|run| run.updated_at_epoch_secs)
                .or(Some(observation.epoch_secs));
        }
        let state = CiRunState {
            repository: key.0.full_name(),
            head_sha: window.head_sha.clone(),
            branch: observation.branch,
            rollup: aggregate_status(&workflows),
            workflows,
            stale: false,
        };
        if !awaiting_generation {
            window.discovery_deadline = None;
        }
        window.observed = Some(state.clone());
        // A terminal head stops polling but stays tracked, so a later same-OID
        // generation reopens it in place and the head cap can retire it.
        let terminal = matches!(
            state.rollup,
            CiRunStatus::Success | CiRunStatus::Failure | CiRunStatus::Cancelled
        );
        window.settled_at = (!awaiting_generation && terminal).then_some(observation.at);
        publications(&window.roots, &CiRunDelta::Set(state))
    }

    fn mark_stale_or_backoff(
        &mut self,
        key: &WindowKey,
        now: Instant,
    ) -> Vec<(PathBuf, CiRunDelta)> {
        let Some(window) = self.windows.get_mut(key) else {
            return Vec::new();
        };
        window.next_attempt = now + window.offline_backoff;
        window.offline_backoff = (window.offline_backoff * 2).min(Duration::from_secs(30));
        if !window.transient_logged {
            warn!(repository = %key.0.full_name(), head = %key.1, "GitHub CI request failed; retrying with bounded backoff");
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

    fn stop_failed_window(&mut self, key: &WindowKey) -> Vec<(PathBuf, CiRunDelta)> {
        let Some(window) = self.windows.remove(key) else {
            return Vec::new();
        };
        let Some(mut state) = window.observed else {
            return Vec::new();
        };
        state.stale = true;
        publications(&window.roots, &CiRunDelta::Set(state))
    }

    /// Drop windows whose discovery window lapsed without a run, and settled
    /// heads old enough that no same-OID generation can still be following
    /// them. Retiring a settled head here keeps a long session from holding a
    /// snapshot per repository it happened to push to once.
    fn expire_discovery(&mut self, now: Instant) {
        self.windows.retain(|_, window| {
            window.settled_at.map_or_else(
                || window.discovery_deadline.is_none_or(|deadline| deadline > now),
                |settled| now.saturating_duration_since(settled) < DISCOVERY_WINDOW,
            )
        });
    }

    fn clear(&mut self) -> Vec<(PathBuf, CiRunDelta)> {
        self.token = None;
        self.details.clear();
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
            .filter(|window| !window.settled())
            .map(|window| window.next_attempt)
            .min()
            .map(|due| self.scheduler.ready_at().map_or(due, |ready| due.max(ready)));
        let expiry = self.windows.values().filter_map(|window| window.discovery_deadline).min();
        let detail = self
            .details
            .values()
            .map(|trace| trace.next_attempt)
            .min()
            .map(|due| self.scheduler.ready_at().map_or(due, |ready| due.max(ready)));
        [next_poll, detail, expiry].into_iter().flatten().min().map(|wake| wake.max(now))
    }

    fn handoff_windows(&self, now: Instant) -> Vec<HandoffCiWindow> {
        self.windows
            .values()
            .filter(|window| !window.settled())
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
            tracker.windows.insert((saved.repository, window.head_sha.clone()), window);
        }
        tracker
    }

    #[cfg(test)]
    fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// Windows still scheduling requests; a settled terminal head is excluded.
    #[cfg(test)]
    fn polling_count(&self) -> usize {
        self.windows.values().filter(|window| !window.settled()).count()
    }

    #[cfg(test)]
    fn roots_for(&self, full_name: &str, head_sha: &str) -> Vec<PathBuf> {
        self.windows
            .iter()
            .find(|((repository, head), _)| repository.full_name() == full_name && head == head_sha)
            .map_or_else(Vec::new, |(_, window)| window.roots.iter().cloned().collect())
    }

    #[cfg(test)]
    fn heads_for(&self, full_name: &str) -> Vec<String> {
        let mut heads = self
            .windows
            .keys()
            .filter(|(repository, _)| repository.full_name() == full_name)
            .map(|(_, head)| head.clone())
            .collect::<Vec<_>>();
        heads.sort();
        heads
    }

    #[cfg(test)]
    fn head_for(&self, full_name: &str) -> Option<&str> {
        self.windows
            .iter()
            .find(|((repository, _), _)| repository.full_name() == full_name)
            .map(|(_, window)| window.head_sha.as_str())
    }
}

fn warn_tracker_error(key: &WindowKey, error: &ApiError, observed: bool) {
    let reason = match error {
        ApiError::Authentication => "gh is unavailable or not authenticated for github.com",
        ApiError::Permission => "the active GitHub account cannot read this repository's Actions",
        ApiError::UntrustedEndpoint => "SCRIBE_GITHUB_API_URL is not a loopback URL",
        ApiError::InvalidPush => "the pushed Git object ID is invalid",
        ApiError::Transient => "the GitHub API is temporarily unavailable",
        ApiError::RateLimited(_) => "GitHub API rate limit delayed polling",
    };
    warn!(repository = %key.0.full_name(), head = %key.1, observed, reason, "GitHub CI window stopped");
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
    let (detail_tx, detail_rx) = mpsc::unbounded_channel();
    let handle =
        GithubCiTrackerHandle { detail_tx: Some(detail_tx), ..GithubCiTrackerHandle::default() };
    let task_handle = handle.clone();
    tokio::spawn(async move {
        run_tracker(
            TrackerRuntime {
                git_ref_watcher,
                workspace_manager,
                window_shares,
                dismissals,
                handle: task_handle,
                detail_rx,
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
    detail_rx: mpsc::UnboundedReceiver<DetailCommand>,
}

struct TrackerPublisher<'a> {
    handle: &'a GithubCiTrackerHandle,
    workspace_manager: &'a std::sync::Arc<RwLock<WorkspaceManager>>,
    window_shares: &'a WindowShares,
    dismissals: &'a CiDismissals,
}

impl TrackerPublisher<'_> {
    async fn accept_push(&self, tracker: &mut GithubCiTracker, push: PushDetected) {
        let Some(result) = tracker.accept_push(push, Instant::now()) else { return };
        self.handle.replace(tracker.handoff_windows(Instant::now()));
        publish_all(
            self.workspace_manager,
            self.window_shares,
            self.dismissals,
            result.publications,
        )
        .await;
    }
}

async fn run_tracker(runtime: TrackerRuntime, restored: Vec<HandoffCiWindow>, api: impl GithubApi) {
    let TrackerRuntime {
        git_ref_watcher,
        workspace_manager,
        window_shares,
        dismissals,
        handle,
        mut detail_rx,
    } = runtime;
    let publisher = TrackerPublisher {
        handle: &handle,
        workspace_manager: &workspace_manager,
        window_shares: &window_shares,
        dismissals: &dismissals,
    };
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

        tokio::select! {
            biased;
            transition = config_rx.changed() => if transition.is_err() { return },
            command = detail_rx.recv() => {
                apply_detail_command(&mut tracker, command, Instant::now());
            }
            event = receive_push(&mut pushes) => {
                match event {
                    Some(Ok(push)) => publisher.accept_push(&mut tracker, push).await,
                    Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                        warn!(skipped, "GitHub CI push receiver lagged");
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => pushes = None,
                    None => {}
                }
            }
            () = sleep_until(tracker.next_wake(Instant::now())) => {
                let step = {
                    let polling = tracker.poll_one(Instant::now(), epoch_secs(), &api);
                    tokio::pin!(polling);
                    tokio::select! {
                        biased;
                        transition = config_rx.changed() => PollStep::Config(transition),
                        event = receive_push(&mut pushes) => PollStep::Push(event),
                        command = detail_rx.recv() => PollStep::Detail(command),
                        publications = &mut polling => PollStep::Complete(publications),
                    }
                };
                match step {
                    PollStep::Config(Err(_)) => return,
                    PollStep::Config(Ok(())) | PollStep::Push(None) => {}
                    PollStep::Detail(command) => {
                        apply_detail_command(&mut tracker, command, Instant::now());
                    }
                    PollStep::Push(Some(Ok(push))) => publisher.accept_push(&mut tracker, push).await,
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

fn apply_detail_command(
    tracker: &mut GithubCiTracker,
    command: Option<DetailCommand>,
    now: Instant,
) {
    match command {
        Some(DetailCommand::Set(interest)) => tracker.set_detail_interest(interest, now),
        Some(DetailCommand::DropWriter(writer)) => tracker.drop_detail_writer(&writer),
        None => tracker.details.clear(),
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
        CiJob, CiRunConclusion, CiRunDelta, CiRunStatus, CiWorkflowRun, CiWorkflowStatus,
    };

    use super::{
        ApiError, ApiRequest, ApiRequestKind, ApiResponse, BoxFuture, DISCOVERY_WINDOW,
        DetailInterest, GithubApi, GithubCiTracker, GithubJobsResponse, GithubRepository,
        HttpGithubApi, PollScheduler, WorkflowKey, github_ci_enabled, set_github_ci_enabled,
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
            same_oid_generation: false,
        }
    }

    fn generation_push(url: &str, root: &str, head: &str) -> PushDetected {
        PushDetected { same_oid_generation: true, ..push(url, root, head) }
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

    // @lat: [[test#GitHub CI Tracking#Concurrent head windows]]
    #[test]
    fn one_head_shares_roots_while_concurrent_heads_track_side_by_side() {
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
            tracker.roots_for("acme/widget", "head-a"),
            vec![std::path::PathBuf::from("/a"), std::path::PathBuf::from("/b")]
        );

        let second = tracker
            .accept_push(
                push("ssh://git@github.com/acme/widget", "/b", "head-b"),
                now + Duration::from_secs(1),
            )
            .expect("second head opens its own window");
        assert!(second.new_head);
        assert!(second.publications.is_empty(), "a concurrent head must not clear another");
        assert_eq!(tracker.heads_for("acme/widget"), vec!["head-a", "head-b"]);
        assert_eq!(
            tracker.roots_for("acme/widget", "head-b"),
            vec![std::path::PathBuf::from("/b")],
            "a newer head must not inherit another head's roots"
        );
    }

    // @lat: [[test#GitHub CI Tracking#Concurrent head windows]]
    #[tokio::test]
    async fn a_fourth_head_retires_the_oldest_tracked_head() {
        let start = Instant::now();
        let api = RunsApi::with_responses(vec![runs_response(CiWorkflowStatus::InProgress, None)]);
        let mut tracker = GithubCiTracker::default();
        for (offset, head) in ["head-a", "head-b", "head-c"].into_iter().enumerate() {
            tracker.accept_push(
                push("git@github.com:acme/widget.git", "/a", head),
                start + Duration::from_secs(offset as u64),
            );
        }
        assert_eq!(tracker.poll_one(start, 1_000, &api).await.len(), 1);

        let fourth = tracker
            .accept_push(
                push("git@github.com:acme/widget.git", "/a", "head-d"),
                start + Duration::from_secs(3),
            )
            .expect("fourth head");
        assert_eq!(tracker.heads_for("acme/widget"), vec!["head-b", "head-c", "head-d"]);
        assert!(
            matches!(
                fourth.publications.as_slice(),
                [(root, CiRunDelta::Cleared { head_sha })]
                    if root == std::path::Path::new("/a") && head_sha == "head-a"
            ),
            "the retired head must clear its own bar: {:?}",
            fourth.publications
        );
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
            "https://api.github.com/repos/acme/widget/actions/runs?head_sha=abc123&per_page=100"
        );
        let fixture = HttpGithubApi::with_override(Some("http://127.0.0.1:8098".into()));
        assert_eq!(
            fixture.prepare(&repository, "abc123").unwrap().url.as_str(),
            "http://127.0.0.1:8098/repos/acme/widget/actions/runs?head_sha=abc123&per_page=100"
        );
        let attacker = HttpGithubApi::with_override(Some("https://example.com".into()));
        assert!(matches!(
            attacker.prepare(&repository, "abc123"),
            Err(ApiError::UntrustedEndpoint)
        ));
    }

    #[test]
    fn jobs_request_uses_the_trusted_repository_and_run_id() {
        let api = HttpGithubApi::with_override(None);
        let repository = GithubRepository::new("acme", "scribe").unwrap();

        let request = api.prepare_jobs(&repository, 42).unwrap();

        assert_eq!(
            request.url.as_str(),
            "https://api.github.com/repos/acme/scribe/actions/runs/42/jobs?filter=latest&per_page=100"
        );
    }

    #[test]
    fn jobs_response_preserves_timing_steps_and_non_color_status() {
        let response: GithubJobsResponse = serde_json::from_str(
            r#"{
                "jobs": [{
                    "id": 7,
                    "name": "rust-linux",
                    "status": "completed",
                    "conclusion": "success",
                    "started_at": "1970-01-01T00:01:40Z",
                    "completed_at": "1970-01-01T00:02:40Z",
                    "steps": [{
                        "name": "just test",
                        "status": "completed",
                        "conclusion": "success"
                    }]
                }]
            }"#,
        )
        .unwrap();

        let jobs = response.into_jobs(42, "quality");

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].workflow_run_id, 42);
        assert_eq!(jobs[0].workflow_name, "quality");
        assert_eq!(jobs[0].started_at_epoch_secs, Some(100));
        assert_eq!(jobs[0].completed_at_epoch_secs, Some(160));
        assert_eq!(jobs[0].steps[0].name, "just test");
        assert_eq!(jobs[0].conclusion, Some(CiRunConclusion::Success));
    }

    struct DetailApi {
        job_calls: AtomicUsize,
    }

    impl DetailApi {
        fn jobs_response(&self, run_id: u64) -> ApiResponse {
            self.job_calls.fetch_add(1, Ordering::Relaxed);
            ApiResponse::Jobs {
                etag: None,
                run_id,
                jobs: vec![CiJob {
                    job_id: 7,
                    workflow_run_id: run_id,
                    workflow_name: String::new(),
                    name: "rust-linux".into(),
                    status: CiWorkflowStatus::InProgress,
                    conclusion: None,
                    started_at_epoch_secs: Some(100),
                    completed_at_epoch_secs: None,
                    steps: Vec::new(),
                }],
            }
        }
    }

    impl GithubApi for DetailApi {
        fn prepare(
            &self,
            repository: &GithubRepository,
            head_sha: &str,
        ) -> Result<ApiRequest, ApiError> {
            Ok(ApiRequest::for_test(repository, head_sha))
        }

        fn prepare_jobs(
            &self,
            repository: &GithubRepository,
            run_id: u64,
        ) -> Result<ApiRequest, ApiError> {
            HttpGithubApi::with_override(None).prepare_jobs(repository, run_id)
        }

        fn authenticate(&self) -> BoxFuture<'_, Result<super::SecretToken, ApiError>> {
            Box::pin(async { Ok(super::SecretToken::new("secret")) })
        }

        fn fetch<'a>(
            &'a self,
            request: &'a ApiRequest,
            _token: &'a super::SecretToken,
            _etag: Option<&'a str>,
        ) -> BoxFuture<'a, Result<ApiResponse, ApiError>> {
            Box::pin(async move {
                match request.kind {
                    ApiRequestKind::Runs { .. } => Ok(ApiResponse::Runs {
                        etag: None,
                        branch: "main".into(),
                        workflows: vec![(pushed(42), run(42, CiWorkflowStatus::InProgress, None))],
                    }),
                    ApiRequestKind::Jobs { run_id } => Ok(self.jobs_response(run_id)),
                }
            })
        }
    }

    // @lat: [[test#GitHub CI Tracking#Detail scheduler follows subscribers]]
    #[tokio::test]
    async fn job_requests_exist_only_while_a_panel_is_interested() {
        let api = DetailApi { job_calls: AtomicUsize::new(0) };
        let start = Instant::now();
        let mut tracker = GithubCiTracker::default();
        tracker
            .accept_push(push("git@github.com:acme/scribe.git", "/work/scribe", "abc123"), start);
        tracker.poll_one(start, 100, &api).await;
        assert_eq!(api.job_calls.load(Ordering::Relaxed), 0);

        let writer = crate::ipc_server::test_shared_writer(tokio::io::sink());
        tracker.set_detail_interest(
            DetailInterest {
                repo_root: "/work/scribe".into(),
                head_sha: "abc123".into(),
                writer: std::sync::Arc::clone(&writer),
                interested: true,
            },
            start,
        );
        tracker.poll_one(start + Duration::from_secs(5), 105, &api).await;
        assert_eq!(api.job_calls.load(Ordering::Relaxed), 1);

        tracker.set_detail_interest(
            DetailInterest {
                repo_root: "/work/scribe".into(),
                head_sha: "abc123".into(),
                writer,
                interested: false,
            },
            start,
        );
        tracker.poll_one(start + Duration::from_secs(10), 110, &api).await;
        assert_eq!(api.job_calls.load(Ordering::Relaxed), 1);
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
        workflows_response(vec![(pushed(1), run(1, status, conclusion))])
    }

    fn workflows_response(workflows: Vec<(WorkflowKey, CiWorkflowRun)>) -> ApiResponse {
        ApiResponse::Runs { etag: Some("etag".into()), branch: "main".into(), workflows }
    }

    /// A `push`-triggered run of workflow file `workflow_id`.
    fn pushed(workflow_id: u64) -> WorkflowKey {
        (workflow_id, "push".into())
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
        assert_eq!(tracker.polling_count(), 0, "a terminal rollup must stop polling");
    }

    // @lat: [[test#GitHub CI Tracking#Active same-SHA generation]]
    #[tokio::test]
    async fn same_sha_generation_reopens_an_active_window_for_a_second_workflow() {
        let start = Instant::now();
        let branch_run = run(201, CiWorkflowStatus::InProgress, None);
        let api = RunsApi::with_responses(vec![
            workflows_response(vec![(pushed(10), branch_run.clone())]),
            workflows_response(vec![
                (pushed(10), branch_run),
                (pushed(20), run(305, CiWorkflowStatus::InProgress, None)),
            ]),
        ]);
        let mut tracker = GithubCiTracker::default();
        tracker.accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), start);
        let first = tracker.poll_one(start, 1_000, &api).await;
        let CiRunDelta::Set(first) = &first[0].1 else { panic!("expected state") };
        assert_eq!(first.workflows.iter().map(|run| run.run_id).collect::<Vec<_>>(), [201]);

        let reopened = tracker
            .accept_push(
                generation_push("git@github.com:acme/widget.git", "/a", "head-a"),
                start + Duration::from_secs(1),
            )
            .expect("trusted generation");
        assert!(
            !reopened.new_head,
            "same-head generation opened a fresh window instead of reopening"
        );
        assert!(
            reopened.publications.is_empty(),
            "reopening an unchanged head must not clear the bar"
        );
        assert_eq!(tracker.window_count(), 1);

        let both = tracker.poll_one(start + Duration::from_secs(5), 1_005, &api).await;
        assert_eq!(both.len(), 1, "no extra delta may accompany the second workflow's arrival");
        let CiRunDelta::Set(both) = &both[0].1 else {
            panic!("expected both workflows to survive, not a Cleared delta")
        };
        let mut ids = both.workflows.iter().map(|run| run.run_id).collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, [201, 305], "a concurrent workflow at the same head was dropped");
        assert_eq!(both.rollup, CiRunStatus::Running);
    }

    // @lat: [[test#GitHub CI Tracking#Retag collapses to the newest run]]
    #[tokio::test]
    async fn retag_collapses_two_runs_sharing_a_workflow_to_the_newest() {
        let start = Instant::now();
        let failed = run(900, CiWorkflowStatus::Completed, Some(CiRunConclusion::Failure));
        let retried = run(901, CiWorkflowStatus::Completed, Some(CiRunConclusion::Success));
        let api = RunsApi::with_responses(vec![
            workflows_response(vec![(pushed(50), failed.clone())]),
            workflows_response(vec![(pushed(50), failed), (pushed(50), retried)]),
        ]);
        let mut tracker = GithubCiTracker::default();
        tracker.accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), start);

        let first = tracker.poll_one(start, 1_000, &api).await;
        let CiRunDelta::Set(first) = &first[0].1 else { panic!("expected state") };
        assert_eq!(first.rollup, CiRunStatus::Failure);
        assert_eq!(tracker.polling_count(), 0, "a terminal rollup must stop polling");

        tracker
            .accept_push(
                generation_push("git@github.com:acme/widget.git", "/a", "head-a"),
                start + Duration::from_secs(1),
            )
            .expect("trusted generation reopens tracking for the retag");

        let second = tracker.poll_one(start + Duration::from_secs(5), 1_005, &api).await;
        let CiRunDelta::Set(second) = &second[0].1 else { panic!("expected state") };
        assert_eq!(second.workflows.iter().map(|run| run.run_id).collect::<Vec<_>>(), [901]);
        assert_eq!(
            second.rollup,
            CiRunStatus::Success,
            "a superseded failed run poisoned the rollup"
        );
    }

    // @lat: [[test#GitHub CI Tracking#Old-only generation response keeps polling]]
    #[tokio::test]
    async fn a_reopened_generation_keeps_polling_through_an_old_only_response() {
        let start = Instant::now();
        let failed = run(900, CiWorkflowStatus::Completed, Some(CiRunConclusion::Failure));
        let api = RunsApi::with_responses(vec![
            workflows_response(vec![(pushed(50), failed.clone())]),
            workflows_response(vec![(pushed(50), failed.clone())]),
            workflows_response(vec![
                (pushed(50), failed),
                (pushed(50), run(901, CiWorkflowStatus::InProgress, None)),
            ]),
        ]);
        let mut tracker = GithubCiTracker::default();
        tracker.accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), start);
        tracker.poll_one(start, 1_000, &api).await;
        assert_eq!(tracker.polling_count(), 0, "a terminal rollup must stop polling");

        tracker
            .accept_push(
                generation_push("git@github.com:acme/widget.git", "/a", "head-a"),
                start + Duration::from_secs(1),
            )
            .expect("trusted generation reopens the settled head");
        let old_only = tracker.poll_one(start + Duration::from_secs(5), 1_005, &api).await;
        let CiRunDelta::Set(old_only) = &old_only[0].1 else { panic!("expected state") };
        assert_eq!(old_only.rollup, CiRunStatus::Failure);
        assert_eq!(
            tracker.polling_count(),
            1,
            "an old-only response closed the generation before its new run appeared"
        );

        let generation = tracker.poll_one(start + Duration::from_secs(10), 1_010, &api).await;
        let CiRunDelta::Set(generation) = &generation[0].1 else { panic!("expected state") };
        assert_eq!(generation.workflows.iter().map(|run| run.run_id).collect::<Vec<_>>(), [901]);
        assert_eq!(generation.rollup, CiRunStatus::Running);
    }

    // @lat: [[test#GitHub CI Tracking#Settled heads leave on the next sweep]]
    #[tokio::test]
    async fn a_settled_head_is_retired_once_no_generation_can_follow_it() {
        let start = Instant::now();
        let api = RunsApi::with_responses(vec![
            runs_response(CiWorkflowStatus::Completed, Some(CiRunConclusion::Success)),
            runs_response(CiWorkflowStatus::InProgress, None),
        ]);
        let mut tracker = GithubCiTracker::default();
        tracker.accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), start);
        tracker.poll_one(start, 1_000, &api).await;
        assert_eq!(tracker.window_count(), 1, "a settled head stays reopenable");

        // Another repository's push drives the sweep; the settled head is still
        // young enough for a retag to reopen it.
        tracker.accept_push(
            push("git@github.com:acme/other.git", "/b", "head-b"),
            start + Duration::from_secs(5),
        );
        tracker.poll_one(start + Duration::from_secs(5), 1_005, &api).await;
        assert_eq!(tracker.window_count(), 2);

        tracker.poll_one(start + DISCOVERY_WINDOW + Duration::from_secs(5), 1_130, &api).await;
        assert_eq!(tracker.heads_for("acme/widget"), Vec::<String>::new());
        assert_eq!(tracker.heads_for("acme/other"), vec!["head-b"], "a live head must not expire");
    }

    // @lat: [[test#GitHub CI Tracking#Old-only generation response keeps polling]]
    #[tokio::test]
    async fn an_awaited_generation_does_not_advance_the_previous_run_clock() {
        let start = Instant::now();
        let failed = run(900, CiWorkflowStatus::Completed, Some(CiRunConclusion::Failure));
        let api = RunsApi::with_responses(vec![
            workflows_response(vec![(pushed(50), failed.clone())]),
            workflows_response(vec![(pushed(50), failed)]),
        ]);
        let mut tracker = GithubCiTracker::default();
        tracker.accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), start);
        tracker.poll_one(start, 1_000, &api).await;
        tracker
            .accept_push(
                generation_push("git@github.com:acme/widget.git", "/a", "head-a"),
                start + Duration::from_secs(1),
            )
            .expect("trusted generation");

        let old_only = tracker.poll_one(start + Duration::from_secs(5), 1_005, &api).await;
        let CiRunDelta::Set(old_only) = &old_only[0].1 else { panic!("expected state") };
        assert_eq!(
            old_only.workflows[0].updated_at_epoch_secs,
            Some(1_000),
            "a run that reported no news must keep the clock the bar already shows"
        );
    }

    // @lat: [[test#GitHub CI Tracking#Every triggering event at the head]]
    #[tokio::test]
    async fn push_and_pull_request_runs_of_one_workflow_both_survive() {
        let start = Instant::now();
        let api = RunsApi::with_responses(vec![workflows_response(vec![
            (pushed(50), run(900, CiWorkflowStatus::InProgress, None)),
            ((50, "pull_request".into()), run(901, CiWorkflowStatus::InProgress, None)),
        ])]);
        let mut tracker = GithubCiTracker::default();
        tracker.accept_push(push("git@github.com:acme/widget.git", "/a", "head-a"), start);

        let observed = tracker.poll_one(start, 1_000, &api).await;
        let CiRunDelta::Set(observed) = &observed[0].1 else { panic!("expected state") };
        assert_eq!(
            observed.workflows.iter().map(|run| run.run_id).collect::<Vec<_>>(),
            [900, 901],
            "one workflow file triggered by two events must keep both runs"
        );
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
