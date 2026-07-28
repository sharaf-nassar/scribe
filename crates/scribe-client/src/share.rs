//! Feature 015 (T015/T020) collaborative window-sharing surfaces, ported into the
//! GPUI rebuild.
//!
//! When a window is shared across machines in
//! [`SharedSingleTypist`](scribe_common::config::SharingMode::SharedSingleTypist)
//! the server broadcasts a full-state
//! [`ShareRoster`](scribe_common::protocol::ServerMessage::ShareRoster) on every
//! membership / control change. The client mirrors the latest roster in a
//! [`ShareState`] and derives whether this machine is the current input holder or
//! a **viewer**. A viewer renders the live terminal exactly as normal (never the
//! frozen dimmed [`crate::lost_control::LostControlState`], reserved for
//! `SingleController` displacement) but suppresses its own keystrokes locally and
//! shows a non-intrusive [`ControlHint`].
//!
//! The current holder (or the owner, when control is unheld) answers an incoming
//! request-and-grant
//! [`ControlRequested`](scribe_common::protocol::ServerMessage::ControlRequested)
//! through a [`ControlRequestPrompt`]. Control passing itself is expressed as a
//! [`ControlIntent`] that maps to the frozen v3
//! [`ControlClaim`](scribe_common::protocol::ClientMessage::ControlClaim) /
//! [`ControlRequest`](scribe_common::protocol::ClientMessage::ControlRequest) /
//! [`ControlGrant`](scribe_common::protocol::ClientMessage::ControlGrant)
//! messages. This port keeps that logic and drops the winit GPU
//! painting in favour of the GPUI overlay elements.
//!
//! [`ShareChrome`] is the live-path aggregate the client shell owns: the IPC
//! reader folds `ShareRoster` / `ControlRequested` / `ControlDenied` /
//! `ShareEnded` into it, the key path runs every keystroke through
//! [`ShareChrome::intercept_key`] before the terminal encoder, and the render
//! pass lowers it with [`share_overlay`] plus the status bar's presence badge.

use std::time::{Duration, Instant};

use gpui::{AnyElement, Rgba, div, prelude::*, px};
use scribe_common::config::SharingMode;
use scribe_common::ids::WindowId;
use scribe_common::protocol::{ClientMessage, ParticipantInfo, ShareEndReason};
use scribe_common::theme::ChromeColors;

use crate::status_bar::SharePresenceData;
use crate::tab_bar::srgba;

/// How long a transient control hint / denied notice stays on screen before the
/// idle-wake loop clears it.
pub const HINT_DURATION: Duration = Duration::from_secs(5);

/// A control-passing intent the app emits on the frozen v3 protocol. Kept as a
/// small `PartialEq` enum so the ported logic is testable without constructing a
/// full [`ClientMessage`]; [`ControlIntent::into_message`] performs the mapping.
/// `ControlRequest` remains a serializable alias the server handles as
/// `ControlClaim`, but the client deliberately emits only the modeled variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlIntent {
    /// A participant takes input control directly (`FreeClaim`, or the owner).
    Claim { window_id: WindowId },
    /// A viewer asks for input control (`RequestAndGrant`).
    Request { window_id: WindowId },
    /// The holder/owner answers a pending request: transfer or deny.
    Grant { window_id: WindowId, participant_id: u64, accept: bool },
}

impl ControlIntent {
    /// Lower the intent to its frozen v3 [`ClientMessage`].
    #[must_use]
    pub fn into_message(self) -> ClientMessage {
        match self {
            Self::Claim { window_id } => ClientMessage::ControlClaim { window_id },
            Self::Request { window_id } => ClientMessage::ControlRequest { window_id },
            Self::Grant { window_id, participant_id, accept } => {
                ClientMessage::ControlGrant { window_id, participant_id, accept }
            }
        }
    }
}

/// The latest [`ShareRoster`](scribe_common::protocol::ServerMessage::ShareRoster)
/// for the client's window, mirrored so the UI can derive roles and render the
/// presence badge. Absent on the client whenever the window is not part of a
/// broadcasting share (`SingleController` / solo).
#[derive(Debug, Clone)]
pub struct ShareState {
    /// The shared window's id.
    pub window_id: WindowId,
    /// The complete current participant roster (server order, by join).
    pub participants: Vec<ParticipantInfo>,
    /// The window's active sharing mode.
    pub mode: SharingMode,
    /// The current input-control holder's participant id, or `None` when unheld.
    pub holder: Option<u64>,
}

impl ShareState {
    /// Number of attached participants (owner + remotes).
    #[must_use]
    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }

    /// Whether more than one machine is attached — the trigger for the presence
    /// badge (T024) and the viewer affordances (T015/T020).
    #[must_use]
    pub fn is_multi(&self) -> bool {
        self.participants.len() > 1
    }

    /// The current control holder's roster entry, if any.
    #[must_use]
    pub fn holder_entry(&self) -> Option<&ParticipantInfo> {
        let holder = self.holder?;
        self.participants.iter().find(|p| p.participant_id == holder)
    }

    /// Display label for the current control holder, or `None` when unheld.
    #[must_use]
    pub fn holder_label(&self) -> Option<String> {
        self.holder_entry().map(participant_label)
    }

    /// Whether the local machine currently holds input control.
    #[must_use]
    pub fn local_is_holder(&self) -> bool {
        self.participants.iter().any(|p| p.is_local && p.is_holder)
    }

    /// The control-passing intent a local viewer emits to take control, chosen by
    /// the window's sharing mode: a single-typist share claims directly under
    /// free-claim acquisition, mirroring the winit client's viewer affordance.
    #[must_use]
    pub fn take_control_intent(&self) -> ControlIntent {
        ControlIntent::Claim { window_id: self.window_id }
    }

    /// Whether this share gates input to one typist at a time. Only that mode
    /// suppresses a non-holder's keystrokes; `FreeForAll` lets everyone type and
    /// `SingleController` never broadcasts a roster at all.
    #[must_use]
    pub fn is_single_typist(&self) -> bool {
        matches!(self.mode, SharingMode::SharedSingleTypist)
    }

    /// The roster rows the presence panel draws, in the server's join order.
    #[must_use]
    pub fn roster_rows(&self) -> Vec<RosterRow> {
        self.participants
            .iter()
            .map(|p| RosterRow {
                label: participant_label(p),
                is_local: p.is_local,
                is_holder: self.holder == Some(p.participant_id),
            })
            .collect()
    }
}

/// One rendered line of the share-presence panel: a participant's label plus the
/// two role flags that decorate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterRow {
    /// `device (login)` display label.
    pub label: String,
    /// `true` for the owning machine's own entry.
    pub is_local: bool,
    /// `true` for the current input-control holder.
    pub is_holder: bool,
}

/// How the server names the owning machine's own roster entry. Its label is
/// already self-describing, so [`RosterRow::text`] does not repeat the marker.
const OWN_MACHINE_LABEL: &str = "this machine";

impl RosterRow {
    /// The full row text drawn in the panel, suffixed with its role.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = self.label.clone();
        if self.is_local && self.label != OWN_MACHINE_LABEL {
            text.push_str(" \u{00B7} this machine");
        }
        if self.is_holder {
            text.push_str(" \u{00B7} has control");
        }
        text
    }
}

/// Human-readable label for a roster entry: `device (login)`, or just the device
/// name when the account is unknown (the owner's `Local` entry is named `this
/// machine` server-side, with an empty login).
#[must_use]
pub fn participant_label(p: &ParticipantInfo) -> String {
    if p.login_name.is_empty() {
        p.device_name.clone()
    } else {
        format!("{} ({})", p.device_name, p.login_name)
    }
}

/// A transient, non-intrusive hint shown to a viewer that just pressed a
/// (suppressed) key: it names the control holder and how to take control. Unlike
/// the lost-control banner it does NOT dim or freeze the window — output keeps
/// streaming live behind it (T015/T020).
#[derive(Debug, Clone)]
pub struct ControlHint {
    text: String,
    expires_at: Instant,
}

impl ControlHint {
    /// Build a hint that expires after [`HINT_DURATION`].
    #[must_use]
    pub fn new(text: String) -> Self {
        Self { text, expires_at: Instant::now() + HINT_DURATION }
    }

    /// The hint text drawn in the strip.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The instant this hint should be cleared (for the idle-wake deadline).
    #[must_use]
    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    /// Whether this hint has outlived [`HINT_DURATION`].
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// A pending incoming control request that this client — the current holder, or
/// the owner when control is unheld — must grant or deny (request-and-grant
/// acquisition, T020). Modal while set: Enter grants, Esc denies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlRequestPrompt {
    window_id: WindowId,
    requester_id: u64,
    requester_label: String,
}

impl ControlRequestPrompt {
    /// Build the prompt from the `from` participant of a `ControlRequested`.
    #[must_use]
    pub fn new(window_id: WindowId, from: &ParticipantInfo) -> Self {
        Self {
            window_id,
            requester_id: from.participant_id,
            requester_label: participant_label(from),
        }
    }

    /// The shared window this request targets.
    #[must_use]
    pub fn window_id(&self) -> WindowId {
        self.window_id
    }

    /// The requesting participant's id — the [`ControlIntent::Grant`] target.
    #[must_use]
    pub fn requester_id(&self) -> u64 {
        self.requester_id
    }

    /// Banner headline: `<requester> wants control`.
    #[must_use]
    pub fn headline(&self) -> String {
        format!("{} wants control", self.requester_label)
    }

    /// The grant/deny hint drawn under the headline.
    #[must_use]
    pub fn hint() -> &'static str {
        "Press Enter to grant \u{00B7} Esc to deny"
    }

    /// The control-passing intent for the user's decision: `accept = true`
    /// transfers control to the requester, `false` denies.
    #[must_use]
    pub fn answer(&self, accept: bool) -> ControlIntent {
        ControlIntent::Grant {
            window_id: self.window_id,
            participant_id: self.requester_id,
            accept,
        }
    }
}

/// The framework-neutral key the share surfaces react to. The GPUI shell lowers
/// a `KeyDownEvent` into this shape so the decision table below stays testable
/// without a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareKey {
    /// Grants a pending request, or claims control from the viewer hint.
    Enter,
    /// Denies a pending request.
    Escape,
    /// Anything else — suppressed while a share surface owns the keyboard.
    Other,
}

/// What the shell must do with a keystroke the share surfaces looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareKeyOutcome {
    /// No share surface claims the key; it continues to the terminal encoder.
    Passthrough,
    /// The key was consumed and produced no wire traffic (a suppressed viewer
    /// keystroke, or a key pressed while the grant/deny prompt is modal).
    Suppressed,
    /// The key was consumed and the shell must send this control intent.
    Emit(ControlIntent),
}

/// Live sharing state of the client's window: the mirrored roster, the pending
/// grant/deny prompt, and the transient hint. Owned by the shell behind a mutex,
/// written by the IPC reader and read by the key path and the render pass.
#[derive(Debug, Default)]
pub struct ShareChrome {
    state: Option<ShareState>,
    prompt: Option<ControlRequestPrompt>,
    hint: Option<ControlHint>,
    self_id: Option<u64>,
}

impl ShareChrome {
    /// A client that is not part of any share yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record this connection's own server-assigned participant id, carried by
    /// `Welcome`. Preferred over the roster's `is_local` flag because it matches
    /// exactly even when two entries share a device name.
    pub fn set_self_id(&mut self, participant_id: Option<u64>) {
        self.self_id = participant_id;
    }

    /// Mirror a fresh `ShareRoster`. A roster that drains back to a single
    /// participant tears the share surfaces down, and regaining control clears a
    /// stale "who has control" hint.
    pub fn apply_roster(&mut self, state: ShareState) {
        if !state.is_multi() {
            self.state = None;
            self.prompt = None;
            self.hint = None;
            return;
        }
        if Self::holder_is_local(&state, self.self_id) {
            self.hint = None;
        }
        self.state = Some(state);
    }

    /// Raise the grant/deny prompt for an incoming `ControlRequested`. The server
    /// only routes it to the current holder (or the owner when unheld), so no
    /// local role check is needed.
    pub fn request(&mut self, prompt: ControlRequestPrompt) {
        self.prompt = Some(prompt);
    }

    /// Surface the transient notice for a `ControlDenied` — this client's own
    /// request was refused, or it was cancelled by a holder / mode change.
    pub fn deny(&mut self) {
        self.hint = Some(ControlHint::new(String::from("Control request denied")));
    }

    /// Tear the share surfaces down on `ShareEnded` and leave a transient notice
    /// naming the reason. The frozen displaced UI, when one applies, is driven
    /// separately by `WindowTakenOver`.
    pub fn end(&mut self, reason: ShareEndReason) {
        self.state = None;
        self.prompt = None;
        self.hint = Some(ControlHint::new(share_end_notice(reason).to_owned()));
    }

    /// The status bar's presence badge inputs, or `None` outside a live share.
    #[must_use]
    pub fn presence(&self) -> Option<SharePresenceData> {
        let state = self.state.as_ref()?;
        Some(SharePresenceData {
            participant_count: state.participant_count(),
            holder: state.holder_label(),
        })
    }

    /// Whether this client is a live viewer whose keystrokes are suppressed
    /// locally — attached to a single-typist share it does not hold control of.
    #[must_use]
    pub fn is_viewer(&self) -> bool {
        self.state.as_ref().is_some_and(|state| {
            state.is_single_typist() && !Self::holder_is_local(state, self.self_id)
        })
    }

    /// Whether a grant/deny prompt is currently modal.
    #[must_use]
    pub fn has_prompt(&self) -> bool {
        self.prompt.is_some()
    }

    /// Route a keystroke through the share surfaces ahead of the terminal
    /// encoder.
    ///
    /// The pending prompt is modal: Enter grants, Esc denies, everything else is
    /// swallowed while the decision is open. Otherwise a viewer's first key
    /// raises the take-control hint and is dropped; pressing Enter while that
    /// hint is up claims control (FR-006). A holder — and any client outside a
    /// single-typist share — passes straight through.
    pub fn intercept_key(&mut self, key: ShareKey) -> ShareKeyOutcome {
        if let Some(prompt) = self.prompt.as_ref() {
            let accept = match key {
                ShareKey::Enter => true,
                ShareKey::Escape => false,
                ShareKey::Other => return ShareKeyOutcome::Suppressed,
            };
            let intent = prompt.answer(accept);
            self.prompt = None;
            return ShareKeyOutcome::Emit(intent);
        }
        if !self.is_viewer() {
            return ShareKeyOutcome::Passthrough;
        }
        let hint_active = self.hint.as_ref().is_some_and(|hint| !hint.is_expired());
        if hint_active && key == ShareKey::Enter {
            return self.claim_control();
        }
        self.show_control_hint();
        ShareKeyOutcome::Suppressed
    }

    /// Claim input control of the shared window. The server applies the owner's
    /// acquisition policy, so the client sends the same `ControlClaim` either way
    /// and learns the result from the next roster (granted) or `ControlDenied`.
    fn claim_control(&mut self) -> ShareKeyOutcome {
        let Some(state) = self.state.as_ref() else {
            return ShareKeyOutcome::Suppressed;
        };
        let intent = state.take_control_intent();
        self.hint = Some(ControlHint::new(String::from("Requesting control\u{2026}")));
        ShareKeyOutcome::Emit(intent)
    }

    /// Raise the non-intrusive viewer hint naming the holder and how to take
    /// control.
    fn show_control_hint(&mut self) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let text = state.holder_label().map_or_else(
            || String::from("No one has control \u{2014} press Enter to take control"),
            |holder| format!("{holder} has control \u{2014} press Enter to take control"),
        );
        self.hint = Some(ControlHint::new(text));
    }

    /// Drop an expired hint on the idle-wake boundary. Returns `true` when the
    /// chrome changed, so the caller can repaint exactly once.
    pub fn expire_hint(&mut self) -> bool {
        if self.hint.as_ref().is_some_and(ControlHint::is_expired) {
            self.hint = None;
            return true;
        }
        false
    }

    /// Whether this client holds control, resolved against its own participant id
    /// when the server supplied one and against the roster's `is_local` flag
    /// otherwise. A client outside a single-typist share always "holds" control:
    /// nothing gates its input.
    fn holder_is_local(state: &ShareState, self_id: Option<u64>) -> bool {
        if !state.is_single_typist() {
            return true;
        }
        self_id.map_or_else(|| state.local_is_holder(), |id| state.holder == Some(id))
    }
}

/// The transient notice text for a `ShareEnded` reason.
#[must_use]
pub fn share_end_notice(reason: ShareEndReason) -> &'static str {
    match reason {
        ShareEndReason::OwnerClosed => "Sharing ended \u{2014} the owner closed the window",
        ShareEndReason::WindowClosed => "Sharing ended \u{2014} the shared window closed",
        ShareEndReason::ModeChangedToSingleController => {
            "Sharing ended \u{2014} the owner switched to a single controller"
        }
    }
}

/// Resolved GPUI colours for the share surfaces, derived from the theme chrome
/// so the roster panel, the hint strip, and the grant/deny modal share the
/// sibling overlays' conventions.
#[derive(Clone, Copy)]
pub struct ShareOverlayColors {
    /// Full-viewport dimming backdrop behind the modal prompt.
    pub backdrop: Rgba,
    /// Panel / modal background.
    pub panel_bg: Rgba,
    /// 1px border colour.
    pub border: Rgba,
    /// Heading text.
    pub title_fg: Rgba,
    /// Body and roster-row text.
    pub body_fg: Rgba,
    /// Accent used for the control holder and the prompt's key hint.
    pub accent: Rgba,
}

impl From<&ChromeColors> for ShareOverlayColors {
    fn from(chrome: &ChromeColors) -> Self {
        Self {
            backdrop: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.55 },
            panel_bg: Rgba { a: 0.98, ..srgba(chrome.tab_bar_active_bg) },
            border: Rgba { a: 0.20, ..srgba(chrome.tab_text) },
            title_fg: srgba(chrome.tab_text_active),
            body_fg: srgba(chrome.tab_text),
            accent: srgba(chrome.accent),
        }
    }
}

/// Lower the live [`ShareChrome`] onto the window's overlay layer, or `None`
/// when nothing is shared and no notice is up.
///
/// Three stacked surfaces, each independent: the roster panel pinned under the
/// tab bar, the transient hint strip above the status bands, and — only while a
/// request is pending — the dimmed grant/deny modal.
#[must_use]
pub fn share_overlay(chrome: &ShareChrome, colors: &ShareOverlayColors) -> Option<AnyElement> {
    let roster = chrome.state.as_ref().map(|state| roster_panel(state, colors));
    let hint = chrome
        .hint
        .as_ref()
        .filter(|hint| !hint.is_expired())
        .map(|hint| hint_strip(hint.text(), colors));
    let prompt = chrome.prompt.as_ref().map(|prompt| request_modal(prompt, colors));
    if roster.is_none() && hint.is_none() && prompt.is_none() {
        return None;
    }
    Some(
        div()
            .absolute()
            .inset_0()
            .children(roster)
            .children(hint)
            .children(prompt)
            .into_any_element(),
    )
}

/// The presence panel: a bordered box naming every attached machine and which
/// one currently holds input control.
fn roster_panel(state: &ShareState, colors: &ShareOverlayColors) -> AnyElement {
    let rows: Vec<AnyElement> = state
        .roster_rows()
        .into_iter()
        .map(|row| {
            let color = if row.is_holder { colors.accent } else { colors.body_fg };
            div().text_xs().text_color(color).child(row.text()).into_any_element()
        })
        .collect();
    div()
        .absolute()
        .top(px(44.0))
        .right(px(12.0))
        .min_w(px(220.0))
        .max_w(px(360.0))
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .bg(colors.panel_bg)
        .border_1()
        .border_color(colors.border)
        .rounded(px(4.0))
        .child(
            div()
                .text_xs()
                .text_color(colors.title_fg)
                .child(format!("Shared window \u{00B7} {} attached", state.participant_count())),
        )
        .children(rows)
        .into_any_element()
}

/// The non-intrusive hint strip. Output keeps streaming live behind it — unlike
/// the lost-control banner it neither dims nor freezes the window.
fn hint_strip(text: &str, colors: &ShareOverlayColors) -> AnyElement {
    div()
        .absolute()
        .bottom(px(64.0))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .px_3()
                .py_1()
                .bg(colors.panel_bg)
                .border_1()
                .border_color(colors.border)
                .rounded(px(4.0))
                .text_xs()
                .text_color(colors.title_fg)
                .child(text.to_owned()),
        )
        .into_any_element()
}

/// The modal grant/deny prompt: a dimmed backdrop under a centered box carrying
/// the requester's headline and the Enter/Esc hint.
fn request_modal(prompt: &ControlRequestPrompt, colors: &ShareOverlayColors) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(colors.backdrop)
        .child(
            div()
                .min_w(px(360.0))
                .max_w(px(560.0))
                .flex()
                .flex_col()
                .gap_2()
                .px_5()
                .py_4()
                .bg(colors.panel_bg)
                .border_1()
                .border_color(colors.border)
                .rounded(px(4.0))
                .child(div().text_sm().text_color(colors.title_fg).child(prompt.headline()))
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.accent)
                        .child(ControlRequestPrompt::hint().to_owned()),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests;
