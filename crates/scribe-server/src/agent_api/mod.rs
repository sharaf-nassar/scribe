//! One-shot request routing for Scribe's local agent API.
//!
//! Capability handlers live in sibling modules. This foundational router keeps
//! admission, policy ordering, and auditing in one place.

pub mod policy;

use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use scribe_common::agent::{
    AgentActionOutcome, AgentActionResult, AgentCapability, AgentError, AgentPayload,
    AgentPolicyMode, AgentRequest, AgentResponse,
};
use scribe_common::config::AgentApiConfig;
use scribe_common::protocol::{AutomationAction, ServerMessage};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

/// Maximum simultaneous agent API requests. The dispatch result retains its
/// permit until the IPC reply is queued; future handlers retain it for their
/// complete request lifetime.
const MAX_IN_FLIGHT_REQUESTS: usize = 4;

/// Server-owned admission state for one-shot agent requests.
#[derive(Clone)]
pub struct AgentApiState {
    policy: AgentApiConfig,
    in_flight: Arc<Semaphore>,
    next_correlation_id: Arc<AtomicU64>,
    pending_actions: Arc<Mutex<HashMap<u64, PendingAction>>>,
}

struct PendingAction {
    writer_key: usize,
    action: AutomationAction,
    completion: oneshot::Sender<Result<AgentActionResult, AgentError>>,
}

impl AgentApiState {
    #[must_use]
    pub fn new(policy: AgentApiConfig) -> Self {
        Self {
            policy,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS)),
            next_correlation_id: Arc::new(AtomicU64::new(1)),
            pending_actions: Arc::new(Mutex::new(HashMap::new())),
        }
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
/// Policy evaluation deliberately precedes any target lookup. The policy and
/// capability handlers land in follow-up modules; the foundational stub is
/// default-safe and returns `Denied` without reading server state.
pub fn dispatch(state: &AgentApiState, request: &AgentRequest) -> AgentDispatch {
    let metadata = RequestMetadata::from(request);
    let (result, permit) = match state.try_acquire() {
        Err(error) => (Err(error), None),
        Ok(permit) => (
            match authorize_before_lookup(&state.policy, metadata.capability) {
                Err(error) => Err(error),
                Ok(()) => route_authorized_request(request),
            },
            Some(permit),
        ),
    };

    let decision = match &result {
        Ok(_) => "allow",
        Err(AgentError::Busy { .. }) => "busy",
        Err(_) => "deny",
    };
    emit_audit(&metadata, decision, 0);
    AgentDispatch {
        response: AgentResponse { request_id: metadata.request_id, result },
        _permit: permit,
    }
}

/// Policy seam. It must stay ahead of all future world/session/window lookup.
fn authorize_before_lookup(
    policy: &AgentApiConfig,
    capability: AgentCapability,
) -> Result<(), AgentError> {
    match policy_mode(policy, capability) {
        AgentPolicyMode::Deny => Err(denied()),
        AgentPolicyMode::Allow | AgentPolicyMode::Prompt => Ok(()),
    }
}

fn policy_mode(policy: &AgentApiConfig, capability: AgentCapability) -> AgentPolicyMode {
    match capability {
        AgentCapability::ReadMetadata => policy.read_metadata,
        AgentCapability::ReadContent => policy.read_content,
        AgentCapability::DispatchAction => policy.dispatch_action,
        AgentCapability::DispatchDestructiveAction => policy.dispatch_destructive_action,
        AgentCapability::WriteInput => policy.write_input,
    }
}

/// Handler-routing seam. Real capability handlers replace this default-safe
/// stub in follow-up work.
fn route_authorized_request(_request: &AgentRequest) -> Result<AgentPayload, AgentError> {
    Err(denied())
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
    use std::sync::Arc;

    use std::time::Duration;

    use scribe_common::agent::{AgentActionOutcome, AgentError, AgentRequest};
    use scribe_common::framing::{read_message, write_message};
    use scribe_common::protocol::{AutomationAction, ClientMessage, ServerMessage};
    use tokio::sync::{Barrier, Semaphore};

    use super::{AgentApiState, MAX_IN_FLIGHT_REQUESTS, dispatch};

    fn request(request_id: u64) -> AgentRequest {
        AgentRequest::Capabilities {
            request_id,
            agent_label: "socket-test".into(),
            origin_session_id: None,
        }
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

        let dispatch = dispatch(&AgentApiState::default(), &request);
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

    async fn hold_dispatch(
        state: AgentApiState,
        request_id: u64,
        ready: Arc<Barrier>,
        release: Arc<Semaphore>,
    ) -> super::AgentDispatch {
        let dispatch = dispatch(&state, &request(request_id));
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

        let response = dispatch(&state, &request(8));
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
