//! Unit and interaction tests for the hover-preview port.

use std::sync::{Arc, Mutex};

use gpui::{AppContext as _, Entity, TestAppContext};
use scribe_common::theme::minimal_dark;

use super::{
    AddingNoteState, MAX_PREVIEW_COLS, MIN_PREVIEW_COLS, WorkspaceNoteSummary,
    WorkspaceNotesPreviewAction, WorkspaceNotesPreviewColors, WorkspaceNotesPreviewView,
    caret_line_index, caret_visible_col, editor_content_cols, editor_rows,
    longest_visible_line_chars, preview_cols, wrap_text_for_editor, wrapped_row_count,
};

fn summary(note_id: &str, text: &str) -> WorkspaceNoteSummary {
    WorkspaceNoteSummary { note_id: note_id.to_owned(), text: text.to_owned() }
}

// @lat: [[client#GPUI Workspace Notes#Preview sizing and wrap geometry]]
#[test]
fn preview_cols_sizes_to_longest_and_clamps() {
    // Empty preview falls back to the "No active notes" width, clamped up to the
    // minimum column count.
    assert_eq!(preview_cols(&[], None), MIN_PREVIEW_COLS);

    // A very long note clamps to the maximum column count.
    let long = "x".repeat(200);
    assert_eq!(preview_cols(&[summary("a", &long)], None), MAX_PREVIEW_COLS);

    // The inline editor widens the preview when its line is longer than notes.
    let mut editor = AddingNoteState::new_from_saved_draft("a longer editor line here".to_owned());
    editor.caret_byte = 0;
    let with_editor = preview_cols(&[summary("a", "hi")], Some(&editor));
    assert!(with_editor > preview_cols(&[summary("a", "hi")], None));
}

// @lat: [[client#GPUI Workspace Notes#Preview sizing and wrap geometry]]
#[test]
fn wrap_splits_on_hard_and_soft_breaks() {
    let text = "abcd\nefghij";
    // cols=3 wraps "abcd" into ["abc","d"]; the hard break then continues into
    // "efg"/"hij" (the newline's fresh line is filled by "efg").
    assert_eq!(wrap_text_for_editor(text, 3), vec!["abc", "d", "efg", "hij"]);
    // The longest logical line (split only on '\n') is "efghij" at 6 chars.
    assert_eq!(longest_visible_line_chars(text), 6);
    // `wrapped_row_count` is a sizing estimate: it reserves a row whenever a
    // column boundary is hit, so a clean wrap of "abcd" (no boundary landing on
    // the last char) reports exactly two rows.
    assert_eq!(wrapped_row_count("abcd", 3), 2);
}

// @lat: [[client#GPUI Workspace Notes#Preview sizing and wrap geometry]]
#[test]
fn editor_rows_clamps_between_one_and_the_cap() {
    let cols = preview_cols(&[], None);
    let content = editor_content_cols(cols);
    assert_eq!(editor_rows("", content, None), 1);
    let many = "line\n".repeat(30);
    assert_eq!(editor_rows(&many, content, Some(4)), 4);
}

// @lat: [[client#GPUI Workspace Notes#Preview sizing and wrap geometry]]
#[test]
fn caret_line_and_column_track_wrapped_position() {
    let text = "abcdef";
    // cols=3 => "abc" / "def"; caret at byte 4 is on line 1, col 1.
    assert_eq!(caret_line_index(text, 4, 3), 1);
    assert_eq!(caret_visible_col(text, 4, 3), 1);
    // Caret after a hard newline starts a fresh line at column 0.
    assert_eq!(caret_line_index("ab\nc", 3, 10), 1);
    assert_eq!(caret_visible_col("ab\nc", 3, 10), 0);
}

fn preview(
    cx: &mut TestAppContext,
) -> (Entity<WorkspaceNotesPreviewView>, Arc<Mutex<Vec<WorkspaceNotesPreviewAction>>>) {
    let colors = WorkspaceNotesPreviewColors::from(&minimal_dark().chrome);
    let view = cx.new(|_| WorkspaceNotesPreviewView::new(colors));
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    cx.update(|app| {
        app.subscribe(&view, move |_, event: &WorkspaceNotesPreviewAction, _| {
            sink.lock().unwrap().push(event.clone());
        })
        .detach();
    });
    cx.update(|_| {});
    (view, log)
}

// @lat: [[client#GPUI Workspace Notes#Preview inline and editor modes]]
#[gpui::test]
fn set_inline_editor_toggles_editing_mode(cx: &mut TestAppContext) {
    let (view, _log) = preview(cx);
    view.update(cx, |p, cx| {
        p.set_summaries(vec![summary("a", "note")], 1, cx);
    });
    view.read_with(cx, |p, _| assert!(!p.is_editing()));
    view.update(cx, |p, cx| {
        p.set_inline_editor(Some(AddingNoteState::new_from_saved_draft("draft".to_owned())), cx);
    });
    view.read_with(cx, |p, _| assert!(p.is_editing()));
    view.update(cx, |p, cx| p.set_inline_editor(None, cx));
    view.read_with(cx, |p, _| assert!(!p.is_editing()));
}
