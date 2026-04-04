use std::collections::HashSet;
use std::os::fd::RawFd;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use scribe_common::ai_state::AiProcessState;
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::{ServerMessage, SessionContext, TerminalSize};

use crate::ipc_server::{
    AttachSessionData, ClientWriter, LiveSessionRegistry, SharedWriter, detect_git_branch,
    resize_term, send_message, set_pty_winsize,
};
use crate::session_manager::snapshot_term;
use crate::workspace_manager::WorkspaceManager;

#[derive(Clone)]
struct AttachEntry {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    shell_name: String,
    client_writer: ClientWriter,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    pty_raw_fd: RawFd,
    target_dims: Option<TerminalSize>,
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
            term: data.term,
            pty_raw_fd: data.pty_raw_fd,
            target_dims: data.target_dims,
            title: data.title,
            codex_task_label: data.codex_task_label,
            cwd: data.cwd,
            context: data.context,
            ai_state: data.ai_state,
        }
    }
}

pub async fn attach_sessions(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
) -> HashSet<SessionId> {
    let entries = prepare_attach_entries(session_ids, dimensions, live_sessions).await;
    attach_prepared_entries(&entries, writer, live_sessions, workspace_manager).await
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
) -> HashSet<SessionId> {
    let mut attached_ids = HashSet::with_capacity(entries.len());

    for entry in entries {
        attach_one_session(entry, writer, live_sessions, workspace_manager).await;
        attached_ids.insert(entry.session_id);
    }

    attached_ids
}

async fn attach_one_session(
    entry: &AttachEntry,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) {
    send_attach_replay(entry, writer, live_sessions, workspace_manager).await;
    install_client_writer(entry, writer).await;
}

async fn send_attach_replay(
    entry: &AttachEntry,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) {
    let session_id = entry.session_id;

    if let Some(size) = entry.target_dims {
        if size.has_grid() {
            resize_term(&entry.term, size.cols, size.rows).await;
            if let Err(error) = set_pty_winsize(entry.pty_raw_fd, size) {
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

async fn install_client_writer(entry: &AttachEntry, writer: &SharedWriter) {
    let mut client_writer = entry.client_writer.lock().await;
    if client_writer.is_some() {
        warn!(
            %entry.session_id,
            "AttachSessions: overwriting existing client writer - previous client may still be connected"
        );
    }
    *client_writer = Some(Arc::clone(writer));
    drop(client_writer);

    info!(session_id = %entry.session_id, "session attached to new client");
}

async fn send_workspace_info(
    writer: &SharedWriter,
    workspace_id: WorkspaceId,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) {
    let wm = workspace_manager.read().await;
    if let Some((name, accent_color, split_direction)) = wm.workspace_info(workspace_id) {
        send_message(
            writer,
            &ServerMessage::WorkspaceInfo { workspace_id, name, accent_color, split_direction },
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
            term: Arc::new(Mutex::new(make_term(session_id))),
            pty_raw_fd: -1,
            target_dims: None,
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

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "replay assertion intentionally checks payload fields explicitly"
    )]
    async fn send_attach_replay_emits_expected_sequence_and_leaves_writer_unset() {
        let live_sessions = crate::ipc_server::new_live_session_registry();
        let workspace_manager = Arc::new(RwLock::new(WorkspaceManager::new(vec![])));
        let workspace_id = workspace_manager.write().await.create_workspace();
        let session_id = SessionId::new();
        let entry = sample_entry(session_id, workspace_id);
        let expected_cwd = entry.cwd.clone().unwrap();
        let expected_context = entry.context.clone().unwrap();
        let expected_ai_state = entry.ai_state.clone().unwrap();
        let expected_snapshot = {
            let term = entry.term.lock().await;
            snapshot_term(&term)
        };

        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer: SharedWriter = Arc::new(Mutex::new(server_write));

        send_attach_replay(&entry, &writer, &live_sessions, &workspace_manager).await;

        assert!(entry.client_writer.lock().await.is_none());

        let messages = [
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
        ];

        assert!(matches!(
            &messages[0],
            ServerMessage::SessionCreated {
                session_id: actual_session_id,
                workspace_id: actual_workspace_id,
                shell_name,
            } if actual_session_id == &session_id
                && actual_workspace_id == &workspace_id
                && shell_name == "zsh"
        ));
        assert!(matches!(
            &messages[1],
            ServerMessage::TitleChanged { session_id: actual_session_id, title }
                if actual_session_id == &session_id && title == "sample"
        ));
        assert!(matches!(
            &messages[2],
            ServerMessage::CodexTaskLabelChanged { session_id: actual_session_id, task_label }
                if actual_session_id == &session_id && task_label == "task"
        ));
        assert!(matches!(
            &messages[3],
            ServerMessage::CwdChanged { session_id: actual_session_id, cwd }
                if actual_session_id == &session_id && cwd == &expected_cwd
        ));
        assert!(matches!(
            &messages[4],
            ServerMessage::GitBranch { session_id: actual_session_id, .. }
                if actual_session_id == &session_id
        ));
        assert!(matches!(
            &messages[5],
            ServerMessage::SessionContextChanged { session_id: actual_session_id, context }
                if actual_session_id == &session_id && context == &expected_context
        ));
        assert!(matches!(
            &messages[6],
            ServerMessage::AiStateChanged { session_id: actual_session_id, ai_state }
                if actual_session_id == &session_id && ai_state == &expected_ai_state
        ));
        assert!(matches!(
            &messages[7],
            ServerMessage::WorkspaceInfo { workspace_id: actual_workspace_id, .. }
                if actual_workspace_id == &workspace_id
        ));
        match &messages[8] {
            ServerMessage::ScreenSnapshot { session_id: actual_session_id, snapshot } => {
                assert_eq!(*actual_session_id, session_id);
                assert_eq!(snapshot.cols, expected_snapshot.cols);
                assert_eq!(snapshot.rows, expected_snapshot.rows);
                assert_eq!(snapshot.cursor_col, expected_snapshot.cursor_col);
                assert_eq!(snapshot.cursor_row, expected_snapshot.cursor_row);
                assert!(matches!(snapshot.cursor_style, CursorStyle::Block));
                assert!(matches!(expected_snapshot.cursor_style, CursorStyle::Block));
                assert_eq!(snapshot.cursor_visible, expected_snapshot.cursor_visible);
                assert_eq!(snapshot.alt_screen, expected_snapshot.alt_screen);
                assert_eq!(snapshot.scrollback_rows, expected_snapshot.scrollback_rows);
                assert_eq!(snapshot.scrollback.len(), expected_snapshot.scrollback.len());
                assert_eq!(snapshot.cells.len(), expected_snapshot.cells.len());

                let actual_cell = snapshot.cells.first().unwrap();
                let expected_cell = expected_snapshot.cells.first().unwrap();
                assert_eq!(actual_cell.c, expected_cell.c);
                assert!(matches!(actual_cell.fg, ScreenColor::Named(256)));
                assert!(matches!(expected_cell.fg, ScreenColor::Named(256)));
                assert!(matches!(actual_cell.bg, ScreenColor::Named(257)));
                assert!(matches!(expected_cell.bg, ScreenColor::Named(257)));
                assert_eq!(actual_cell.flags.bold, expected_cell.flags.bold);
                assert_eq!(actual_cell.flags.italic, expected_cell.flags.italic);
                assert_eq!(actual_cell.flags.underline, expected_cell.flags.underline);
                assert_eq!(actual_cell.flags.strikethrough, expected_cell.flags.strikethrough);
                assert_eq!(actual_cell.flags.dim, expected_cell.flags.dim);
                assert_eq!(actual_cell.flags.inverse, expected_cell.flags.inverse);
                assert_eq!(actual_cell.flags.hidden, expected_cell.flags.hidden);
                assert_eq!(actual_cell.flags.wide, expected_cell.flags.wide);
            }
            other => panic!("expected ScreenSnapshot, got {other:?}"),
        }
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

        let attached =
            attach_prepared_entries(&entries, &writer, &live_sessions, &workspace_manager).await;

        assert_eq!(attached.len(), 2);
        assert!(attached.contains(&entries[0].session_id));
        assert!(attached.contains(&entries[1].session_id));
    }
}
