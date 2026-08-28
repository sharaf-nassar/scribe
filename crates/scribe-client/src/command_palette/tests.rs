//! Unit and interaction tests for the command palette port.

use std::sync::{Arc, Mutex};

use gpui::{AppContext as _, Entity, TestAppContext};
use scribe_common::ids::{WindowId, WorkspaceId};
use scribe_common::protocol::{AutomationAction, WorkspaceMoveOperation, WorkspaceTreeEdge};
use scribe_common::theme::minimal_dark;

use super::{
    CommandPaletteColors, CommandPaletteEntry, CommandPaletteEvent, CommandPaletteView,
    PaletteAction, WorkspaceMoveTarget, base_entries, build_entries, filter_entries,
    profile_entries, workspace_move_entries,
};
use crate::layout::FocusDirection;
use crate::workspace_drag::swap_hint_text;

// @lat: [[client#GPUI Overlays#Palette base entries and update row]]
#[test]
fn base_entries_lead_with_settings_and_end_with_remote_connect() {
    let entries = base_entries();
    assert_eq!(entries.first().unwrap().label, "Open Settings");
    let last = entries.last().unwrap();
    assert_eq!(last.label, "Connect to remote machine…");
    assert_eq!(last.action, PaletteAction::OpenRemoteConnect);
}

#[test]
fn workspace_move_actions_are_visible() {
    let entries = base_entries();
    let moves: Vec<_> = entries
        .iter()
        .filter(|entry| entry.label.starts_with("Move workspace"))
        .map(|entry| (entry.label.as_str(), entry.action.clone()))
        .collect();
    assert_eq!(
        moves,
        vec![
            ("Move workspace to new window", PaletteAction::MoveWorkspaceToNewWindow),
            ("Move workspace left", PaletteAction::MoveWorkspace(FocusDirection::Left)),
            ("Move workspace right", PaletteAction::MoveWorkspace(FocusDirection::Right)),
            ("Move workspace up", PaletteAction::MoveWorkspace(FocusDirection::Up)),
            ("Move workspace down", PaletteAction::MoveWorkspace(FocusDirection::Down)),
        ]
    );
}

#[test]
fn update_row_is_appended_only_when_an_update_is_available() {
    let none = build_entries(None, &[], None, &[]);
    assert!(none.iter().all(|e| !e.label.starts_with("Update Scribe")));

    let some = build_entries(Some("9.9.9"), &[], None, &[]);
    let update = some.iter().find(|e| e.label == "Update Scribe to v9.9.9").unwrap();
    assert_eq!(update.action, PaletteAction::Automation(AutomationAction::OpenUpdateDialog));
}

// @lat: [[test#GPUI Workspace Drag]]
#[test]
fn each_eligible_window_offers_one_edge_insert_and_one_explained_swap() {
    let named = WorkspaceMoveTarget {
        window: WindowId::new(),
        workspace: WorkspaceId::new(),
        workspace_name: Some("Docs".to_owned()),
    };
    let unnamed = WorkspaceMoveTarget {
        window: WindowId::new(),
        workspace: WorkspaceId::new(),
        // A blank name is not a usable name, so this row takes generic wording.
        workspace_name: Some("  ".to_owned()),
    };
    let rows = workspace_move_entries(&[named.clone(), unnamed.clone()]);
    assert_eq!(rows.len(), 4, "two deterministic targets per eligible window");

    assert_eq!(rows[0].label, format!("Move workspace right of Docs in {}", named.window));
    assert_eq!(
        rows[0].action,
        PaletteAction::MoveWorkspaceToWindow {
            target_window: named.window,
            target_workspace: named.workspace,
            operation: WorkspaceMoveOperation::InsertAtEdge { edge: WorkspaceTreeEdge::Right },
        }
    );
    assert_eq!(rows[0].detail, None, "an insert needs no explanation");

    assert_eq!(rows[1].label, format!("Swap workspace with Docs in {}", named.window));
    assert_eq!(
        rows[1].action,
        PaletteAction::MoveWorkspaceToWindow {
            target_window: named.window,
            target_workspace: named.workspace,
            operation: WorkspaceMoveOperation::Swap,
        }
    );
    // Byte-identical to the sentence the pointer's centre zone paints.
    assert_eq!(rows[1].detail.as_deref(), Some(swap_hint_text(Some("Docs")).as_str()));

    assert_eq!(rows[2].label, format!("Move workspace right of workspace in {}", unnamed.window));
    assert_eq!(rows[3].detail.as_deref(), Some(swap_hint_text(None).as_str()));

    // No eligible window means no move rows at all, so nothing unreachable is
    // offered on a single-window desktop.
    assert!(workspace_move_entries(&[]).is_empty());
    let full = build_entries(None, &[], None, &[named]);
    assert_eq!(full.iter().filter(|entry| entry.detail.is_some()).count(), 1);
}

// @lat: [[client#GPUI Overlays#Palette profile rows tag the active profile]]
#[test]
fn profile_rows_tag_the_active_profile() {
    let names = vec!["work".to_owned(), "home".to_owned()];
    let rows = profile_entries(&names, Some("home"));
    assert_eq!(rows[0].label, "Switch Profile: work");
    assert_eq!(rows[1].label, "Switch Profile: home (active)");
    assert_eq!(
        rows[1].action,
        PaletteAction::Automation(AutomationAction::SwitchProfile { name: "home".into() })
    );
}

// @lat: [[client#GPUI Overlays#Palette query filters case-insensitively]]
#[test]
fn filter_is_case_insensitive_substring_and_empty_keeps_all() {
    let entries = base_entries();
    let all = filter_entries(&entries, "   ");
    assert_eq!(all.len(), entries.len());

    let split = filter_entries(&entries, "SPLIT");
    assert_eq!(split.len(), 2);
    assert!(split.iter().all(|e| e.label.to_lowercase().contains("split")));
}

fn palette(
    cx: &mut TestAppContext,
) -> (Entity<CommandPaletteView>, Arc<Mutex<Vec<CommandPaletteEvent>>>) {
    let colors = CommandPaletteColors::from(&minimal_dark().chrome);
    let entries = build_entries(Some("1.2.3"), &["work".to_owned()], Some("work"), &[]);
    let view = cx.new(|cx| CommandPaletteView::new(colors, entries, cx));
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    cx.update(|app| {
        app.subscribe(&view, move |_, event: &CommandPaletteEvent, _| {
            if let Ok(mut g) = sink.lock() {
                g.push(event.clone());
            }
        })
        .detach();
    });
    cx.update(|_| {});
    (view, log)
}

// @lat: [[client#GPUI Overlays#Palette typing and paste drive the filter]]
#[gpui::test]
fn typing_filters_and_paste_strips_control_chars(cx: &mut TestAppContext) {
    let (view, _log) = palette(cx);
    view.update(cx, |view, cx| {
        view.push_char('s', cx);
        view.push_char('e', cx);
    });
    view.read_with(cx, |view, _| {
        assert_eq!(view.query(), "se");
        assert!(view.filtered().iter().all(|e| e.label.to_lowercase().contains("se")));
    });
    // Pasting a multi-line payload collapses to non-control chars.
    view.update(cx, |view, cx| view.push_str("t\nti\tng", cx));
    view.read_with(cx, |view, _| assert_eq!(view.query(), "setting"));
}

// @lat: [[client#GPUI Overlays#Palette selection wraps and confirms an action]]
#[gpui::test]
fn selection_wraps_and_confirm_emits_the_action(cx: &mut TestAppContext) {
    let (view, log) = palette(cx);
    // Narrow to the two split rows, then wrap the selection.
    view.update(cx, |view, cx| {
        view.push_str("split", cx);
        view.next_item(cx); // -> index 1
        view.next_item(cx); // wraps -> index 0
    });
    view.read_with(cx, |view, _| assert_eq!(view.selected_index(), 0));
    view.update(cx, CommandPaletteView::confirm);
    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[CommandPaletteEvent::Execute(PaletteAction::Automation(AutomationAction::SplitVertical))]
    );
}

#[gpui::test]
fn confirm_on_empty_filter_is_a_noop(cx: &mut TestAppContext) {
    let (view, log) = palette(cx);
    view.update(cx, |view, cx| {
        view.push_str("zzzznomatch", cx);
        view.confirm(cx);
    });
    assert!(log.lock().unwrap().is_empty());
}

#[gpui::test]
fn set_entries_resets_query_and_selection(cx: &mut TestAppContext) {
    let (view, _log) = palette(cx);
    view.update(cx, |view, cx| {
        view.push_str("split", cx);
        view.next_item(cx);
        view.set_entries(
            vec![CommandPaletteEntry::automation("Only", AutomationAction::NewTab)],
            cx,
        );
    });
    view.read_with(cx, |view, _| {
        assert_eq!(view.query(), "");
        assert_eq!(view.selected_index(), 0);
        assert_eq!(view.filtered().len(), 1);
    });
}
