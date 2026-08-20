//! One-shot request routing for Scribe's local agent API.
//!
//! Capability handlers live in sibling modules. This foundational router keeps
//! admission, policy ordering, and auditing in one place.

pub mod activity;
pub mod policy;
pub mod text;

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
use scribe_common::config::AgentApiConfig;
use scribe_common::ids::{SessionId, WindowId};
use scribe_common::protocol::{AutomationAction, ServerMessage};
use scribe_pty::event_listener::ScribeEventListener;
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

/// Dispatch one transient agent request and reply on the same connection.
///
/// Policy evaluation deliberately precedes target lookup. Prompt-mode calls
/// park here until a capable local client answers, so no handler can
/// accidentally treat `Prompt` as `Allow`.
pub async fn dispatch<Lookup, LookupFuture, RunAction, RunActionFuture, SendPrompt, PromptFuture>(
    state: &AgentApiState,
    caller: usize,
    request: &AgentRequest,
    handlers: (Lookup, RunAction),
    prompt_sender: Option<SendPrompt>,
) -> AgentDispatch
where
    Lookup: FnOnce(SessionId) -> LookupFuture,
    LookupFuture: Future<Output = Option<ScreenReadTarget>>,
    RunAction: FnOnce(Option<WindowId>, AutomationAction) -> RunActionFuture,
    RunActionFuture: Future<Output = Result<AgentActionResult, AgentError>>,
    SendPrompt: FnOnce(ServerMessage) -> PromptFuture,
    PromptFuture: Future<Output = ()>,
{
    let metadata = RequestMetadata::from(request);
    let (lookup_screen, run_action) = handlers;
    let (result, permit, activity) = match state.try_acquire() {
        Err(error) => (Err(error), None, None),
        Ok(permit) => {
            let (result, activity) = if matches!(request, AgentRequest::Capabilities { .. }) {
                (Ok(capabilities_payload(&state.policy.config())), None)
            } else if let Err(error) =
                authorize_before_lookup(state, &metadata, prompt_sender).await
            {
                (Err(error), None)
            } else {
                route_authorized_request(state, caller, request, lookup_screen, run_action).await
            };
            (result, Some(permit), activity)
        }
    };

    let decision = match &result {
        Ok(_) => "allow",
        Err(AgentError::Busy { .. }) => "busy",
        Err(_) => "deny",
    };
    let response_bytes = response_content_bytes(&result);
    emit_audit(&metadata, decision, response_bytes);
    AgentDispatch {
        response: AgentResponse { request_id: metadata.request_id, result },
        _permit: permit,
        _activity: activity,
    }
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
pub struct ScreenReadTarget {
    pub term: Arc<tokio::sync::Mutex<Term<ScribeEventListener>>>,
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
}

/// Handler-routing seam. Unimplemented capability variants stay default-safe.
async fn route_authorized_request<Lookup, LookupFuture, RunAction, RunActionFuture>(
    state: &AgentApiState,
    caller: usize,
    request: &AgentRequest,
    lookup_screen: Lookup,
    run_action: RunAction,
) -> (Result<AgentPayload, AgentError>, Option<AgentActivityLease>)
where
    Lookup: FnOnce(SessionId) -> LookupFuture,
    LookupFuture: Future<Output = Option<ScreenReadTarget>>,
    RunAction: FnOnce(Option<WindowId>, AutomationAction) -> RunActionFuture,
    RunActionFuture: Future<Output = Result<AgentActionResult, AgentError>>,
{
    match request {
        AgentRequest::ReadScreen { session_id, scrollback_lines, .. } => {
            match read_screen(
                state,
                caller,
                *session_id,
                scrollback_lines.unwrap_or(0),
                lookup_screen,
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
        _ => (Err(denied()), None),
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
    LookupFuture: Future<Output = Option<ScreenReadTarget>>,
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
    target: ScreenReadTarget,
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

fn response_content_bytes(result: &Result<AgentPayload, AgentError>) -> usize {
    match result {
        Ok(AgentPayload::ReadScreen { screen }) => screen.text.len(),
        _ => 0,
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
    tracing::info!(
        target: "scribe::agent_api",
        agent_label = metadata.agent_label,
        capability = ?metadata.capability,
        target_kind = metadata.target_kind,
        target_id = metadata.target_id,
        decision,
        response_bytes,
        "agent_call"
    );
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use alacritty_terminal::Term;
    use alacritty_terminal::grid::Dimensions;
    use scribe_common::agent::{
        AGENT_SURFACE_VERSION, AgentActionOutcome, AgentActionResult, AgentCapability,
        AgentCapabilityStatus, AgentError, AgentPayload, AgentPolicyMode, AgentRequest,
    };
    use scribe_common::config::AgentApiConfig;
    use scribe_common::framing::{read_message, write_message};
    use scribe_common::ids::{SessionId, WindowId};
    use scribe_common::protocol::{
        AutomationAction, ClientMessage, ClipboardDecision, ServerMessage,
    };
    use scribe_pty::event_listener::ScribeEventListener;
    use tokio::sync::{Barrier, Mutex, Semaphore, mpsc};
    use vte::ansi::Processor as AnsiProcessor;

    use super::{
        ALL_CAPABILITIES, AgentApiState, MAX_IN_FLIGHT_REQUESTS, RequestMetadata, ScreenReadTarget,
        authorize_before_lookup, dispatch,
    };
    use crate::ipc_server::test_shared_writer;
    use crate::session_manager::build_term_config;

    // `World` stands in for every capability handler still stubbed behind
    // this dispatcher; `Capabilities` is the one variant with its own
    // wiring below, so it is deliberately excluded from this generic helper.
    fn request(request_id: u64) -> AgentRequest {
        AgentRequest::World {
            request_id,
            agent_label: "socket-test".into(),
            origin_session_id: None,
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
        AgentRequest::ReadScreen {
            request_id,
            agent_label: "screen-test".into(),
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

    async fn dispatch_headless(
        state: &AgentApiState,
        request: &AgentRequest,
    ) -> super::AgentDispatch {
        dispatch(
            state,
            0,
            request,
            (
                |_| async { None },
                |_, _| async { Err(AgentError::Internal { message: "unexpected action".into() }) },
            ),
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
        let target = ScreenReadTarget {
            term: term_with_bytes(b"old\r\nview1\r\nview2", 8, 2),
            title: Some("build".into()),
            cwd: Some(PathBuf::from("/work/scribe")),
        };
        let response = dispatch(
            &state,
            0,
            &screen_request(8, session_id),
            (
                move |_| async move { Some(target) },
                |_, _| async { Err(AgentError::Internal { message: "unexpected action".into() }) },
            ),
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
        let target = ScreenReadTarget {
            term: term_with_bytes(b"hello\r\nworld", 8, 2),
            title: None,
            cwd: None,
        };
        let response = dispatch(
            &state,
            0,
            &screen_request(9, session_id),
            (
                move |_| async move { Some(target) },
                |_, _| async { Err(AgentError::Internal { message: "unexpected action".into() }) },
            ),
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

    #[tokio::test]
    async fn read_screen_deny_returns_no_content_before_session_lookup() {
        let request = screen_request(10, SessionId::new());
        let response = dispatch(
            &AgentApiState::default(),
            0,
            &request,
            (
                |_| async { panic!("denied read must not look up terminal content") },
                |_, _| async { panic!("denied read must not run an action") },
            ),
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
            (
                |_| async { None },
                move |target, action| async move {
                    assert_eq!(target, Some(window_id));
                    assert!(matches!(action, AutomationAction::NewTab));
                    Ok(AgentActionResult {
                        action,
                        outcome: AgentActionOutcome::Completed,
                        created_session_id: Some(created_session_id),
                    })
                },
            ),
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
            (
                |_| async { None },
                |_, _| async { panic!("destructive action used the benign capability") },
            ),
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
            (|_| async { None }, |_, _| async { panic!("prompt-denied action reached dispatch") }),
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
