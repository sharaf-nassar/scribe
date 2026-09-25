use super::*;
use scribe_common::agent::{AgentError, AgentPayload, AgentPolicyMode, AgentRequest};
use serde::Deserialize;

// Freeze the pre-background-wait enum so the decoder cannot silently acquire
// support for new states along with the production DTOs. Unchanged fields may
// be ignored, but the nested AI state must pass the old schema.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyAiState {
    IdlePrompt,
    Processing,
    WaitingForInput,
    PermissionPrompt,
    Error,
}

#[derive(Debug, Deserialize)]
struct LegacySession {
    ai_state: Option<LegacyAiState>,
    context_fill_percent: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct LegacySnapshot {
    sessions: Vec<LegacySession>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LegacyPayload {
    World { snapshot: LegacySnapshot },
    Siblings { snapshot: LegacySnapshot },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum LegacyMessage {
    AgentResponse { request_id: u64, result: Result<LegacyPayload, AgentError> },
}

// @lat: [[test#Agent Control Surface#Background wait snapshot compatibility]]
#[tokio::test]
async fn agent_snapshot_background_wait_is_negotiated_without_hello() {
    let (pane_writer, _pane_client) = ci_test_writer();
    let (session_id, live_sessions, _slaves) = live_session_with_sink(80, 24, &pane_writer).await;
    let mut waiting =
        AiProcessState::new_with_provider(AiProvider::Pi, AiState::WaitingForBackground);
    waiting.context = Some(42);
    live_sessions.write().await.get_mut(&session_id).unwrap().ai_state = Some(waiting);
    let mut manager = WorkspaceManager::new(Vec::new());
    let workspace_id = manager.create_workspace();
    manager.add_session(workspace_id, session_id, None);
    manager.assign_session_to_window(WindowId::new(), session_id);
    let mut server =
        transfer_server_state(Arc::new(RwLock::new(manager)), live_sessions, new_window_shares());
    server.agent_api =
        crate::agent_api::AgentApiState::new(scribe_common::config::AgentApiConfig {
            read_metadata: AgentPolicyMode::Allow,
            ..Default::default()
        });

    for (kind, capability) in [
        ("world", None),
        ("world", Some(false)),
        ("world", Some(true)),
        ("siblings", None),
        ("siblings", Some(false)),
        ("siblings", Some(true)),
    ] {
        let mut frame = serde_json::json!({
            "request_type": kind,
            "request_id": 7,
            "agent_label": "compat-test",
            "origin_session_id": session_id,
        });
        if let Some(supported) = capability {
            frame["ai_background_wait"] = supported.into();
        }
        let request: AgentRequest = serde_json::from_value(frame).unwrap();
        let (writer, mut client) = ci_test_writer();
        handle_transient_agent_request(&server, &writer, &request).await;
        if capability != Some(true) {
            let LegacyMessage::AgentResponse { request_id, result } =
                read_message(&mut client).await.expect("old client must decode the snapshot");
            assert_eq!(request_id, 7);
            let (LegacyPayload::World { snapshot } | LegacyPayload::Siblings { snapshot }) =
                result.unwrap();
            assert_eq!(snapshot.sessions.len(), 1);
            assert_eq!(snapshot.sessions[0].ai_state, Some(LegacyAiState::Processing));
            assert_eq!(snapshot.sessions[0].context_fill_percent, Some(42));
            continue;
        }
        let message: ServerMessage = read_message(&mut client).await.unwrap();
        let ServerMessage::AgentResponse(response) = message else {
            panic!("expected agent response");
        };
        assert_eq!(response.request_id, 7);
        let (AgentPayload::World { snapshot } | AgentPayload::Siblings { snapshot }) =
            response.result.unwrap()
        else {
            panic!("expected world or siblings");
        };
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].ai_state, Some(AiState::WaitingForBackground));
        assert_eq!(snapshot.sessions[0].context_fill_percent, Some(42));
    }
    assert_eq!(
        server.live_sessions.read().await[&session_id].ai_state.as_ref().unwrap().state,
        AiState::WaitingForBackground,
        "compatibility must not mutate retained state",
    );
}
