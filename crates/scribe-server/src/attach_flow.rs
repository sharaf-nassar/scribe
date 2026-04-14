use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use scribe_common::ai_state::AiProcessState;
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::{ServerMessage, SessionContext, TerminalSize};

use crate::ipc_server::{
    AttachSessionData, AttachedSessionIds, ClientWriter, LiveSessionRegistry, SessionAttachment,
    SharedWriter, detect_git_branch, resize_term, send_message, set_pty_winsize,
};
use crate::session_manager::snapshot_term;
use crate::workspace_manager::WorkspaceManager;

#[derive(Clone)]
struct AttachEntry {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    shell_name: String,
    client_writer: ClientWriter,
    attachment: SessionAttachment,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    resize_fd: Arc<OwnedFd>,
    target_dims: Option<TerminalSize>,
    has_handoff_snapshot: bool,
    title: String,
    codex_task_label: Option<String>,
    cwd: Option<std::path::PathBuf>,
    context: Option<SessionContext>,
    ai_state: Option<AiProcessState>,
}

impl From<AttachSessionData> for AttachEntry {
    fn from(data: AttachSessionData) -> Self {
        Self {
            session_id: data.session_id,
            workspace_id: data.workspace_id,
            shell_name: data.shell_name,
            client_writer: data.client_writer,
            attachment: data.attachment,
            term: data.term,
            resize_fd: data.resize_fd,
            target_dims: data.target_dims,
            has_handoff_snapshot: data.has_handoff_snapshot,
            title: data.title,
            codex_task_label: data.codex_task_label,
            cwd: data.cwd,
            context: data.context,
            ai_state: data.ai_state,
        }
    }
}

#[derive(Clone, Copy)]
pub struct AttachClientContext<'a> {
    pub writer: &'a SharedWriter,
    pub attached_ids: &'a AttachedSessionIds,
}

pub async fn attach_sessions(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    client: AttachClientContext<'_>,
) -> HashSet<SessionId> {
    let entries = prepare_attach_entries(session_ids, dimensions, live_sessions).await;
    attach_prepared_entries(
        &entries,
        client.writer,
        live_sessions,
        workspace_manager,
        client.attached_ids,
    )
    .await
}

async fn prepare_attach_entries(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    live_sessions: &LiveSessionRegistry,
) -> Vec<AttachEntry> {
    let mut sessions = live_sessions.write().await;
    let mut entries = Vec::with_capacity(session_ids.len());

    for (i, &session_id) in session_ids.iter().enumerate() {
        let target_dims = dimensions.get(i).copied();
        if let Some(session) = sessions.get_mut(&session_id) {
            entries.push(AttachEntry::from(session.prepare_attach_data(session_id, target_dims)));
        } else {
            warn!(%session_id, "AttachSessions: session not found");
        }
    }

    entries
}

async fn attach_prepared_entries(
    entries: &[AttachEntry],
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    attached_ids: &AttachedSessionIds,
) -> HashSet<SessionId> {
    let mut attached = HashSet::with_capacity(entries.len());

    for entry in entries {
        attach_one_session(entry, writer, live_sessions, workspace_manager, attached_ids).await;
        attached.insert(entry.session_id);
    }

    attached
}

async fn attach_one_session(
    entry: &AttachEntry,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    attached_ids: &AttachedSessionIds,
) {
    send_attach_replay(entry, writer, live_sessions, workspace_manager).await;
    install_client_writer(entry, writer, attached_ids).await;
}

async fn send_attach_replay(
    entry: &AttachEntry,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) {
    let session_id = entry.session_id;

    if let Some(size) = entry.target_dims
        && !entry.has_handoff_snapshot
    {
        // Handoff-restored sessions replay the preserved pre-upgrade snapshot
        // first; resizing before that replay can make a live foreground
        // process redraw and overwrite the restored history immediately.
        if size.has_grid() {
            resize_term(&entry.term, size.cols, size.rows).await;
            if let Err(error) = set_pty_winsize(entry.resize_fd.as_ref(), size) {
                warn!(%session_id, "pre-snapshot TIOCSWINSZ failed: {error}");
            }
        }
    }

    send_message(
        writer,
        &ServerMessage::SessionCreated {
            session_id,
            workspace_id: entry.workspace_id,
            shell_name: entry.shell_name.clone(),
        },
    )
    .await;

    send_stored_metadata(writer, session_id, entry).await;
    send_workspace_info(writer, entry.workspace_id, workspace_manager).await;

    let snapshot = take_session_snapshot(session_id, &entry.term, live_sessions).await;
    send_message(writer, &ServerMessage::ScreenSnapshot { session_id, snapshot }).await;
}

async fn install_client_writer(
    entry: &AttachEntry,
    writer: &SharedWriter,
    attached_ids: &AttachedSessionIds,
) {
    let mut client_writer = entry.client_writer.lock().await;
    if client_writer.is_some() {
        warn!(
            %entry.session_id,
            "AttachSessions: overwriting existing client writer - previous client may still be connected"
        );
    }
    *client_writer = Some(Arc::clone(writer));
    drop(client_writer);
    *entry.attachment.lock().await = Some(Arc::clone(attached_ids));

    info!(session_id = %entry.session_id, "session attached to new client");
}

async fn send_workspace_info(
    writer: &SharedWriter,
    workspace_id: WorkspaceId,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) {
    let wm = workspace_manager.read().await;
    if let Some((name, accent_color, split_direction, project_root)) =
        wm.workspace_info(workspace_id)
    {
        send_message(
            writer,
            &ServerMessage::WorkspaceInfo {
                workspace_id,
                name,
                accent_color,
                split_direction,
                project_root,
            },
        )
        .await;
    }
}

async fn send_stored_metadata(writer: &SharedWriter, session_id: SessionId, entry: &AttachEntry) {
    if entry.title != "shell" {
        send_message(
            writer,
            &ServerMessage::TitleChanged { session_id, title: entry.title.clone() },
        )
        .await;
    }

    if let Some(task_label) = entry.codex_task_label.as_deref() {
        if !task_label.trim().is_empty() {
            send_message(
                writer,
                &ServerMessage::CodexTaskLabelChanged {
                    session_id,
                    task_label: task_label.to_owned(),
                },
            )
            .await;
        }
    }

    if let Some(cwd) = entry.cwd.as_ref() {
        send_message(writer, &ServerMessage::CwdChanged { session_id, cwd: cwd.clone() }).await;
        send_message(
            writer,
            &ServerMessage::GitBranch { session_id, branch: detect_git_branch(cwd) },
        )
        .await;
    }

    if let Some(context) = entry.context.as_ref() {
        send_message(
            writer,
            &ServerMessage::SessionContextChanged { session_id, context: context.clone() },
        )
        .await;
    }

    if let Some(ai_state) = entry.ai_state.as_ref() {
        send_message(
            writer,
            &ServerMessage::AiStateChanged { session_id, ai_state: ai_state.clone() },
        )
        .await;
    }
}

pub async fn take_session_snapshot(
    session_id: SessionId,
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    live_sessions: &LiveSessionRegistry,
) -> scribe_common::screen::ScreenSnapshot {
    let handoff_snapshot = {
        let mut registry = live_sessions.write().await;
        registry
            .get_mut(&session_id)
            .and_then(crate::ipc_server::LiveSession::take_handoff_snapshot)
    };

    if let Some(snapshot) = handoff_snapshot {
        snapshot
    } else {
        let guard = term.lock().await;
        snapshot_term(&guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::sync::Arc;

    use alacritty_terminal::Term;
    use alacritty_terminal::grid::Dimensions;
    use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
    use scribe_common::framing::read_message;
    use scribe_common::protocol::{ServerMessage, SessionContext};
    use scribe_common::screen::{CellFlags, CursorStyle, ScreenCell, ScreenColor, ScreenSnapshot};
    use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
    use tokio::sync::{Mutex, mpsc};

    use crate::session_manager::{build_term_config, snapshot_term};

    struct TestDimensions;

    impl Dimensions for TestDimensions {
        fn total_lines(&self) -> usize {
            1
        }

        fn screen_lines(&self) -> usize {
            1
        }

        fn columns(&self) -> usize {
            1
        }
    }

    fn unix_stream_pair() -> (tokio::net::UnixStream, tokio::net::UnixStream) {
        let (left, right) = StdUnixStream::pair().unwrap();
        left.set_nonblocking(true).unwrap();
        right.set_nonblocking(true).unwrap();
        (
            tokio::net::UnixStream::from_std(left).unwrap(),
            tokio::net::UnixStream::from_std(right).unwrap(),
        )
    }

    fn make_term(session_id: SessionId) -> Term<ScribeEventListener> {
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<SessionEvent>();
        let listener = ScribeEventListener::new(session_id, event_tx);
        Term::new(build_term_config(1), &TestDimensions, listener)
    }

    fn sample_snapshot() -> ScreenSnapshot {
        ScreenSnapshot {
            cells: vec![ScreenCell {
                c: 'x',
                fg: ScreenColor::Named(256),
                bg: ScreenColor::Named(257),
                flags: CellFlags::default(),
            }],
            cols: 1,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cursor_style: CursorStyle::Block,
            cursor_visible: true,
            alt_screen: false,
            scrollback: vec![],
            scrollback_rows: 0,
        }
    }

    fn sample_entry(session_id: SessionId, workspace_id: WorkspaceId) -> AttachEntry {
        let _ = sample_snapshot();
        AttachEntry {
            session_id,
            workspace_id,
            shell_name: String::from("zsh"),
            client_writer: Arc::new(Mutex::new(None)),
            attachment: Arc::new(Mutex::new(None)),
            term: Arc::new(Mutex::new(make_term(session_id))),
            resize_fd: Arc::new(std::fs::File::open("/dev/null").unwrap().into()),
            target_dims: None,
            has_handoff_snapshot: false,
            title: String::from("sample"),
            codex_task_label: Some(String::from("task")),
            cwd: Some(std::env::temp_dir()),
            context: Some(SessionContext {
                remote: true,
                host: Some(String::from("example-host")),
                tmux_session: Some(String::from("session")),
            }),
            ai_state: Some(AiProcessState::new_with_provider(
                AiProvider::CodexCode,
                AiState::Processing,
            )),
        }
    }

    struct AttachReplayExpectation {
        session_id: SessionId,
        workspace_id: WorkspaceId,
        cwd: std::path::PathBuf,
        context: SessionContext,
        ai_state: AiProcessState,
        snapshot: ScreenSnapshot,
    }

    impl AttachReplayExpectation {
        async fn capture(entry: &AttachEntry) -> Self {
            let snapshot = {
                let term = entry.term.lock().await;
                snapshot_term(&term)
            };

            Self {
                session_id: entry.session_id,
                workspace_id: entry.workspace_id,
                cwd: entry.cwd.clone().unwrap(),
                context: entry.context.clone().unwrap(),
                ai_state: entry.ai_state.clone().unwrap(),
                snapshot,
            }
        }
    }

    #[tokio::test]
    async fn send_attach_replay_emits_expected_sequence_and_leaves_writer_unset() {
        let live_sessions = crate::ipc_server::new_live_session_registry();
        let workspace_manager = Arc::new(RwLock::new(WorkspaceManager::new(vec![])));
        let workspace_id = workspace_manager.write().await.create_workspace();
        let session_id = SessionId::new();
        let entry = sample_entry(session_id, workspace_id);
        let expected = AttachReplayExpectation::capture(&entry).await;

        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer: SharedWriter = Arc::new(Mutex::new(server_write));

        send_attach_replay(&entry, &writer, &live_sessions, &workspace_manager).await;

        assert!(entry.client_writer.lock().await.is_none());

        let messages = read_attach_replay_messages(&mut client_read).await;
        assert_attach_replay_sequence(&messages, &expected);
    }

    #[tokio::test]
    async fn attach_prepared_entries_returns_processed_session_ids() {
        let live_sessions = crate::ipc_server::new_live_session_registry();
        let workspace_manager = Arc::new(RwLock::new(WorkspaceManager::new(vec![])));
        let mut wm = workspace_manager.write().await;
        let workspace_a = wm.create_workspace();
        let workspace_b = wm.create_workspace();
        drop(wm);

        let entries = vec![
            sample_entry(SessionId::new(), workspace_a),
            sample_entry(SessionId::new(), workspace_b),
        ];

        let (server, _client) = unix_stream_pair();
        let (_read, write) = tokio::io::split(server);
        let writer: SharedWriter = Arc::new(Mutex::new(write));
        let attached_ids = Arc::new(Mutex::new(HashSet::new()));

        let attached = attach_prepared_entries(
            &entries,
            &writer,
            &live_sessions,
            &workspace_manager,
            &attached_ids,
        )
        .await;

        assert_eq!(attached.len(), 2);
        assert!(attached.contains(&entries[0].session_id));
        assert!(attached.contains(&entries[1].session_id));
    }

    async fn read_attach_replay_messages(
        client_read: &mut tokio::io::ReadHalf<tokio::net::UnixStream>,
    ) -> Vec<ServerMessage> {
        let mut messages = Vec::with_capacity(9);
        for _ in 0..9 {
            messages.push(read_message::<ServerMessage, _>(client_read).await.unwrap());
        }
        messages
    }

    fn assert_attach_replay_sequence(
        messages: &[ServerMessage],
        expected: &AttachReplayExpectation,
    ) {
        assert_session_created_message(&messages[0], expected.session_id, expected.workspace_id);
        assert_title_changed_message(&messages[1], expected.session_id);
        assert_codex_task_label_message(&messages[2], expected.session_id);
        assert_cwd_changed_message(&messages[3], expected.session_id, &expected.cwd);
        assert_git_branch_message(&messages[4], expected.session_id);
        assert_context_message(&messages[5], expected.session_id, &expected.context);
        assert_ai_state_message(&messages[6], expected.session_id, &expected.ai_state);
        assert_workspace_info_message(&messages[7], expected.workspace_id);
        assert_snapshot_message(&messages[8], expected.session_id, &expected.snapshot);
    }

    fn assert_session_created_message(
        message: &ServerMessage,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) {
        assert!(matches!(
            message,
            ServerMessage::SessionCreated {
                session_id: actual_session_id,
                workspace_id: actual_workspace_id,
                shell_name,
            } if actual_session_id == &session_id
                && actual_workspace_id == &workspace_id
                && shell_name == "zsh"
        ));
    }

    fn assert_title_changed_message(message: &ServerMessage, session_id: SessionId) {
        assert!(matches!(
            message,
            ServerMessage::TitleChanged { session_id: actual_session_id, title }
                if actual_session_id == &session_id && title == "sample"
        ));
    }

    fn assert_codex_task_label_message(message: &ServerMessage, session_id: SessionId) {
        assert!(matches!(
            message,
            ServerMessage::CodexTaskLabelChanged { session_id: actual_session_id, task_label }
                if actual_session_id == &session_id && task_label == "task"
        ));
    }

    fn assert_cwd_changed_message(
        message: &ServerMessage,
        session_id: SessionId,
        expected_cwd: &std::path::PathBuf,
    ) {
        assert!(matches!(
            message,
            ServerMessage::CwdChanged { session_id: actual_session_id, cwd }
                if actual_session_id == &session_id && cwd == expected_cwd
        ));
    }

    fn assert_git_branch_message(message: &ServerMessage, session_id: SessionId) {
        assert!(matches!(
            message,
            ServerMessage::GitBranch { session_id: actual_session_id, .. }
                if actual_session_id == &session_id
        ));
    }

    fn assert_context_message(
        message: &ServerMessage,
        session_id: SessionId,
        expected_context: &SessionContext,
    ) {
        assert!(matches!(
            message,
            ServerMessage::SessionContextChanged { session_id: actual_session_id, context }
                if actual_session_id == &session_id && context == expected_context
        ));
    }

    fn assert_ai_state_message(
        message: &ServerMessage,
        session_id: SessionId,
        expected_ai_state: &AiProcessState,
    ) {
        assert!(matches!(
            message,
            ServerMessage::AiStateChanged { session_id: actual_session_id, ai_state }
                if actual_session_id == &session_id && ai_state == expected_ai_state
        ));
    }

    fn assert_workspace_info_message(message: &ServerMessage, workspace_id: WorkspaceId) {
        assert!(matches!(
            message,
            ServerMessage::WorkspaceInfo { workspace_id: actual_workspace_id, .. }
                if actual_workspace_id == &workspace_id
        ));
    }

    fn assert_snapshot_message(
        message: &ServerMessage,
        session_id: SessionId,
        expected_snapshot: &ScreenSnapshot,
    ) {
        let ServerMessage::ScreenSnapshot { session_id: actual_session_id, snapshot } = message
        else {
            panic!("expected ScreenSnapshot, got {message:?}");
        };

        assert_eq!(*actual_session_id, session_id);
        assert_snapshot_header(snapshot, expected_snapshot);
        assert_snapshot_cursor(snapshot, expected_snapshot);
        assert_snapshot_cells(snapshot, expected_snapshot);
    }

    fn assert_snapshot_header(snapshot: &ScreenSnapshot, expected_snapshot: &ScreenSnapshot) {
        assert_eq!(snapshot.cols, expected_snapshot.cols);
        assert_eq!(snapshot.rows, expected_snapshot.rows);
        assert_eq!(snapshot.alt_screen, expected_snapshot.alt_screen);
        assert_eq!(snapshot.scrollback_rows, expected_snapshot.scrollback_rows);
        assert_eq!(snapshot.scrollback.len(), expected_snapshot.scrollback.len());
        assert_eq!(snapshot.cells.len(), expected_snapshot.cells.len());
    }

    fn assert_snapshot_cursor(snapshot: &ScreenSnapshot, expected_snapshot: &ScreenSnapshot) {
        assert_eq!(snapshot.cursor_col, expected_snapshot.cursor_col);
        assert_eq!(snapshot.cursor_row, expected_snapshot.cursor_row);
        assert!(matches!(snapshot.cursor_style, CursorStyle::Block));
        assert!(matches!(expected_snapshot.cursor_style, CursorStyle::Block));
        assert_eq!(snapshot.cursor_visible, expected_snapshot.cursor_visible);
    }

    fn assert_snapshot_cells(snapshot: &ScreenSnapshot, expected_snapshot: &ScreenSnapshot) {
        let actual_cell = snapshot.cells.first().unwrap();
        let expected_cell = expected_snapshot.cells.first().unwrap();
        assert_eq!(actual_cell.c, expected_cell.c);
        assert_named_color(actual_cell.fg, 256);
        assert_named_color(expected_cell.fg, 256);
        assert_named_color(actual_cell.bg, 257);
        assert_named_color(expected_cell.bg, 257);
        assert_cell_flags(&actual_cell.flags, &expected_cell.flags);
    }

    fn assert_named_color(color: ScreenColor, expected_index: u16) {
        assert!(
            matches!(color, ScreenColor::Named(actual_index) if actual_index == expected_index)
        );
    }

    fn assert_cell_flags(
        actual: &scribe_common::screen::CellFlags,
        expected: &scribe_common::screen::CellFlags,
    ) {
        assert_eq!(actual.bold(), expected.bold());
        assert_eq!(actual.italic(), expected.italic());
        assert_eq!(actual.underline(), expected.underline());
        assert_eq!(actual.strikethrough(), expected.strikethrough());
        assert_eq!(actual.dim(), expected.dim());
        assert_eq!(actual.inverse(), expected.inverse());
        assert_eq!(actual.hidden(), expected.hidden());
        assert_eq!(actual.wide(), expected.wide());
    }
}
