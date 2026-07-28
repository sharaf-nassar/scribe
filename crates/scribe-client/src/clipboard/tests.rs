//! Unit coverage for the OSC 52 host clipboard bridge routing, the FR-019
//! focus gate, primary-selection read/write, and reply-message construction.
//!
//! An in-memory [`FakeClipboard`] stands in for the live arboard handle so the
//! read+write bridge roundtrip and per-selection routing are exercised without
//! a display server — the scripted OSC 52 bridge roundtrip at the unit level.
//! The real arboard-backed E2E stays a manual / launch-gate parity item.

use scribe_common::protocol::{
    BridgeError, ClientMessage, ClipboardDecision, ClipboardSelection, PromptId,
};

use super::*;
use crate::clipboard_cleanup::CopyTextOptions;

/// In-memory clipboard with independent system and primary buffers, plus an
/// availability flag to model a dead arboard handle.
#[derive(Default)]
struct FakeClipboard {
    system: Option<String>,
    primary: Option<String>,
    available: bool,
}

impl FakeClipboard {
    fn available() -> Self {
        Self { available: true, ..Self::default() }
    }
}

impl ClipboardBackend for FakeClipboard {
    fn read(&mut self, selection: ClipboardSelection) -> Result<String, BridgeError> {
        if !self.available {
            return Err(BridgeError::Unavailable);
        }
        let slot = match selection {
            ClipboardSelection::Primary => &self.primary,
            ClipboardSelection::Clipboard => &self.system,
        };
        Ok(slot.clone().unwrap_or_default())
    }

    fn write(&mut self, selection: ClipboardSelection, payload: String) -> Result<(), BridgeError> {
        if !self.available {
            return Err(BridgeError::Unavailable);
        }
        match selection {
            ClipboardSelection::Primary => self.primary = Some(payload),
            ClipboardSelection::Clipboard => self.system = Some(payload),
        }
        Ok(())
    }
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Write-read roundtrip on the system clipboard]]
#[test]
fn write_then_read_roundtrips_on_system_clipboard() {
    let mut cb = FakeClipboard::available();
    bridge_write(
        &mut cb,
        ClipboardSelection::Clipboard,
        "payload".into(),
        FocusGate { focus_gate_writes: false, window_focused: true },
    )
    .unwrap();
    let read = bridge_read(&mut cb, ClipboardSelection::Clipboard).unwrap();
    assert_eq!(read, "payload");
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Primary and system selections stay independent]]
#[test]
fn primary_and_system_selections_are_independent() {
    let mut cb = FakeClipboard::available();
    bridge_write(
        &mut cb,
        ClipboardSelection::Clipboard,
        "sys".into(),
        FocusGate { focus_gate_writes: false, window_focused: true },
    )
    .unwrap();
    bridge_write(
        &mut cb,
        ClipboardSelection::Primary,
        "pri".into(),
        FocusGate { focus_gate_writes: false, window_focused: true },
    )
    .unwrap();
    assert_eq!(bridge_read(&mut cb, ClipboardSelection::Clipboard).unwrap(), "sys");
    assert_eq!(bridge_read(&mut cb, ClipboardSelection::Primary).unwrap(), "pri");
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Unavailable backend reports a bridge error]]
#[test]
fn unavailable_backend_reports_bridge_error() {
    let mut cb = FakeClipboard::default();
    assert_eq!(bridge_read(&mut cb, ClipboardSelection::Clipboard), Err(BridgeError::Unavailable));
    assert_eq!(
        bridge_write(
            &mut cb,
            ClipboardSelection::Clipboard,
            "x".into(),
            FocusGate { focus_gate_writes: false, window_focused: true }
        ),
        Err(BridgeError::Unavailable)
    );
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Focus gate drops only enabled unfocused writes]]
#[test]
fn focus_gate_drops_write_only_when_enabled_and_unfocused() {
    assert!(FocusGate { focus_gate_writes: true, window_focused: false }.drops_write());
    assert!(!FocusGate { focus_gate_writes: true, window_focused: true }.drops_write());
    assert!(!FocusGate { focus_gate_writes: false, window_focused: false }.drops_write());
    assert!(!FocusGate { focus_gate_writes: false, window_focused: true }.drops_write());
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Gated write is a silent no-op]]
#[test]
fn gated_write_is_silent_noop_and_leaves_clipboard_untouched() {
    let mut cb = FakeClipboard::available();
    cb.system = Some("original".into());
    // Gate enabled + window unfocused: Ok(()) but no mutation.
    bridge_write(
        &mut cb,
        ClipboardSelection::Clipboard,
        "hijack".into(),
        FocusGate { focus_gate_writes: true, window_focused: false },
    )
    .unwrap();
    assert_eq!(cb.system.as_deref(), Some("original"));
    // Focused: the write goes through.
    bridge_write(
        &mut cb,
        ClipboardSelection::Clipboard,
        "allowed".into(),
        FocusGate { focus_gate_writes: true, window_focused: true },
    )
    .unwrap();
    assert_eq!(cb.system.as_deref(), Some("allowed"));
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Read reply wraps the payload]]
#[test]
fn read_reply_wraps_payload_under_request_id() {
    let mut cb = FakeClipboard::available();
    cb.system = Some("copied".into());
    let msg = read_reply(&mut cb, PromptId(7), ClipboardSelection::Clipboard);
    match msg {
        ClientMessage::ClipboardBridgeReadReply { request_id, payload } => {
            assert_eq!(request_id, PromptId(7));
            assert_eq!(payload, Ok("copied".to_string()));
        }
        other => panic!("expected ClipboardBridgeReadReply, got {other:?}"),
    }
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Read reply forwards a bridge error]]
#[test]
fn read_reply_forwards_bridge_error() {
    let mut cb = FakeClipboard::default();
    let msg = read_reply(&mut cb, PromptId(1), ClipboardSelection::Primary);
    match msg {
        ClientMessage::ClipboardBridgeReadReply { payload, .. } => {
            assert_eq!(payload, Err(BridgeError::Unavailable));
        }
        other => panic!("expected ClipboardBridgeReadReply, got {other:?}"),
    }
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Prompt response echoes id and decision]]
#[test]
fn prompt_response_echoes_request_id_and_decision() {
    let msg = prompt_response(PromptId(42), ClipboardDecision::AlwaysAllow);
    match msg {
        ClientMessage::ClipboardPromptResponse { request_id, decision } => {
            assert_eq!(request_id, PromptId(42));
            assert_eq!(decision, ClipboardDecision::AlwaysAllow);
        }
        other => panic!("expected ClipboardPromptResponse, got {other:?}"),
    }
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Primary read skips empty content]]
#[test]
fn read_primary_returns_none_on_empty_and_some_on_content() {
    let mut cb = FakeClipboard::available();
    assert_eq!(read_primary(&mut cb), None);
    cb.primary = Some(String::new());
    assert_eq!(read_primary(&mut cb), None);
    cb.primary = Some("middle-click".into());
    assert_eq!(read_primary(&mut cb).as_deref(), Some("middle-click"));
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Primary write applies cleanup]]
#[test]
fn set_primary_applies_cleanup_before_writing() {
    let mut cb = FakeClipboard::available();
    // Shared 4-space indent; dedent strips it and hard-wrapped lines rejoin
    // when cleanup is on, proving the transforms ran before the write.
    let raw = "    line one\n    line two";
    set_primary(&mut cb, raw, CopyTextOptions { ai_session_active: true, cleanup_enabled: true });
    assert_eq!(cb.primary.as_deref(), Some("line one line two"));
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Primary write is verbatim when cleanup off]]
#[test]
fn set_primary_writes_verbatim_when_cleanup_disabled_and_skips_empty() {
    let mut cb = FakeClipboard::available();
    set_primary(&mut cb, "", CopyTextOptions { ai_session_active: true, cleanup_enabled: true });
    assert_eq!(cb.primary, None);
    let raw = "    keep indent";
    set_primary(&mut cb, raw, CopyTextOptions { ai_session_active: false, cleanup_enabled: true });
    assert_eq!(cb.primary.as_deref(), Some("    keep indent"));
}

/// A prompt request with the given id, standing in for one the server parked.
fn prompt(id: u64) -> ClipboardPrompt {
    ClipboardPrompt {
        request_id: PromptId(id),
        op: scribe_common::protocol::ClipboardOp::Write,
        selection: ClipboardSelection::Clipboard,
        preview: Some("export TOKEN=hunter2".to_owned()),
    }
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Bridge starts ungated]]
#[test]
fn bridge_starts_ungated_and_adopts_the_welcome_bit() {
    let mut bridge = ClipboardBridge::default();
    assert!(!bridge.gating());
    bridge.set_gating(true);
    assert!(bridge.gating());
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Parked prompt is taken once]]
#[test]
fn parked_prompt_is_taken_exactly_once() {
    let mut bridge = ClipboardBridge::default();
    assert_eq!(bridge.take_prompt(), None);
    bridge.park_prompt(prompt(7));
    assert_eq!(bridge.take_prompt(), Some(prompt(7)));
    assert_eq!(bridge.take_prompt(), None);
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Bridge jobs drain in arrival order]]
#[test]
fn bridge_jobs_drain_in_arrival_order() {
    let mut bridge = ClipboardBridge::default();
    assert!(!bridge.push_job(BridgeJob::Write {
        selection: ClipboardSelection::Clipboard,
        payload: "first".to_owned(),
    }));
    assert!(!bridge.push_job(BridgeJob::Read {
        request_id: PromptId(1),
        selection: ClipboardSelection::Primary,
    }));
    let drained = bridge.drain_jobs();
    assert_eq!(drained.len(), 2);
    assert!(matches!(drained[0], BridgeJob::Write { .. }));
    assert!(matches!(drained[1], BridgeJob::Read { .. }));
    assert!(bridge.drain_jobs().is_empty());
}

// @lat: [[test#GPUI OSC 52 Clipboard Bridge#Bridge queue is bounded]]
#[test]
fn bridge_queue_drops_the_oldest_job_when_full() {
    let mut bridge = ClipboardBridge::default();
    for index in 0..MAX_PENDING_BRIDGE_JOBS {
        assert!(!bridge.push_job(BridgeJob::Read {
            request_id: PromptId(index as u64),
            selection: ClipboardSelection::Clipboard,
        }));
    }
    assert!(bridge.push_job(BridgeJob::Read {
        request_id: PromptId(999),
        selection: ClipboardSelection::Clipboard,
    }));
    let drained = bridge.drain_jobs();
    assert_eq!(drained.len(), MAX_PENDING_BRIDGE_JOBS);
    // The oldest went, the newest stayed.
    assert_eq!(
        drained[0],
        BridgeJob::Read { request_id: PromptId(1), selection: ClipboardSelection::Clipboard }
    );
    assert_eq!(
        drained[MAX_PENDING_BRIDGE_JOBS - 1],
        BridgeJob::Read { request_id: PromptId(999), selection: ClipboardSelection::Clipboard }
    );
}
