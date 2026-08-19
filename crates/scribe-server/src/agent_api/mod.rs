//! One-shot request routing for Scribe's local agent API.
//!
//! Capability handlers live in sibling modules. This foundational router keeps
//! admission, policy ordering, and auditing in one place.

pub mod policy;

use std::sync::Arc;

use scribe_common::agent::{
    AgentCapability, AgentError, AgentPayload, AgentPolicyMode, AgentRequest, AgentResponse,
};
use scribe_common::config::AgentApiConfig;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Maximum simultaneous agent API requests. The dispatch result retains its
/// permit until the IPC reply is queued; future handlers retain it for their
/// complete request lifetime.
const MAX_IN_FLIGHT_REQUESTS: usize = 4;

/// Server-owned admission state for one-shot agent requests.
#[derive(Clone)]
pub struct AgentApiState {
    policy: AgentApiConfig,
    in_flight: Arc<Semaphore>,
}

impl AgentApiState {
    #[must_use]
    pub fn new(policy: AgentApiConfig) -> Self {
        Self { policy, in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS)) }
    }

    fn try_acquire(&self) -> Result<OwnedSemaphorePermit, AgentError> {
        Arc::clone(&self.in_flight)
            .try_acquire_owned()
            .map_err(|_| AgentError::Busy { message: "agent request capacity reached".into() })
    }
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

    use scribe_common::agent::{AgentError, AgentRequest};
    use scribe_common::framing::{read_message, write_message};
    use scribe_common::protocol::{ClientMessage, ServerMessage};
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
