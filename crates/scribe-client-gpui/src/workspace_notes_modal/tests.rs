//! Unit, interaction, and draft-debounce timing tests for the modal port.

use std::sync::{Arc, Mutex};

use gpui::{AppContext as _, Entity, TestAppContext};
use scribe_common::ids::WorkspaceId;
use scribe_common::protocol::{WorkspaceNoteEntry, WorkspaceNoteStatus, WorkspaceNotesMutation};
use scribe_common::theme::minimal_dark;

use super::{
    ArchiveReason, DraftDebounce, DraftDebounceEvent, WORKSPACE_NOTES_DEBOUNCE,
    WorkspaceNotesEditMode, WorkspaceNotesModalColors, WorkspaceNotesModalView, WorkspaceNotesView,
};

fn entry(workspace_id: WorkspaceId, note_id: &str, text: &str) -> WorkspaceNoteEntry {
    WorkspaceNoteEntry {
        note_id: note_id.to_owned(),
        workspace_id,
        text: text.to_owned(),
        status: WorkspaceNoteStatus::Active,
        created_at_ms: 0,
        updated_at_ms: 0,
        archived_at_ms: None,
        archive_reason: None,
    }
}

fn modal(cx: &mut TestAppContext) -> Entity<WorkspaceNotesModalView> {
    let colors = WorkspaceNotesModalColors::from(&minimal_dark().chrome);
    cx.new(|cx| WorkspaceNotesModalView::new(&colors, cx))
}

// @lat: [[client#GPUI Workspace Notes#Modal view and edit-mode state machine]]
#[gpui::test]
fn open_seeds_draft_and_close_resets_editor(cx: &mut TestAppContext) {
    let workspace_id = WorkspaceId::new();
    let view = modal(cx);
    view.update(cx, |m, cx| m.open(workspace_id, "seed".to_owned(), cx));
    view.read_with(cx, |m, _| {
        assert!(m.is_open());
        assert_eq!(m.workspace_id(), Some(workspace_id));
        assert_eq!(m.draft_text(), "seed");
        assert_eq!(m.view(), WorkspaceNotesView::Active);
    });
    view.update(cx, WorkspaceNotesModalView::close);
    view.read_with(cx, |m, _| assert!(!m.is_open()));
}

// @lat: [[client#GPUI Workspace Notes#Modal view and edit-mode state machine]]
#[gpui::test]
fn switching_view_cancels_a_non_draft_edit(cx: &mut TestAppContext) {
    let workspace_id = WorkspaceId::new();
    let view = modal(cx);
    view.update(cx, |m, cx| {
        m.open(workspace_id, String::new(), cx);
        m.begin_active_edit(&entry(workspace_id, "n1", "hello"), cx);
    });
    view.read_with(cx, |m, _| {
        assert!(matches!(m.edit_mode(), WorkspaceNotesEditMode::ActiveNote { .. }));
        assert_eq!(m.active_text(), "hello");
    });
    view.update(cx, |m, cx| m.set_view(WorkspaceNotesView::Archive, cx));
    view.read_with(cx, |m, _| {
        assert_eq!(m.edit_mode(), &WorkspaceNotesEditMode::Draft);
        assert_eq!(m.view(), WorkspaceNotesView::Archive);
    });
}

// @lat: [[client#GPUI Workspace Notes#Modal view and edit-mode state machine]]
#[gpui::test]
fn draft_typing_marks_dirty_and_edit_typing_does_not(cx: &mut TestAppContext) {
    let workspace_id = WorkspaceId::new();
    let view = modal(cx);
    view.update(cx, |m, cx| m.open(workspace_id, String::new(), cx));
    view.update(cx, |m, cx| {
        m.push_char('h', cx);
        m.push_char('i', cx);
    });
    view.read_with(cx, |m, _| {
        assert_eq!(m.draft_text(), "hi");
        assert!(m.draft_dirty());
    });
    view.update(cx, |m, _| m.mark_draft_synced());
    view.read_with(cx, |m, _| assert!(!m.draft_dirty()));

    // Editing an existing note writes edit_text, never the draft-dirty flag.
    view.update(cx, |m, cx| {
        m.begin_active_edit(&entry(workspace_id, "n1", "base"), cx);
        m.push_char('!', cx);
    });
    view.read_with(cx, |m, _| {
        assert_eq!(m.active_text(), "base!");
        assert!(!m.draft_dirty());
    });
}

// @lat: [[client#GPUI Workspace Notes#Modal save maps to a mutation]]
#[gpui::test]
fn save_mutation_maps_each_edit_mode(cx: &mut TestAppContext) {
    let workspace_id = WorkspaceId::new();
    let view = modal(cx);
    view.update(cx, |m, cx| m.open(workspace_id, String::new(), cx));

    // Blank draft yields nothing.
    view.read_with(cx, |m, _| assert!(m.save_mutation().is_none()));

    // Non-blank draft -> CreateActiveNote.
    view.update(cx, |m, cx| m.push_char('x', cx));
    view.read_with(cx, |m, _| {
        assert!(matches!(
            m.save_mutation(),
            Some(WorkspaceNotesMutation::CreateActiveNote { text, .. }) if text == "x"
        ));
    });

    // Active-note edit -> EditNote.
    view.update(cx, |m, cx| m.begin_active_edit(&entry(workspace_id, "n7", "body"), cx));
    view.read_with(cx, |m, _| {
        assert!(matches!(
            m.save_mutation(),
            Some(WorkspaceNotesMutation::EditNote { note_id, .. }) if note_id == "n7"
        ));
    });

    // Bulk archive edit -> BulkEditArchived split on \n---\n.
    view.update(cx, |m, cx| {
        m.begin_archive_bulk_edit(
            &[entry(workspace_id, "a", "one"), entry(workspace_id, "b", "two")],
            cx,
        );
    });
    view.read_with(cx, |m, _| {
        let updates = m.archive_bulk_updates();
        assert_eq!(
            updates,
            vec![("a".to_owned(), "one".to_owned()), ("b".to_owned(), "two".to_owned())]
        );
        assert!(matches!(m.save_mutation(), Some(WorkspaceNotesMutation::BulkEditArchived { .. })));
    });
}

// @lat: [[client#GPUI Workspace Notes#Modal save maps to a mutation]]
#[gpui::test]
fn archive_mutation_carries_reason(cx: &mut TestAppContext) {
    let workspace_id = WorkspaceId::new();
    let view = modal(cx);
    view.update(cx, |m, cx| m.open(workspace_id, String::new(), cx));
    view.read_with(cx, |m, _| {
        assert!(matches!(
            m.archive_mutation("n1", ArchiveReason::Removed),
            Some(WorkspaceNotesMutation::ArchiveNote { reason: ArchiveReason::Removed, .. })
        ));
    });
}

fn record_debounce(
    debounce: &Entity<DraftDebounce>,
    cx: &mut TestAppContext,
) -> Arc<Mutex<Vec<DraftDebounceEvent>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    cx.update(|app| {
        app.subscribe(debounce, move |_, event: &DraftDebounceEvent, _| {
            sink.lock().unwrap().push(*event);
        })
        .detach();
    });
    cx.update(|_| {});
    events
}

// @lat: [[client#GPUI Workspace Notes#Draft debounce coalesces and fires once]]
#[gpui::test]
fn draft_debounce_fires_once_after_the_window(cx: &mut TestAppContext) {
    let debounce = cx.new(|_| DraftDebounce::new());
    let events = record_debounce(&debounce, cx);

    debounce.update(cx, DraftDebounce::mark_dirty);
    debounce.read_with(cx, |d, _| assert!(d.is_pending()));

    // Before the window elapses, nothing flushes.
    cx.executor().advance_clock(WORKSPACE_NOTES_DEBOUNCE / 2);
    cx.run_until_parked();
    assert!(events.lock().unwrap().is_empty(), "no flush before the window elapses");

    // Past the window, exactly one flush.
    cx.executor().advance_clock(WORKSPACE_NOTES_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(events.lock().unwrap().as_slice(), &[DraftDebounceEvent::Flush]);
    debounce.read_with(cx, |d, _| assert!(!d.is_pending()));
}

// @lat: [[client#GPUI Workspace Notes#Draft debounce coalesces and fires once]]
#[gpui::test]
fn continuous_typing_coalesces_into_one_flush(cx: &mut TestAppContext) {
    let debounce = cx.new(|_| DraftDebounce::new());
    let events = record_debounce(&debounce, cx);

    // Three keystrokes, each within the window, restart the timer every time.
    for _ in 0..3 {
        debounce.update(cx, DraftDebounce::mark_dirty);
        cx.executor().advance_clock(WORKSPACE_NOTES_DEBOUNCE / 2);
        cx.run_until_parked();
    }
    assert!(events.lock().unwrap().is_empty(), "restarted timer never fired mid-burst");

    // After a full quiet window, one coalesced flush.
    cx.executor().advance_clock(WORKSPACE_NOTES_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(events.lock().unwrap().as_slice(), &[DraftDebounceEvent::Flush]);
}

// @lat: [[client#GPUI Workspace Notes#Draft debounce coalesces and fires once]]
#[gpui::test]
fn flush_now_forces_an_immediate_flush_and_cancels_the_timer(cx: &mut TestAppContext) {
    let debounce = cx.new(|_| DraftDebounce::new());
    let events = record_debounce(&debounce, cx);

    debounce.update(cx, DraftDebounce::mark_dirty);
    debounce.update(cx, DraftDebounce::flush_now);
    assert_eq!(events.lock().unwrap().as_slice(), &[DraftDebounceEvent::Flush]);

    // The cancelled timer must not fire a second flush later.
    cx.executor().advance_clock(WORKSPACE_NOTES_DEBOUNCE * 2);
    cx.run_until_parked();
    assert_eq!(events.lock().unwrap().len(), 1, "cancelled timer stayed silent");
}
