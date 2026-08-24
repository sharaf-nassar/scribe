//! Production-socket oracle for workspace transfer persistence and compatibility.

use std::path::Path;
use std::time::Duration;

use scribe_common::agent::{AgentPayload, AgentRequest};
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{
    ClientMessage, LayoutDirection, ServerMessage, WorkspaceTransferRefusal,
    WorkspaceTransferResult, WorkspaceTreeNode,
};
use scribe_common::terminal_images::TerminalImageCapabilities;
use serde::{Deserialize, Serialize};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize)]
struct Evidence {
    schema_version: u32,
    status: &'static str,
    checks: Vec<&'static str>,
    source_window_id: WindowId,
    target_window_id: WindowId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
}

struct Fixture {
    source_window: WindowId,
    first_workspace: WorkspaceId,
    first_session: SessionId,
    moved_workspace: WorkspaceId,
    moved_session: SessionId,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum LegacyClientMessage {
    Hello { window_id: Option<WindowId>, clipboard_gating: bool, takeover: bool },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum LegacyServerMessage {
    Welcome {
        window_id: WindowId,
        other_windows: Vec<WindowId>,
        clipboard_gating: bool,
        participant_id: Option<u64>,
    },
}

struct Peer {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    window_id: WindowId,
}

impl Peer {
    async fn connect(
        requested_window: Option<WindowId>,
        workspace_transfer: bool,
    ) -> Result<Self, String> {
        let stream = crate::ipc::connect().await.map_err(|error| error.to_string())?;
        let (mut reader, mut writer) = stream.into_split();
        send(
            &mut writer,
            &ClientMessage::Hello {
                window_id: requested_window,
                clipboard_gating: true,
                takeover: false,
                join_window: false,
                terminal_images: TerminalImageCapabilities::default(),
                ci_run_bar: false,
                pi_provider: false,
                agent_api: false,
                workspace_transfer,
            },
        )
        .await?;
        let welcome =
            recv_matching(&mut reader, |message| matches!(message, ServerMessage::Welcome { .. }))
                .await?;
        let ServerMessage::Welcome { window_id, workspace_transfer: advertised, .. } = welcome
        else {
            return Err("server returned a non-Welcome frame during handshake".to_owned());
        };
        if !advertised {
            return Err("new server did not advertise workspace transfer".to_owned());
        }
        Ok(Self { reader, writer, window_id })
    }

    async fn send(&mut self, message: &ClientMessage) -> Result<(), String> {
        send(&mut self.writer, message).await
    }

    async fn recv_matching(
        &mut self,
        predicate: impl Fn(&ServerMessage) -> bool,
    ) -> Result<ServerMessage, String> {
        recv_matching(&mut self.reader, predicate).await
    }

    async fn session_list(
        &mut self,
    ) -> Result<(Vec<scribe_common::protocol::SessionInfo>, Option<WorkspaceTreeNode>), String>
    {
        self.send(&ClientMessage::ListSessions).await?;
        let message = self
            .recv_matching(|message| matches!(message, ServerMessage::SessionList { .. }))
            .await?;
        let ServerMessage::SessionList { sessions, workspace_tree, .. } = message else {
            return Err("server returned a non-SessionList frame after ListSessions".to_owned());
        };
        Ok((sessions, workspace_tree))
    }
}

async fn send<T: Serialize>(writer: &mut OwnedWriteHalf, message: &T) -> Result<(), String> {
    write_message(writer, message).await.map_err(|error| error.to_string())
}

async fn recv_matching(
    reader: &mut OwnedReadHalf,
    predicate: impl Fn(&ServerMessage) -> bool,
) -> Result<ServerMessage, String> {
    tokio::time::timeout(FRAME_TIMEOUT, async {
        loop {
            let message: ServerMessage =
                read_message(reader).await.map_err(|error| error.to_string())?;
            if predicate(&message) {
                return Ok(message);
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for server frame".to_owned())?
}

async fn create_workspace(
    peer: &mut Peer,
    existing_workspace: Option<WorkspaceId>,
) -> Result<WorkspaceId, String> {
    peer.send(&ClientMessage::CreateWorkspace).await?;
    let message = peer
        .recv_matching(|message| {
            matches!(message, ServerMessage::WorkspaceInfo { workspace_id, .. } if Some(*workspace_id) != existing_workspace)
        })
        .await?;
    let ServerMessage::WorkspaceInfo { workspace_id, .. } = message else {
        return Err("server returned a non-WorkspaceInfo frame after CreateWorkspace".to_owned());
    };
    Ok(workspace_id)
}

async fn create_session(peer: &mut Peer, workspace_id: WorkspaceId) -> Result<SessionId, String> {
    peer.send(&ClientMessage::CreateSession {
        workspace_id,
        split_direction: None,
        cwd: None,
        size: None,
        command: Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "while :; do sleep 30; done".to_owned(),
        ]),
        ai_launch: None,
        shell_tool: None,
        env_envelope_id: None,
    })
    .await?;
    let message = peer
        .recv_matching(|message| {
            matches!(message, ServerMessage::SessionCreated { workspace_id: found, .. } if *found == workspace_id)
        })
        .await?;
    let ServerMessage::SessionCreated { session_id, .. } = message else {
        return Err("server returned a non-SessionCreated frame after CreateSession".to_owned());
    };
    Ok(session_id)
}

fn leaf(workspace_id: WorkspaceId, session_id: SessionId) -> WorkspaceTreeNode {
    WorkspaceTreeNode::Leaf {
        workspace_id,
        session_ids: vec![session_id],
        pane_trees: vec![None],
        active_tab_index: 0,
    }
}

fn split(
    first_workspace: WorkspaceId,
    first_session: SessionId,
    second_workspace: WorkspaceId,
    second_session: SessionId,
) -> WorkspaceTreeNode {
    WorkspaceTreeNode::Split {
        direction: LayoutDirection::Horizontal,
        ratio: 0.5,
        first: Box::new(leaf(first_workspace, first_session)),
        second: Box::new(leaf(second_workspace, second_session)),
    }
}

async fn assert_legacy_client_refusal(workspace_id: WorkspaceId) -> Result<(), String> {
    let stream = crate::ipc::connect().await.map_err(|error| error.to_string())?;
    let (mut reader, mut writer) = stream.into_split();
    send(
        &mut writer,
        &LegacyClientMessage::Hello { window_id: None, clipboard_gating: true, takeover: false },
    )
    .await?;
    let welcome =
        recv_matching(&mut reader, |message| matches!(message, ServerMessage::Welcome { .. }))
            .await?;
    let ServerMessage::Welcome { window_id, .. } = welcome else {
        return Err("legacy client did not receive Welcome".to_owned());
    };
    let target = WindowId::new();
    send(
        &mut writer,
        &ClientMessage::TransferWorkspace {
            transfer_id: 6,
            workspace_id,
            target_window_id: target,
        },
    )
    .await?;
    let result = recv_matching(&mut reader, |message| {
        matches!(message, ServerMessage::WorkspaceTransferResult { transfer_id: 6, .. })
    })
    .await?;
    let refused = matches!(
        result,
        ServerMessage::WorkspaceTransferResult {
            result: WorkspaceTransferResult::Refused {
                reason: WorkspaceTransferRefusal::CapabilityAbsent
            },
            ..
        }
    );
    drop(writer);
    drop(reader);
    if window_id == target {
        return Err("legacy capability probe reused its target id".to_owned());
    }
    if !refused {
        return Err("legacy client transfer was not refused for absent capability".to_owned());
    }
    Ok(())
}

fn assert_old_server_schema_compatibility(source_window: WindowId) -> Result<(), String> {
    let old = LegacyServerMessage::Welcome {
        window_id: source_window,
        other_windows: Vec::new(),
        clipboard_gating: true,
        participant_id: None,
    };
    let old_bytes = rmp_serde::to_vec_named(&old).map_err(|error| error.to_string())?;
    let decoded: ServerMessage =
        rmp_serde::from_slice(&old_bytes).map_err(|error| error.to_string())?;
    if !matches!(decoded, ServerMessage::Welcome { workspace_transfer: false, .. }) {
        return Err("old Welcome did not default workspace-transfer capability to false".to_owned());
    }

    let current = ServerMessage::Welcome {
        window_id: source_window,
        other_windows: Vec::new(),
        clipboard_gating: true,
        participant_id: None,
        terminal_images: TerminalImageCapabilities::default(),
        beads_detail: false,
        beads_write: false,
        beads_flow: false,
        pi_provider: false,
        agent_api: false,
        workspace_transfer: true,
    };
    let current_bytes = rmp_serde::to_vec_named(&current).map_err(|error| error.to_string())?;
    let _: LegacyServerMessage =
        rmp_serde::from_slice(&current_bytes).map_err(|error| error.to_string())?;
    Ok(())
}

async fn agent_world() -> Result<scribe_common::agent::AgentWorldSnapshot, String> {
    let stream = UnixStream::connect(scribe_common::socket::server_socket_path())
        .await
        .map_err(|error| error.to_string())?;
    let (mut reader, mut writer) = stream.into_split();
    send(
        &mut writer,
        &ClientMessage::AgentRequest(AgentRequest::World {
            request_id: 91,
            agent_label: "workspace-transfer-e2e".to_owned(),
            origin_session_id: None,
        }),
    )
    .await?;
    let message = recv_matching(&mut reader, |message| {
        matches!(message, ServerMessage::AgentResponse(response) if response.request_id == 91)
    })
    .await?;
    let ServerMessage::AgentResponse(response) = message else {
        return Err("agent request did not receive AgentResponse".to_owned());
    };
    match response.result {
        Ok(AgentPayload::World { snapshot }) => Ok(snapshot),
        Ok(_) => Err("agent world returned the wrong payload".to_owned()),
        Err(_) => Err("agent world was refused".to_owned()),
    }
}

fn assert_world_owner(
    snapshot: &scribe_common::agent::AgentWorldSnapshot,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    window_id: WindowId,
) -> Result<(), String> {
    let workspace_ok = snapshot.workspaces.iter().any(|workspace| {
        workspace.workspace_id == workspace_id && workspace.window_id == window_id
    });
    let session_ok = snapshot.sessions.iter().any(|session| {
        session.session_id == session_id
            && session.workspace_id == workspace_id
            && session.window_id == window_id
    });
    if workspace_ok && session_ok {
        Ok(())
    } else {
        Err("agent world did not expose one atomic workspace/session owner".to_owned())
    }
}

pub fn run(evidence_path: &Path) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    runtime.block_on(run_async(evidence_path))
}

async fn reconnect_fixture() -> Result<(Peer, Fixture), String> {
    let mut source = Peer::connect(None, true).await?;
    let source_window = source.window_id;
    let first_workspace = create_workspace(&mut source, None).await?;
    let first_session = create_session(&mut source, first_workspace).await?;
    let moved_workspace = create_workspace(&mut source, Some(first_workspace)).await?;
    let moved_session = create_session(&mut source, moved_workspace).await?;
    let rearranged = split(moved_workspace, moved_session, first_workspace, first_session);
    source.send(&ClientMessage::ReportWorkspaceTree { tree: rearranged.clone() }).await?;
    drop(source);

    let mut reconnected = Peer::connect(Some(source_window), true).await?;
    let (_, tree) = reconnected.session_list().await?;
    if tree.as_ref() != Some(&rearranged) {
        return Err("reconnected client did not receive the reported rearranged tree".to_owned());
    }
    Ok((
        reconnected,
        Fixture { source_window, first_workspace, first_session, moved_workspace, moved_session },
    ))
}

async fn transfer_during_upgrade(mut source: Peer, fixture: &Fixture) -> Result<WindowId, String> {
    let target_window = WindowId::new();
    source
        .send(&ClientMessage::TransferWorkspace {
            transfer_id: 77,
            workspace_id: fixture.moved_workspace,
            target_window_id: target_window,
        })
        .await?;
    // Leave the result unread and hand the serving fd to its successor. The
    // shared gate means the successor inherits a complete state plus ledger.
    tokio::time::sleep(Duration::from_millis(10)).await;
    crate::server::upgrade().await.map_err(|error| error.to_string())?;
    Ok(target_window)
}

async fn assert_source_after_upgrade(
    fixture: &Fixture,
    target_window: WindowId,
) -> Result<(), String> {
    let mut source = Peer::connect(Some(fixture.source_window), true).await?;
    source
        .send(&ClientMessage::TransferWorkspace {
            transfer_id: 77,
            workspace_id: fixture.moved_workspace,
            target_window_id: target_window,
        })
        .await?;
    let retry = source
        .recv_matching(|message| {
            matches!(message, ServerMessage::WorkspaceTransferResult { transfer_id: 77, .. })
        })
        .await?;
    if !matches!(
        retry,
        ServerMessage::WorkspaceTransferResult { result: WorkspaceTransferResult::Transferred, .. }
    ) {
        return Err("lost-ACK retry after upgrade did not return Transferred".to_owned());
    }
    let (sessions, tree) = source.session_list().await?;
    let expected = leaf(fixture.first_workspace, fixture.first_session);
    if tree.as_ref() != Some(&expected)
        || sessions.iter().any(|session| session.session_id == fixture.moved_session)
    {
        return Err("source window exposed a partial post-transfer state".to_owned());
    }
    Ok(())
}

async fn assert_target_after_upgrade(
    fixture: &Fixture,
    target_window: WindowId,
) -> Result<(), String> {
    let mut target = Peer::connect(Some(target_window), true).await?;
    if target.window_id != target_window || target_window == fixture.source_window {
        return Err("target window id was not fresh and claimable".to_owned());
    }
    let (sessions, tree) = target.session_list().await?;
    let expected = leaf(fixture.moved_workspace, fixture.moved_session);
    if tree.as_ref() != Some(&expected)
        || !sessions.iter().any(|session| session.session_id == fixture.moved_session)
        || sessions.iter().any(|session| session.session_id == fixture.first_session)
    {
        return Err(format!(
            "target window exposed a partial post-transfer state: tree={tree:?}, sessions={:?}",
            sessions.iter().map(|session| session.session_id).collect::<Vec<_>>()
        ));
    }
    Ok(())
}

fn write_evidence(
    evidence_path: &Path,
    fixture: &Fixture,
    target_window: WindowId,
) -> Result<(), String> {
    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        checks: vec![
            "reconnect_tree_persisted",
            "upgrade_with_transfer_in_flight",
            "lost_ack_retry_replayed_after_upgrade",
            "source_and_target_trees_atomic",
            "old_client_capability_refused_without_mutation",
            "old_server_capability_defaults_false",
            "new_messages_decode_in_old_schemas",
            "agent_world_window_ids_flipped_atomically",
        ],
        source_window_id: fixture.source_window,
        target_window_id: target_window,
        workspace_id: fixture.moved_workspace,
        session_id: fixture.moved_session,
    };
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let json = serde_json::to_vec_pretty(&evidence).map_err(|error| error.to_string())?;
    std::fs::write(evidence_path, json).map_err(|error| error.to_string())
}

async fn run_async(evidence_path: &Path) -> Result<(), String> {
    let (source, fixture) = reconnect_fixture().await?;
    assert_legacy_client_refusal(fixture.moved_workspace).await?;
    assert_old_server_schema_compatibility(fixture.source_window)?;

    let before = agent_world().await?;
    assert_world_owner(
        &before,
        fixture.moved_workspace,
        fixture.moved_session,
        fixture.source_window,
    )?;
    let target_window = transfer_during_upgrade(source, &fixture).await?;
    assert_source_after_upgrade(&fixture, target_window).await?;
    assert_target_after_upgrade(&fixture, target_window).await?;

    let after = agent_world().await?;
    assert_world_owner(&after, fixture.moved_workspace, fixture.moved_session, target_window)?;
    if after.workspaces.iter().any(|workspace| {
        workspace.workspace_id == fixture.moved_workspace
            && workspace.window_id == fixture.source_window
    }) || after.sessions.iter().any(|session| {
        session.session_id == fixture.moved_session && session.window_id == fixture.source_window
    }) {
        return Err("agent world retained stale source ownership".to_owned());
    }
    write_evidence(evidence_path, &fixture, target_window)
}
