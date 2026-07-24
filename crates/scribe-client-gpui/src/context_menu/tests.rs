//! Unit and interaction tests for the context menu port.

use std::sync::{Arc, Mutex};

use gpui::{AppContext as _, Entity, TestAppContext, point, px};
use scribe_common::config::SmartSelectionActionKind;
use scribe_common::theme::minimal_dark;

use super::{
    ContextMenuAction, ContextMenuColors, ContextMenuEvent, ContextMenuRequest, ContextMenuView,
    MenuItem, build_menu_items, smart_selection_menu_item,
};
use crate::smart_selection::ResolvedSmartSelectionAction;

fn labels(items: &[MenuItem]) -> Vec<String> {
    items.iter().map(|i| i.label.clone()).collect()
}

// @lat: [[client#GPUI Overlays#Context menu head reflects selection state]]
#[test]
fn head_is_copy_paste_select_all_with_copy_gated_on_selection() {
    let no_sel = build_menu_items(ContextMenuRequest::default());
    assert_eq!(labels(&no_sel), ["Copy", "Paste", "Select All"]);
    assert!(!no_sel[0].enabled, "Copy is disabled without a selection");
    assert!(no_sel[1].enabled && no_sel[2].enabled);

    let with_sel =
        build_menu_items(ContextMenuRequest { has_selection: true, ..Default::default() });
    assert!(with_sel[0].enabled, "Copy is enabled with a selection");
}

// @lat: [[client#GPUI Overlays#Context menu OSC 8 precedence and copy entry]]
#[test]
fn osc8_uri_takes_open_url_precedence_and_adds_copy_entry() {
    let items = build_menu_items(ContextMenuRequest {
        url: Some("http://heuristic".into()),
        osc8_uri: Some("mailto:a@b.dev".into()),
        ..Default::default()
    });
    // "Open URL" carries the OSC 8 URI via OpenOsc8Url, not the heuristic URL.
    let open = items.iter().find(|i| i.label == "Open URL").unwrap();
    assert_eq!(open.action, ContextMenuAction::OpenOsc8Url("mailto:a@b.dev".into()));
    // The copy-hyperlink entry is appended.
    let copy = items.iter().find(|i| i.label == "Copy hyperlink address").unwrap();
    assert_eq!(copy.action, ContextMenuAction::CopyHyperlinkAddress("mailto:a@b.dev".into()));
}

#[test]
fn heuristic_url_used_when_no_osc8_uri() {
    let items = build_menu_items(ContextMenuRequest {
        url: Some("http://heuristic".into()),
        ..Default::default()
    });
    let open = items.iter().find(|i| i.label == "Open URL").unwrap();
    assert_eq!(open.action, ContextMenuAction::OpenUrl("http://heuristic".into()));
    assert!(items.iter().all(|i| i.label != "Copy hyperlink address"));
}

// @lat: [[client#GPUI Overlays#Context menu appends smart-selection actions]]
#[test]
fn smart_selection_actions_append_and_drop_empty_parameters() {
    let run = smart_selection_menu_item(ResolvedSmartSelectionAction {
        label: "IP: Run Command".into(),
        kind: SmartSelectionActionKind::RunCommand,
        parameter: "ping 1.1.1.1".into(),
    });
    assert!(run.is_some());
    // An empty expanded parameter drops the row entirely.
    let empty = smart_selection_menu_item(ResolvedSmartSelectionAction {
        label: "Nope".into(),
        kind: SmartSelectionActionKind::OpenUrl,
        parameter: String::new(),
    });
    assert!(empty.is_none());

    let items = build_menu_items(ContextMenuRequest {
        file_path: Some("/etc/hosts".into()),
        smart_actions: run.into_iter().collect(),
        ..Default::default()
    });
    // File entry then the smart action come after the fixed head.
    let names = labels(&items);
    assert_eq!(names.last().unwrap(), "IP: Run Command");
    assert!(names.contains(&"Open File".to_owned()));
}

fn menu_with_items(
    request: ContextMenuRequest,
    cx: &mut TestAppContext,
) -> (Entity<ContextMenuView>, Arc<Mutex<Vec<ContextMenuEvent>>>) {
    let colors = ContextMenuColors::from(&minimal_dark().chrome);
    let menu = cx.new(|cx| ContextMenuView::new(colors, request, point(px(40.0), px(60.0)), cx));
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    cx.update(|app| {
        app.subscribe(&menu, move |_, event: &ContextMenuEvent, _| {
            if let Ok(mut g) = sink.lock() {
                g.push(event.clone());
            }
        })
        .detach();
    });
    cx.update(|_| {});
    (menu, log)
}

// @lat: [[client#GPUI Overlays#Context menu click dispatches or dismisses]]
#[gpui::test]
fn clicking_an_enabled_item_emits_its_action(cx: &mut TestAppContext) {
    let (menu, log) =
        menu_with_items(ContextMenuRequest { has_selection: true, ..Default::default() }, cx);
    // Index 0 == Copy, enabled because a selection exists.
    menu.update(cx, |menu, cx| menu.activate(0, cx));
    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[ContextMenuEvent::Selected(ContextMenuAction::Copy)]
    );
}

#[gpui::test]
fn clicking_a_disabled_item_is_a_noop(cx: &mut TestAppContext) {
    let (menu, log) = menu_with_items(ContextMenuRequest::default(), cx);
    // Copy is disabled without a selection.
    menu.update(cx, |menu, cx| menu.activate(0, cx));
    assert!(log.lock().unwrap().is_empty());
}

#[gpui::test]
fn dismiss_emits_dismissed(cx: &mut TestAppContext) {
    let (menu, log) = menu_with_items(ContextMenuRequest::default(), cx);
    menu.update(cx, ContextMenuView::dismiss);
    assert_eq!(log.lock().unwrap().as_slice(), &[ContextMenuEvent::Dismissed]);
}
