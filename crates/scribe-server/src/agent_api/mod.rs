//! One-shot request routing for Scribe's local agent API.
//!
//! Capability handlers live in sibling modules. This foundational router keeps
//! admission, policy ordering, and auditing in one place.

pub mod activity;
pub mod policy;
pub mod text;
pub mod world;

use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alacritty_terminal::Term;
use scribe_common::agent::{
    AGENT_SURFACE_VERSION, AgentActionOutcome, AgentActionResult, AgentCapability,
    AgentCapabilityStatus, AgentError, AgentPayload, AgentPolicyMode, AgentRequest, AgentResponse,
    AgentScreenText,
};
use scribe_common::config::{AGENT_MAX_RESPONSE_BYTES_CEILING, AgentApiConfig};
use scribe_common::ids::{SessionId, WindowId};
use scribe_common::protocol::{AutomationAction, ServerMessage};
use scribe_pty::{async_fd::AsyncPtyFd, event_listener::ScribeEventListener};
use tokio::io::{AsyncWrite, AsyncWriteExt, WriteHalf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use self::activity::{ActivityTransition, AgentActivityLease, AgentActivityTracker};
use self::policy::{AgentPolicyEngine, PolicyResolution, mode_for};
use self::text::{copy_rows, format_rows};

/// Maximum simultaneous agent API requests. The dispatch result retains its
/// permit until the IPC reply is queued; future handlers retain it for their
/// complete request lifetime.
const MAX_IN_FLIGHT_REQUESTS: usize = 4;

/// Server-owned admission state for one-shot agent requests.
#[derive(Clone)]
pub struct AgentApiState {
    policy: AgentPolicyEngine,
    in_flight: Arc<Semaphore>,
    next_correlation_id: Arc<AtomicU64>,
    next_snapshot_id: Arc<AtomicU64>,
    pending_actions: Arc<Mutex<HashMap<u64, PendingAction>>>,
    activity: AgentActivityTracker,
    activity_transitions: Arc<Mutex<Option<mpsc::UnboundedReceiver<ActivityTransition>>>>,
}

struct PendingAction {
    writer_key: usize,
    action: AutomationAction,
    completion: oneshot::Sender<Result<AgentActionResult, AgentError>>,
}

impl AgentApiState {
    #[must_use]
    pub fn new(policy: AgentApiConfig) -> Self {
        let (transitions, receiver) = mpsc::unbounded_channel();
        let activity =
            AgentActivityTracker::new(Duration::from_millis(policy.activity_dwell_ms), transitions);
        Self {
            policy: AgentPolicyEngine::new(policy),
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS)),
            next_correlation_id: Arc::new(AtomicU64::new(1)),
            next_snapshot_id: Arc::new(AtomicU64::new(1)),
            pending_actions: Arc::new(Mutex::new(HashMap::new())),
            activity,
            activity_transitions: Arc::new(Mutex::new(Some(receiver))),
        }
    }

    /// Replace live policy and release held activity when every capability is denied.
    pub fn refresh_policy(&self, policy: AgentApiConfig) {
        let disabled = policy.read_metadata == AgentPolicyMode::Deny
            && policy.read_content == AgentPolicyMode::Deny
            && policy.dispatch_action == AgentPolicyMode::Deny
            && policy.dispatch_destructive_action == AgentPolicyMode::Deny
            && policy.write_input == AgentPolicyMode::Deny;
        self.policy.refresh(policy);
        if disabled {
            self.activity.release_all();
        }
    }

    /// Per-session activity leases behind the tab agent indicator.
    #[must_use]
    pub fn activity(&self) -> &AgentActivityTracker {
        &self.activity
    }

    /// Take the indicator transition stream. The server consumes it once at startup.
    pub fn take_activity_transitions(&self) -> Option<mpsc::UnboundedReceiver<ActivityTransition>> {
        self.activity_transitions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()
    }

    /// Resolve a user decision for one pending capability prompt.
    pub fn resolve_prompt(
        &self,
        prompt_id: scribe_common::protocol::PromptId,
        decision: scribe_common::protocol::ClipboardDecision,
    ) -> bool {
        self.policy.resolve(prompt_id, decision)
    }

    fn try_acquire(&self) -> Result<OwnedSemaphorePermit, AgentError> {
        Arc::clone(&self.in_flight)
            .try_acquire_owned()
            .map_err(|_| AgentError::Busy { message: "agent request capacity reached".into() })
    }

    /// Reserve a correlation before the caller queues `message()` to a client.
    pub fn begin_correlated_action(
        &self,
        client_key: usize,
        action: AutomationAction,
    ) -> PendingCorrelatedAction {
        let correlation_id = self.next_correlation_id.fetch_add(1, Ordering::Relaxed);
        let (completion, receiver) = oneshot::channel();
        self.pending_actions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(
            correlation_id,
            PendingAction { writer_key: client_key, action: action.clone(), completion },
        );
        PendingCorrelatedAction {
            correlation_id,
            message: ServerMessage::RunActionCorrelated { correlation_id, action },
            receiver: Some(receiver),
            pending_actions: Arc::clone(&self.pending_actions),
        }
    }

    /// Send one action and wait for the target client's execution report.
    pub async fn run_correlated_action<Send, Sent>(
        &self,
        client_key: usize,
        action: AutomationAction,
        timeout: Duration,
        send: Send,
    ) -> Result<AgentActionResult, AgentError>
    where
        Send: FnOnce(ServerMessage) -> Sent,
        Sent: Future<Output = bool>,
    {
        let pending = self.begin_correlated_action(client_key, action);
        if !send(pending.message().clone()).await {
            return Err(action_failed("client disconnected before action dispatch"));
        }
        pending.wait(timeout).await
    }

    /// Resolve one action completion from the same client it was sent to.
    pub fn complete_action(
        &self,
        client_key: usize,
        correlation_id: u64,
        outcome: AgentActionOutcome,
        created_session_id: Option<scribe_common::ids::SessionId>,
    ) -> bool {
        let Some(pending) = self.take_action_for_writer(correlation_id, client_key) else {
            return false;
        };
        pending
            .completion
            .send(Ok(AgentActionResult { action: pending.action, outcome, created_session_id }))
            .is_ok()
    }

    /// Fail every action still awaiting this client during connection teardown.
    pub fn fail_actions_for_client(&self, writer_key: usize) {
        let pending = {
            let mut actions =
                self.pending_actions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let ids = actions
                .iter()
                .filter_map(|(id, action)| (action.writer_key == writer_key).then_some(*id))
                .collect::<Vec<_>>();
            ids.into_iter().filter_map(|id| actions.remove(&id)).collect::<Vec<_>>()
        };
        for action in pending {
            drop(
                action
                    .completion
                    .send(Err(action_failed("client disconnected before action completion"))),
            );
        }
    }

    fn take_action_for_writer(
        &self,
        correlation_id: u64,
        writer_key: usize,
    ) -> Option<PendingAction> {
        let mut actions =
            self.pending_actions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if actions.get(&correlation_id)?.writer_key != writer_key {
            return None;
        }
        actions.remove(&correlation_id)
    }
}

/// One registered correlation waiting for client foreground execution.
pub struct PendingCorrelatedAction {
    correlation_id: u64,
    message: ServerMessage,
    receiver: Option<oneshot::Receiver<Result<AgentActionResult, AgentError>>>,
    pending_actions: Arc<Mutex<HashMap<u64, PendingAction>>>,
}

impl PendingCorrelatedAction {
    #[must_use]
    pub fn message(&self) -> &ServerMessage {
        &self.message
    }

    pub async fn wait(mut self, timeout: Duration) -> Result<AgentActionResult, AgentError> {
        let Some(receiver) = self.receiver.take() else {
            return Err(AgentError::Internal {
                message: "action completion receiver missing".into(),
            });
        };
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(action_failed("client disconnected before action completion")),
            Err(_) => Err(action_failed("client did not complete the action before timeout")),
        }
    }
}

impl Drop for PendingCorrelatedAction {
    fn drop(&mut self) {
        self.pending_actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.correlation_id);
    }
}

fn action_failed(message: &str) -> AgentError {
    AgentError::ActionFailed { message: message.into() }
}

impl Default for AgentApiState {
    fn default() -> Self {
        Self::new(AgentApiConfig::default())
    }
}

/// A dispatched response that retains its admission permit until the caller
/// queues the one-shot reply.
pub struct AgentDispatch {
    response: AgentResponse,
    _permit: Option<OwnedSemaphorePermit>,
    _activity: Option<AgentActivityLease>,
}

impl AgentDispatch {
    #[must_use]
    pub fn response(&self) -> &AgentResponse {
        &self.response
    }
}

struct RequestMetadata<'a> {
    request_id: u64,
    agent_label: &'a str,
    capability: AgentCapability,
    target_kind: &'static str,
    target_id: String,
}

impl<'a> From<&'a AgentRequest> for RequestMetadata<'a> {
    fn from(request: &'a AgentRequest) -> Self {
        match request {
            AgentRequest::World { request_id, agent_label, .. }
            | AgentRequest::Capabilities { request_id, agent_label, .. } => Self {
                request_id: *request_id,
                agent_label,
                capability: AgentCapability::ReadMetadata,
                target_kind: "server",
                target_id: "server".into(),
            },
            AgentRequest::Siblings { request_id, agent_label, origin_session_id } => Self {
                request_id: *request_id,
                agent_label,
                capability: AgentCapability::ReadMetadata,
                target_kind: "session",
                target_id: origin_session_id.map_or_else(|| "none".into(), |id| id.to_string()),
            },
            AgentRequest::ReadScreen { request_id, agent_label, session_id, .. } => Self {
                request_id: *request_id,
                agent_label,
                capability: AgentCapability::ReadContent,
                target_kind: "session",
                target_id: session_id.to_string(),
            },
            AgentRequest::DispatchAction { request_id, agent_label, action, window, .. } => Self {
                request_id: *request_id,
                agent_label,
                capability: AgentCapability::for_action(action),
                target_kind: if window.is_some() { "window" } else { "server" },
                target_id: window.map_or_else(|| "server".into(), |id| id.to_string()),
            },
            AgentRequest::WriteInput { request_id, agent_label, session_id, .. } => Self {
                request_id: *request_id,
                agent_label,
                capability: AgentCapability::WriteInput,
                target_kind: "session",
                target_id: session_id.to_string(),
            },
        }
    }
}

/// Transport-owned registry seams: how an authorized request reaches server
/// state. Every one runs only after `authorize_before_lookup` admits the
/// request, so a denied call touches no registry, terminal, or client.
pub struct DispatchSources<CaptureWorld, Lookup, RunAction> {
    /// One ordered capture of the world registries for `World`/`Siblings`.
    pub capture_world: CaptureWorld,
    /// Terminal and PTY handle lookup for `ReadScreen` and `WriteInput`.
    pub lookup_session: Lookup,
    /// Correlated foreground execution for `DispatchAction`.
    pub run_action: RunAction,
}

/// Dispatch one transient agent request and reply on the same connection.
///
/// Policy evaluation deliberately precedes target lookup. Prompt-mode calls
/// park here until a capable local client answers, so no handler can
/// accidentally treat `Prompt` as `Allow`.
pub async fn dispatch<
    CaptureWorld,
    WorldFuture,
    Lookup,
    LookupFuture,
    RunAction,
    RunActionFuture,
    SendPrompt,
    PromptFuture,
>(
    state: &AgentApiState,
    caller: usize,
    request: &AgentRequest,
    sources: DispatchSources<CaptureWorld, Lookup, RunAction>,
    prompt_sender: Option<SendPrompt>,
) -> AgentDispatch
where
    CaptureWorld: FnOnce() -> WorldFuture,
    WorldFuture: Future<Output = world::Capture>,
    Lookup: FnOnce(SessionId) -> LookupFuture,
    LookupFuture: Future<Output = Option<AgentSessionTarget>>,
    RunAction: FnOnce(Option<WindowId>, AutomationAction) -> RunActionFuture,
    RunActionFuture: Future<Output = Result<AgentActionResult, AgentError>>,
    SendPrompt: FnOnce(ServerMessage) -> PromptFuture,
    PromptFuture: Future<Output = ()>,
{
    let metadata = RequestMetadata::from(request);
    let (result, permit, activity) = if let Err(error) = validate_request_bounds(state, request) {
        (Err(error), None, None)
    } else {
        match state.try_acquire() {
            Err(error) => (Err(error), None, None),
            Ok(permit) => {
                let (result, activity) = if matches!(request, AgentRequest::Capabilities { .. }) {
                    (Ok(capabilities_payload(&state.policy.config())), None)
                } else if let Err(error) =
                    authorize_before_lookup(state, &metadata, prompt_sender).await
                {
                    (Err(error), None)
                } else {
                    route_authorized_request(state, caller, request, sources).await
                };
                (result, Some(permit), activity)
            }
        }
    };

    let decision = match &result {
        Ok(_) => "allow",
        Err(AgentError::Busy { .. }) => "busy",
        Err(_) => "deny",
    };
    let mut response = AgentResponse { request_id: metadata.request_id, result };
    enforce_serialized_response_ceiling(&mut response);
    emit_audit(&metadata, decision, serialized_response_bytes(&response));
    AgentDispatch { response, _permit: permit, _activity: activity }
}

/// Reject bounded requests before policy can raise a confirmation prompt.
fn validate_request_bounds(
    state: &AgentApiState,
    request: &AgentRequest,
) -> Result<(), AgentError> {
    let AgentRequest::WriteInput { text, .. } = request else {
        return Ok(());
    };
    let max_bytes = state.policy.config().max_input_bytes;
    let input_bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
    if input_bytes > max_bytes {
        return Err(AgentError::TooLarge {
            message: format!("agent input is {input_bytes} bytes; maximum is {max_bytes}"),
        });
    }
    Ok(())
}

/// Shared policy seam for capability handlers. Target lookup must happen only
/// after this future resolves successfully.
async fn authorize_before_lookup<SendPrompt, PromptFuture>(
    state: &AgentApiState,
    metadata: &RequestMetadata<'_>,
    prompt_sender: Option<SendPrompt>,
) -> Result<(), AgentError>
where
    SendPrompt: FnOnce(ServerMessage) -> PromptFuture,
    PromptFuture: Future<Output = ()>,
{
    match state.policy.authorize(
        metadata.agent_label,
        metadata.capability,
        &metadata.target_id,
        prompt_sender.is_some(),
    ) {
        PolicyResolution::Allow => Ok(()),
        PolicyResolution::Deny => Err(denied()),
        PolicyResolution::Prompt { prompt, pending } => {
            let Some(send_prompt) = prompt_sender else {
                return Err(denied());
            };
            send_prompt(ServerMessage::AgentPromptRequest {
                prompt_id: prompt.prompt_id,
                agent_label: prompt.agent_label,
                capability: prompt.capability,
                target: prompt.target,
            })
            .await;
            pending.wait().await
        }
        PolicyResolution::Parked(pending) => pending.wait().await,
    }
}

/// Every capability this build supports, in the order `Capabilities` reports
/// them.
const ALL_CAPABILITIES: [AgentCapability; 5] = [
    AgentCapability::ReadMetadata,
    AgentCapability::ReadContent,
    AgentCapability::DispatchAction,
    AgentCapability::DispatchDestructiveAction,
    AgentCapability::WriteInput,
];

/// Surface version plus every supported capability's current policy mode.
/// Always answerable, independent of any of those modes, so a caller learns
/// what is available — and which setting unlocks it — instead of spending a
/// turn on a refusal.
fn capabilities_payload(policy: &AgentApiConfig) -> AgentPayload {
    AgentPayload::Capabilities {
        version: AGENT_SURFACE_VERSION,
        capabilities: ALL_CAPABILITIES
            .into_iter()
            .map(|capability| AgentCapabilityStatus {
                capability,
                mode: mode_for(policy, capability),
            })
            .collect(),
    }
}

/// Terminal handles and identifying metadata copied from the live-session
/// registry only after policy authorizes the lookup.
pub struct AgentSessionTarget {
    pub term: Arc<tokio::sync::Mutex<Term<ScribeEventListener>>>,
    pub pty_write: Arc<tokio::sync::Mutex<WriteHalf<AsyncPtyFd>>>,
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
}

/// Handler-routing seam. Unimplemented capability variants stay default-safe.
/// `sources` is consumed only here — after `authorize_before_lookup` — so no
/// registry is read for a request policy has not admitted.
async fn route_authorized_request<
    CaptureWorld,
    WorldFuture,
    Lookup,
    LookupFuture,
    RunAction,
    RunActionFuture,
>(
    state: &AgentApiState,
    caller: usize,
    request: &AgentRequest,
    sources: DispatchSources<CaptureWorld, Lookup, RunAction>,
) -> (Result<AgentPayload, AgentError>, Option<AgentActivityLease>)
where
    CaptureWorld: FnOnce() -> WorldFuture,
    WorldFuture: Future<Output = world::Capture>,
    Lookup: FnOnce(SessionId) -> LookupFuture,
    LookupFuture: Future<Output = Option<AgentSessionTarget>>,
    RunAction: FnOnce(Option<WindowId>, AutomationAction) -> RunActionFuture,
    RunActionFuture: Future<Output = Result<AgentActionResult, AgentError>>,
{
    let DispatchSources { capture_world, lookup_session, run_action } = sources;
    match request {
        AgentRequest::World { origin_session_id, .. } => (
            Ok(AgentPayload::World {
                snapshot: world::world(capture_world().await, *origin_session_id),
            }),
            None,
        ),
        AgentRequest::Siblings { origin_session_id, .. } => (
            world::siblings(capture_world().await, *origin_session_id)
                .map(|snapshot| AgentPayload::Siblings { snapshot }),
            None,
        ),
        AgentRequest::ReadScreen { session_id, scrollback_lines, .. } => {
            match read_screen(
                state,
                caller,
                *session_id,
                scrollback_lines.unwrap_or(0),
                lookup_session,
            )
            .await
            {
                Ok((payload, activity)) => (Ok(payload), Some(activity)),
                Err(error) => (Err(error), None),
            }
        }
        AgentRequest::DispatchAction { action, window, .. } => (
            run_action(*window, action.clone())
                .await
                .map(|result| AgentPayload::DispatchAction { result }),
            None,
        ),
        AgentRequest::WriteInput { session_id, text, submit, .. } => {
            let Some(target) = lookup_session(*session_id).await else {
                return (Err(not_found(*session_id)), None);
            };
            let activity = state.activity.acquire(*session_id, caller);
            let mut writer = target.pty_write.lock().await;
            let result = write_agent_input(&mut *writer, *session_id, text, *submit)
                .await
                .map(|()| AgentPayload::WriteInput);
            (result, Some(activity))
        }
        // `dispatch` answers `Capabilities` before this seam is reached; the
        // arm stays default-safe so a routing change cannot silently allow it.
        AgentRequest::Capabilities { .. } => (Err(denied()), None),
    }
}

async fn read_screen<Lookup, LookupFuture>(
    state: &AgentApiState,
    caller: usize,
    session_id: SessionId,
    requested_scrollback: u32,
    lookup_screen: Lookup,
) -> Result<(AgentPayload, AgentActivityLease), AgentError>
where
    Lookup: FnOnce(SessionId) -> LookupFuture,
    LookupFuture: Future<Output = Option<AgentSessionTarget>>,
{
    let Some(target) = lookup_screen(session_id).await else {
        return Err(not_found(session_id));
    };
    let activity = state.activity.acquire(session_id, caller);
    let payload =
        screen_payload(state, session_id, target, requested_scrollback, &state.policy.config())
            .await;
    Ok((payload, activity))
}

async fn screen_payload(
    state: &AgentApiState,
    session_id: SessionId,
    target: AgentSessionTarget,
    requested_scrollback: u32,
    policy: &AgentApiConfig,
) -> AgentPayload {
    let (rows, captured_at, snapshot_id) =
        capture_rows(state, &target.term, requested_scrollback, policy.max_scrollback_lines).await;
    let max_bytes = usize::try_from(policy.max_response_bytes).unwrap_or(usize::MAX);
    let extracted = format_rows(&rows, max_bytes);
    AgentPayload::ReadScreen {
        screen: AgentScreenText {
            session_id,
            title: target.title,
            cwd: target.cwd,
            text: extracted.text,
            lines: extracted.lines,
            truncated: extracted.truncated,
            captured_at,
            snapshot_id,
        },
    }
}

async fn write_agent_input<W: AsyncWrite + Unpin>(
    writer: &mut W,
    session_id: SessionId,
    text: &str,
    submit: bool,
) -> Result<(), AgentError> {
    let mut payload = Vec::with_capacity(text.len() + usize::from(submit));
    payload.extend_from_slice(text.as_bytes());
    if submit {
        payload.push(b'\r');
    }
    writer.write_all(&payload).await.map_err(|error| AgentError::ActionFailed {
        message: format!("failed to write input to session {session_id}: {error}"),
    })
}

async fn capture_rows(
    state: &AgentApiState,
    term: &Arc<tokio::sync::Mutex<Term<ScribeEventListener>>>,
    requested_scrollback: u32,
    max_scrollback_lines: u32,
) -> (Vec<text::CopiedRow>, u64, u64) {
    let term = term.lock().await;
    let rows = copy_rows(&term, requested_scrollback, max_scrollback_lines);
    let captured_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let snapshot_id = state.next_snapshot_id.fetch_add(1, Ordering::Relaxed);
    (rows, captured_at, snapshot_id)
}

fn serialized_response_bytes(response: &AgentResponse) -> usize {
    rmp_serde::to_vec_named(&ServerMessage::AgentResponse(response.clone()))
        .map_or(0, |reply| reply.len())
}

fn enforce_serialized_response_ceiling(response: &mut AgentResponse) {
    let ceiling = usize::try_from(AGENT_MAX_RESPONSE_BYTES_CEILING).unwrap_or(usize::MAX);
    loop {
        let response_bytes = serialized_response_bytes(response);
        if response_bytes <= ceiling {
            break;
        }
        let overflow = response_bytes.saturating_sub(ceiling).max(1);
        let Ok(AgentPayload::ReadScreen { screen }) = &mut response.result else {
            break;
        };
        if screen.text.is_empty() {
            response.result = Err(AgentError::TooLarge {
                message: "agent screen response metadata exceeds maximum size".into(),
            });
            break;
        }
        let mut keep = screen.text.len().saturating_sub(overflow);
        while keep > 0 && !screen.text.is_char_boundary(keep) {
            keep -= 1;
        }
        screen.text.truncate(keep);
        screen.lines = u32::try_from(screen.text.lines().count()).unwrap_or(u32::MAX);
        screen.truncated = true;
    }
}

fn not_found(session_id: SessionId) -> AgentError {
    AgentError::NotFound { message: format!("session {session_id} not found") }
}

fn denied() -> AgentError {
    AgentError::Denied { message: "agent capability denied by policy".into() }
}

/// Emit exactly one metadata-only audit event per request.
fn emit_audit(metadata: &RequestMetadata<'_>, decision: &'static str, response_bytes: usize) {
    tracing::event!(
        name: "agent_call",
        target: "scribe::agent_api",
        tracing::Level::INFO,
        agent_label = metadata.agent_label,
        capability = ?metadata.capability,
        target_kind = metadata.target_kind,
        target_id = metadata.target_id,
        decision,
        response_bytes,
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read as _;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use alacritty_terminal::Term;
    use alacritty_terminal::grid::Dimensions;
    use scribe_common::agent::{
        AGENT_SURFACE_VERSION, AgentActionOutcome, AgentActionResult, AgentCapability,
        AgentCapabilityStatus, AgentError, AgentPayload, AgentPolicyMode, AgentRequest,
        AgentResponse, AgentScreenText,
    };
    use scribe_common::config::AgentApiConfig;
    use scribe_common::framing::{read_message, write_message};
    use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
    use scribe_common::protocol::{
        AutomationAction, ClientMessage, ClipboardDecision, ServerMessage,
    };
    use scribe_pty::{async_fd::AsyncPtyFd, event_listener::ScribeEventListener};
    use tokio::io::AsyncReadExt;
    use tokio::sync::{Barrier, Mutex, Semaphore, mpsc};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use vte::ansi::Processor as AnsiProcessor;

    use super::{
        AGENT_MAX_RESPONSE_BYTES_CEILING, ALL_CAPABILITIES, AgentApiState, AgentSessionTarget,
        DispatchSources, MAX_IN_FLIGHT_REQUESTS, RequestMetadata, authorize_before_lookup,
        dispatch, enforce_serialized_response_ceiling, serialized_response_bytes, world,
        write_agent_input,
    };
    use crate::ipc_server::{WindowShares, test_shared_writer};
    use crate::session_manager::build_term_config;
    use crate::workspace_manager::WorkspaceManager;

    // `World` routes through the read-metadata gate, so under the default
    // all-`Deny` policy it exercises the deny path without touching any
    // registry; `Capabilities` is the one variant with its own wiring below,
    // so it is deliberately excluded from this generic helper.
    fn request(request_id: u64) -> AgentRequest {
        AgentRequest::World {
            request_id,
            agent_label: "socket-test".into(),
            origin_session_id: None,
        }
    }

    fn world_request(request_id: u64, origin: Option<SessionId>) -> AgentRequest {
        AgentRequest::World {
            request_id,
            agent_label: "world-test".into(),
            origin_session_id: origin,
        }
    }

    fn siblings_request(request_id: u64, origin: Option<SessionId>) -> AgentRequest {
        AgentRequest::Siblings {
            request_id,
            agent_label: "world-test".into(),
            origin_session_id: origin,
        }
    }

    fn capabilities_request(request_id: u64) -> AgentRequest {
        AgentRequest::Capabilities {
            request_id,
            agent_label: "capability-test".into(),
            origin_session_id: None,
        }
    }

    fn screen_request(request_id: u64, session_id: SessionId) -> AgentRequest {
        screen_request_for("screen-test", request_id, session_id)
    }

    fn screen_request_for(
        agent_label: &str,
        request_id: u64,
        session_id: SessionId,
    ) -> AgentRequest {
        AgentRequest::ReadScreen {
            request_id,
            agent_label: agent_label.into(),
            origin_session_id: None,
            session_id,
            scrollback_lines: Some(1),
        }
    }

    fn action_request(
        request_id: u64,
        action: AutomationAction,
        window: Option<WindowId>,
    ) -> AgentRequest {
        AgentRequest::DispatchAction {
            request_id,
            agent_label: "action-test".into(),
            origin_session_id: None,
            action,
            window,
        }
    }

    fn deny_destructive_prompt(state: &AgentApiState, message: &ServerMessage) {
        let ServerMessage::AgentPromptRequest { prompt_id, capability, .. } = message else {
            panic!("expected agent prompt");
        };
        assert_eq!(*capability, AgentCapability::DispatchDestructiveAction);
        assert!(state.resolve_prompt(*prompt_id, ClipboardDecision::DenyOnce));
    }

    fn write_request(
        request_id: u64,
        session_id: SessionId,
        text: impl Into<String>,
        submit: bool,
    ) -> AgentRequest {
        AgentRequest::WriteInput {
            request_id,
            agent_label: "write-test".into(),
            origin_session_id: None,
            session_id,
            text: text.into(),
            submit,
        }
    }

    #[derive(Clone, Debug)]
    struct CapturedEvent {
        name: &'static str,
        target: &'static str,
        fields: BTreeMap<String, String>,
    }

    #[derive(Clone)]
    struct AuditCapture(Arc<StdMutex<Vec<CapturedEvent>>>);

    impl<S> tracing_subscriber::Layer<S> for AuditCapture
    where
        S: tracing::Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
            let metadata = event.metadata();
            if metadata.name() != "agent_call" || metadata.target() != "scribe::agent_api" {
                return;
            }
            let mut fields = FieldCapture::default();
            event.record(&mut fields);
            self.0.lock().unwrap().push(CapturedEvent {
                name: metadata.name(),
                target: metadata.target(),
                fields: fields.0,
            });
        }
    }

    #[derive(Default)]
    struct FieldCapture(BTreeMap<String, String>);

    fn install_audit_capture() -> Arc<StdMutex<Vec<CapturedEvent>>> {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(AuditCapture(Arc::clone(&events)));
        tracing::subscriber::set_global_default(subscriber).expect("install audit capture");
        tracing::callsite::rebuild_interest_cache();
        events
    }

    impl Visit for FieldCapture {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().into(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().into(), value.into());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().into(), value.to_string());
        }
    }

    struct TestDims {
        cols: usize,
        rows: usize,
    }

    impl Dimensions for TestDims {
        fn total_lines(&self) -> usize {
            self.rows
        }

        fn screen_lines(&self) -> usize {
            self.rows
        }

        fn columns(&self) -> usize {
            self.cols
        }
    }

    fn term_with_bytes(
        bytes: &[u8],
        cols: usize,
        rows: usize,
    ) -> Arc<Mutex<Term<ScribeEventListener>>> {
        let (sender, _receiver) = mpsc::unbounded_channel();
        let listener = ScribeEventListener::new(SessionId::new(), sender);
        let mut term = Term::new(build_term_config(100), &TestDims { cols, rows }, listener);
        let mut processor: AnsiProcessor = AnsiProcessor::new();
        processor.advance(&mut term, bytes);
        Arc::new(Mutex::new(term))
    }

    fn session_target(
        term: Arc<Mutex<Term<ScribeEventListener>>>,
        title: Option<String>,
        cwd: Option<PathBuf>,
    ) -> (AgentSessionTarget, StdUnixStream) {
        let (writer, reader) = StdUnixStream::pair().expect("socket pair");
        writer.set_nonblocking(true).expect("writer nonblocking");
        let writer: OwnedFd = writer.into();
        let writer = AsyncPtyFd::new(writer).expect("register test writer");
        let (_read, write) = tokio::io::split(writer);
        (AgentSessionTarget { term, pty_write: Arc::new(Mutex::new(write)), title, cwd }, reader)
    }

    fn read_peer(mut peer: &StdUnixStream, expected: &[u8]) {
        let mut received = vec![0; expected.len()];
        peer.read_exact(&mut received).expect("read written input");
        assert_eq!(received, expected);
    }

    fn allow_prompt(state: &AgentApiState, message: &ServerMessage, expected: AgentCapability) {
        let ServerMessage::AgentPromptRequest { prompt_id, capability, .. } = message else {
            panic!("expected agent prompt request");
        };
        assert_eq!(*capability, expected);
        assert!(state.resolve_prompt(*prompt_id, ClipboardDecision::AllowOnce));
    }

    async fn dispatch_headless(
        state: &AgentApiState,
        request: &AgentRequest,
    ) -> super::AgentDispatch {
        dispatch(
            state,
            0,
            request,
            DispatchSources {
                capture_world: || async { panic!("headless dispatch has no world registries") },
                lookup_session: |_| async { None },
                run_action: |_, _| async {
                    Err(AgentError::Internal { message: "unexpected action".into() })
                },
            },
            None::<fn(ServerMessage) -> std::future::Ready<()>>,
        )
        .await
    }

    fn unix_stream_pair() -> (tokio::net::UnixStream, tokio::net::UnixStream) {
        let (left, right) = StdUnixStream::pair().expect("socket pair");
        left.set_nonblocking(true).expect("left nonblocking");
        right.set_nonblocking(true).expect("right nonblocking");
        (
            tokio::net::UnixStream::from_std(left).expect("tokio left"),
            tokio::net::UnixStream::from_std(right).expect("tokio right"),
        )
    }

    #[tokio::test]
    async fn agent_request_over_a_real_socket_reaches_the_denied_stub() {
        let (server, client) = unix_stream_pair();
        let (mut server_reader, mut server_writer) = tokio::io::split(server);
        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        write_message(&mut client_writer, &ClientMessage::AgentRequest(request(7)))
            .await
            .expect("write request");
        let ClientMessage::AgentRequest(request) =
            read_message(&mut server_reader).await.expect("read request")
        else {
            panic!("expected agent request");
        };

        let dispatch = dispatch_headless(&AgentApiState::default(), &request).await;
        write_message(
            &mut server_writer,
            &ServerMessage::AgentResponse(dispatch.response().clone()),
        )
        .await
        .expect("write reply");

        let reply: ServerMessage = read_message(&mut client_reader).await.expect("read reply");
        assert!(matches!(
            reply,
            ServerMessage::AgentResponse(reply_response)
                if reply_response.request_id == 7
                    && matches!(reply_response.result, Err(AgentError::Denied { .. }))
        ));
    }

    #[tokio::test]
    async fn read_screen_returns_viewport_scrollback_and_identity() {
        let state = AgentApiState::new(AgentApiConfig {
            read_content: AgentPolicyMode::Allow,
            ..AgentApiConfig::default()
        });
        let session_id = SessionId::new();
        let (target, _peer) = session_target(
            term_with_bytes(b"old\r\nview1\r\nview2", 8, 2),
            Some("build".into()),
            Some(PathBuf::from("/work/scribe")),
        );
        let response = dispatch(
            &state,
            0,
            &screen_request(8, session_id),
            DispatchSources {
                capture_world: || async { panic!("screen reads must not capture the world") },
                lookup_session: move |_| async move { Some(target) },
                run_action: |_, _| async {
                    Err(AgentError::Internal { message: "unexpected action".into() })
                },
            },
            None::<fn(ServerMessage) -> std::future::Ready<()>>,
        )
        .await;
        let Ok(AgentPayload::ReadScreen { screen }) = response.response().result.clone() else {
            panic!("expected read-screen payload");
        };
        assert_eq!(screen.session_id, session_id);
        assert_eq!(screen.title.as_deref(), Some("build"));
        assert_eq!(screen.cwd.as_deref(), Some(std::path::Path::new("/work/scribe")));
        assert_eq!(screen.text, "old\nview1\nview2");
        assert_eq!(screen.lines, 3);
        assert!(!screen.truncated);
        assert!(screen.captured_at > 0);
        assert_eq!(screen.snapshot_id, 1);
    }

    #[tokio::test]
    async fn read_screen_stays_within_content_cap_and_marks_truncation() {
        let state = AgentApiState::new(AgentApiConfig {
            read_content: AgentPolicyMode::Allow,
            max_response_bytes: 7,
            ..AgentApiConfig::default()
        });
        let session_id = SessionId::new();
        let (target, _peer) = session_target(term_with_bytes(b"hello\r\nworld", 8, 2), None, None);
        let response = dispatch(
            &state,
            0,
            &screen_request(9, session_id),
            DispatchSources {
                capture_world: || async { panic!("screen reads must not capture the world") },
                lookup_session: move |_| async move { Some(target) },
                run_action: |_, _| async {
                    Err(AgentError::Internal { message: "unexpected action".into() })
                },
            },
            None::<fn(ServerMessage) -> std::future::Ready<()>>,
        )
        .await;
        let Ok(AgentPayload::ReadScreen { screen }) = response.response().result.clone() else {
            panic!("expected read-screen payload");
        };
        assert!(screen.text.len() <= 7);
        assert_eq!(screen.text, "hello\nw");
        assert!(screen.truncated);
    }

    #[test]
    fn serialized_screen_response_stays_within_the_hard_ceiling() {
        let mut response = AgentResponse {
            request_id: 35,
            result: Ok(AgentPayload::ReadScreen {
                screen: AgentScreenText {
                    session_id: SessionId::new(),
                    title: Some("benchmark".into()),
                    cwd: Some(PathBuf::from("/work/scribe")),
                    text: "é".repeat(usize::try_from(AGENT_MAX_RESPONSE_BYTES_CEILING).unwrap()),
                    lines: 1,
                    truncated: false,
                    captured_at: 1,
                    snapshot_id: 1,
                },
            }),
        };
        enforce_serialized_response_ceiling(&mut response);
        assert!(
            serialized_response_bytes(&response)
                <= usize::try_from(AGENT_MAX_RESPONSE_BYTES_CEILING).unwrap()
        );
        let Ok(AgentPayload::ReadScreen { screen }) = &response.result else {
            panic!("expected read-screen payload");
        };
        assert!(screen.truncated);
        assert!(screen.text.is_char_boundary(screen.text.len()));
        assert_eq!(screen.lines, u32::try_from(screen.text.lines().count()).unwrap());
    }

    #[tokio::test]
    async fn read_screen_deny_returns_no_content_before_session_lookup() {
        let request = screen_request(10, SessionId::new());
        let response = dispatch(
            &AgentApiState::default(),
            0,
            &request,
            DispatchSources {
                capture_world: || async { panic!("denied read must not capture the world") },
                lookup_session: |_| async {
                    panic!("denied read must not look up terminal content")
                },
                run_action: |_, _| async { panic!("denied read must not run an action") },
            },
            None::<fn(ServerMessage) -> std::future::Ready<()>>,
        )
        .await;
        assert!(matches!(response.response().result, Err(AgentError::Denied { .. })));
    }

    #[tokio::test]
    async fn authorized_read_of_missing_session_returns_not_found() {
        let state = AgentApiState::new(AgentApiConfig {
            read_content: AgentPolicyMode::Allow,
            ..AgentApiConfig::default()
        });
        let response = dispatch_headless(&state, &screen_request(11, SessionId::new())).await;
        assert!(matches!(response.response().result, Err(AgentError::NotFound { .. })));
    }

    #[tokio::test]
    async fn oversized_write_is_rejected_before_prompt_or_session_lookup() {
        let state = AgentApiState::new(AgentApiConfig {
            write_input: AgentPolicyMode::Prompt,
            max_input_bytes: 3,
            ..AgentApiConfig::default()
        });
        let session_id = SessionId::new();
        let looked_up = Arc::new(AtomicBool::new(false));
        let lookup_called = Arc::clone(&looked_up);
        let prompt_count = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::clone(&prompt_count);
        let response = dispatch(
            &state,
            0,
            &write_request(20, session_id, "éé", false),
            DispatchSources {
                capture_world: || async { panic!("writes must not capture the world") },
                lookup_session: move |_| async move {
                    lookup_called.store(true, Ordering::Relaxed);
                    None
                },
                run_action: |_, _| async {
                    Err(AgentError::Internal { message: "unexpected action".into() })
                },
            },
            Some(move |_| async move {
                prompts.fetch_add(1, Ordering::Relaxed);
                panic!("oversized input must not prompt");
            }),
        )
        .await;
        assert!(matches!(response.response().result, Err(AgentError::TooLarge { .. })));
        assert!(!looked_up.load(Ordering::Relaxed));
        assert_eq!(prompt_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn prompted_write_uses_one_decision_for_utf8_text_and_submit() {
        let state = AgentApiState::new(AgentApiConfig {
            write_input: AgentPolicyMode::Prompt,
            max_input_bytes: 16,
            ..AgentApiConfig::default()
        });
        let session_id = SessionId::new();
        let (target, peer) = session_target(term_with_bytes(b"", 8, 2), None, None);
        let prompt_count = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::clone(&prompt_count);
        let resolver = state.clone();
        let response = dispatch(
            &state,
            0,
            &write_request(21, session_id, "hé", true),
            DispatchSources {
                capture_world: || async { panic!("writes must not capture the world") },
                lookup_session: move |_| async move { Some(target) },
                run_action: |_, _| async {
                    Err(AgentError::Internal { message: "unexpected action".into() })
                },
            },
            Some(move |message| async move {
                prompts.fetch_add(1, Ordering::Relaxed);
                allow_prompt(&resolver, &message, AgentCapability::WriteInput);
            }),
        )
        .await;
        assert!(matches!(response.response().result, Ok(AgentPayload::WriteInput)));
        assert_eq!(prompt_count.load(Ordering::Relaxed), 1);
        read_peer(&peer, b"h\xc3\xa9\r");
    }

    #[tokio::test]
    async fn write_without_submit_injects_text_only() {
        let state = AgentApiState::new(AgentApiConfig {
            write_input: AgentPolicyMode::Allow,
            ..AgentApiConfig::default()
        });
        let session_id = SessionId::new();
        let (target, peer) = session_target(term_with_bytes(b"", 8, 2), None, None);
        let response = dispatch(
            &state,
            0,
            &write_request(22, session_id, "plain", false),
            DispatchSources {
                capture_world: || async { panic!("writes must not capture the world") },
                lookup_session: move |_| async move { Some(target) },
                run_action: |_, _| async {
                    Err(AgentError::Internal { message: "unexpected action".into() })
                },
            },
            None::<fn(ServerMessage) -> std::future::Ready<()>>,
        )
        .await;
        assert!(matches!(response.response().result, Ok(AgentPayload::WriteInput)));
        read_peer(&peer, b"plain");
    }

    #[tokio::test]
    async fn authorized_write_of_missing_session_returns_not_found() {
        let state = AgentApiState::new(AgentApiConfig {
            write_input: AgentPolicyMode::Allow,
            ..AgentApiConfig::default()
        });
        let response =
            dispatch_headless(&state, &write_request(23, SessionId::new(), "missing", false)).await;
        assert!(matches!(response.response().result, Err(AgentError::NotFound { .. })));
    }

    #[tokio::test]
    async fn pty_write_failure_maps_to_action_failed() {
        let state = AgentApiState::new(AgentApiConfig {
            write_input: AgentPolicyMode::Allow,
            ..AgentApiConfig::default()
        });
        let session_id = SessionId::new();
        let (target, peer) = session_target(term_with_bytes(b"", 8, 2), None, None);
        drop(peer);
        let response = dispatch(
            &state,
            0,
            &write_request(24, session_id, "fail", true),
            DispatchSources {
                capture_world: || async { panic!("writes must not capture the world") },
                lookup_session: move |_| async move { Some(target) },
                run_action: |_, _| async {
                    Err(AgentError::Internal { message: "unexpected action".into() })
                },
            },
            None::<fn(ServerMessage) -> std::future::Ready<()>>,
        )
        .await;
        assert!(matches!(response.response().result, Err(AgentError::ActionFailed { .. })));
    }

    #[tokio::test]
    async fn write_acknowledgement_waits_for_the_full_payload() {
        let session_id = SessionId::new();
        let (mut writer, mut reader) = tokio::io::duplex(1);
        let write =
            tokio::spawn(
                async move { write_agent_input(&mut writer, session_id, "abc", true).await },
            );
        tokio::task::yield_now().await;
        assert!(!write.is_finished());

        let mut payload = [0; 4];
        reader.read_exact(&mut payload).await.expect("read full payload");
        assert_eq!(&payload, b"abc\r");
        assert!(write.await.expect("write task").is_ok());
    }

    /// Three live sessions across two windows, captured through the real
    /// `world::capture` registries the `ipc_server` call site uses.
    struct WorldFixture {
        live: Arc<tokio::sync::RwLock<std::collections::HashMap<SessionId, WorkspaceId>>>,
        shares: WindowShares,
        workspaces: Arc<tokio::sync::RwLock<WorkspaceManager>>,
        caller: SessionId,
        caller_window: WindowId,
    }

    fn world_fixture() -> WorldFixture {
        let caller = SessionId::new();
        let sibling = SessionId::new();
        let other = SessionId::new();
        let caller_window = WindowId::new();
        let other_window = WindowId::new();
        let mut manager = WorkspaceManager::new(Vec::new());
        let workspace_id = manager.create_workspace();
        for (session, window) in
            [(caller, caller_window), (sibling, caller_window), (other, other_window)]
        {
            manager.add_session(workspace_id, session, None);
            manager.assign_session_to_window(window, session);
        }
        WorldFixture {
            live: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::from([
                (caller, workspace_id),
                (sibling, workspace_id),
                (other, workspace_id),
            ]))),
            shares: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            workspaces: Arc::new(tokio::sync::RwLock::new(manager)),
            caller,
            caller_window,
        }
    }

    impl WorldFixture {
        async fn capture(&self) -> world::Capture {
            world::capture(&self.live, &self.shares, &self.workspaces, |id, workspace, window| {
                world::CapturedSession {
                    session_id: id,
                    window_id: window.expect("fixture sessions are window-assigned"),
                    workspace_id: *workspace,
                    title: None,
                    cwd: None,
                    ai_state: None,
                    ai_provider_hint: None,
                    task_label: None,
                }
            })
            .await
        }

        async fn dispatch(&self, request: &AgentRequest) -> super::AgentDispatch {
            let state = AgentApiState::new(AgentApiConfig {
                read_metadata: AgentPolicyMode::Allow,
                ..AgentApiConfig::default()
            });
            dispatch(
                &state,
                0,
                request,
                DispatchSources {
                    capture_world: || self.capture(),
                    lookup_session: |_| async {
                        panic!("world routing must not look up terminal content")
                    },
                    run_action: |_, _| async { panic!("world routing must not run an action") },
                },
                None::<fn(ServerMessage) -> std::future::Ready<()>>,
            )
            .await
        }
    }

    #[tokio::test]
    async fn world_request_returns_one_snapshot_with_the_caller_marked() {
        let fixture = world_fixture();
        let response = fixture.dispatch(&world_request(30, Some(fixture.caller))).await;
        let Ok(AgentPayload::World { snapshot }) = response.response().result.clone() else {
            panic!("expected world payload");
        };
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.workspaces.len(), 2);
        assert_eq!(snapshot.sessions.len(), 3);
        assert!(snapshot.snapshot_id > 0);
        assert!(snapshot.captured_at > 0);
        let callers: Vec<_> =
            snapshot.sessions.iter().filter(|session| session.is_caller).collect();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers.first().map(|session| session.session_id), Some(fixture.caller));
    }

    #[tokio::test]
    async fn siblings_request_narrows_the_snapshot_to_the_origin_window() {
        let fixture = world_fixture();
        let response = fixture.dispatch(&siblings_request(31, Some(fixture.caller))).await;
        let Ok(AgentPayload::Siblings { snapshot }) = response.response().result.clone() else {
            panic!("expected siblings payload");
        };
        assert_eq!(snapshot.sessions.len(), 2);
        assert!(snapshot.windows.iter().all(|window| window.window_id == fixture.caller_window));
        assert!(snapshot.sessions.iter().all(|session| session.window_id == fixture.caller_window));
        assert_eq!(snapshot.sessions.iter().filter(|session| session.is_caller).count(), 1,);
    }

    #[tokio::test]
    async fn siblings_request_with_an_invalid_origin_is_not_found() {
        let fixture = world_fixture();
        let stale = fixture.dispatch(&siblings_request(32, Some(SessionId::new()))).await;
        assert!(matches!(stale.response().result, Err(AgentError::NotFound { .. })));
        let absent = fixture.dispatch(&siblings_request(33, None)).await;
        assert!(matches!(absent.response().result, Err(AgentError::NotFound { .. })));
    }

    #[tokio::test]
    async fn world_deny_returns_before_any_registry_capture() {
        let response = dispatch(
            &AgentApiState::default(),
            0,
            &world_request(34, None),
            DispatchSources {
                capture_world: || async { panic!("denied world must not capture registries") },
                lookup_session: |_| async {
                    panic!("denied world must not look up terminal content")
                },
                run_action: |_, _| async { panic!("denied world must not run an action") },
            },
            None::<fn(ServerMessage) -> std::future::Ready<()>>,
        )
        .await;
        assert!(matches!(response.response().result, Err(AgentError::Denied { .. })));
    }

    async fn prompted_authorization(decision: ClipboardDecision) -> Result<(), AgentError> {
        let state = AgentApiState::new(AgentApiConfig {
            read_content: AgentPolicyMode::Prompt,
            ..AgentApiConfig::default()
        });
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let prompt_writer = test_shared_writer(server_write);
        let authorizer = state.clone();
        let request = screen_request(12, SessionId::new());
        let pending = tokio::spawn(async move {
            let metadata = RequestMetadata::from(&request);
            authorize_before_lookup(
                &authorizer,
                &metadata,
                Some(move |message| async move {
                    crate::ipc_server::send_message(&prompt_writer, &message).await;
                }),
            )
            .await
        });

        let ServerMessage::AgentPromptRequest { prompt_id, capability, .. } =
            read_message(&mut client_read).await.expect("read agent prompt")
        else {
            panic!("expected agent prompt request");
        };
        assert_eq!(capability, AgentCapability::ReadContent);
        assert!(state.resolve_prompt(prompt_id, decision));
        pending.await.expect("authorization task")
    }

    #[tokio::test]
    async fn prompt_allow_and_deny_resolve_the_parked_read() {
        assert!(prompted_authorization(ClipboardDecision::AllowOnce).await.is_ok());
        assert!(matches!(
            prompted_authorization(ClipboardDecision::DenyOnce).await,
            Err(AgentError::Denied { .. })
        ));
    }

    #[tokio::test]
    async fn prompt_without_capable_client_denies_headlessly() {
        let state = AgentApiState::new(AgentApiConfig {
            read_content: AgentPolicyMode::Prompt,
            ..AgentApiConfig::default()
        });
        let request = screen_request(13, SessionId::new());
        let metadata = RequestMetadata::from(&request);
        assert!(matches!(
            authorize_before_lookup(
                &state,
                &metadata,
                None::<fn(ServerMessage) -> std::future::Ready<()>>,
            )
            .await,
            Err(AgentError::Denied { .. })
        ));
    }

    #[tokio::test]
    async fn action_dispatch_returns_the_correlated_completion() {
        let state = AgentApiState::new(AgentApiConfig {
            dispatch_action: AgentPolicyMode::Allow,
            ..AgentApiConfig::default()
        });
        let window_id = WindowId::new();
        let created_session_id = SessionId::new();
        let request = action_request(14, AutomationAction::NewTab, Some(window_id));
        let dispatched = dispatch(
            &state,
            0,
            &request,
            DispatchSources {
                capture_world: || async { panic!("actions must not capture the world") },
                lookup_session: |_| async { None },
                run_action: move |target, action| async move {
                    assert_eq!(target, Some(window_id));
                    assert!(matches!(action, AutomationAction::NewTab));
                    Ok(AgentActionResult {
                        action,
                        outcome: AgentActionOutcome::Completed,
                        created_session_id: Some(created_session_id),
                    })
                },
            },
            None::<fn(ServerMessage) -> std::future::Ready<()>>,
        )
        .await;

        assert!(matches!(
            &dispatched.response().result,
            Ok(AgentPayload::DispatchAction { result })
                if result.created_session_id == Some(created_session_id)
                    && result.outcome == AgentActionOutcome::Completed
        ));
    }

    #[tokio::test]
    async fn denied_and_prompt_denied_actions_never_reach_dispatch() {
        let denied = action_request(15, AutomationAction::ClosePane, None);
        let denied_response = dispatch(
            &AgentApiState::new(AgentApiConfig {
                dispatch_action: AgentPolicyMode::Allow,
                ..AgentApiConfig::default()
            }),
            0,
            &denied,
            DispatchSources {
                capture_world: || async { panic!("denied action must not capture the world") },
                lookup_session: |_| async { None },
                run_action: |_, _| async {
                    panic!("destructive action used the benign capability")
                },
            },
            None::<fn(ServerMessage) -> std::future::Ready<()>>,
        )
        .await;
        assert!(matches!(denied_response.response().result, Err(AgentError::Denied { .. })));

        let state = AgentApiState::new(AgentApiConfig {
            dispatch_destructive_action: AgentPolicyMode::Prompt,
            ..AgentApiConfig::default()
        });
        let resolver = state.clone();
        let prompt_response = dispatch(
            &state,
            0,
            &action_request(16, AutomationAction::OpenUpdateDialog, None),
            DispatchSources {
                capture_world: || async { panic!("denied action must not capture the world") },
                lookup_session: |_| async { None },
                run_action: |_, _| async { panic!("prompt-denied action reached dispatch") },
            },
            Some(move |message| async move {
                deny_destructive_prompt(&resolver, &message);
            }),
        )
        .await;
        assert!(matches!(prompt_response.response().result, Err(AgentError::Denied { .. })));
    }

    fn capability_mode(
        capabilities: &[AgentCapabilityStatus],
        capability: AgentCapability,
    ) -> Option<AgentPolicyMode> {
        capabilities.iter().find(|status| status.capability == capability).map(|status| status.mode)
    }

    #[tokio::test]
    async fn capabilities_reports_every_capability_and_mode_without_a_grant() {
        // Default policy is all-`Deny`; `Capabilities` must still answer.
        let state = AgentApiState::default();
        let dispatched = dispatch_headless(&state, &capabilities_request(1)).await;

        let Ok(AgentPayload::Capabilities { version, capabilities }) =
            dispatched.response().result.clone()
        else {
            panic!("Capabilities must succeed under default (all-Deny) policy");
        };
        assert_eq!(version, AGENT_SURFACE_VERSION);
        assert_eq!(capabilities.len(), ALL_CAPABILITIES.len());
        for capability in ALL_CAPABILITIES {
            assert_eq!(capability_mode(&capabilities, capability), Some(AgentPolicyMode::Deny));
        }
    }

    #[tokio::test]
    async fn capabilities_reflects_a_live_policy_change_with_no_restart() {
        let state = AgentApiState::default();
        state.refresh_policy(AgentApiConfig {
            read_content: AgentPolicyMode::Allow,
            write_input: AgentPolicyMode::Prompt,
            ..AgentApiConfig::default()
        });

        let dispatched = dispatch_headless(&state, &capabilities_request(2)).await;
        let Ok(AgentPayload::Capabilities { capabilities, .. }) =
            dispatched.response().result.clone()
        else {
            panic!("Capabilities must succeed after a live policy refresh");
        };
        assert_eq!(
            capability_mode(&capabilities, AgentCapability::ReadContent),
            Some(AgentPolicyMode::Allow)
        );
        assert_eq!(
            capability_mode(&capabilities, AgentCapability::WriteInput),
            Some(AgentPolicyMode::Prompt)
        );
        // Untouched capabilities stay at their default, proving the refresh
        // updates the live policy rather than replacing it wholesale.
        assert_eq!(
            capability_mode(&capabilities, AgentCapability::ReadMetadata),
            Some(AgentPolicyMode::Deny)
        );
    }

    struct ExpectedAudit<'a> {
        agent_label: &'a str,
        capability: &'a str,
        target_kind: &'a str,
        target_id: String,
        decision: &'a str,
        response: &'a scribe_common::agent::AgentResponse,
    }

    fn assert_audit(event: &CapturedEvent, expected: &ExpectedAudit<'_>) {
        assert_eq!(event.name, "agent_call");
        assert_eq!(event.target, "scribe::agent_api");
        assert_eq!(event.fields.len(), 6);
        let expected_fields = [
            ("agent_label", expected.agent_label.to_owned()),
            ("capability", expected.capability.to_owned()),
            ("target_kind", expected.target_kind.to_owned()),
            ("target_id", expected.target_id.clone()),
            ("decision", expected.decision.to_owned()),
            (
                "response_bytes",
                rmp_serde::to_vec_named(&ServerMessage::AgentResponse(expected.response.clone()))
                    .expect("serialize agent reply")
                    .len()
                    .to_string(),
            ),
        ];
        for (field, value) in expected_fields {
            assert_eq!(event.fields.get(field), Some(&value));
        }
    }

    fn deny_agent_prompt(state: &AgentApiState, message: &ServerMessage) {
        let ServerMessage::AgentPromptRequest { prompt_id, .. } = message else {
            panic!("expected agent prompt");
        };
        assert!(state.resolve_prompt(*prompt_id, ClipboardDecision::DenyOnce));
    }

    #[test]
    fn request_metadata_uses_only_supported_target_kinds() {
        let session_id = SessionId::new();
        let window_id = scribe_common::ids::WindowId::new();
        let requests = [
            request(1),
            capabilities_request(2),
            AgentRequest::Siblings {
                request_id: 3,
                agent_label: "target-test".into(),
                origin_session_id: Some(session_id),
            },
            screen_request(4, session_id),
            AgentRequest::DispatchAction {
                request_id: 5,
                agent_label: "target-test".into(),
                origin_session_id: None,
                action: AutomationAction::NewTab,
                window: Some(window_id),
            },
            AgentRequest::WriteInput {
                request_id: 6,
                agent_label: "target-test".into(),
                origin_session_id: None,
                session_id,
                text: String::new(),
                submit: false,
            },
        ];
        for request in &requests {
            assert!(matches!(
                RequestMetadata::from(request).target_kind,
                "server" | "window" | "session"
            ));
        }
    }

    fn assert_dispatch_audits(
        captured: &[CapturedEvent],
        sessions: [SessionId; 3],
        responses: [&super::AgentDispatch; 4],
    ) {
        let [allowed_session, denied_session, prompted_session] = sessions;
        let [allowed, denied, prompted, busy] = responses;
        let expected = [
            ExpectedAudit {
                agent_label: "allowed-agent",
                capability: "ReadContent",
                target_kind: "session",
                target_id: allowed_session.to_string(),
                decision: "allow",
                response: allowed.response(),
            },
            ExpectedAudit {
                agent_label: "denied-agent",
                capability: "ReadContent",
                target_kind: "session",
                target_id: denied_session.to_string(),
                decision: "deny",
                response: denied.response(),
            },
            ExpectedAudit {
                agent_label: "prompted-agent",
                capability: "ReadContent",
                target_kind: "session",
                target_id: prompted_session.to_string(),
                decision: "deny",
                response: prompted.response(),
            },
            ExpectedAudit {
                agent_label: "busy-agent",
                capability: "ReadMetadata",
                target_kind: "server",
                target_id: "server".into(),
                decision: "busy",
                response: busy.response(),
            },
        ];
        for (event, expected_audit) in captured.iter().zip(&expected) {
            assert_audit(event, expected_audit);
        }
    }

    /// One prompt-mode read whose confirmation is denied. Every seam panics:
    /// the refusal must be decided before any of them is reached.
    async fn prompt_denied_read_dispatch(session_id: SessionId) -> super::AgentDispatch {
        let state = AgentApiState::new(AgentApiConfig {
            read_content: AgentPolicyMode::Prompt,
            ..AgentApiConfig::default()
        });
        let resolver = state.clone();
        dispatch(
            &state,
            0,
            &screen_request_for("prompted-agent", 22, session_id),
            DispatchSources {
                capture_world: || async { panic!("denied prompt must not capture the world") },
                lookup_session: |_| async {
                    panic!("denied prompt must not look up terminal content")
                },
                run_action: |_, _| async { panic!("denied prompt must not run an action") },
            },
            Some(move |message| {
                deny_agent_prompt(&resolver, &message);
                std::future::ready(())
            }),
        )
        .await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatcher_emits_one_complete_metadata_only_audit_for_every_outcome() {
        const TERMINAL_SECRET: &str = "terminal-secret-must-not-enter-audit";

        let events = install_audit_capture();

        let allowed_state = AgentApiState::new(AgentApiConfig {
            read_content: AgentPolicyMode::Allow,
            ..AgentApiConfig::default()
        });
        let allowed_session = SessionId::new();
        let allowed_request = screen_request_for("allowed-agent", 20, allowed_session);
        let (allowed_target, _peer) = session_target(
            term_with_bytes(TERMINAL_SECRET.as_bytes(), 64, 1),
            Some("sensitive pane".into()),
            Some(PathBuf::from("/sensitive/worktree")),
        );
        let allowed = dispatch(
            &allowed_state,
            0,
            &allowed_request,
            DispatchSources {
                capture_world: || async { panic!("screen reads must not capture the world") },
                lookup_session: move |_| async move { Some(allowed_target) },
                run_action: |_, _| async {
                    Err(AgentError::Internal { message: "unexpected action".into() })
                },
            },
            None::<fn(ServerMessage) -> std::future::Ready<()>>,
        )
        .await;

        let denied_session = SessionId::new();
        let denied_request = screen_request_for("denied-agent", 21, denied_session);
        let denied = dispatch_headless(&AgentApiState::default(), &denied_request).await;

        let prompted_session = SessionId::new();
        let prompted = prompt_denied_read_dispatch(prompted_session).await;

        let busy_state = AgentApiState::default();
        let permits = (0..MAX_IN_FLIGHT_REQUESTS)
            .map(|_| busy_state.try_acquire().expect("capacity available"))
            .collect::<Vec<_>>();
        let busy_request = AgentRequest::World {
            request_id: 23,
            agent_label: "busy-agent".into(),
            origin_session_id: None,
        };
        let busy = dispatch_headless(&busy_state, &busy_request).await;
        drop(permits);

        let captured = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| {
                matches!(
                    event.fields.get("agent_label").map(String::as_str),
                    Some("allowed-agent" | "denied-agent" | "prompted-agent" | "busy-agent")
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(captured.len(), 4, "one audit event per dispatch outcome");
        assert_dispatch_audits(
            &captured,
            [allowed_session, denied_session, prompted_session],
            [&allowed, &denied, &prompted, &busy],
        );
        assert!(!format!("{captured:?}").contains(TERMINAL_SECRET));
    }

    async fn hold_dispatch(
        state: AgentApiState,
        request_id: u64,
        ready: Arc<Barrier>,
        release: Arc<Semaphore>,
    ) -> super::AgentDispatch {
        let dispatch = dispatch_headless(&state, &request(request_id)).await;
        ready.wait().await;
        let release_permit = release.acquire().await.expect("release remains open");
        drop(release_permit);
        dispatch
    }

    #[tokio::test]
    async fn correlated_action_waits_for_client_completion() {
        let state = AgentApiState::default();
        let pending = state.begin_correlated_action(7, AutomationAction::NewTab);
        let ServerMessage::RunActionCorrelated { correlation_id, action } = pending.message()
        else {
            panic!("expected correlated action");
        };
        assert!(matches!(action, AutomationAction::NewTab));
        let correlation_id = *correlation_id;
        let created_session_id = scribe_common::ids::SessionId::new();
        assert!(state.complete_action(
            7,
            correlation_id,
            AgentActionOutcome::Completed,
            Some(created_session_id),
        ));

        let result = pending.wait(Duration::from_secs(1)).await.expect("completed action");
        assert_eq!(result.created_session_id, Some(created_session_id));
        assert_eq!(result.outcome, AgentActionOutcome::Completed);
    }

    #[tokio::test]
    async fn correlated_action_timeout_and_disconnect_are_action_failed() {
        let state = AgentApiState::default();
        let send_failed = state
            .run_correlated_action(
                6,
                AutomationAction::OpenFind,
                Duration::from_secs(1),
                |_| async { false },
            )
            .await;
        assert!(matches!(send_failed, Err(AgentError::ActionFailed { .. })));

        let timed_out = state
            .begin_correlated_action(7, AutomationAction::OpenFind)
            .wait(Duration::from_millis(1))
            .await;
        assert!(matches!(timed_out, Err(AgentError::ActionFailed { .. })));

        let pending = state.begin_correlated_action(8, AutomationAction::OpenSettings);
        state.fail_actions_for_client(8);
        assert!(matches!(
            pending.wait(Duration::from_secs(1)).await,
            Err(AgentError::ActionFailed { .. })
        ));
    }

    #[tokio::test]
    async fn fifth_concurrent_request_returns_busy() {
        let state = AgentApiState::default();
        let ready = Arc::new(Barrier::new(MAX_IN_FLIGHT_REQUESTS + 1));
        let release = Arc::new(Semaphore::new(0));
        let requests: Vec<_> = (0..MAX_IN_FLIGHT_REQUESTS)
            .map(|id| {
                let request_id = u64::try_from(id).expect("request id fits u64");
                tokio::spawn(hold_dispatch(
                    state.clone(),
                    request_id,
                    Arc::clone(&ready),
                    Arc::clone(&release),
                ))
            })
            .collect();
        ready.wait().await;

        let response = dispatch_headless(&state, &request(8)).await;
        assert!(matches!(
            response.response(),
            scribe_common::agent::AgentResponse {
                request_id: 8,
                result: Err(AgentError::Busy { .. }),
            }
        ));

        release.add_permits(MAX_IN_FLIGHT_REQUESTS);
        for request in requests {
            drop(request.await.expect("in-flight task"));
        }
    }
}
