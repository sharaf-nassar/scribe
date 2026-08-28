//! Tab/session strip tests.
//!
//! Covers the selection rules the `new_tab` / `next_tab` / `prev_tab` /
//! `select_tab_N` shortcuts drive, plus the server-side `SessionList` /
//! `SessionCreated` / `SessionExited` mutations, so the shell's tab strip and
//! its attach target can never disagree.
//!
//! Every selection rule here is scoped to one region. That used to be a set of
//! filters layered over a flat window-global list; it is now the shape of the
//! data, so most of these tests are asserting that the partition holds rather
//! than that a filter was remembered.

use scribe_common::ids::{SessionId, WorkspaceId};

use super::{TabEntry, TabSessions, WorkspaceTabs};

/// Build a strip of `count` tabs in one region, returning it with the ids.
fn strip(count: usize) -> (TabSessions, WorkspaceId, Vec<SessionId>) {
    let workspace_id = WorkspaceId::new();
    let ids: Vec<SessionId> = (0..count).map(|_| SessionId::new()).collect();
    let mut tabs = TabSessions::new();
    let entries = ids
        .iter()
        .enumerate()
        .map(|(i, id)| TabEntry::new(*id, workspace_id, format!("shell{i}")))
        .collect();
    tabs.reconcile(entries, None);
    (tabs, workspace_id, ids)
}

/// Every session in strip order, which is region order then tab order.
fn order(tabs: &TabSessions) -> Vec<SessionId> {
    tabs.entries().map(|tab| tab.session_id).collect()
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab session strip]]
#[test]
fn new_tab_appends_and_focuses() {
    let (mut tabs, workspace_id, ids) = strip(1);
    assert_eq!(tabs.active_session_in(workspace_id), Some(ids[0]));

    let created = SessionId::new();
    let added = tabs.insert_active(TabEntry::new(created, workspace_id, "shell".to_owned()));

    assert!(added);
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs.active_session_in(workspace_id), Some(created));
    // The new tab is the one the titlebar renders as active.
    let data = tabs.to_tab_data();
    assert_eq!(data.len(), 2);
    assert!(!data[0].is_active);
    assert!(data[1].is_active);
}

/// A pane split creates a live session but never a second strip tab. Focusing
/// that pane attaches it and the server acknowledges every `AttachSessions`
/// with a fresh `SessionCreated`, and a later `SessionList` names it again, so
/// both echoes have to leave the strip alone.
#[test]
fn pane_session_stays_out_of_the_tab_strip() {
    let (mut tabs, workspace_id, ids) = strip(1);
    let pane = SessionId::new();
    tabs.insert_pane(pane, workspace_id);

    assert_eq!(tabs.len(), 1, "a split leaves the tab count unchanged");
    assert_eq!(tabs.live_session_ids().len(), 2, "the pane remains live");
    assert_eq!(tabs.workspace_of(pane), Some(workspace_id));
    assert_eq!(tabs.region_of_tab(pane), None, "a pane session holds no tab of its own");

    let echo = TabEntry::new(pane, workspace_id, "split".to_owned());
    assert!(!tabs.insert_active(echo), "a focused pane's attach echo is not a new tab");
    assert_eq!(tabs.len(), 1);

    tabs.reconcile(
        vec![
            TabEntry::new(ids[0], workspace_id, "shell0".to_owned()),
            TabEntry::new(pane, workspace_id, "split".to_owned()),
        ],
        Some(pane),
    );
    assert_eq!(tabs.len(), 1, "a reconnect must keep split panes out of the strip");
}

/// The server re-announces `SessionCreated` to acknowledge every
/// `AttachSessions`. Treating that echo as a new tab would attach again and
/// loop forever, so a known session must report "not added" and leave the
/// selection exactly where it was.
#[test]
fn attach_acknowledgement_is_not_a_new_tab() {
    let (mut tabs, workspace_id, ids) = strip(2);
    tabs.select(workspace_id, 0);

    let echo = TabEntry::new(ids[1], workspace_id, "shell1".to_owned());
    assert!(!tabs.insert_active(echo), "a re-announced session is not a new tab");

    assert_eq!(tabs.len(), 2, "a duplicate SessionCreated must not add a tab");
    assert_eq!(tabs.active_session_in(workspace_id), Some(ids[0]), "the selection must not move");
}

#[test]
fn next_and_prev_wrap_around() {
    let (mut tabs, ws, ids) = strip(3);
    assert_eq!(tabs.active_session_in(ws), Some(ids[0]));

    assert_eq!(tabs.focus_next(ws), Some(ids[1]));
    assert_eq!(tabs.focus_next(ws), Some(ids[2]));
    assert_eq!(tabs.focus_next(ws), Some(ids[0]), "next wraps past the last tab");
    assert_eq!(tabs.focus_prev(ws), Some(ids[2]), "prev wraps before the first tab");
}

#[test]
fn single_tab_navigation_reports_no_change() {
    let (mut tabs, ws, _) = strip(1);
    assert_eq!(tabs.focus_next(ws), None, "one tab: no attach should be issued");
    assert_eq!(tabs.focus_prev(ws), None);
    assert_eq!(tabs.select(ws, 0), None, "reselecting the active tab is a no-op");
    assert_eq!(tabs.select(ws, 7), None, "out-of-range select_tab_N is ignored");
    assert_eq!(tabs.focus_next(WorkspaceId::new()), None, "an unknown region has no tabs");
}

#[test]
fn select_jumps_to_index() {
    let (mut tabs, ws, ids) = strip(4);
    assert_eq!(tabs.select(ws, 2), Some(ids[2]));
    assert_eq!(tabs.select(ws, 0), Some(ids[0]));
}

#[test]
fn removing_active_tab_clamps_selection() {
    let (mut tabs, ws, ids) = strip(3);
    tabs.select(ws, 2);

    // Removing the last (active) tab clamps back onto the new last tab.
    assert_eq!(tabs.remove(ids[2]), Some(ids[1]));
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs.active_session_in(ws), Some(ids[1]));

    // Removing an inactive tab ahead of the cursor leaves the selection alone.
    assert_eq!(tabs.remove(ids[0]), None, "inactive removal issues no attach");
    assert_eq!(tabs.active_session_in(ws), Some(ids[1]));

    // Removing the sole remaining tab empties the strip.
    assert_eq!(tabs.remove(ids[1]), None);
    assert!(tabs.is_empty());
    assert_eq!(tabs.active_session_in(ws), None);
    assert!(tabs.regions().is_empty(), "an emptied region is dropped, not kept dangling");
}

/// Two-region strip `[a1, a2 | b1, b2]`, with region b showing `b1`.
fn two_workspace_strip() -> (TabSessions, (WorkspaceId, WorkspaceId), Vec<SessionId>) {
    let ws_a = WorkspaceId::new();
    let ws_b = WorkspaceId::new();
    let ids: Vec<SessionId> = (0..4).map(|_| SessionId::new()).collect();
    let mut tabs = TabSessions::new();
    tabs.reconcile(
        vec![
            TabEntry::new(ids[0], ws_a, "a1".to_owned()),
            TabEntry::new(ids[1], ws_a, "a2".to_owned()),
            TabEntry::new(ids[2], ws_b, "b1".to_owned()),
            TabEntry::new(ids[3], ws_b, "b2".to_owned()),
        ],
        None,
    );
    tabs.select(ws_b, 0);
    (tabs, (ws_a, ws_b), ids)
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab session strip#Selection never leaves its region]]
#[test]
fn selection_never_leaves_its_region() {
    let (mut tabs, (ws_a, ws_b), ids) = two_workspace_strip();
    let before_a = tabs.active_session_in(ws_a);

    // Every way a tab can be chosen — a digit, a click resolved to a session,
    // and a next/prev walk off the end — targets one region and leaves the
    // other's shown tab exactly where it was.
    assert_eq!(tabs.select(ws_b, 1), Some(ids[3]));
    assert_eq!(tabs.active_session_in(ws_a), before_a);

    assert_eq!(tabs.show(ids[2]), Some(ids[2]), "a click names a session, not a strip position");
    assert_eq!(tabs.active_session_in(ws_a), before_a);

    assert_eq!(tabs.focus_next(ws_b), Some(ids[3]));
    assert_eq!(tabs.focus_next(ws_b), Some(ids[2]), "the walk wraps inside region b");
    assert_eq!(tabs.active_session_in(ws_a), before_a, "region a never moved");

    // Selecting into region a moves only region a.
    assert_eq!(tabs.show(ids[1]), Some(ids[1]));
    assert_eq!(tabs.active_session_in(ws_a), Some(ids[1]));
    assert_eq!(tabs.active_session_in(ws_b), Some(ids[2]));
    assert_eq!(tabs.show(ids[1]), None, "re-showing the shown tab issues no attach");
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab session strip#Exit refocus stays inside the workspace]]
#[test]
fn exit_refocus_stays_inside_the_workspace() {
    // Removing b1 must refocus b2 (same region), not a2 (strip neighbour).
    let (mut tabs, (ws_a, ws_b), ids) = two_workspace_strip();
    assert_eq!(tabs.remove(ids[2]), Some(ids[3]));
    assert_eq!(tabs.active_session_in(ws_b), Some(ids[3]));
    assert_eq!(tabs.active_session_in(ws_a), Some(ids[0]), "region a is untouched");
}

/// The predecessor wins when the removed tab was its region's last.
#[test]
fn exit_refocus_prefers_the_workspace_predecessor() {
    let (mut tabs, (_, ws_b), ids) = two_workspace_strip();
    tabs.select(ws_b, 1);
    assert_eq!(tabs.remove(ids[3]), Some(ids[2]));
    assert_eq!(tabs.active_session_in(ws_b), Some(ids[2]));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab session strip#Emptying a region drops it]]
#[test]
fn emptying_a_region_drops_it_without_touching_the_neighbour() {
    let (mut tabs, (ws_a, ws_b), ids) = two_workspace_strip();
    assert_eq!(tabs.remove(ids[2]), Some(ids[3]));
    // The region's last tab has nowhere same-region to go, so the region is
    // dropped and nothing is refocused. With a flat strip this fell back to the
    // strip-adjacent tab, which named a session in region a and dragged it out
    // of its own region when the reconcile pass adopted the answer.
    assert_eq!(tabs.remove(ids[3]), None);
    assert_eq!(tabs.regions().len(), 1);
    assert_eq!(tabs.active_session_in(ws_a), Some(ids[0]), "region a keeps its own shown tab");
    assert_eq!(tabs.active_session_in(ws_b), None);
    assert_eq!(tabs.workspace_of(ids[1]), Some(ws_a), "survivors keep their workspace");
    assert_eq!(tabs.workspace_of(ids[3]), None, "removed sessions are gone");
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab session strip#Digit select is workspace-scoped]]
#[test]
fn digit_select_is_workspace_scoped() {
    // Digit 2 aimed at region b must land on b2 — not a2, the strip's second
    // tab, which is what a window-global index would have named.
    let (mut tabs, (ws_a, ws_b), ids) = two_workspace_strip();
    assert_eq!(tabs.select(ws_b, 1), Some(ids[3]));
    assert_eq!(tabs.select(ws_b, 0), Some(ids[2]), "digit 1 targets b1");
    assert_eq!(tabs.select(ws_b, 2), None, "digits past the region are ignored");
    assert_eq!(tabs.select(ws_b, 0), None, "reselecting the shown tab is a no-op");
    assert_eq!(tabs.select(ws_a, 1), Some(ids[1]), "the same digit in region a targets a2");
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab session strip#Reorder and addresses stay region-local]]
#[test]
fn reorder_and_addresses_stay_region_local() {
    let (mut tabs, (ws_a, ws_b), ids) = two_workspace_strip();

    // Region-local positions: swapping b's two tabs must not disturb a's run,
    // and the shown tab travels with its slot rather than with its index.
    assert!(tabs.reorder(ws_b, 0, 1));
    assert_eq!(order(&tabs), [ids[0], ids[1], ids[3], ids[2]]);
    assert_eq!(tabs.active_session_in(ws_b), Some(ids[2]), "b still shows the tab it showed");
    assert!(!tabs.reorder(ws_b, 0, 0), "an unchanged move is not a reorder");
    assert!(!tabs.reorder(ws_b, 0, 5), "an out-of-region position is refused, not clamped");
    assert!(!tabs.reorder(WorkspaceId::new(), 0, 1), "an unknown region has nothing to move");

    // The addresses the bars are clicked through pair 1:1 with the render
    // model, so a row position always resolves to the tab that drew there.
    let data = tabs.to_tab_data();
    let slots: Vec<_> = tabs.addresses().collect();
    assert_eq!(data.len(), slots.len());
    assert_eq!(slots.iter().map(|slot| slot.session_id).collect::<Vec<_>>(), order(&tabs));
    assert_eq!(slots[0].workspace_id, ws_a);
    assert_eq!(slots[0].index, 0);
    assert_eq!(slots[2].workspace_id, ws_b);
    assert_eq!(slots[2].index, 0, "the third strip tab is region b's first");
    // Each region underlines its own shown tab, not one tab for the window.
    assert_eq!(data.iter().filter(|tab| tab.is_active).count(), 2);
}

#[test]
fn session_list_rebuild_preserves_order_and_active_session() {
    let (mut tabs, workspace_id, ids) = strip(3);
    tabs.select(workspace_id, 1);

    // A reconnect re-lists the same sessions in a different order. The list is
    // authoritative for what exists, never for what order the user put them in,
    // so the strip keeps its own — otherwise every drag-reorder would be undone
    // by the next list, and the server's HashMap-grouped order would win.
    let reordered = vec![
        TabEntry::new(ids[2], workspace_id, "shell2".to_owned()),
        TabEntry::new(ids[1], workspace_id, "shell1".to_owned()),
        TabEntry::new(ids[0], workspace_id, "shell0".to_owned()),
    ];
    assert_eq!(tabs.reconcile(reordered, Some(ids[1])), Some(ids[1]));
    assert_eq!(order(&tabs), ids);
    assert_eq!(tabs.active_session_in(workspace_id), Some(ids[1]));

    // A session the list no longer names is dropped; a new one is appended.
    let fresh = SessionId::new();
    let next = vec![
        TabEntry::new(fresh, workspace_id, "fresh".to_owned()),
        TabEntry::new(ids[1], workspace_id, "shell1".to_owned()),
        TabEntry::new(ids[0], workspace_id, "shell0".to_owned()),
    ];
    assert_eq!(tabs.reconcile(next, Some(ids[1])), Some(ids[1]));
    assert_eq!(order(&tabs), [ids[0], ids[1], fresh]);
    assert_eq!(tabs.active_session_in(workspace_id), Some(ids[1]));

    // When the attached session is gone the strip falls back to the first
    // region's shown tab.
    let survivors = vec![TabEntry::new(ids[0], workspace_id, "shell0".to_owned())];
    assert_eq!(tabs.reconcile(survivors, Some(ids[1])), Some(ids[0]));
}

/// A `SessionList` that re-files a session under another workspace is the
/// server correcting the strip, so the tab moves to that region rather than
/// keeping a stale filing that a later `new_tab` would inherit.
#[test]
fn session_list_refiles_a_moved_session() {
    let (mut tabs, (ws_a, ws_b), ids) = two_workspace_strip();
    let moved = vec![
        TabEntry::new(ids[0], ws_a, "a1".to_owned()),
        TabEntry::new(ids[1], ws_b, "a2".to_owned()),
        TabEntry::new(ids[2], ws_b, "b1".to_owned()),
        TabEntry::new(ids[3], ws_b, "b2".to_owned()),
    ];
    tabs.reconcile(moved, Some(ids[2]));
    assert_eq!(tabs.workspace_of(ids[1]), Some(ws_b));
    assert_eq!(tabs.region(ws_a).map(WorkspaceTabs::len), Some(1));
    assert_eq!(tabs.region(ws_b).map(WorkspaceTabs::len), Some(3));
    assert_eq!(tabs.active_session_in(ws_b), Some(ids[2]), "region b keeps its shown tab");
}

/// A source region with an active middle tab and a one-tab target.
fn active_middle_source_strip() -> (TabSessions, (WorkspaceId, WorkspaceId), Vec<SessionId>) {
    let source = WorkspaceId::new();
    let target = WorkspaceId::new();
    let ids: Vec<SessionId> = (0..4).map(|_| SessionId::new()).collect();
    let mut tabs = TabSessions::new();
    tabs.reconcile(
        vec![
            TabEntry::new(ids[0], source, "source-0".to_owned()),
            TabEntry::new(ids[1], source, "source-1".to_owned()),
            TabEntry::new(ids[2], source, "source-2".to_owned()),
            TabEntry::new(ids[3], target, "target-0".to_owned()),
        ],
        None,
    );
    tabs.select(source, 1);
    (tabs, (source, target), ids)
}

/// A workspace-split seed is re-filed optimistically when its pane reaches the
/// target region. Its source keeps the successor at the departed tab's slot.
#[test]
fn optimistic_refile_of_active_middle_tab_keeps_source_successor() {
    let (mut tabs, (source, target), ids) = active_middle_source_strip();

    assert!(tabs.set_workspace(ids[1], target));
    assert_eq!(tabs.active_session_in(source), Some(ids[2]));
    assert_eq!(tabs.active_session_in(target), Some(ids[1]), "the target selects its moved seed");
}

/// The authoritative server re-file follows the same source departure seam as
/// the optimistic move, rather than resetting the source selection to tab zero.
#[test]
fn session_list_refile_of_active_middle_tab_keeps_source_successor() {
    let (mut tabs, (source, target), ids) = active_middle_source_strip();

    tabs.reconcile(
        vec![
            TabEntry::new(ids[0], source, "source-0".to_owned()),
            TabEntry::new(ids[2], source, "source-2".to_owned()),
            TabEntry::new(ids[3], target, "target-0".to_owned()),
            TabEntry::new(ids[1], target, "source-1".to_owned()),
        ],
        Some(ids[0]),
    );

    assert_eq!(tabs.active_session_in(source), Some(ids[2]));
    assert_eq!(tabs.active_session_in(target), Some(ids[3]), "the target keeps its shown tab");
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab session strip#Region runs are structural]]
#[test]
fn a_tab_opened_in_an_earlier_region_joins_that_region_run() {
    let left = WorkspaceId::new();
    let right = WorkspaceId::new();
    let left_first = SessionId::new();
    let right_first = SessionId::new();
    let left_new = SessionId::new();
    let mut tabs = TabSessions::new();
    tabs.reconcile(
        vec![
            TabEntry::new(left_first, left, "left-1".to_owned()),
            TabEntry::new(right_first, right, "right-1".to_owned()),
        ],
        None,
    );

    // Going back to the left region and opening a tab there used to append to a
    // window-global list, producing `left, right, left` — a run the titlebar
    // could not anchor without overlapping two groups at one region edge, so a
    // regrouping sort had to run before every paint. The tab now joins its own
    // region's list and the run is contiguous with no sort at all.
    assert!(tabs.insert_active(TabEntry::new(left_new, left, "left-2".to_owned())));

    assert_eq!(order(&tabs), [left_first, left_new, right_first]);
    assert_eq!(tabs.active_session_in(left), Some(left_new));
    assert_eq!(tabs.active_session_in(right), Some(right_first), "the right region is untouched");
}

/// A reconnect adoption restores both the region order and each region's tab
/// order from the tree the window last reported.
#[test]
fn order_by_restores_regions_and_their_tabs() {
    let (mut tabs, (ws_a, ws_b), ids) = two_workspace_strip();
    tabs.order_by(&[ids[3], ids[2], ids[1], ids[0]]);

    assert_eq!(order(&tabs), [ids[3], ids[2], ids[1], ids[0]]);
    assert_eq!(tabs.regions()[0].workspace_id, ws_b, "region b now sits first");
    assert_eq!(tabs.regions()[1].workspace_id, ws_a);
    assert_eq!(tabs.active_session_in(ws_b), Some(ids[2]), "each region keeps its shown tab");
    assert_eq!(tabs.active_session_in(ws_a), Some(ids[0]));
}

/// The client learns of a cross-region move before the server confirms it: a
/// pane in a freshly split region adopts a session created through the previous
/// region's workspace.
#[test]
fn set_workspace_moves_a_tab_between_regions() {
    let (mut tabs, (ws_a, ws_b), ids) = two_workspace_strip();
    assert!(tabs.set_workspace(ids[0], ws_b));
    assert_eq!(tabs.workspace_of(ids[0]), Some(ws_b));
    assert_eq!(tabs.region(ws_b).map(WorkspaceTabs::len), Some(3));
    assert_eq!(tabs.active_session_in(ws_b), Some(ids[0]), "the adopting region shows it");
    assert_eq!(tabs.active_session_in(ws_a), Some(ids[1]), "the source region reclamps");
    assert!(!tabs.set_workspace(ids[0], ws_b), "an unchanged filing is not a move");
    assert!(!tabs.set_workspace(SessionId::new(), ws_a), "an unknown session cannot move");

    // Moving a region's last tab out drops the region.
    assert!(tabs.set_workspace(ids[1], ws_b));
    assert_eq!(tabs.regions().len(), 1);
}

// @lat: [[test#GPUI Client Headless Suites#GPUI tab task labels]]
#[test]
fn native_title_outranks_task_label_and_reset_reveals_fallbacks() {
    let (mut tabs, _, ids) = strip(2);

    assert!(tabs.set_task_label(ids[0], Some("Ship the tab labels")));
    assert_eq!(tabs.to_tab_data()[0].title, "Ship the tab labels");
    assert_eq!(tabs.to_tab_data()[1].title, "shell1", "siblings are untouched");
    assert!(!tabs.set_task_label(ids[0], Some("Ship the tab labels")), "identical is no change");

    assert!(tabs.set_title(ids[0], Some("editor".to_owned())));
    assert_eq!(tabs.to_tab_data()[0].title, "editor");

    assert!(tabs.set_icon_title(ids[0], Some("vim icon".to_owned())));
    assert_eq!(tabs.to_tab_data()[0].title, "vim icon");
    assert!(tabs.set_title(ids[0], Some("newer window".to_owned())));
    assert_eq!(tabs.to_tab_data()[0].title, "vim icon");
    assert!(tabs.set_icon_title(ids[0], Some(String::new())));
    assert_eq!(tabs.to_tab_data()[0].title, "newer window");

    assert!(tabs.set_title(ids[0], Some("   ".to_owned())));
    assert_eq!(tabs.to_tab_data()[0].title, "Ship the tab labels");

    // A blank label is the provider clearing it, never a blank tab.
    assert!(tabs.set_task_label(ids[0], Some("   ")));
    assert_eq!(tabs.to_tab_data()[0].title, "shell0");

    assert!(tabs.set_task_label(ids[0], Some("Second task")));
    assert!(tabs.set_task_label(ids[0], None));
    assert_eq!(tabs.to_tab_data()[0].title, "shell0");
    assert!(!tabs.set_task_label(ids[0], None), "already cleared is no change");
    assert!(!tabs.set_task_label(SessionId::new(), Some("ghost")));
}

#[test]
fn retitle_updates_only_on_change() {
    let (mut tabs, _, ids) = strip(2);
    assert!(tabs.set_title(ids[0], Some("claude".to_owned())));
    assert!(!tabs.set_title(ids[0], Some("claude".to_owned())), "identical title is not a change");
    assert!(!tabs.set_title(SessionId::new(), Some("ghost".to_owned())));
    assert_eq!(tabs.to_tab_data()[0].title, "claude");

    assert!(tabs.set_icon_title(ids[0], Some("icon".to_owned())));
    assert!(!tabs.set_icon_title(ids[0], Some("icon".to_owned())));
    assert!(!tabs.set_icon_title(SessionId::new(), Some("ghost".to_owned())));
    assert_eq!(tabs.to_tab_data()[0].title, "icon");
}
