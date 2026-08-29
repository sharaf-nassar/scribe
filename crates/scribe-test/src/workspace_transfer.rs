//! Production-socket oracle for workspace transfer persistence and compatibility.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use scribe_common::agent::{AgentPayload, AgentRequest, AgentWorldSnapshot};
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{
    ClientMessage, LayoutDirection, ServerMessage, WorkspaceMoveOperation, WorkspaceMoveRefusal,
    WorkspaceMoveResult, WorkspaceTransferRefusal, WorkspaceTransferResult, WorkspaceTreeEdge,
    WorkspaceTreeNode,
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

struct MoveFixture {
    source: Peer,
    target: Peer,
    source_retained_workspace: WorkspaceId,
    source_retained_session: SessionId,
    source_moved_workspace: WorkspaceId,
    source_moved_session: SessionId,
    target_workspace: WorkspaceId,
    target_session: SessionId,
    target_retained_workspace: WorkspaceId,
    target_retained_session: SessionId,
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

enum WorkspaceCapabilities {
    Transfer,
    TransferAndMove,
}

impl WorkspaceCapabilities {
    const fn workspace_move(self) -> bool {
        matches!(self, Self::TransferAndMove)
    }
}

struct Peer {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    window_id: WindowId,
}

impl Peer {
    async fn connect(
        requested_window: Option<WindowId>,
        capabilities: WorkspaceCapabilities,
    ) -> Result<Self, String> {
        let workspace_transfer = true;
        let workspace_move = capabilities.workspace_move();
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
                workspace_move,
            },
        )
        .await?;
        let welcome =
            recv_matching(&mut reader, |message| matches!(message, ServerMessage::Welcome { .. }))
                .await?;
        let ServerMessage::Welcome {
            window_id,
            workspace_transfer: transfer_advertised,
            workspace_move: move_advertised,
            ..
        } = welcome
        else {
            return Err("server returned a non-Welcome frame during handshake".to_owned());
        };
        if workspace_transfer && !transfer_advertised {
            return Err("new server did not advertise workspace transfer".to_owned());
        }
        if workspace_move && !move_advertised {
            return Err("new server did not advertise workspace move".to_owned());
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

    async fn next_without_session_created(&mut self) -> Result<ServerMessage, String> {
        let message = read_message(&mut self.reader).await.map_err(|error| error.to_string())?;
        if matches!(message, ServerMessage::SessionCreated { .. }) {
            return Err("workspace move unexpectedly created a session".to_owned());
        }
        Ok(message)
    }

    async fn recv_without_session_created(
        &mut self,
        predicate: impl Fn(&ServerMessage) -> bool,
    ) -> Result<ServerMessage, String> {
        tokio::time::timeout(FRAME_TIMEOUT, self.recv_until_without_session_created(predicate))
            .await
            .map_err(|_| "timed out waiting for server frame".to_owned())?
    }

    async fn recv_until_without_session_created(
        &mut self,
        predicate: impl Fn(&ServerMessage) -> bool,
    ) -> Result<ServerMessage, String> {
        loop {
            let message = self.next_without_session_created().await?;
            if predicate(&message) {
                return Ok(message);
            }
        }
    }

    async fn session_list(
        &mut self,
    ) -> Result<(Vec<scribe_common::protocol::SessionInfo>, Option<WorkspaceTreeNode>), String>
    {
        self.send(&ClientMessage::ListSessions).await?;
        let message = self
            .recv_without_session_created(|message| {
                matches!(message, ServerMessage::SessionList { .. })
            })
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

async fn create_session(
    peer: &mut Peer,
    workspace_id: WorkspaceId,
    command: &str,
) -> Result<SessionId, String> {
    peer.send(&ClientMessage::CreateSession {
        workspace_id,
        split_direction: None,
        cwd: None,
        size: None,
        command: Some(vec!["/bin/sh".to_owned(), "-c".to_owned(), command.to_owned()]),
        ai_launch: None,
        shell_tool: None,
        // Exercise the live session's environment coordinate too. A disabled
        // persistence store has no file to copy, but the launch id remains the
        // server-owned identity that a move must preserve.
        env_envelope_id: Some(scribe_common::ids::new_launch_id()),
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

async fn create_idle_session(
    peer: &mut Peer,
    workspace_id: WorkspaceId,
) -> Result<SessionId, String> {
    create_session(peer, workspace_id, "while :; do sleep 30; done").await
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

async fn legacy_peer() -> Result<(OwnedReadHalf, OwnedWriteHalf, WindowId), String> {
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
    Ok((reader, writer, window_id))
}

async fn assert_legacy_transfer_refusal(workspace_id: WorkspaceId) -> Result<(), String> {
    let (mut reader, mut writer, window_id) = legacy_peer().await?;
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
    if window_id == target {
        return Err("legacy capability probe reused its target id".to_owned());
    }
    if !matches!(
        result,
        ServerMessage::WorkspaceTransferResult {
            result: WorkspaceTransferResult::Refused {
                reason: WorkspaceTransferRefusal::CapabilityAbsent
            },
            ..
        }
    ) {
        return Err("legacy client transfer was not refused for absent capability".to_owned());
    }
    Ok(())
}

async fn assert_legacy_move_refusal(
    workspace_id: WorkspaceId,
    target_window_id: WindowId,
    target_workspace_id: WorkspaceId,
) -> Result<(), String> {
    let (mut reader, mut writer, _) = legacy_peer().await?;
    send(
        &mut writer,
        &ClientMessage::MoveWorkspace {
            move_id: 7,
            workspace_id,
            target_window_id,
            target_workspace_id,
            operation: WorkspaceMoveOperation::Swap,
        },
    )
    .await?;
    let result = recv_matching(&mut reader, |message| {
        matches!(message, ServerMessage::WorkspaceMoveResult { move_id: 7, .. })
    })
    .await?;
    if !matches!(
        result,
        ServerMessage::WorkspaceMoveResult {
            result: WorkspaceMoveResult::Refused { reason: WorkspaceMoveRefusal::CapabilityAbsent },
            ..
        }
    ) {
        return Err("legacy client move was not refused for absent capability".to_owned());
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
    if !matches!(
        decoded,
        ServerMessage::Welcome { workspace_transfer: false, workspace_move: false, .. }
    ) {
        return Err("old Welcome did not default workspace capabilities to false".to_owned());
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
        workspace_move: true,
    };
    let current_bytes = rmp_serde::to_vec_named(&current).map_err(|error| error.to_string())?;
    let _: LegacyServerMessage =
        rmp_serde::from_slice(&current_bytes).map_err(|error| error.to_string())?;
    Ok(())
}

async fn agent_snapshot(
    request: AgentRequest,
    expect_siblings: bool,
) -> Result<AgentWorldSnapshot, String> {
    let stream = UnixStream::connect(scribe_common::socket::server_socket_path())
        .await
        .map_err(|error| error.to_string())?;
    let (mut reader, mut writer) = stream.into_split();
    let request_id = match &request {
        AgentRequest::World { request_id, .. } | AgentRequest::Siblings { request_id, .. } => {
            *request_id
        }
        _ => return Err("workspace fixture sent an unsupported agent request".to_owned()),
    };
    send(&mut writer, &ClientMessage::AgentRequest(request)).await?;
    let message = recv_matching(&mut reader, |message| {
        matches!(message, ServerMessage::AgentResponse(response) if response.request_id == request_id)
    })
    .await?;
    let ServerMessage::AgentResponse(response) = message else {
        return Err("agent request did not receive AgentResponse".to_owned());
    };
    match (expect_siblings, response.result) {
        (false, Ok(AgentPayload::World { snapshot }))
        | (true, Ok(AgentPayload::Siblings { snapshot })) => Ok(snapshot),
        (_, Ok(_)) => Err("agent request returned the wrong payload".to_owned()),
        (_, Err(_)) => Err("agent request was refused".to_owned()),
    }
}

async fn agent_world() -> Result<AgentWorldSnapshot, String> {
    agent_snapshot(
        AgentRequest::World {
            request_id: 91,
            agent_label: "workspace-transfer-e2e".to_owned(),
            origin_session_id: None,
            progress_ack: false,
        },
        false,
    )
    .await
}

async fn agent_siblings(
    origin_session_id: SessionId,
    request_id: u64,
) -> Result<AgentWorldSnapshot, String> {
    agent_snapshot(
        AgentRequest::Siblings {
            request_id,
            agent_label: "workspace-transfer-e2e".to_owned(),
            origin_session_id: Some(origin_session_id),
            progress_ack: false,
        },
        true,
    )
    .await
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

fn collect_tree_members(
    tree: &WorkspaceTreeNode,
    members: &mut HashMap<WorkspaceId, Vec<SessionId>>,
) {
    match tree {
        WorkspaceTreeNode::Leaf { workspace_id, session_ids, .. } => {
            members.insert(*workspace_id, session_ids.clone());
        }
        WorkspaceTreeNode::Split { first, second, .. } => {
            collect_tree_members(first, members);
            collect_tree_members(second, members);
        }
    }
}

fn comparable_members(members: &mut HashMap<WorkspaceId, Vec<SessionId>>) {
    for sessions in members.values_mut() {
        sessions.sort_by_key(ToString::to_string);
    }
}

async fn assert_window_state(
    peer: &mut Peer,
    expected: &[(WorkspaceId, SessionId)],
) -> Result<HashMap<SessionId, String>, String> {
    let (sessions, tree) = peer.session_list().await?;
    if sessions.len() != expected.len() {
        return Err(format!(
            "window {} listed {} sessions, expected {}",
            peer.window_id,
            sessions.len(),
            expected.len()
        ));
    }
    let mut expected_members = HashMap::<WorkspaceId, Vec<SessionId>>::new();
    for &(workspace_id, session_id) in expected {
        expected_members.entry(workspace_id).or_default().push(session_id);
    }
    let Some(tree) = tree.as_ref() else {
        return Err(format!("window {} did not retain a workspace tree", peer.window_id));
    };
    let mut actual_members = HashMap::new();
    collect_tree_members(tree, &mut actual_members);
    comparable_members(&mut expected_members);
    comparable_members(&mut actual_members);
    if actual_members != expected_members {
        return Err(format!(
            "window {} tree differs after workspace move: actual={actual_members:?}, expected={expected_members:?}",
            peer.window_id
        ));
    }
    let mut envelopes = HashMap::new();
    for session in sessions {
        if !expected.contains(&(session.workspace_id, session.session_id)) {
            return Err(format!(
                "window {} listed session {} under the wrong workspace {}",
                peer.window_id, session.session_id, session.workspace_id
            ));
        }
        let Some(launch_id) = session.launch_id else {
            return Err(format!(
                "session {} lost its environment launch identity",
                session.session_id
            ));
        };
        envelopes.insert(session.session_id, launch_id);
    }
    Ok(envelopes)
}

struct WorkspaceMove {
    move_id: u64,
    workspace_id: WorkspaceId,
    target_window_id: WindowId,
    target_workspace_id: WorkspaceId,
    operation: WorkspaceMoveOperation,
}

async fn request_workspace_move(
    peer: &mut Peer,
    request: WorkspaceMove,
) -> Result<WorkspaceMoveResult, String> {
    let WorkspaceMove { move_id, workspace_id, target_window_id, target_workspace_id, operation } =
        request;
    peer.send(&ClientMessage::MoveWorkspace {
        move_id,
        workspace_id,
        target_window_id,
        target_workspace_id,
        operation,
    })
    .await?;
    let message = peer
        .recv_without_session_created(|message| {
            matches!(message, ServerMessage::WorkspaceMoveResult { move_id: found, .. } if *found == move_id)
        })
        .await?;
    let ServerMessage::WorkspaceMoveResult { result, .. } = message else {
        return Err("server returned a non-workspace-move result".to_owned());
    };
    Ok(result)
}

async fn assert_no_source_pty_output(peer: &mut Peer, session_id: SessionId) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let message =
            tokio::time::timeout(remaining, read_message::<ServerMessage, _>(&mut peer.reader))
                .await;
        let Ok(message) = message else { return Ok(()) };
        let message = message.map_err(|error| error.to_string())?;
        if matches!(message, ServerMessage::PtyOutput { session_id: found, .. } if found == session_id)
        {
            return Err("stale source input reached a moved session's PTY".to_owned());
        }
    }
}

async fn attach_and_assert_destination_pty(
    peer: &mut Peer,
    session_id: SessionId,
) -> Result<(), String> {
    peer.send(&ClientMessage::AttachSessions {
        session_ids: vec![session_id],
        dimensions: Vec::new(),
    })
    .await?;
    peer.recv_matching(|message| {
        matches!(message, ServerMessage::SessionCreated { session_id: found, .. } if *found == session_id)
    })
    .await?;
    peer.send(&ClientMessage::Subscribe { session_ids: vec![session_id] }).await?;
    peer.send(&ClientMessage::KeyInput {
        session_id,
        data: b"destination-only\n".to_vec(),
        dismisses_attention: false,
    })
    .await?;
    let output = peer
        .recv_matching(|message| {
            matches!(message, ServerMessage::PtyOutput { session_id: found, data }
                if *found == session_id && data.windows(b"workspace-move:destination-only".len()).any(|part| part == b"workspace-move:destination-only"))
        })
        .await?;
    if !matches!(output, ServerMessage::PtyOutput { .. }) {
        return Err("destination attachment did not receive its PTY output".to_owned());
    }
    Ok(())
}

async fn fresh_move_peer() -> Result<Peer, String> {
    // A previous handoff oracle deliberately leaves detached windows behind;
    // name this fixture's windows so it never adopts that unrelated state.
    Peer::connect(Some(WindowId::new()), WorkspaceCapabilities::TransferAndMove).await
}

async fn move_fixture() -> Result<MoveFixture, String> {
    let mut source = fresh_move_peer().await?;
    let source_retained_workspace = create_workspace(&mut source, None).await?;
    let source_retained_session =
        create_idle_session(&mut source, source_retained_workspace).await?;
    let source_moved_workspace =
        create_workspace(&mut source, Some(source_retained_workspace)).await?;
    let source_moved_session = create_session(
        &mut source,
        source_moved_workspace,
        "while IFS= read -r line; do printf 'workspace-move:%s\\n' \"$line\"; done",
    )
    .await?;
    source
        .send(&ClientMessage::ReportWorkspaceTree {
            tree: split(
                source_retained_workspace,
                source_retained_session,
                source_moved_workspace,
                source_moved_session,
            ),
        })
        .await?;

    let mut target = fresh_move_peer().await?;
    let target_workspace = create_workspace(&mut target, None).await?;
    let target_session = create_idle_session(&mut target, target_workspace).await?;
    let target_retained_workspace = create_workspace(&mut target, Some(target_workspace)).await?;
    let target_retained_session =
        create_idle_session(&mut target, target_retained_workspace).await?;
    target
        .send(&ClientMessage::ReportWorkspaceTree {
            tree: split(
                target_workspace,
                target_session,
                target_retained_workspace,
                target_retained_session,
            ),
        })
        .await?;

    Ok(MoveFixture {
        source,
        target,
        source_retained_workspace,
        source_retained_session,
        source_moved_workspace,
        source_moved_session,
        target_workspace,
        target_session,
        target_retained_workspace,
        target_retained_session,
    })
}

async fn assert_legacy_move_refusal_keeps_source(
    fixture: &mut MoveFixture,
) -> Result<HashMap<SessionId, String>, String> {
    let source_before = assert_window_state(
        &mut fixture.source,
        &[
            (fixture.source_retained_workspace, fixture.source_retained_session),
            (fixture.source_moved_workspace, fixture.source_moved_session),
        ],
    )
    .await?;
    assert_window_state(
        &mut fixture.target,
        &[
            (fixture.target_workspace, fixture.target_session),
            (fixture.target_retained_workspace, fixture.target_retained_session),
        ],
    )
    .await?;
    assert_legacy_move_refusal(
        fixture.source_moved_workspace,
        fixture.target.window_id,
        fixture.target_workspace,
    )
    .await?;
    let source_after = assert_window_state(
        &mut fixture.source,
        &[
            (fixture.source_retained_workspace, fixture.source_retained_session),
            (fixture.source_moved_workspace, fixture.source_moved_session),
        ],
    )
    .await?;
    if source_after != source_before {
        return Err(
            "legacy workspace-move refusal mutated source environment identities".to_owned()
        );
    }
    Ok(source_before)
}

async fn assert_agent_move_owner(
    workspace_id: WorkspaceId,
    session_id: SessionId,
    window_id: WindowId,
    request_id: u64,
) -> Result<(), String> {
    let world = agent_world().await?;
    let siblings = agent_siblings(session_id, request_id).await?;
    for snapshot in [&world, &siblings] {
        assert_world_owner(snapshot, workspace_id, session_id, window_id)?;
    }
    Ok(())
}

async fn assert_edge_move_and_agent_snapshots() -> Result<(), String> {
    let mut fixture = move_fixture().await?;
    let source_window = fixture.source.window_id;
    let target_window = fixture.target.window_id;
    let source_before = assert_legacy_move_refusal_keeps_source(&mut fixture).await?;
    assert_agent_move_owner(
        fixture.source_moved_workspace,
        fixture.source_moved_session,
        source_window,
        92,
    )
    .await?;
    if !matches!(
        request_workspace_move(
            &mut fixture.source,
            WorkspaceMove {
                move_id: 201,
                workspace_id: fixture.source_moved_workspace,
                target_window_id: target_window,
                target_workspace_id: fixture.target_workspace,
                operation: WorkspaceMoveOperation::InsertAtEdge { edge: WorkspaceTreeEdge::Right },
            },
        )
        .await?,
        WorkspaceMoveResult::Moved
    ) {
        return Err("edge insertion was refused".to_owned());
    }
    assert_window_state(
        &mut fixture.source,
        &[(fixture.source_retained_workspace, fixture.source_retained_session)],
    )
    .await?;
    let target_after = assert_window_state(
        &mut fixture.target,
        &[
            (fixture.target_workspace, fixture.target_session),
            (fixture.source_moved_workspace, fixture.source_moved_session),
            (fixture.target_retained_workspace, fixture.target_retained_session),
        ],
    )
    .await?;
    if target_after.get(&fixture.source_moved_session)
        != source_before.get(&fixture.source_moved_session)
    {
        return Err(
            "edge insertion did not retain the moved session environment identity".to_owned()
        );
    }

    assert_agent_move_owner(
        fixture.source_moved_workspace,
        fixture.source_moved_session,
        target_window,
        93,
    )
    .await?;
    fixture
        .source
        .send(&ClientMessage::KeyInput {
            session_id: fixture.source_moved_session,
            data: b"stale-source\n".to_vec(),
            dismisses_attention: false,
        })
        .await?;
    assert_no_source_pty_output(&mut fixture.source, fixture.source_moved_session).await?;
    attach_and_assert_destination_pty(&mut fixture.target, fixture.source_moved_session).await
}

async fn assert_bidirectional_swap() -> Result<(), String> {
    let mut fixture = move_fixture().await?;
    let result = request_workspace_move(
        &mut fixture.source,
        WorkspaceMove {
            move_id: 202,
            workspace_id: fixture.source_moved_workspace,
            target_window_id: fixture.target.window_id,
            target_workspace_id: fixture.target_workspace,
            operation: WorkspaceMoveOperation::Swap,
        },
    )
    .await?;
    if !matches!(result, WorkspaceMoveResult::Moved) {
        return Err(format!("centre swap was refused: {result:?}"));
    }
    assert_window_state(
        &mut fixture.source,
        &[
            (fixture.source_retained_workspace, fixture.source_retained_session),
            (fixture.target_workspace, fixture.target_session),
        ],
    )
    .await?;
    assert_window_state(
        &mut fixture.target,
        &[
            (fixture.source_moved_workspace, fixture.source_moved_session),
            (fixture.target_retained_workspace, fixture.target_retained_session),
        ],
    )
    .await?;
    Ok(())
}

async fn assert_sole_source_reattach() -> Result<(), String> {
    let mut source = fresh_move_peer().await?;
    let source_workspace = create_workspace(&mut source, None).await?;
    let source_session = create_idle_session(&mut source, source_workspace).await?;
    source
        .send(&ClientMessage::ReportWorkspaceTree { tree: leaf(source_workspace, source_session) })
        .await?;
    let source_window = source.window_id;

    let mut target = fresh_move_peer().await?;
    let target_workspace = create_workspace(&mut target, None).await?;
    let target_session = create_idle_session(&mut target, target_workspace).await?;
    target
        .send(&ClientMessage::ReportWorkspaceTree { tree: leaf(target_workspace, target_session) })
        .await?;
    if !matches!(
        request_workspace_move(
            &mut source,
            WorkspaceMove {
                move_id: 203,
                workspace_id: source_workspace,
                target_window_id: target.window_id,
                target_workspace_id: target_workspace,
                operation: WorkspaceMoveOperation::InsertAtEdge { edge: WorkspaceTreeEdge::Left },
            },
        )
        .await?,
        WorkspaceMoveResult::Moved
    ) {
        return Err("sole-source reattachment was refused".to_owned());
    }
    source
        .recv_without_session_created(|message| {
            matches!(message, ServerMessage::WindowClosed { window_id } if *window_id == source_window)
        })
        .await?;
    assert_window_state(
        &mut target,
        &[(source_workspace, source_session), (target_workspace, target_session)],
    )
    .await?;
    Ok(())
}

pub fn run(evidence_path: &Path) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    runtime.block_on(run_async(evidence_path))
}

async fn reconnect_fixture() -> Result<(Peer, Fixture), String> {
    let mut source = Peer::connect(None, WorkspaceCapabilities::Transfer).await?;
    let source_window = source.window_id;
    let first_workspace = create_workspace(&mut source, None).await?;
    let first_session = create_idle_session(&mut source, first_workspace).await?;
    let moved_workspace = create_workspace(&mut source, Some(first_workspace)).await?;
    let moved_session = create_idle_session(&mut source, moved_workspace).await?;
    let rearranged = split(moved_workspace, moved_session, first_workspace, first_session);
    source.send(&ClientMessage::ReportWorkspaceTree { tree: rearranged.clone() }).await?;
    drop(source);

    let mut reconnected =
        Peer::connect(Some(source_window), WorkspaceCapabilities::Transfer).await?;
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
    let mut source =
        Peer::connect(Some(fixture.source_window), WorkspaceCapabilities::Transfer).await?;
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
    let mut target = Peer::connect(Some(target_window), WorkspaceCapabilities::Transfer).await?;
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
        schema_version: 2,
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
            "existing_target_edge_insert_preserves_identity_tree_and_env",
            "bidirectional_centre_swap_preserves_both_trees",
            "sole_source_reattach_acknowledges_source_close",
            "workspace_move_never_creates_a_replacement_session",
            "legacy_workspace_move_refusal_leaves_state_unchanged",
            "agent_world_and_siblings_flip_workspace_owner",
            "stale_source_input_cannot_reach_moved_pty",
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
    assert_legacy_transfer_refusal(fixture.moved_workspace).await?;
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
    assert_edge_move_and_agent_snapshots().await?;
    assert_bidirectional_swap().await?;
    assert_sole_source_reattach().await?;
    write_evidence(evidence_path, &fixture, target_window)
}
