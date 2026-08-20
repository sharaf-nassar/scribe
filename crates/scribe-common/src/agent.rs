//! Shared DTOs for Scribe's local agent control surface.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ai_state::{AiProvider, AiState};
use crate::config::SharingMode;
use crate::ids::{SessionId, WindowId, WorkspaceId};
use crate::protocol::AutomationAction;

// @lat: [[common#Common#Agent Control Contract]]
/// One request made through the local agent API.
#[derive(Debug, Clone, Serialize, Deserialize)]
// `ClientMessage` already uses `type` as its outer tag. A distinct nested key
// keeps `ClientMessage::AgentRequest(AgentRequest)` decodable on MessagePack.
#[serde(tag = "request_type", rename_all = "snake_case")]
pub enum AgentRequest {
    World {
        request_id: u64,
        agent_label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_session_id: Option<SessionId>,
    },
    Siblings {
        request_id: u64,
        agent_label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_session_id: Option<SessionId>,
    },
    ReadScreen {
        request_id: u64,
        agent_label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_session_id: Option<SessionId>,
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scrollback_lines: Option<u32>,
    },
    DispatchAction {
        request_id: u64,
        agent_label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_session_id: Option<SessionId>,
        action: AutomationAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<WindowId>,
    },
    WriteInput {
        request_id: u64,
        agent_label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_session_id: Option<SessionId>,
        session_id: SessionId,
        text: String,
        submit: bool,
    },
    Capabilities {
        request_id: u64,
        agent_label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin_session_id: Option<SessionId>,
    },
}

/// Reply to one [`AgentRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub request_id: u64,
    pub result: Result<AgentPayload, AgentError>,
}

/// Version of the local agent control surface this build implements.
/// Reported by [`AgentPayload::Capabilities`] so a caller can detect an
/// unsupported build instead of guessing from a refusal.
pub const AGENT_SURFACE_VERSION: u32 = 1;

/// Successful payloads returned by the local agent API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentPayload {
    World { snapshot: AgentWorldSnapshot },
    Siblings { snapshot: AgentWorldSnapshot },
    ReadScreen { screen: AgentScreenText },
    DispatchAction { result: AgentActionResult },
    WriteInput,
    Capabilities { version: u32, capabilities: Vec<AgentCapabilityStatus> },
}

/// Capability checked before an agent operation is serviced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCapability {
    ReadMetadata,
    ReadContent,
    DispatchAction,
    DispatchDestructiveAction,
    WriteInput,
}

impl AgentCapability {
    /// Map an automation action to its policy gate.
    #[must_use]
    pub fn for_action(action: &AutomationAction) -> Self {
        match action {
            AutomationAction::ClosePane
            | AutomationAction::CloseTab
            | AutomationAction::OpenUpdateDialog => Self::DispatchDestructiveAction,
            AutomationAction::OpenSettings
            | AutomationAction::OpenFind
            | AutomationAction::NewTab
            | AutomationAction::NewClaudeTab
            | AutomationAction::NewClaudeResumeTab
            | AutomationAction::NewCodexTab
            | AutomationAction::NewCodexResumeTab
            | AutomationAction::SplitVertical
            | AutomationAction::SplitHorizontal
            | AutomationAction::NewWindow
            | AutomationAction::SwitchProfile { .. }
            | AutomationAction::FocusSession { .. } => Self::DispatchAction,
        }
    }
}

/// Policy decision for one agent capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPolicyMode {
    #[default]
    Deny,
    Allow,
    Prompt,
}

/// One capability's identity paired with its currently effective policy
/// mode, as reported by [`AgentPayload::Capabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilityStatus {
    pub capability: AgentCapability,
    pub mode: AgentPolicyMode,
}

/// Typed failure returned by the local agent API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum AgentError {
    Denied { message: String },
    PromptTimeout { message: String },
    NotFound { message: String },
    AmbiguousTarget { message: String },
    Unsupported { message: String },
    TooLarge { message: String },
    Busy { message: String },
    VersionMismatch { message: String },
    ActionFailed { message: String },
    Internal { message: String },
}

/// Consistent server-wide view returned by `World` and `Siblings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWorldSnapshot {
    pub windows: Vec<AgentWindow>,
    pub workspaces: Vec<AgentWorkspace>,
    pub sessions: Vec<AgentSession>,
    pub snapshot_id: u64,
    pub captured_at: u64,
}

/// Window metadata exposed to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWindow {
    pub window_id: WindowId,
    pub workspace_names: Vec<String>,
    pub session_count: usize,
    pub connected: bool,
    pub sharing_mode: SharingMode,
    pub participant_count: usize,
}

/// Workspace metadata exposed to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWorkspace {
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub window_id: WindowId,
    pub session_ids: Vec<SessionId>,
}

/// Session metadata exposed to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub session_id: SessionId,
    pub window_id: WindowId,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AiProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_state: Option<AiState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_fill_percent: Option<u8>,
    pub is_caller: bool,
}

/// Text captured from one terminal session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentScreenText {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub text: String,
    pub lines: u32,
    pub truncated: bool,
    pub captured_at: u64,
    pub snapshot_id: u64,
}

/// Completion state for a dispatched automation action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActionOutcome {
    Completed,
    Failed,
}

/// Result of a dispatched automation action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentActionResult {
    pub action: AutomationAction,
    pub outcome: AgentActionOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_session_id: Option<SessionId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_id() -> SessionId {
        SessionId::new()
    }

    fn window_id() -> WindowId {
        WindowId::new()
    }

    fn workspace_id() -> WorkspaceId {
        WorkspaceId::new()
    }

    fn action() -> AutomationAction {
        AutomationAction::OpenSettings
    }

    fn snapshot() -> AgentWorldSnapshot {
        AgentWorldSnapshot {
            windows: vec![AgentWindow {
                window_id: window_id(),
                workspace_names: vec![String::from("main")],
                session_count: 1,
                connected: true,
                sharing_mode: SharingMode::SingleController,
                participant_count: 0,
            }],
            workspaces: vec![AgentWorkspace {
                workspace_id: workspace_id(),
                name: Some(String::from("main")),
                window_id: window_id(),
                session_ids: vec![session_id()],
            }],
            sessions: vec![AgentSession {
                session_id: session_id(),
                window_id: window_id(),
                workspace_id: workspace_id(),
                title: Some(String::from("shell")),
                cwd: Some(PathBuf::from("/tmp")),
                provider: Some(AiProvider::Pi),
                ai_state: Some(AiState::Processing),
                task_label: Some(String::from("build")),
                context_fill_percent: Some(42),
                is_caller: true,
            }],
            snapshot_id: 7,
            captured_at: 8,
        }
    }

    fn screen() -> AgentScreenText {
        AgentScreenText {
            session_id: session_id(),
            title: Some(String::from("shell")),
            cwd: Some(PathBuf::from("/tmp")),
            text: String::from("output"),
            lines: 1,
            truncated: false,
            captured_at: 8,
            snapshot_id: 7,
        }
    }

    fn result() -> AgentActionResult {
        AgentActionResult {
            action: action(),
            outcome: AgentActionOutcome::Completed,
            created_session_id: Some(session_id()),
        }
    }

    fn capabilities() -> AgentPayload {
        AgentPayload::Capabilities {
            version: AGENT_SURFACE_VERSION,
            capabilities: vec![AgentCapabilityStatus {
                capability: AgentCapability::ReadMetadata,
                mode: AgentPolicyMode::Deny,
            }],
        }
    }

    fn assert_redacted<T: Serialize>(value: &T) {
        let encoded = rmp_serde::to_vec_named(value);
        assert!(encoded.is_ok(), "agent DTO should serialize: {encoded:?}");
        let bytes = encoded.unwrap_or_default();
        for key in [
            "launch_id",
            "prompt_state",
            "prompt",
            "first_prompt",
            "latest_prompt",
            "conversation_id",
            "model",
            "tool",
            "agent",
        ] {
            let encoded_key = rmp_serde::to_vec(key).unwrap_or_default();
            assert!(
                !bytes.windows(encoded_key.len()).any(|window| window == encoded_key),
                "serialized DTO leaked forbidden key {key:?}"
            );
        }
    }

    #[test]
    fn every_agent_dto_excludes_session_and_ai_metadata() {
        let sid = session_id();
        let wid = window_id();
        let requests = [
            AgentRequest::World {
                request_id: 1,
                agent_label: String::from("runner"),
                origin_session_id: Some(sid),
            },
            AgentRequest::Siblings {
                request_id: 1,
                agent_label: String::from("runner"),
                origin_session_id: Some(sid),
            },
            AgentRequest::ReadScreen {
                request_id: 1,
                agent_label: String::from("runner"),
                origin_session_id: Some(sid),
                session_id: sid,
                scrollback_lines: Some(10),
            },
            AgentRequest::DispatchAction {
                request_id: 1,
                agent_label: String::from("runner"),
                origin_session_id: Some(sid),
                action: action(),
                window: Some(wid),
            },
            AgentRequest::WriteInput {
                request_id: 1,
                agent_label: String::from("runner"),
                origin_session_id: Some(sid),
                session_id: sid,
                text: String::from("input"),
                submit: true,
            },
            AgentRequest::Capabilities {
                request_id: 1,
                agent_label: String::from("runner"),
                origin_session_id: Some(sid),
            },
        ];
        for request in &requests {
            assert_redacted(request);
        }

        let payloads = [
            AgentPayload::World { snapshot: snapshot() },
            AgentPayload::Siblings { snapshot: snapshot() },
            AgentPayload::ReadScreen { screen: screen() },
            AgentPayload::DispatchAction { result: result() },
            AgentPayload::WriteInput,
            capabilities(),
        ];
        for payload in &payloads {
            assert_redacted(payload);
            assert_redacted(&AgentResponse { request_id: 1, result: Ok(payload.clone()) });
        }

        assert_redacted(&AgentCapability::ReadMetadata);
        assert_redacted(&AgentPolicyMode::Deny);
        assert_redacted(&AgentError::Denied { message: String::from("denied") });
        let world = snapshot();
        assert_redacted(&world);
        assert_eq!(world.windows.len(), 1);
        assert_eq!(world.workspaces.len(), 1);
        assert_eq!(world.sessions.len(), 1);
        for window in &world.windows {
            assert_redacted(window);
        }
        for workspace in &world.workspaces {
            assert_redacted(workspace);
        }
        for session in &world.sessions {
            assert_redacted(session);
        }
        assert_redacted(&screen());
        assert_redacted(&result());
        assert_redacted(&AgentActionOutcome::Completed);
    }

    #[test]
    fn automation_actions_use_their_intended_capability_gate() {
        let destructive = [
            AutomationAction::ClosePane,
            AutomationAction::CloseTab,
            AutomationAction::OpenUpdateDialog,
        ];
        for action in &destructive {
            assert_eq!(
                AgentCapability::for_action(action),
                AgentCapability::DispatchDestructiveAction
            );
        }

        let ordinary = [
            AutomationAction::OpenSettings,
            AutomationAction::OpenFind,
            AutomationAction::NewTab,
            AutomationAction::NewClaudeTab,
            AutomationAction::NewClaudeResumeTab,
            AutomationAction::NewCodexTab,
            AutomationAction::NewCodexResumeTab,
            AutomationAction::SplitVertical,
            AutomationAction::SplitHorizontal,
            AutomationAction::NewWindow,
            AutomationAction::SwitchProfile { name: String::from("work") },
            AutomationAction::FocusSession { session_id: session_id() },
        ];
        for action in &ordinary {
            assert_eq!(AgentCapability::for_action(action), AgentCapability::DispatchAction);
        }
    }

    #[test]
    fn policy_defaults_to_deny() {
        assert_eq!(AgentPolicyMode::default(), AgentPolicyMode::Deny);
    }
}
