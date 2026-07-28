//! Parity + interaction tests for the ported modal dialog suite.

use std::sync::{Arc, Mutex};

use gpui::{AppContext as _, Entity, TestAppContext};
use scribe_common::protocol::{ClipboardOp, ClipboardSelection, PromptId};
use scribe_common::theme::minimal_dark;

use super::{
    AnyDialog, ButtonTone, ClipboardDialog, ClipboardDialogAction, CloseAction, CloseDialog,
    DialogColors, DialogEvent, DialogOutcome, DialogView, DisallowedSchemeAction,
    DisallowedSchemeDialog, PasteConfirmationAction, PasteConfirmationDialog, UpdateAction,
    UpdateDialog, UpdateDialogKind,
};
use crate::paste::{ParkedPaste, PasteRisk};

fn labels(dialog: &AnyDialog) -> Vec<String> {
    dialog.spec().buttons.iter().map(|b| b.label.clone()).collect()
}

fn tones(dialog: &AnyDialog) -> Vec<ButtonTone> {
    dialog.spec().buttons.iter().map(|b| b.tone).collect()
}

// @lat: [[client#GPUI Dialogs#Close dialog buttons and safe default]]
#[test]
fn close_dialog_buttons_default_focus_and_session_warning() {
    let dialog = AnyDialog::Close(CloseDialog::new(3));
    let spec = dialog.spec();
    assert_eq!(labels(&dialog), vec!["Quit Scribe", "Kill Window", "Cancel"]);
    assert_eq!(tones(&dialog), vec![ButtonTone::Accent, ButtonTone::Danger, ButtonTone::Normal]);
    // Cancel (index 2) is the safe default focus.
    assert_eq!(spec.focused, 2);
    assert_eq!(dialog.confirm(), DialogOutcome::Close(CloseAction::Cancel));
    // The active-session warning line appears only when sessions are open.
    assert!(spec.body.iter().any(|l| l.contains("3 active session(s) will be lost")));
    let quiet = AnyDialog::Close(CloseDialog::new(0));
    assert!(quiet.spec().body.iter().all(|l| !l.contains("active session")));
}

// @lat: [[client#GPUI Dialogs#Close dialog focus cycling maps to actions]]
#[test]
fn close_dialog_focus_cycles_and_maps_to_actions() {
    let mut dialog = AnyDialog::Close(CloseDialog::new(0));
    // From Cancel (2), next wraps to Quit (0); prev from 2 lands on Kill (1).
    dialog.focus_next();
    assert_eq!(dialog.confirm(), DialogOutcome::Close(CloseAction::QuitAll));
    dialog.focus_prev();
    assert_eq!(dialog.confirm(), DialogOutcome::Close(CloseAction::Cancel));
    dialog.focus_prev();
    assert_eq!(dialog.confirm(), DialogOutcome::Close(CloseAction::CloseWindow));
    assert_eq!(dialog.action_at(0), Some(DialogOutcome::Close(CloseAction::QuitAll)));
    assert_eq!(dialog.action_at(3), None);
    assert_eq!(dialog.cancel(), DialogOutcome::Close(CloseAction::Cancel));
}

// @lat: [[client#GPUI Dialogs#Update dialog install and restart flows]]
#[test]
fn update_dialog_install_and_restart_flows() {
    let install = AnyDialog::Update(UpdateDialog::new_install("1.2.3".to_owned()));
    assert_eq!(install.spec().title, "Update Available");
    assert_eq!(labels(&install), vec!["Update Now", "Later"]);
    assert_eq!(tones(&install), vec![ButtonTone::Accent, ButtonTone::Normal]);
    // Primary (Update Now) is the default focus for install.
    assert_eq!(install.confirm(), DialogOutcome::Update(UpdateAction::Primary));
    assert!(install.spec().body.iter().any(|l| l.contains("preserved")));

    let restart = UpdateDialog::new_restart_required("9.9.9".to_owned());
    assert_eq!(restart.kind(), UpdateDialogKind::RestartRequired);
    let restart = AnyDialog::Update(restart);
    assert_eq!(restart.spec().title, "Restart Required");
    assert_eq!(labels(&restart), vec!["Continue", "Cancel"]);
    assert!(restart.spec().body.iter().any(|l| l.contains("cold restart")));
    // Esc / backdrop always resolves to the safe Secondary action.
    assert_eq!(restart.cancel(), DialogOutcome::Update(UpdateAction::Secondary));
}

fn parked(text: &str) -> ParkedPaste {
    ParkedPaste { text: text.to_owned(), bracketed: false, risk: risk(text) }
}

fn risk(text: &str) -> PasteRisk {
    crate::paste::classify_paste(text).expect("test inputs are always risky")
}

// @lat: [[client#GPUI Dialogs#Paste gate reason line distinguishes risk]]
#[test]
fn paste_dialog_reason_line_distinguishes_risk() {
    // Multiline only.
    let ml = AnyDialog::Paste(PasteConfirmationDialog::new(parked("one\ntwo\nthree")));
    assert_eq!(ml.spec().body[0], "3 lines");
    // Control only (single ESC, no newline).
    let ctrl = AnyDialog::Paste(PasteConfirmationDialog::new(parked("echo \x1b[31mhi")));
    assert_eq!(ctrl.spec().body[0], "contains a control character");
    // Both.
    let both = AnyDialog::Paste(PasteConfirmationDialog::new(parked("a\nb\x07c")));
    assert_eq!(both.spec().body[0], "2 lines · 1 control character");
    // Cancel is the default focus and safe action.
    assert_eq!(both.spec().focused, 0);
    assert_eq!(both.confirm(), DialogOutcome::Paste(PasteConfirmationAction::Cancel));
    assert_eq!(labels(&both), vec!["Cancel", "Paste"]);
}

// @lat: [[client#GPUI Dialogs#Paste preview is caret-escaped]]
#[test]
fn paste_dialog_preview_is_caret_escaped_and_never_raw() {
    let dialog = AnyDialog::Paste(PasteConfirmationDialog::new(parked("echo \x1b[31mred")));
    let body = dialog.spec().body;
    // No line in the rendered spec contains a raw control byte; ESC shows as ^[.
    assert!(body.iter().all(|l| l.chars().all(|c| !c.is_control())));
    assert!(body.iter().any(|l| l.contains("^[")));
    // The parked paste round-trips verbatim for byte-identical delivery.
    if let AnyDialog::Paste(d) = dialog {
        assert_eq!(d.into_parked_paste().text, "echo \x1b[31mred");
    } else {
        panic!("expected a paste dialog");
    }
}

// @lat: [[client#GPUI Dialogs#Clipboard dialog four-button policy]]
#[test]
fn clipboard_dialog_four_button_policy_and_default_deny() {
    let write = AnyDialog::Clipboard(ClipboardDialog::new(
        PromptId(7),
        ClipboardOp::Write,
        ClipboardSelection::Clipboard,
        Some("secret-token".to_owned()),
    ));
    assert_eq!(write.spec().title, "Allow clipboard write?");
    assert_eq!(labels(&write), vec!["Deny once", "Always deny", "Allow once", "Always allow"]);
    assert_eq!(
        tones(&write),
        vec![ButtonTone::Normal, ButtonTone::Normal, ButtonTone::Danger, ButtonTone::Danger]
    );
    // Deny once (index 0) is the safe default focus (FR-005).
    assert_eq!(write.spec().focused, 0);
    assert_eq!(write.confirm(), DialogOutcome::Clipboard(ClipboardDialogAction::DenyOnce));
    // The write payload preview is shown.
    assert!(write.spec().body.iter().any(|l| l.contains("secret-token")));

    // Each index maps to the expected policy action.
    assert_eq!(
        write.action_at(2),
        Some(DialogOutcome::Clipboard(ClipboardDialogAction::AllowOnce))
    );
    assert_eq!(
        write.action_at(3),
        Some(DialogOutcome::Clipboard(ClipboardDialogAction::AlwaysAllow))
    );

    // A read request shows no preview and titles differently.
    let read =
        ClipboardDialog::new(PromptId(8), ClipboardOp::Read, ClipboardSelection::Primary, None);
    assert_eq!(read.request_id(), PromptId(8));
    assert_eq!(read.op(), ClipboardOp::Read);
    let read = AnyDialog::Clipboard(read);
    assert_eq!(read.spec().title, "Allow clipboard read?");
    assert!(read.spec().body.iter().all(|l| !l.contains("Payload preview")));
    assert!(read.spec().body.iter().any(|l| l.contains("primary selection")));
}

// @lat: [[client#GPUI Dialogs#Disallowed scheme dialog truncation]]
#[test]
fn disallowed_scheme_dialog_truncates_but_keeps_uri() {
    let long = format!("javascript://{}evil", "a".repeat(80));
    let dialog = AnyDialog::DisallowedScheme(DisallowedSchemeDialog::new(
        long.clone(),
        "javascript".to_owned(),
    ));
    let spec = dialog.spec();
    assert_eq!(spec.title, "Unsafe URI Scheme");
    assert_eq!(labels(&dialog), vec!["Cancel", "Open Anyway"]);
    assert_eq!(tones(&dialog), vec![ButtonTone::Normal, ButtonTone::Danger]);
    assert_eq!(spec.focused, 0);
    assert_eq!(dialog.confirm(), DialogOutcome::DisallowedScheme(DisallowedSchemeAction::Cancel));
    // The body mentions the scheme and shows a head+tail-truncated URI (both the
    // scheme prefix and the tail suffix stay visible).
    assert!(spec.body.iter().any(|l| l.contains("`javascript:`")));
    let uri_line = spec.body.last().unwrap();
    assert!(uri_line.contains("..."));
    assert!(uri_line.starts_with("javascript://"));
    assert!(uri_line.ends_with("evil"));
    // The full URI is preserved verbatim for activation.
    if let AnyDialog::DisallowedScheme(d) = dialog {
        assert_eq!(d.into_pending_uri(), long);
    } else {
        panic!("expected a disallowed-scheme dialog");
    }
}

fn dialog_view(
    cx: &mut TestAppContext,
    dialog: AnyDialog,
) -> (Entity<DialogView>, Arc<Mutex<Vec<DialogEvent>>>) {
    let colors = DialogColors::from(&minimal_dark().chrome);
    let view = cx.new(|cx| DialogView::new(dialog, colors, cx));
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    cx.update(|app| {
        app.subscribe(&view, move |_, event: &DialogEvent, _| {
            if let Ok(mut g) = sink.lock() {
                g.push(*event);
            }
        })
        .detach();
    });
    cx.update(|_| {});
    (view, log)
}

// @lat: [[client#GPUI Dialogs#Dialog view confirms the focused button]]
#[gpui::test]
fn dialog_view_confirms_the_focused_button(cx: &mut TestAppContext) {
    let (view, log) = dialog_view(cx, AnyDialog::Close(CloseDialog::new(0)));
    // Default focus is Cancel; cycle forward to Quit Scribe and confirm.
    view.update(cx, |view, cx| {
        view.focus_next(cx); // Cancel -> Quit
        view.confirm(cx);
    });
    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[DialogEvent::Chosen(DialogOutcome::Close(CloseAction::QuitAll))]
    );
}

// @lat: [[client#GPUI Dialogs#Dialog view click and dismissal resolve]]
#[gpui::test]
fn dialog_view_click_and_dismiss_resolve(cx: &mut TestAppContext) {
    // A direct click activates that button regardless of focus.
    let (view, log) = dialog_view(
        cx,
        AnyDialog::Clipboard(ClipboardDialog::new(
            PromptId(1),
            ClipboardOp::Read,
            ClipboardSelection::Clipboard,
            None,
        )),
    );
    view.update(cx, |view, cx| view.activate(2, cx));
    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[DialogEvent::Chosen(DialogOutcome::Clipboard(ClipboardDialogAction::AllowOnce))]
    );

    // Esc / backdrop resolves to the safe cancel action.
    let (dismiss_view, dismiss_log) = dialog_view(
        cx,
        AnyDialog::Clipboard(ClipboardDialog::new(
            PromptId(2),
            ClipboardOp::Read,
            ClipboardSelection::Clipboard,
            None,
        )),
    );
    dismiss_view.update(cx, DialogView::dismiss);
    assert_eq!(
        dismiss_log.lock().unwrap().as_slice(),
        &[DialogEvent::Chosen(DialogOutcome::Clipboard(ClipboardDialogAction::DenyOnce))]
    );
}
