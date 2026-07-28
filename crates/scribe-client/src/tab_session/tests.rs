//! Tab/session strip tests.
//!
//! Covers the selection rules the `new_tab` / `next_tab` / `prev_tab` /
//! `select_tab_N` shortcuts drive, plus the server-side `SessionList` /
//! `SessionCreated` / `SessionExited` mutations, so the shell's tab strip and
//! its attach target can never disagree.

use scribe_common::ids::{SessionId, WorkspaceId};

use super::{TabEntry, TabSessions};

/// Build a strip of `count` tabs in one workspace, returning it with the ids.
fn strip(count: usize) -> (TabSessions, WorkspaceId, Vec<SessionId>) {
    let workspace_id = WorkspaceId::new();
    let ids: Vec<SessionId> = (0..count).map(|_| SessionId::new()).collect();
    let mut tabs = TabSessions::new();
    let entries = ids
        .iter()
        .enumerate()
        .map(|(i, id)| TabEntry::new(*id, workspace_id, format!("shell{i}")))
        .collect();
    tabs.replace_all(entries);
    (tabs, workspace_id, ids)
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab session strip]]
#[test]
fn new_tab_appends_and_focuses() {
    let (mut tabs, workspace_id, ids) = strip(1);
    assert_eq!(tabs.active_session(), Some(ids[0]));

    let created = SessionId::new();
    let added = tabs.insert_active(TabEntry::new(created, workspace_id, "shell".to_owned()));

    assert!(added);
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs.active_session(), Some(created));
    // The new tab is the one the titlebar renders as active.
    let data = tabs.to_tab_data();
    assert_eq!(data.len(), 2);
    assert!(!data[0].is_active);
    assert!(data[1].is_active);
}

/// The server re-announces `SessionCreated` to acknowledge every
/// `AttachSessions`. Treating that echo as a new tab would attach again and
/// loop forever, so a known session must report "not added" and leave the
/// selection exactly where it was.
#[test]
fn attach_acknowledgement_is_not_a_new_tab() {
    let (mut tabs, workspace_id, ids) = strip(2);
    tabs.select(0);

    let echo = TabEntry::new(ids[1], workspace_id, "shell1".to_owned());
    assert!(!tabs.insert_active(echo), "a re-announced session is not a new tab");

    assert_eq!(tabs.len(), 2, "a duplicate SessionCreated must not add a tab");
    assert_eq!(tabs.active_session(), Some(ids[0]), "the selection must not move");
}

#[test]
fn next_and_prev_wrap_around() {
    let (mut tabs, _, ids) = strip(3);
    assert_eq!(tabs.active_session(), Some(ids[0]));

    assert_eq!(tabs.focus_next(), Some(ids[1]));
    assert_eq!(tabs.focus_next(), Some(ids[2]));
    assert_eq!(tabs.focus_next(), Some(ids[0]), "next wraps past the last tab");
    assert_eq!(tabs.focus_prev(), Some(ids[2]), "prev wraps before the first tab");
}

#[test]
fn single_tab_navigation_reports_no_change() {
    let (mut tabs, _, _) = strip(1);
    assert_eq!(tabs.focus_next(), None, "one tab: no attach should be issued");
    assert_eq!(tabs.focus_prev(), None);
    assert_eq!(tabs.select(0), None, "reselecting the active tab is a no-op");
    assert_eq!(tabs.select(7), None, "out-of-range select_tab_N is ignored");
}

#[test]
fn select_jumps_to_index() {
    let (mut tabs, _, ids) = strip(4);
    assert_eq!(tabs.select(2), Some(ids[2]));
    assert_eq!(tabs.select(0), Some(ids[0]));
}

#[test]
fn removing_active_tab_clamps_selection() {
    let (mut tabs, _, ids) = strip(3);
    tabs.select(2);

    // Removing the last (active) tab clamps back onto the new last tab.
    assert_eq!(tabs.remove(ids[2]), Some(ids[1]));
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs.active_session(), Some(ids[1]));

    // Removing an inactive tab ahead of the cursor leaves the selection alone.
    assert_eq!(tabs.remove(ids[0]), None, "inactive removal issues no attach");
    assert_eq!(tabs.active_session(), Some(ids[1]));

    // Removing the sole remaining tab empties the strip.
    assert_eq!(tabs.remove(ids[1]), None);
    assert!(tabs.is_empty());
    assert_eq!(tabs.active_session(), None);
    assert_eq!(tabs.active_workspace(), None);
}

#[test]
fn session_list_rebuild_preserves_active_session() {
    let (mut tabs, workspace_id, ids) = strip(3);
    tabs.select(1);

    // A reconnect re-lists the same sessions in a different order.
    let reordered = vec![
        TabEntry::new(ids[2], workspace_id, "shell2".to_owned()),
        TabEntry::new(ids[1], workspace_id, "shell1".to_owned()),
        TabEntry::new(ids[0], workspace_id, "shell0".to_owned()),
    ];
    assert_eq!(tabs.replace_all(reordered), Some(ids[1]));
    assert_eq!(tabs.active_session(), Some(ids[1]));

    // When the active session is gone the strip falls back to the first tab.
    let survivors = vec![TabEntry::new(ids[0], workspace_id, "shell0".to_owned())];
    assert_eq!(tabs.replace_all(survivors), Some(ids[0]));
}

#[test]
fn new_tab_targets_the_active_workspace() {
    let (tabs, workspace_id, _) = strip(2);
    assert_eq!(tabs.active_workspace(), Some(workspace_id));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab task labels]]
#[test]
fn task_label_outranks_the_title_until_cleared() {
    let (mut tabs, _, ids) = strip(2);
    assert!(tabs.set_title(ids[0], "zsh".to_owned()));

    assert!(tabs.set_task_label(ids[0], Some("Ship the tab labels")));
    assert_eq!(tabs.to_tab_data()[0].title, "Ship the tab labels");
    assert_eq!(tabs.to_tab_data()[1].title, "shell1", "siblings are untouched");
    assert!(!tabs.set_task_label(ids[0], Some("Ship the tab labels")), "identical is no change");

    // A title arriving mid-task is stored but stays behind the label.
    assert!(tabs.set_title(ids[0], "bash".to_owned()));
    assert_eq!(tabs.to_tab_data()[0].title, "Ship the tab labels");

    // A blank label is the provider clearing it, never a blank tab.
    assert!(tabs.set_task_label(ids[0], Some("   ")));
    assert_eq!(tabs.to_tab_data()[0].title, "bash");

    assert!(tabs.set_task_label(ids[0], Some("Second task")));
    assert!(tabs.set_task_label(ids[0], None));
    assert_eq!(tabs.to_tab_data()[0].title, "bash");
    assert!(!tabs.set_task_label(ids[0], None), "already cleared is no change");
    assert!(!tabs.set_task_label(SessionId::new(), Some("ghost")));
}

#[test]
fn retitle_updates_only_on_change() {
    let (mut tabs, _, ids) = strip(2);
    assert!(tabs.set_title(ids[0], "claude".to_owned()));
    assert!(!tabs.set_title(ids[0], "claude".to_owned()), "identical title is not a change");
    assert!(!tabs.set_title(SessionId::new(), "ghost".to_owned()));
    assert_eq!(tabs.to_tab_data()[0].title, "claude");
}
