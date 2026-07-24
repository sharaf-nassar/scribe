//! Feature 014 (T018) owning-side LAN device-approval prompt, ported into the
//! GPUI rebuild.
//!
//! When an unknown LAN device completes the mutual-TLS handshake, the owning
//! server holds it pending (revealing NO window or session data) and pushes a
//! [`LanApprovalRequest`](scribe_common::protocol::ServerMessage::LanApprovalRequest)
//! to its OWN local client over the Unix socket. The GPUI chrome renders this as
//! a dialog — the requesting device's name, the trusted network it arrived on,
//! and its identity fingerprint words — with equally-prominent Approve / Decline
//! actions (UX-002). The choice becomes a
//! [`LanApprovalDecision`](scribe_common::protocol::ClientMessage::LanApprovalDecision):
//! Approve writes a `TrustedDevice` and proceeds; Decline refuses and reveals
//! nothing (FR-004/006, SEC-001/002).
//!
//! This port keeps the state model — the pending request's display fields, the
//! **Decline-default** focus, focus cycling, the activation intent, and the
//! word-wrapped body copy — and drops the winit `CellInstance` painting and pixel
//! hit-testing in favour of the GPUI dialog element.

/// What the owning user chose on the approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanApprovalAction {
    /// Trust this device: write a `TrustedDevice` and let it attach.
    Approve,
    /// Refuse this device: reveal nothing and remember nothing (default).
    Decline,
}

/// Width the body prose is word-wrapped to. Matches the winit dialog's interior
/// (60 cols minus two 3-col pads) so the ported copy wraps identically.
pub const BODY_WRAP_COLS: usize = 54;

/// Index of the currently focused button. `Decline` is index 0 and the default
/// focus so the safe choice is pre-selected — pressing Enter on an unexpected
/// prompt never silently grants trust (mirrors the deny-default of the paste,
/// disallowed-scheme, and clipboard dialogs).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonIndex {
    Decline = 0,
    Approve = 1,
}

impl ButtonIndex {
    fn next(self) -> Self {
        match self {
            Self::Decline => Self::Approve,
            Self::Approve => Self::Decline,
        }
    }

    fn to_action(self) -> LanApprovalAction {
        match self {
            Self::Decline => LanApprovalAction::Decline,
            Self::Approve => LanApprovalAction::Approve,
        }
    }
}

/// State for the in-app LAN device-approval overlay. Holds the pending request's
/// display fields plus the `request_id` that correlates the user's decision back
/// to the held connection (data-model `ApprovalRequest`).
pub struct LanApprovalDialog {
    /// Correlates the decision reply with the held connection.
    request_id: u64,
    /// Requesting device's advertised name (display only; never a trust key).
    device_name: String,
    /// The peer's identity fingerprint words (research D8).
    fingerprint_words: String,
    /// The trusted network the request arrived on.
    network_label: String,
    /// `true` when an already-trusted device shares this advertised name — an
    /// informational hint only, added as an extra body line.
    name_collision: bool,
    /// Currently keyboard-focused button. Defaults to `Decline` (index 0).
    focused: ButtonIndex,
}

impl LanApprovalDialog {
    /// Construct a new approval dialog for a pending LAN device.
    #[must_use]
    pub fn new(
        request_id: u64,
        device_name: String,
        fingerprint_words: String,
        network_label: String,
        name_collision: bool,
    ) -> Self {
        Self {
            request_id,
            device_name,
            fingerprint_words,
            network_label,
            name_collision,
            focused: ButtonIndex::Decline,
        }
    }

    /// The `request_id` this prompt answers, echoed in the decision reply.
    #[must_use]
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    /// Cycle focus to the next button (two buttons, so next == prev).
    pub fn focus_next(&mut self) {
        self.focused = self.focused.next();
    }

    /// Cycle focus to the previous button (two buttons, so prev == next).
    pub fn focus_prev(&mut self) {
        self.focused = self.focused.next();
    }

    /// Whether Approve currently holds focus (drives the chrome's highlight).
    #[must_use]
    pub fn approve_focused(&self) -> bool {
        matches!(self.focused, ButtonIndex::Approve)
    }

    /// The action for the currently focused button (Enter / activate).
    #[must_use]
    pub fn confirm(&self) -> LanApprovalAction {
        self.focused.to_action()
    }

    /// The two button labels in render order (Decline, Approve).
    #[must_use]
    pub fn button_labels() -> [&'static str; 2] {
        ["Decline", "Approve"]
    }

    /// The dialog title.
    #[must_use]
    pub fn title() -> &'static str {
        "Approve device?"
    }

    /// Build the body text lines: the primary "who wants control" sentence, the
    /// device's fingerprint words, and — only when the advertised name collides
    /// with an already-trusted device — an informational collision hint. All prose
    /// is word-wrapped so a long name or fingerprint stays inside the box.
    #[must_use]
    pub fn body_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();

        let headline = format!(
            "{} on {} wants to control this machine.",
            self.device_name, self.network_label
        );
        lines.extend(wrap_text(&headline, BODY_WRAP_COLS));

        lines.push(String::new());
        lines.push(String::from("Fingerprint:"));
        lines.extend(wrap_text(&self.fingerprint_words, BODY_WRAP_COLS));

        if self.name_collision {
            lines.push(String::new());
            let hint = format!(
                "You already trust a different device named {} — approve only if you recognize this one.",
                self.device_name
            );
            lines.extend(wrap_text(&hint, BODY_WRAP_COLS));
        }

        lines
    }
}

/// Word-wrap `text` to at most `max_cols` display columns per line, breaking on
/// whitespace and hard-splitting any single token longer than `max_cols`, so a
/// long device name or fingerprint word list can never overflow the dialog box.
/// Char-based throughout, so a multibyte name never splits mid-codepoint.
fn wrap_text(text: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for word in text.split_whitespace() {
        let word_len = word.chars().count();

        if word_len > max_cols {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_len = 0;
            }
            lines.extend(split_long_token(word, max_cols));
            continue;
        }

        let sep = usize::from(!current.is_empty());
        if current_len + sep + word_len > max_cols {
            lines.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_len += 1;
        }
        current.push_str(word);
        current_len += word_len;
    }

    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Split a single overlong token into `max_cols`-column, codepoint-aligned chunks.
/// `max_cols` is always non-zero at the call site.
fn split_long_token(token: &str, max_cols: usize) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    chars.chunks(max_cols).map(|chunk| chunk.iter().collect()).collect()
}

#[cfg(test)]
mod tests;
