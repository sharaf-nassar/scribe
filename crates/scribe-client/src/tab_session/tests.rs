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

/// Two-workspace strip `[a1, a2 | b1, b2]`, selection on `b1` (index 2).
fn two_workspace_strip() -> (TabSessions, (WorkspaceId, WorkspaceId), Vec<SessionId>) {
    let ws_a = WorkspaceId::new();
    let ws_b = WorkspaceId::new();
    let ids: Vec<SessionId> = (0..4).map(|_| SessionId::new()).collect();
    let mut tabs = TabSessions::new();
    tabs.replace_all(vec![
        TabEntry::new(ids[0], ws_a, "a1".to_owned()),
        TabEntry::new(ids[1], ws_a, "a2".to_owned()),
        TabEntry::new(ids[2], ws_b, "b1".to_owned()),
        TabEntry::new(ids[3], ws_b, "b2".to_owned()),
    ]);
    tabs.select(2);
    (tabs, (ws_a, ws_b), ids)
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab session strip#Exit refocus stays inside the workspace]]
#[test]
fn exit_refocus_stays_inside_the_workspace() {
    // Removing b1 must refocus b2 (same workspace), not a2 (strip neighbour).
    let (mut tabs, (_, ws_b), ids) = two_workspace_strip();
    assert_eq!(tabs.remove(ids[2]), Some(ids[3]));
    assert_eq!(tabs.active_workspace(), Some(ws_b));
}

/// The predecessor wins when the removed tab was its workspace's last in
/// strip order.
#[test]
fn exit_refocus_prefers_the_workspace_predecessor() {
    let (mut tabs, (_, ws_b), ids) = two_workspace_strip();
    tabs.select(3);
    assert_eq!(tabs.remove(ids[3]), Some(ids[2]));
    assert_eq!(tabs.active_workspace(), Some(ws_b));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab session strip#Last tab of a workspace falls back across the strip]]
#[test]
fn last_tab_of_a_workspace_falls_back_across_the_strip() {
    let (mut tabs, (ws_a, _), ids) = two_workspace_strip();
    assert_eq!(tabs.remove(ids[2]), Some(ids[3]));
    // Removing the workspace's last tab has nowhere same-workspace to go, so
    // the strip-global clamp applies and the region collapse takes over.
    assert_eq!(tabs.remove(ids[3]), Some(ids[1]));
    assert_eq!(tabs.active_workspace(), Some(ws_a));
    assert_eq!(tabs.workspace_of(ids[1]), Some(ws_a), "survivors keep their workspace");
    assert_eq!(tabs.workspace_of(ids[3]), None, "removed sessions are gone");
    assert_eq!(tabs.workspace_of(ids[0]), Some(ws_a));
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

#[test]
fn workspace_tabs_form_one_run_without_losing_the_active_session() {
    let left = WorkspaceId::new();
    let right = WorkspaceId::new();
    let left_first = SessionId::new();
    let right_first = SessionId::new();
    let left_new = SessionId::new();
    let mut tabs = TabSessions::new();
    tabs.replace_all(vec![
        TabEntry::new(left_first, left, "left-1".to_owned()),
        TabEntry::new(right_first, right, "right-1".to_owned()),
        TabEntry::new(left_new, left, "left-2".to_owned()),
    ]);
    tabs.select(2);

    tabs.group_by_workspace();

    assert_eq!(
        tabs.tabs().iter().map(|tab| tab.session_id).collect::<Vec<_>>(),
        [left_first, left_new, right_first]
    );
    assert_eq!(tabs.active_session(), Some(left_new));
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
