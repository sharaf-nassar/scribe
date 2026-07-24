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
