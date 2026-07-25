//! Bracketed-paste detection and the confirmation gate for risky pastes.
//!
//! Ports the winit client's spec-011 paste gate
//! ([`crate`](../../scribe-client/src/paste_confirmation_dialog.rs)). A paste is
//! "risky" when it contains a line break or a non-tab control/escape byte AND
//! the focused pane has NOT enabled bracketed paste AND the
//! `terminal.paste_confirmation` config is on. Risky pastes are parked behind a
//! confirmation before any byte reaches the PTY; everything else is delivered
//! unchanged.
//!
//! [`classify_paste`] is the pure, allocation-free classifier. [`PasteGate`]
//! wraps the decision in a `gpui::Entity` so the view subscribes to
//! [`PasteGateEvent`]: `Send` delivers the bytes straight away, `Confirm` opens
//! the dialog with the parked text. The gate holds the parked request so a
//! later confirm resumes delivery on the exact original bytes, bypassing the
//! gate (matching the winit resume path).

use gpui::{Context, EventEmitter};

/// Bracketed-paste mode start marker (DEC 2004).
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed-paste mode end marker (DEC 2004).
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// Largest payload one `ClientMessage::KeyInput` may carry. The server rejects
/// anything above it, so a paste bigger than this has to be split.
pub const MAX_KEY_INPUT_CHUNK: usize = 4 * 1024;

/// Split `text` into `KeyInput`-sized payloads for the focused pane.
///
/// When `bracketed` is set the DEC 2004 start marker rides on the FIRST chunk
/// and the end marker on the LAST, so the shell sees one contiguous paste
/// region no matter how many frames carried it; the markers are budgeted out
/// of those chunks' payloads so no frame can exceed
/// [`MAX_KEY_INPUT_CHUNK`]. Ported from the winit client's
/// `try_send_single_paste` / `send_chunked_paste` pair, whose fast path is the
/// single-chunk case this function returns as a one-element vector. Empty
/// input yields no chunks, so a caller never sends an empty `KeyInput`.
#[must_use]
pub fn paste_chunks(text: &str, bracketed: bool) -> Vec<Vec<u8>> {
    let raw = text.as_bytes();
    if raw.is_empty() {
        return Vec::new();
    }
    let markers =
        if bracketed { BRACKETED_PASTE_START.len() + BRACKETED_PASTE_END.len() } else { 0 };
    if raw.len() + markers <= MAX_KEY_INPUT_CHUNK {
        let mut chunk = Vec::with_capacity(raw.len() + markers);
        if bracketed {
            chunk.extend_from_slice(BRACKETED_PASTE_START);
        }
        chunk.extend_from_slice(raw);
        if bracketed {
            chunk.extend_from_slice(BRACKETED_PASTE_END);
        }
        return vec![chunk];
    }

    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut first = true;
    while offset < raw.len() {
        let mut budget = MAX_KEY_INPUT_CHUNK;
        if first && bracketed {
            budget -= BRACKETED_PASTE_START.len();
        }
        let reaches_end = offset + budget >= raw.len();
        if reaches_end && bracketed {
            budget = budget.saturating_sub(BRACKETED_PASTE_END.len());
        }
        let payload_len = (raw.len() - offset).min(budget);
        let last = offset + payload_len >= raw.len();

        let mut chunk = Vec::with_capacity(MAX_KEY_INPUT_CHUNK);
        if first && bracketed {
            chunk.extend_from_slice(BRACKETED_PASTE_START);
        }
        if let Some(slice) = raw.get(offset..offset + payload_len) {
            chunk.extend_from_slice(slice);
        }
        if last && bracketed {
            chunk.extend_from_slice(BRACKETED_PASTE_END);
        }
        chunks.push(chunk);
        offset += payload_len;
        first = false;
    }
    chunks
}

/// Result of classifying paste content for the confirmation gate.
///
/// At least one flag is always set when this is produced by [`classify_paste`];
/// a value with both flags `false` is never returned from there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteRisk {
    /// Content contains `\n` or `\r` (including a single trailing newline).
    pub has_line_break: bool,
    /// Content contains a control/escape character that is NOT `\t`/`\n`/`\r`
    /// (C0 except tab/LF/CR, DEL, or C1).
    pub has_control: bool,
}

/// Classify paste `text`, returning `Some(risk)` iff it should be gated.
///
/// A line break is `'\n'` or `'\r'`. A control character is any
/// [`char::is_control`] other than `'\t'`, `'\n'`, or `'\r'`. Returns `Some` iff
/// `has_line_break || has_control`, else `None`. Pure and allocation-free; O(n)
/// in the char length of `text`. Byte-for-byte identical to the winit
/// [`crate`](../../scribe-client/src/paste_confirmation_dialog.rs) classifier.
#[must_use]
pub fn classify_paste(text: &str) -> Option<PasteRisk> {
    let mut has_line_break = false;
    let mut has_control = false;
    for ch in text.chars() {
        match ch {
            '\n' | '\r' => has_line_break = true,
            '\t' => {}
            c if c.is_control() => has_control = true,
            _ => {}
        }
    }
    if has_line_break || has_control {
        Some(PasteRisk { has_line_break, has_control })
    } else {
        None
    }
}

/// A paste request captured at the moment the gate parks it.
///
/// Holds the exact text and the target's bracketed-paste flag so a later
/// [`PasteGate::confirm`] resends byte-identically, skipping the gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedPaste {
    /// The full paste text, rendered display-only in the confirmation dialog.
    pub text: String,
    /// Whether the focused pane had bracketed paste enabled at request time.
    pub bracketed: bool,
    /// Why the paste was gated.
    pub risk: PasteRisk,
}

/// Event emitted by [`PasteGate`] for one paste request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteGateEvent {
    /// Deliver `text` to the focused pane now (bracketed flag on the pane).
    Send { text: String, bracketed: bool },
    /// Park the paste behind the confirmation dialog before any byte is sent.
    Confirm(ParkedPaste),
}

/// GPUI entity that decides whether a paste needs confirmation.
///
/// Holds the `terminal.paste_confirmation` config flag and, while a risky paste
/// is parked, the [`ParkedPaste`] awaiting the user's answer.
pub struct PasteGate {
    /// Whether the confirmation gate is enabled in config.
    confirmation_enabled: bool,
    /// The paste currently parked behind the dialog, if any.
    parked: Option<ParkedPaste>,
}

impl EventEmitter<PasteGateEvent> for PasteGate {}

impl PasteGate {
    /// Create a gate with the given `terminal.paste_confirmation` setting.
    #[must_use]
    pub const fn new(confirmation_enabled: bool) -> Self {
        Self { confirmation_enabled, parked: None }
    }

    /// Swap the `terminal.paste_confirmation` setting after a config reload.
    ///
    /// A paste already parked is deliberately left alone: the user is looking
    /// at a modal about specific bytes, and the answer they give must resolve
    /// those bytes whichever way the setting moved underneath.
    pub const fn set_confirmation_enabled(&mut self, enabled: bool) {
        self.confirmation_enabled = enabled;
    }

    /// Whether a paste is currently parked behind the confirmation dialog.
    #[must_use]
    pub const fn is_parked(&self) -> bool {
        self.parked.is_some()
    }

    /// Request a paste of `text` into a pane whose bracketed-paste mode is
    /// `bracketed`.
    ///
    /// Emits [`PasteGateEvent::Confirm`] and parks the request iff the gate is
    /// enabled, the pane has not enabled bracketed paste, and the content
    /// classifies as risky; otherwise emits [`PasteGateEvent::Send`]. Empty
    /// pastes are dropped. The enabled/bracketed checks short-circuit before
    /// classification so the common path adds no work.
    pub fn request(&mut self, text: &str, bracketed: bool, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        if self.confirmation_enabled
            && !bracketed
            && let Some(risk) = classify_paste(text)
        {
            let parked = ParkedPaste { text: text.to_owned(), bracketed, risk };
            self.parked = Some(parked.clone());
            cx.emit(PasteGateEvent::Confirm(parked));
            return;
        }
        cx.emit(PasteGateEvent::Send { text: text.to_owned(), bracketed });
    }

    /// Resume a parked paste after the user confirms: emit [`PasteGateEvent::Send`]
    /// on the original bytes (bypassing the gate) and clear the parked state.
    /// A no-op when nothing is parked.
    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(parked) = self.parked.take() {
            cx.emit(PasteGateEvent::Send { text: parked.text, bracketed: parked.bracketed });
        }
    }

    /// Discard a parked paste after the user cancels. A no-op when nothing is
    /// parked; no bytes are ever sent.
    pub fn cancel(&mut self) {
        self.parked = None;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::{AppContext as _, Entity, TestAppContext};

    use super::{PasteGate, PasteGateEvent, classify_paste};

    // @lat: [[test#GPUI Paste Chunking#Small paste is one frame]]
    #[test]
    fn small_paste_is_one_frame_wrapped_only_when_bracketed() {
        assert_eq!(super::paste_chunks("hello", false), vec![b"hello".to_vec()]);
        assert_eq!(super::paste_chunks("hello", true), vec![b"\x1b[200~hello\x1b[201~".to_vec()]);
        assert!(super::paste_chunks("", true).is_empty());
    }

    // @lat: [[test#GPUI Paste Chunking#Large paste splits under the limit]]
    #[test]
    fn large_paste_splits_into_frames_the_server_accepts() {
        let text = "x".repeat(super::MAX_KEY_INPUT_CHUNK * 2 + 17);
        let chunks = super::paste_chunks(&text, false);
        assert!(chunks.len() > 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= super::MAX_KEY_INPUT_CHUNK));
        assert_eq!(chunks.concat(), text.as_bytes());
    }

    // @lat: [[test#GPUI Paste Chunking#Markers ride the first and last frame]]
    #[test]
    fn bracketed_markers_ride_only_the_first_and_last_frame() {
        let text = "y".repeat(super::MAX_KEY_INPUT_CHUNK * 2);
        let chunks = super::paste_chunks(&text, true);
        assert!(chunks.len() >= 3);
        assert!(chunks.iter().all(|chunk| chunk.len() <= super::MAX_KEY_INPUT_CHUNK));
        let joined = chunks.concat();
        assert!(joined.starts_with(b"\x1b[200~"));
        assert!(joined.ends_with(b"\x1b[201~"));
        // Exactly one marker pair across the whole paste, so the shell sees one
        // contiguous region rather than one per frame.
        assert_eq!(joined.len(), text.len() + 12);
    }

    // @lat: [[client#GPUI Client Spike#Bracketed Paste Gate]]
    #[test]
    fn classify_flags_line_breaks_and_control_but_not_tab() {
        assert!(classify_paste("plain text").is_none());
        assert!(classify_paste("tab\tseparated").is_none());
        assert!(classify_paste("two\nlines").unwrap().has_line_break);
        assert!(classify_paste("bell\x07here").unwrap().has_control);
    }

    /// Collect every event a gate emits into a shared vector.
    fn record_events(
        gate: &Entity<PasteGate>,
        cx: &mut TestAppContext,
    ) -> Arc<Mutex<Vec<PasteGateEvent>>> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        cx.update(|app| {
            app.subscribe(gate, move |_, event: &PasteGateEvent, _| {
                sink.lock().unwrap().push(event.clone());
            })
            .detach();
        });
        cx.update(|_| {});
        events
    }

    // @lat: [[client#GPUI Client Spike#Bracketed Paste Gate]]
    #[gpui::test]
    fn risky_unbracketed_paste_is_confirmed(cx: &mut TestAppContext) {
        let gate = cx.new(|_| PasteGate::new(true));
        let events = record_events(&gate, cx);

        gate.update(cx, |gate, cx| gate.request("rm -rf /\nyes", false, cx));

        gate.read_with(cx, |gate, _| assert!(gate.is_parked()));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], PasteGateEvent::Confirm(_)));
    }

    // @lat: [[client#GPUI Client Spike#Bracketed Paste Gate]]
    #[gpui::test]
    fn bracketed_paste_bypasses_the_gate(cx: &mut TestAppContext) {
        let gate = cx.new(|_| PasteGate::new(true));
        let events = record_events(&gate, cx);

        // Same risky content, but the pane enabled bracketed paste.
        gate.update(cx, |gate, cx| gate.request("rm -rf /\nyes", true, cx));

        gate.read_with(cx, |gate, _| assert!(!gate.is_parked()));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], PasteGateEvent::Send { bracketed: true, .. }));
    }

    // @lat: [[client#GPUI Client Spike#Bracketed Paste Gate]]
    #[gpui::test]
    fn disabled_gate_sends_risky_paste_directly(cx: &mut TestAppContext) {
        let gate = cx.new(|_| PasteGate::new(false));
        let events = record_events(&gate, cx);

        gate.update(cx, |gate, cx| gate.request("multi\nline", false, cx));

        gate.read_with(cx, |gate, _| assert!(!gate.is_parked()));
        assert!(matches!(events.lock().unwrap()[0], PasteGateEvent::Send { bracketed: false, .. }));
    }

    // @lat: [[client#GPUI Client Spike#Bracketed Paste Gate]]
    #[gpui::test]
    fn plain_paste_is_sent_without_confirmation(cx: &mut TestAppContext) {
        let gate = cx.new(|_| PasteGate::new(true));
        let events = record_events(&gate, cx);

        gate.update(cx, |gate, cx| gate.request("echo hello", false, cx));

        gate.read_with(cx, |gate, _| assert!(!gate.is_parked()));
        assert!(matches!(events.lock().unwrap()[0], PasteGateEvent::Send { .. }));
    }

    // @lat: [[client#GPUI Client Spike#Bracketed Paste Gate]]
    #[gpui::test]
    fn confirm_resends_parked_bytes_and_clears(cx: &mut TestAppContext) {
        let gate = cx.new(|_| PasteGate::new(true));
        let events = record_events(&gate, cx);

        gate.update(cx, |gate, cx| gate.request("danger\ncmd", false, cx));
        gate.update(cx, super::PasteGate::confirm);

        gate.read_with(cx, |gate, _| assert!(!gate.is_parked()));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2, "one Confirm then one Send on resume");
        assert!(matches!(events[0], PasteGateEvent::Confirm(_)));
        match &events[1] {
            PasteGateEvent::Send { text, bracketed } => {
                assert_eq!(text, "danger\ncmd");
                assert!(!bracketed);
            }
            PasteGateEvent::Confirm(_) => panic!("resume must Send, not Confirm"),
        }
    }

    // @lat: [[client#GPUI Client Spike#Bracketed Paste Gate]]
    #[gpui::test]
    fn cancel_drops_parked_paste_without_sending(cx: &mut TestAppContext) {
        let gate = cx.new(|_| PasteGate::new(true));
        let events = record_events(&gate, cx);

        gate.update(cx, |gate, cx| gate.request("danger\ncmd", false, cx));
        gate.update(cx, |gate, _| gate.cancel());

        gate.read_with(cx, |gate, _| assert!(!gate.is_parked()));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "cancel sends nothing");
        assert!(matches!(events[0], PasteGateEvent::Confirm(_)));
    }
}
