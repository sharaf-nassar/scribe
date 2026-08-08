//! Terminal-chrome metadata store tests.
//!
//! Covers the merge rules the IPC reader drives from `CwdChanged`,
//! `GitBranch`, `SessionContextChanged`, `EnvStatus`, `WorkspaceNamed` and the
//! authoritative `SessionList`, so the status bar never renders a stale or
//! blank segment for the attached pane.

use std::path::{Path, PathBuf};

use scribe_common::{
    ids::{SessionId, WorkspaceId},
    protocol::{EnvStatusState, SessionContext, SessionInfo, WorkspaceListEntry},
};

use super::ChromeMetadata;

/// A `SessionInfo` carrying only the chrome fields the store reads.
fn info(session_id: SessionId, workspace_id: WorkspaceId) -> SessionInfo {
    SessionInfo {
        session_id,
        workspace_id,
        shell_name: "zsh".to_owned(),
        title: None,
        context: None,
        task_label: None,
        codex_task_label: None,
        cwd: None,
        git_branch: None,
        ai_state: None,
        ai_provider_hint: None,
        prompt_state: None,
    }
}

/// A named workspace list entry.
fn workspace(workspace_id: WorkspaceId, name: Option<&str>) -> WorkspaceListEntry {
    WorkspaceListEntry {
        workspace_id,
        name: name.map(ToOwned::to_owned),
        accent_color: "#a78bfa".to_owned(),
        split_direction: None,
        project_root: None,
    }
}

// @lat: [[test#GPUI Client Headless Suites#GPUI terminal chrome metadata]]
#[test]
fn per_session_fields_update_independently() {
    let session = SessionId::new();
    let other = SessionId::new();
    let mut chrome = ChromeMetadata::new();
    assert!(chrome.session(session).is_none());

    chrome.set_cwd(session, PathBuf::from("/home/tester/work/scribe"));
    chrome.set_git_branch(session, Some("main".to_owned()));
    chrome.set_env_status(session, EnvStatusState::Degraded { reason: "keystore".to_owned() });
    chrome.set_cwd(other, PathBuf::from("/tmp"));

    let entry = chrome.session(session).expect("session recorded");
    assert_eq!(entry.cwd.as_deref(), Some(Path::new("/home/tester/work/scribe")));
    assert_eq!(entry.git_branch.as_deref(), Some("main"));
    assert!(matches!(entry.env_status, Some(EnvStatusState::Degraded { .. })));
    // A sibling pane's update never leaks onto this one.
    assert_eq!(chrome.session(other).and_then(|c| c.git_branch.as_deref()), None);

    // Leaving a repository clears the branch rather than keeping the old one.
    chrome.set_git_branch(session, None);
    assert_eq!(chrome.session(session).and_then(|c| c.git_branch.as_deref()), None);

    chrome.forget_session(session);
    assert!(chrome.session(session).is_none());
    assert!(chrome.session(other).is_some());
}

// @lat: [[test#GPUI Client Headless Suites#GPUI terminal chrome labels]]
#[test]
fn context_labels_gate_on_the_remote_flag() {
    let session = SessionId::new();
    let mut chrome = ChromeMetadata::new();

    chrome.set_context(
        session,
        SessionContext {
            remote: false,
            host: Some("build-box".to_owned()),
            tmux_session: Some("dev".to_owned()),
        },
    );
    let local = chrome.session(session).expect("context recorded");
    // A local shell keeps this machine's own host label, but still shows tmux.
    assert_eq!(local.host_label(), None);
    assert_eq!(local.tmux_label(), Some("dev"));

    chrome.set_context(
        session,
        SessionContext { remote: true, host: Some("build-box".to_owned()), tmux_session: None },
    );
    let remote = chrome.session(session).expect("context recorded");
    assert_eq!(remote.host_label(), Some("build-box"));
    assert_eq!(remote.tmux_label(), None);

    chrome.set_context(
        session,
        SessionContext { remote: true, host: Some(String::new()), tmux_session: None },
    );
    // An empty host is not a label; fall back to the local one.
    assert_eq!(chrome.session(session).and_then(super::SessionChrome::host_label), None);
}

// @lat: [[test#GPUI Client Headless Suites#GPUI workspace naming and reseed]]
#[test]
fn session_list_seeds_and_prunes() {
    let workspace_id = WorkspaceId::new();
    let live = SessionId::new();
    let gone = SessionId::new();
    let mut chrome = ChromeMetadata::new();

    chrome.set_cwd(gone, PathBuf::from("/tmp"));
    chrome.set_git_branch(live, Some("feature".to_owned()));

    let mut listed = info(live, workspace_id);
    listed.cwd = Some(PathBuf::from("/srv/app"));
    listed.context =
        Some(SessionContext { remote: true, host: Some("laptop".to_owned()), tmux_session: None });
    chrome.seed_from_session_list(&[listed], &[workspace(workspace_id, Some("app"))]);

    let entry = chrome.session(live).expect("listed session seeded");
    assert_eq!(entry.cwd.as_deref(), Some(Path::new("/srv/app")));
    // The list omits the branch, so the live value survives the reseed.
    assert_eq!(entry.git_branch.as_deref(), Some("feature"));
    assert_eq!(entry.host_label(), Some("laptop"));
    // A session missing from the authoritative list is dropped entirely.
    assert!(chrome.session(gone).is_none());
    assert_eq!(chrome.workspace_name(workspace_id), Some("app"));

    // An empty rename clears the segment instead of blanking it.
    chrome.name_workspace(workspace_id, "  ".to_owned());
    assert_eq!(chrome.workspace_name(workspace_id), None);

    // A reconnect snapshot clears a listed unnamed workspace and prunes names
    // for workspaces omitted from the authoritative list.
    let omitted = WorkspaceId::new();
    chrome.name_workspace(workspace_id, "stale".to_owned());
    chrome.name_workspace(omitted, "gone".to_owned());
    chrome.seed_from_session_list(&[info(live, workspace_id)], &[workspace(workspace_id, None)]);
    assert_eq!(chrome.workspace_name(workspace_id), None);
    assert_eq!(chrome.workspace_name(omitted), None);
    // Workspace-name replacement does not discard independent session chrome.
    let retained = chrome.session(live).expect("session metadata retained");
    assert_eq!(retained.cwd.as_deref(), Some(Path::new("/srv/app")));
    assert_eq!(retained.git_branch.as_deref(), Some("feature"));
}
