//! Modal dialog suite for the GPUI client rebuild.
//!
//! Ports the winit client's GPU-painted modals —
//! [`crate::close_dialog`](../../scribe-client/src/close_dialog.rs) equivalent
//! ([`CloseDialog`]), the update-confirmation dialog ([`UpdateDialog`]), the
//! risky-paste gate ([`PasteConfirmationDialog`]), the OSC 52 clipboard-policy
//! prompt ([`ClipboardDialog`]), and the disallowed-URI-scheme prompt
//! ([`DisallowedSchemeDialog`]) — into display-independent state models plus a
//! single GPUI [`DialogView`] entity that lowers any of them onto a themed,
//! rounded, drop-shadowed modal (the same split the [`crate::command_palette`]
//! port uses).
//!
//! The feature-014 LAN device-approval prompt joins them as a sixth model. Its
//! state lives in [`crate::lan_approval`] rather than here — the request's
//! display fields and the Decline-default focus are ported from the winit
//! overlay — and [`AnyDialog::LanApproval`] wraps it so it inherits this modal's
//! backdrop, focus cycling, and click activation unchanged.
//!
//! Each model keeps the winit dialog's parity-critical behaviour byte-for-byte:
//! the button set and their labels, the **safe default focus** (Cancel / Deny /
//! Later — pressing Enter on an unexpected prompt never performs the risky
//! action), Tab/Shift+Tab focus cycling, and the derived title/body copy. The
//! paste gate additionally reuses [`crate::paste::classify_paste`] and renders
//! the parked content in caret notation so a malicious control sequence in the
//! preview can never drive the terminal (spec 011 FR-005). The winit
//! GPU quad painting and pixel hit-testing are dropped in favour of
//! GPUI flex layout and `on_click` listeners.

use gpui::{Context, EventEmitter, FocusHandle, Rgba, div, prelude::*, px};
use scribe_common::protocol::{ClipboardDecision, ClipboardOp, ClipboardSelection, PromptId};
use scribe_common::theme::ChromeColors;

use crate::lan_approval::{LanApprovalAction, LanApprovalDialog};
use crate::paste::ParkedPaste;
use crate::tab_bar::srgba;

// ---------------------------------------------------------------------------
// Shared preview helpers (ported from the winit paste-confirmation dialog).
// ---------------------------------------------------------------------------

/// Maximum number of preview lines shown in a dialog body; extra lines collapse
/// into a `… (+N more lines)` trailer.
const MAX_PREVIEW_LINES: usize = 8;

/// Maximum number of columns each preview/URI/payload line is truncated to.
const MAX_PREVIEW_COLS: usize = 56;

/// Number of spaces a tab is rendered as in a preview (legible, never a raw
/// control byte).
const TAB_PREVIEW: &str = "  ";

/// Render `content` as a caret-escaped, per-line-truncated preview.
///
/// Splits on `'\n'`, takes at most [`MAX_PREVIEW_LINES`] lines (appending a
/// `… (+N more lines)` summary when there are more), replaces every
/// control/escape byte with caret notation, renders tabs as spaces, and
/// truncates each rendered line to [`MAX_PREVIEW_COLS`] columns. A raw control
/// byte is never emitted into the returned strings (FR-005).
fn caret_preview(content: &str) -> Vec<String> {
    let raw_lines: Vec<&str> = content.split('\n').collect();
    let total = raw_lines.len();
    let shown = total.min(MAX_PREVIEW_LINES);

    let mut out: Vec<String> = Vec::with_capacity(shown + 1);
    for line in raw_lines.iter().take(shown) {
        out.push(truncate_for_display(&escape_line(line), MAX_PREVIEW_COLS));
    }
    if total > shown {
        let more = total - shown;
        let plural = if more == 1 { "line" } else { "lines" };
        out.push(format!("… (+{more} more {plural})"));
    }
    out
}

/// Replace control/escape characters in a single line with caret notation,
/// leaving printable characters untouched. Never emits a raw control byte.
fn escape_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for ch in line.chars() {
        match ch {
            '\t' => out.push_str(TAB_PREVIEW),
            c if c.is_control() => out.push_str(&caret_escape(c)),
            c => out.push(c),
        }
    }
    out
}

/// Render a single control character in caret / unicode-escape notation.
///
/// DEL (`0x7F`) → `^?`; C0 (`0x00..=0x1F`) → `^` + `(byte ^ 0x40)` (e.g. ESC →
/// `^[`); C1 and anything else → `\u{NN}`.
fn caret_escape(c: char) -> String {
    let code = c as u32;
    match code {
        0x7F => String::from("^?"),
        0x00..=0x1F => {
            let caret = u8::try_from(code).map_or(b'?', |b| b ^ 0x40);
            format!("^{}", caret as char)
        }
        _ => format!("\\u{{{code:02X}}}"),
    }
}

/// Shrink `text` to fit `max_cols` columns by keeping a head and tail slice with
/// `...` between them.
///
/// Showing both halves (rather than head-only) keeps the start and the end of a
/// long line legible — important for a URI where a domain-confusion suffix would
/// otherwise be hidden, and for a paste preview where a trailing control
/// sequence must stay visible. Char-based, so a multibyte codepoint never
/// splits.
fn truncate_for_display(text: &str, max_cols: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_cols {
        return text.to_owned();
    }
    if max_cols <= 3 {
        return chars.into_iter().take(max_cols).collect();
    }
    let budget = max_cols.saturating_sub(3);
    let head_chars = budget.div_ceil(2);
    let tail_chars = budget - head_chars;
    let mut out: String = chars.iter().take(head_chars).collect();
    out.push_str("...");
    out.extend(chars.iter().skip(chars.len() - tail_chars));
    out
}

// ---------------------------------------------------------------------------
// Spec + tones (what the generic view renders).
// ---------------------------------------------------------------------------

/// Visual emphasis of a dialog button, mapping to the winit per-button colour
/// scheme: the accent highlight, the warm-red destructive treatment reserved for
/// proceed-anyway / kill actions, or the subtle default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonTone {
    /// Subtle low-contrast treatment (Cancel, Later, Deny).
    Normal,
    /// Themed accent highlight (Quit Scribe, Update Now).
    Accent,
    /// Warm-red destructive treatment (Kill Window, Open Anyway, Allow).
    Danger,
}

/// One rendered button: its label and its emphasis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogButton {
    /// The button label as shown.
    pub label: String,
    /// The button's visual emphasis.
    pub tone: ButtonTone,
}

impl DialogButton {
    fn new(label: &str, tone: ButtonTone) -> Self {
        Self { label: label.to_owned(), tone }
    }
}

/// A fully-resolved description of a modal: title, body lines, buttons in render
/// order, and the index of the currently focused button. The [`DialogView`]
/// renders purely from this, so every model is testable by asserting its
/// [`DialogSpec`] without a live window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogSpec {
    /// Centred dialog title.
    pub title: String,
    /// Body lines (an empty string renders as a blank spacer row).
    pub body: Vec<String>,
    /// Buttons in left-to-right render order.
    pub buttons: Vec<DialogButton>,
    /// Index of the keyboard-focused button within `buttons`.
    pub focused: usize,
}

// ---------------------------------------------------------------------------
// Close dialog (quit / kill / cancel).
// ---------------------------------------------------------------------------

/// What the user chose in the close dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    /// Quit Scribe (all windows close, sessions preserved).
    QuitAll,
    /// Kill this window only (its sessions terminate).
    CloseWindow,
    /// Dismiss the dialog and do nothing (default focus).
    Cancel,
}

/// Close-dialog state: the active-session count shown in the warning plus the
/// focused button, defaulting to the safe `Cancel`.
pub struct CloseDialog {
    session_count: usize,
    focused: usize,
}

impl CloseDialog {
    /// Order of the three actions, matching the render order.
    const ACTIONS: [CloseAction; 3] =
        [CloseAction::QuitAll, CloseAction::CloseWindow, CloseAction::Cancel];

    /// Build a close dialog warning about `session_count` active sessions.
    #[must_use]
    pub fn new(session_count: usize) -> Self {
        // Cancel (index 2) is the safe default focus.
        Self { session_count, focused: 2 }
    }

    fn spec(&self) -> DialogSpec {
        let mut body = vec![
            String::from("Quit Scribe"),
            String::from("  Close all windows. Sessions are"),
            String::from("  preserved and can be reattached."),
            String::new(),
            String::from("Kill Window"),
            String::from("  Close this window only. Its"),
            String::from("  sessions will be terminated."),
        ];
        if self.session_count > 0 {
            body.push(String::new());
            body.push(format!("  {} active session(s) will be lost.", self.session_count));
        }
        DialogSpec {
            title: String::from("Close Scribe"),
            body,
            buttons: vec![
                DialogButton::new("Quit Scribe", ButtonTone::Accent),
                DialogButton::new("Kill Window", ButtonTone::Danger),
                DialogButton::new("Cancel", ButtonTone::Normal),
            ],
            focused: self.focused,
        }
    }
}

// ---------------------------------------------------------------------------
// Update dialog (install-available / restart-required, incl. helper cold
// restart).
// ---------------------------------------------------------------------------

/// Which update flow the dialog is confirming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateDialogKind {
    /// Confirm download/install of a newly available update (live reload keeps
    /// sessions).
    InstallAvailable,
    /// Confirm a deferred cold restart after an install completed (closes
    /// sessions — the helper cold-restart path).
    RestartRequired,
}

/// What the user chose in the update dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateAction {
    /// Activate the primary button (Update Now / Continue).
    Primary,
    /// Activate the secondary button (Later / Cancel) — the safe default.
    Secondary,
}

/// Update-dialog state: which flow it confirms plus the version string.
pub struct UpdateDialog {
    kind: UpdateDialogKind,
    version: String,
    focused: usize,
}

impl UpdateDialog {
    const ACTIONS: [UpdateAction; 2] = [UpdateAction::Primary, UpdateAction::Secondary];

    /// Build the install-available confirmation for `version`.
    #[must_use]
    pub fn new_install(version: String) -> Self {
        Self { kind: UpdateDialogKind::InstallAvailable, version, focused: 0 }
    }

    /// Build the restart-required (helper cold-restart) confirmation for
    /// `version`.
    #[must_use]
    pub fn new_restart_required(version: String) -> Self {
        Self { kind: UpdateDialogKind::RestartRequired, version, focused: 0 }
    }

    /// Which update flow this dialog is confirming.
    #[must_use]
    pub fn kind(&self) -> UpdateDialogKind {
        self.kind
    }

    fn spec(&self) -> DialogSpec {
        let (title, body, labels) = match self.kind {
            UpdateDialogKind::InstallAvailable => (
                "Update Available",
                vec![
                    format!("Version {} is ready to install.", self.version),
                    String::new(),
                    String::from("Your terminal sessions will be preserved"),
                    String::from("during the update via live reload."),
                ],
                ["Update Now", "Later"],
            ),
            UpdateDialogKind::RestartRequired => (
                "Restart Required",
                vec![
                    format!("Version {} has been installed.", self.version),
                    String::new(),
                    String::from("Applying it now requires a cold restart,"),
                    String::from("which will close all open terminal sessions."),
                ],
                ["Continue", "Cancel"],
            ),
        };
        DialogSpec {
            title: title.to_owned(),
            body,
            buttons: vec![
                DialogButton::new(labels[0], ButtonTone::Accent),
                DialogButton::new(labels[1], ButtonTone::Normal),
            ],
            focused: self.focused,
        }
    }
}

// ---------------------------------------------------------------------------
// Paste-confirmation dialog (risky paste gate).
// ---------------------------------------------------------------------------

/// What the user chose in the paste-confirmation dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteConfirmationAction {
    /// Deliver the parked paste to the focused pane (byte-identical).
    Paste,
    /// Drop the parked paste; send nothing (default focus).
    Cancel,
}

/// Paste-confirmation state, parking the [`ParkedPaste`] while awaiting the
/// user's choice. `Cancel` is the default focus.
pub struct PasteConfirmationDialog {
    parked: ParkedPaste,
    focused: usize,
}

impl PasteConfirmationDialog {
    const ACTIONS: [PasteConfirmationAction; 2] =
        [PasteConfirmationAction::Cancel, PasteConfirmationAction::Paste];

    /// Park `parked` behind the confirmation dialog.
    #[must_use]
    pub fn new(parked: ParkedPaste) -> Self {
        Self { parked, focused: 0 }
    }

    /// Consume the dialog, returning the parked paste so the caller can resume
    /// byte-identical delivery on confirm.
    #[must_use]
    pub fn into_parked_paste(self) -> ParkedPaste {
        self.parked
    }

    /// Derive the human-readable reason line from the parked risk,
    /// distinguishing the multiline-only, control-only, and both cases.
    fn reason_line(&self) -> String {
        let content = &self.parked.text;
        let risk = self.parked.risk;
        let line_count = content.matches('\n').count() + 1;
        let control_count = if risk.has_control {
            content
                .chars()
                .filter(|c| c.is_control() && *c != '\t' && *c != '\n' && *c != '\r')
                .count()
        } else {
            0
        };

        match (risk.has_line_break, risk.has_control) {
            (true, true) => {
                let lines_word = if line_count == 1 { "line" } else { "lines" };
                let ctrl_word =
                    if control_count == 1 { "control character" } else { "control characters" };
                format!("{line_count} {lines_word} · {control_count} {ctrl_word}")
            }
            (true, false) => {
                let lines_word = if line_count == 1 { "line" } else { "lines" };
                format!("{line_count} {lines_word}")
            }
            (false, true) => {
                if control_count == 1 {
                    String::from("contains a control character")
                } else {
                    String::from("contains control characters")
                }
            }
            (false, false) => String::from("risky paste"),
        }
    }

    fn spec(&self) -> DialogSpec {
        let mut body = vec![self.reason_line(), String::new()];
        body.extend(caret_preview(&self.parked.text));
        DialogSpec {
            title: String::from("Confirm Paste"),
            body,
            buttons: vec![
                DialogButton::new("Cancel", ButtonTone::Normal),
                DialogButton::new("Paste", ButtonTone::Normal),
            ],
            focused: self.focused,
        }
    }
}

// ---------------------------------------------------------------------------
// Clipboard dialog (OSC 52 four-button policy).
// ---------------------------------------------------------------------------

/// User decision from the OSC 52 confirmation dialog. The `Always*` pair
/// persists the matching `terminal.clipboard.{read,write}_mode` axis in addition
/// to resolving the in-flight prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardDialogAction {
    /// Allow this single request through.
    AllowOnce,
    /// Deny this single request silently (default focus).
    DenyOnce,
    /// Allow this request and persist the matching axis to `"allow"`.
    AlwaysAllow,
    /// Deny this request and persist the matching axis to `"deny"`.
    AlwaysDeny,
}

impl ClipboardDialogAction {
    /// The wire decision this button sends back as
    /// `ClientMessage::ClipboardPromptResponse`.
    ///
    /// The mapping lives here rather than at the shell's routing site so the
    /// button order and the protocol enum can only ever be paired in one place.
    #[must_use]
    pub const fn decision(self) -> ClipboardDecision {
        match self {
            Self::AllowOnce => ClipboardDecision::AllowOnce,
            Self::DenyOnce => ClipboardDecision::DenyOnce,
            Self::AlwaysAllow => ClipboardDecision::AlwaysAllow,
            Self::AlwaysDeny => ClipboardDecision::AlwaysDeny,
        }
    }
}

/// Clipboard-dialog state for one OSC 52 request. `Deny once` is the default
/// focus; the four buttons render `Deny once`, `Always deny`, `Allow once`,
/// `Always allow` left-to-right.
pub struct ClipboardDialog {
    request_id: PromptId,
    op: ClipboardOp,
    selection: ClipboardSelection,
    preview: Option<String>,
    focused: usize,
}

impl ClipboardDialog {
    const ACTIONS: [ClipboardDialogAction; 4] = [
        ClipboardDialogAction::DenyOnce,
        ClipboardDialogAction::AlwaysDeny,
        ClipboardDialogAction::AllowOnce,
        ClipboardDialogAction::AlwaysAllow,
    ];

    /// Build a clipboard confirmation dialog for the given OSC 52 request.
    #[must_use]
    pub fn new(
        request_id: PromptId,
        op: ClipboardOp,
        selection: ClipboardSelection,
        preview: Option<String>,
    ) -> Self {
        // Index 0 (`Deny once`) is the safe default focus (FR-005).
        Self { request_id, op, selection, preview, focused: 0 }
    }

    /// Wire-side `request_id` the response must echo back to the server.
    #[must_use]
    pub fn request_id(&self) -> PromptId {
        self.request_id
    }

    /// The OSC 52 op (read or write) — drives which config axis an `Always*`
    /// choice persists.
    #[must_use]
    pub fn op(&self) -> ClipboardOp {
        self.op
    }

    fn spec(&self) -> DialogSpec {
        let selection_word = match self.selection {
            ClipboardSelection::Clipboard => "clipboard",
            ClipboardSelection::Primary => "primary selection",
        };
        let (title, intro) = match self.op {
            ClipboardOp::Read => (
                "Allow clipboard read?",
                format!("A program in this terminal wants to read the {selection_word}."),
            ),
            ClipboardOp::Write => (
                "Allow clipboard write?",
                format!("A program in this terminal wants to overwrite the {selection_word}."),
            ),
        };
        let mut body =
            vec![intro, String::new(), String::from("Allow only if you recognise this action.")];
        if let Some(preview) = self.preview.as_deref() {
            body.push(String::new());
            body.push(String::from("Payload preview:"));
            body.push(truncate_for_display(preview, MAX_PREVIEW_COLS));
        }
        DialogSpec {
            title: title.to_owned(),
            body,
            buttons: vec![
                DialogButton::new("Deny once", ButtonTone::Normal),
                DialogButton::new("Always deny", ButtonTone::Normal),
                DialogButton::new("Allow once", ButtonTone::Danger),
                DialogButton::new("Always allow", ButtonTone::Danger),
            ],
            focused: self.focused,
        }
    }
}

// ---------------------------------------------------------------------------
// Disallowed-scheme dialog (OSC 8 outside the allowlist).
// ---------------------------------------------------------------------------

/// What the user chose in the disallowed-scheme confirmation dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisallowedSchemeAction {
    /// Proceed with opening the URI.
    OpenAnyway,
    /// Dismiss without opening anything (default focus).
    Cancel,
}

/// Disallowed-scheme state: the pending URI (preserved verbatim for activation)
/// and its scheme. `Cancel` is the default focus.
pub struct DisallowedSchemeDialog {
    pending_uri: String,
    scheme: String,
    focused: usize,
}

impl DisallowedSchemeDialog {
    const ACTIONS: [DisallowedSchemeAction; 2] =
        [DisallowedSchemeAction::Cancel, DisallowedSchemeAction::OpenAnyway];

    /// Build a dialog gating `uri` whose `scheme` is outside the allowlist.
    #[must_use]
    pub fn new(uri: String, scheme: String) -> Self {
        Self { pending_uri: uri, scheme, focused: 0 }
    }

    /// Take the verbatim URI, consuming the dialog state.
    #[must_use]
    pub fn into_pending_uri(self) -> String {
        self.pending_uri
    }

    fn spec(&self) -> DialogSpec {
        DialogSpec {
            title: String::from("Unsafe URI Scheme"),
            body: vec![
                format!("Scheme `{}:` is normally blocked.", self.scheme),
                String::new(),
                String::from("Open the following URI anyway?"),
                String::new(),
                truncate_for_display(&self.pending_uri, MAX_PREVIEW_COLS),
            ],
            buttons: vec![
                DialogButton::new("Cancel", ButtonTone::Normal),
                DialogButton::new("Open Anyway", ButtonTone::Danger),
            ],
            focused: self.focused,
        }
    }
}

// ---------------------------------------------------------------------------
// LAN device-approval prompt (feature 014, owning side).
// ---------------------------------------------------------------------------

/// Resolve the ported [`LanApprovalDialog`] state into the shape this view
/// renders from.
///
/// The model owns the parity-critical parts — the Decline-default focus, the
/// word-wrapped "who wants control" / fingerprint / name-collision copy — so all
/// that is added here is the button emphasis: Approve carries the destructive
/// tone because it writes a `TrustedDevice` and admits a machine that has so far
/// been shown nothing (SEC-001/002), exactly like "Allow" on the clipboard
/// prompt.
fn lan_approval_spec(dialog: &LanApprovalDialog) -> DialogSpec {
    let [decline, approve] = LanApprovalDialog::button_labels();
    DialogSpec {
        title: LanApprovalDialog::title().to_owned(),
        body: dialog.body_lines(),
        buttons: vec![
            DialogButton::new(decline, ButtonTone::Normal),
            DialogButton::new(approve, ButtonTone::Danger),
        ],
        focused: dialog.focused_index(),
    }
}

// ---------------------------------------------------------------------------
// AnyDialog / DialogOutcome (uniform driver over the six models).
// ---------------------------------------------------------------------------

/// The resolved choice from any modal, tagged by dialog so the shell routes it
/// to the matching handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogOutcome {
    /// The close dialog's choice.
    Close(CloseAction),
    /// The update dialog's choice.
    Update(UpdateAction),
    /// The paste-confirmation dialog's choice.
    Paste(PasteConfirmationAction),
    /// The clipboard dialog's choice.
    Clipboard(ClipboardDialogAction),
    /// The disallowed-scheme dialog's choice.
    DisallowedScheme(DisallowedSchemeAction),
    /// The feature-014 LAN device-approval prompt's choice.
    LanApproval(LanApprovalAction),
}

/// One of the six modal state models, wrapped so a single [`DialogView`] can
/// drive focus, rendering, and activation uniformly.
pub enum AnyDialog {
    /// Quit / kill / cancel.
    Close(CloseDialog),
    /// Install-available / restart-required.
    Update(UpdateDialog),
    /// Risky-paste gate.
    Paste(PasteConfirmationDialog),
    /// OSC 52 clipboard policy.
    Clipboard(ClipboardDialog),
    /// Disallowed OSC 8 scheme.
    DisallowedScheme(DisallowedSchemeDialog),
    /// Feature-014 LAN device approval, pushed by this machine's own server.
    LanApproval(LanApprovalDialog),
}

impl AnyDialog {
    /// Fully-resolved description the view renders from.
    #[must_use]
    pub fn spec(&self) -> DialogSpec {
        match self {
            Self::Close(d) => d.spec(),
            Self::Update(d) => d.spec(),
            Self::Paste(d) => d.spec(),
            Self::Clipboard(d) => d.spec(),
            Self::DisallowedScheme(d) => d.spec(),
            Self::LanApproval(d) => lan_approval_spec(d),
        }
    }

    fn focused(&self) -> usize {
        match self {
            Self::Close(d) => d.focused,
            Self::Update(d) => d.focused,
            Self::Paste(d) => d.focused,
            Self::Clipboard(d) => d.focused,
            Self::DisallowedScheme(d) => d.focused,
            Self::LanApproval(d) => d.focused_index(),
        }
    }

    fn button_count(&self) -> usize {
        match self {
            Self::Close(_) => CloseDialog::ACTIONS.len(),
            Self::Update(_) => UpdateDialog::ACTIONS.len(),
            Self::Paste(_) => PasteConfirmationDialog::ACTIONS.len(),
            Self::Clipboard(_) => ClipboardDialog::ACTIONS.len(),
            Self::DisallowedScheme(_) => DisallowedSchemeDialog::ACTIONS.len(),
            Self::LanApproval(_) => LanApprovalDialog::ACTIONS.len(),
        }
    }

    fn set_focused(&mut self, idx: usize) {
        match self {
            Self::Close(d) => d.focused = idx,
            Self::Update(d) => d.focused = idx,
            Self::Paste(d) => d.focused = idx,
            Self::Clipboard(d) => d.focused = idx,
            Self::DisallowedScheme(d) => d.focused = idx,
            Self::LanApproval(d) => d.set_focused_index(idx),
        }
    }

    /// Cycle focus to the next button (wrapping).
    pub fn focus_next(&mut self) {
        let count = self.button_count();
        self.set_focused((self.focused() + 1) % count);
    }

    /// Cycle focus to the previous button (wrapping).
    pub fn focus_prev(&mut self) {
        let count = self.button_count();
        self.set_focused((self.focused() + count - 1) % count);
    }

    /// The outcome for the button at `idx`, if any (mouse-click activation).
    #[must_use]
    pub fn action_at(&self, idx: usize) -> Option<DialogOutcome> {
        match self {
            Self::Close(_) => CloseDialog::ACTIONS.get(idx).copied().map(DialogOutcome::Close),
            Self::Update(_) => UpdateDialog::ACTIONS.get(idx).copied().map(DialogOutcome::Update),
            Self::Paste(_) => {
                PasteConfirmationDialog::ACTIONS.get(idx).copied().map(DialogOutcome::Paste)
            }
            Self::Clipboard(_) => {
                ClipboardDialog::ACTIONS.get(idx).copied().map(DialogOutcome::Clipboard)
            }
            Self::DisallowedScheme(_) => DisallowedSchemeDialog::ACTIONS
                .get(idx)
                .copied()
                .map(DialogOutcome::DisallowedScheme),
            Self::LanApproval(_) => {
                LanApprovalDialog::ACTIONS.get(idx).copied().map(DialogOutcome::LanApproval)
            }
        }
    }

    /// The outcome for the currently focused button (Enter / activate). The
    /// focused index is always in bounds, but falling back to the safe
    /// [`cancel`](Self::cancel) action keeps this total without a panic.
    #[must_use]
    pub fn confirm(&self) -> DialogOutcome {
        self.action_at(self.focused()).unwrap_or_else(|| self.cancel())
    }

    /// The outcome for dismissing the dialog (Esc / backdrop click) — always the
    /// safe action: Cancel / Later / Deny once.
    #[must_use]
    pub fn cancel(&self) -> DialogOutcome {
        match self {
            Self::Close(_) => DialogOutcome::Close(CloseAction::Cancel),
            Self::Update(_) => DialogOutcome::Update(UpdateAction::Secondary),
            Self::Paste(_) => DialogOutcome::Paste(PasteConfirmationAction::Cancel),
            Self::Clipboard(_) => DialogOutcome::Clipboard(ClipboardDialogAction::DenyOnce),
            Self::DisallowedScheme(_) => {
                DialogOutcome::DisallowedScheme(DisallowedSchemeAction::Cancel)
            }
            // Esc / a backdrop click on an approval prompt is a REFUSAL, not a
            // dismissal: the connection is held open on the server until this
            // reply arrives, so walking away must reveal nothing (FR-004/006).
            Self::LanApproval(_) => DialogOutcome::LanApproval(LanApprovalAction::Decline),
        }
    }
}

// ---------------------------------------------------------------------------
// Colours.
// ---------------------------------------------------------------------------

/// Theme-derived colours for the modal chrome, mirroring the winit dialogs'
/// `DialogColors` mapping (accent for the primary, warm red for destructive).
#[derive(Clone, Copy)]
pub struct DialogColors {
    /// Full-viewport dimming backdrop.
    pub backdrop: Rgba,
    /// Modal box background.
    pub dialog_bg: Rgba,
    /// Modal border.
    pub border: Rgba,
    /// Separator rule above the buttons.
    pub separator: Rgba,
    /// Title text.
    pub title_fg: Rgba,
    /// Body text.
    pub body_fg: Rgba,
    /// Idle button text.
    pub button_fg: Rgba,
    /// Idle button background.
    pub button_bg: Rgba,
    /// Focused/hovered button text.
    pub button_active_fg: Rgba,
    /// Focused/hovered (Normal-tone) button background.
    pub button_active_bg: Rgba,
    /// Focused/hovered (Accent-tone) button background.
    pub accent_bg: Rgba,
    /// Idle destructive button text.
    pub danger_fg: Rgba,
    /// Focused/hovered destructive button background.
    pub danger_bg: Rgba,
}

/// Warm red used for the destructive-action treatment (matches the winit
/// dialogs' `danger_red`).
const DANGER_RED: Rgba = Rgba { r: 0.85, g: 0.25, b: 0.25, a: 1.0 };

impl From<&ChromeColors> for DialogColors {
    fn from(chrome: &ChromeColors) -> Self {
        Self {
            backdrop: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.55 },
            dialog_bg: {
                let mut c = srgba(chrome.tab_bar_active_bg);
                c.a = 0.98;
                c
            },
            border: with_alpha(srgba(chrome.tab_text), 0.20),
            separator: with_alpha(srgba(chrome.tab_text), 0.12),
            title_fg: srgba(chrome.tab_text_active),
            body_fg: srgba(chrome.tab_text),
            button_fg: srgba(chrome.tab_text),
            button_bg: with_alpha(srgba(chrome.tab_text), 0.06),
            button_active_fg: srgba(chrome.tab_bar_bg),
            button_active_bg: with_alpha(srgba(chrome.tab_text), 0.85),
            accent_bg: srgba(chrome.accent),
            danger_fg: DANGER_RED,
            danger_bg: DANGER_RED,
        }
    }
}

/// Return `color` with a replaced alpha channel.
fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    Rgba { a: alpha, ..color }
}

// ---------------------------------------------------------------------------
// View.
// ---------------------------------------------------------------------------

/// Event emitted when the modal resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogEvent {
    /// The user picked an action (button click, Enter, Esc, or backdrop). The
    /// outcome is the safe [`AnyDialog::cancel`] action for Esc/backdrop.
    Chosen(DialogOutcome),
}

/// GPUI modal view rendering any [`AnyDialog`]. Owns the dialog state, the
/// hovered-button index, and a focus handle; emits a single [`DialogEvent`] when
/// resolved and then expects the shell to tear it down.
pub struct DialogView {
    dialog: AnyDialog,
    colors: DialogColors,
    hovered: Option<usize>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DialogEvent> for DialogView {}

impl DialogView {
    /// Build a modal view over `dialog`.
    pub fn new(dialog: AnyDialog, colors: DialogColors, cx: &mut Context<Self>) -> Self {
        Self { dialog, colors, hovered: None, focus_handle: cx.focus_handle() }
    }

    /// Cycle focus to the next button.
    pub fn focus_next(&mut self, cx: &mut Context<Self>) {
        self.dialog.focus_next();
        cx.notify();
    }

    /// Cycle focus to the previous button.
    pub fn focus_prev(&mut self, cx: &mut Context<Self>) {
        self.dialog.focus_prev();
        cx.notify();
    }

    /// Activate the currently focused button (Enter).
    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        cx.emit(DialogEvent::Chosen(self.dialog.confirm()));
    }

    /// Dismiss with the safe action (Esc / backdrop click).
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(DialogEvent::Chosen(self.dialog.cancel()));
    }

    /// Activate the button at `idx` (mouse click).
    pub fn activate(&mut self, idx: usize, cx: &mut Context<Self>) {
        if let Some(outcome) = self.dialog.action_at(idx) {
            cx.emit(DialogEvent::Chosen(outcome));
        }
    }

    fn render_button(
        &self,
        index: usize,
        button: &DialogButton,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        let active = focused || self.hovered == Some(index);
        let (fg, bg) = button_colors(button.tone, active, &colors);
        div()
            .id(("dialog-button", index))
            .px_4()
            .py_1p5()
            .rounded_md()
            .text_sm()
            .text_color(fg)
            .bg(bg)
            .when(focused, |el| el.border_1().border_color(colors.button_active_fg))
            .child(button.label.clone())
            .on_mouse_move(cx.listener(move |this, _, _win, ctx| {
                if this.hovered != Some(index) {
                    this.hovered = Some(index);
                    ctx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _win, ctx| this.activate(index, ctx)))
            .into_any_element()
    }
}

/// Resolve a button's `(fg, bg)` from its tone and active state, mirroring the
/// winit per-dialog `button_colors` helpers.
fn button_colors(tone: ButtonTone, active: bool, colors: &DialogColors) -> (Rgba, Rgba) {
    match (tone, active) {
        (ButtonTone::Accent, true) => (colors.button_active_fg, colors.accent_bg),
        (ButtonTone::Danger, true) => (colors.button_active_fg, colors.danger_bg),
        (ButtonTone::Normal, true) => (colors.button_active_fg, colors.button_active_bg),
        (ButtonTone::Danger, false) => (colors.danger_fg, colors.button_bg),
        (_, false) => (colors.button_fg, colors.button_bg),
    }
}

impl Render for DialogView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        let spec = self.dialog.spec();

        let mut body_rows = Vec::with_capacity(spec.body.len());
        for line in &spec.body {
            if line.is_empty() {
                body_rows.push(div().h(px(8.0)).into_any_element());
            } else {
                body_rows.push(
                    div()
                        .text_sm()
                        .text_color(colors.body_fg)
                        .child(line.clone())
                        .into_any_element(),
                );
            }
        }

        let mut buttons = Vec::with_capacity(spec.buttons.len());
        for (index, button) in spec.buttons.iter().enumerate() {
            buttons.push(self.render_button(index, button, index == spec.focused, cx));
        }

        // Backdrop: a click outside the box dismisses with the safe action.
        div()
            .track_focus(&self.focus_handle)
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.backdrop)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _win, ctx| this.dismiss(ctx)),
            )
            .child(
                div()
                    .min_w(px(420.0))
                    .max_w(px(640.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_5()
                    .bg(colors.dialog_bg)
                    .border_1()
                    .border_color(colors.border)
                    .rounded_lg()
                    .shadow_lg()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .text_color(colors.title_fg)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(spec.title.clone()),
                    )
                    .child(div().flex().flex_col().gap_0p5().children(body_rows))
                    .child(div().h(px(1.0)).w_full().bg(colors.separator))
                    .child(div().flex().flex_wrap().justify_center().gap_2().children(buttons)),
            )
    }
}

#[cfg(test)]
mod tests;
