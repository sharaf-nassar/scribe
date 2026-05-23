//! Per-session OSC 52 clipboard gating state (spec 010 E3).
//!
//! Owns the policy snapshot taken at session creation plus the burst-state
//! machinery so the [`crate::ipc_server`] `SessionEvent::ClipboardStore` /
//! `ClipboardLoad` arms can decide whether to allow, deny, prompt, defer,
//! or reuse a prior decision for each request without consulting a shared
//! state.
//!
//! Wave 4 introduces the full burst-decision-reuse machine from FR-016 /
//! FR-017 / FR-018. While a prompt is in flight, additional same-op
//! requests are deferred onto [`ClipboardBurstState::pending_for_prompt`]
//! (bounded to 64; excess requests fall back to the silent-deny path).
//! After the user resolves the prompt, every deferred request inherits the
//! decision, and the resolved decision is cached in
//! [`ClipboardBurstState::last_decision`] so subsequent same-op requests
//! within `policy.burst_window_ms` reuse the decision without prompting.
//
// @lat: [[server#Sessions#Clipboard Gating]]

use std::time::Instant;

use scribe_common::config::ClipboardPolicyConfig;
use scribe_common::protocol::{ClipboardDecision, ClipboardOp, ClipboardSelection, PromptId};

/// Closure shape produced by `alacritty_terminal` for OSC 52 read replies.
/// Mirror of the private alias in `scribe_pty::event_listener`; lives here
/// so [`DeferredRequest`] can hold a parked formatter without crossing the
/// `clipboard_state` ⇄ `ipc_server` module boundary.
pub type ClipboardReplyFormatter = std::sync::Arc<dyn Fn(&str) -> String + Sync + Send + 'static>;

/// Maximum number of OSC 52 requests that may be parked on a single open
/// prompt (research decision 5). A tmux-style burst inside a prompt window
/// is typically 1–20 ops; 64 leaves comfortable head-room while bounding
/// memory and drain cost. Requests beyond the cap fall back to the silent-
/// deny path (writes drop, reads reply empty).
pub const MAX_PENDING_FOR_PROMPT: usize = 64;

/// A queued OSC 52 request waiting for an in-flight prompt to resolve.
/// Each entry captures enough state to replay the resolved decision: write
/// requests carry the payload (already size-checked at enqueue time); read
/// requests carry the alacritty formatter so the empty / bridge-read reply
/// path can build the OSC 52 wire reply later.
pub struct DeferredRequest {
    /// Per-request id allocated at defer time. Carried for diagnostic
    /// symmetry with the originating prompt's `PromptId`; the drain path
    /// reuses the prompt's resolution rather than echoing this id back.
    /// Logged when the request is drained so burst replays are traceable
    /// against the originating defer call.
    pub request_id: PromptId,
    pub op: ClipboardOp,
    pub selection: ClipboardSelection,
    pub payload_for_write: Option<String>,
    pub read_formatter: Option<ClipboardReplyFormatter>,
}

/// Per-session OSC 52 burst-state and policy snapshot (spec 010 data-model E3).
///
/// One instance lives on each `PtyReaderState`; dropped together with the PTY
/// reader task when the session exits. Re-initialized empty on cold-restart
/// handoff per the data-model lifetime contract.
pub struct ClipboardBurstState {
    /// `Some` when a `ServerMessage::ClipboardPromptRequest` has been emitted
    /// to the client and no `ClientMessage::ClipboardPromptResponse` has been
    /// received yet. Same-op requests arriving in this state are deferred
    /// onto `pending_for_prompt`; cross-op requests fall back to the silent-
    /// deny / silent-drop path (cross-op deferring is not in scope for v1).
    pub outstanding_prompt: Option<PromptId>,
    /// Snapshot of the OSC 52 policy taken at session creation. Refreshed
    /// on `ConfigReloaded` via `ClipboardCommand::RefreshPolicy`. Wave 4 may
    /// also mutate this in-memory directly from `handle_clipboard_prompt_response`
    /// on an `AlwaysAllow` / `AlwaysDeny` decision so the next OSC 52 op
    /// sees the new mode immediately (the eventual `ConfigReloaded` from
    /// the disk write is idempotent against the same value).
    pub policy: ClipboardPolicyConfig,
    /// Same-op OSC 52 requests parked while `outstanding_prompt.is_some()`.
    /// Drained and replayed against the resolved decision when the prompt
    /// response arrives (research decision 5; FR-016 deferral semantics).
    /// Bounded to `MAX_PENDING_FOR_PROMPT`; overflow falls back to silent-
    /// deny / silent-drop with a single `debug!` log per overflow event.
    pub pending_for_prompt: Vec<DeferredRequest>,
    /// Most recent resolved decision plus the timestamp at resolution time
    /// (FR-017 reuse source). Subsequent same-op OSC 52 events within
    /// `policy.burst_window_ms` reuse this decision without prompting the
    /// user; events outside the window fall through to a fresh prompt.
    /// `DenyOnce` / `AlwaysDeny` are reused identically to their allow
    /// counterparts so a tmux-style "no, deny everything" choice silences
    /// the burst rather than re-prompting per op.
    pub last_decision: Option<(ClipboardOp, ClipboardDecision, Instant)>,
}

impl ClipboardBurstState {
    /// Build a fresh per-session state from a policy snapshot.
    #[must_use]
    pub fn new(policy: ClipboardPolicyConfig) -> Self {
        Self {
            outstanding_prompt: None,
            policy,
            pending_for_prompt: Vec::new(),
            last_decision: None,
        }
    }

    /// Try to push a request onto the deferred queue. Returns `true` if
    /// the request was enqueued; `false` when the cap is reached and the
    /// caller must fall back to the silent-deny / silent-drop path.
    #[must_use]
    pub fn try_defer(&mut self, request: DeferredRequest) -> bool {
        if self.pending_for_prompt.len() >= MAX_PENDING_FOR_PROMPT {
            return false;
        }
        self.pending_for_prompt.push(request);
        true
    }

    /// Drain every deferred request out of the queue, leaving it empty.
    #[must_use]
    pub fn drain_pending(&mut self) -> Vec<DeferredRequest> {
        std::mem::take(&mut self.pending_for_prompt)
    }

    /// Returns the cached decision for `op` if one exists AND the burst
    /// window has not yet elapsed. Otherwise returns `None` and the caller
    /// must open a fresh prompt. A zero `burst_window_ms` value disables
    /// reuse entirely (per the data-model E1 clamp range).
    #[must_use]
    pub fn reusable_decision(&self, op: ClipboardOp) -> Option<ClipboardDecision> {
        let (cached_op, decision, ts) = self.last_decision.as_ref()?;
        if *cached_op != op {
            return None;
        }
        let window = std::time::Duration::from_millis(self.policy.burst_window_ms);
        if window.is_zero() {
            return None;
        }
        if ts.elapsed() >= window {
            return None;
        }
        Some(*decision)
    }

    /// Cache `decision` for `op` at "now". Called by the prompt-response
    /// handler after it resolves the user's choice (including denies).
    pub fn record_decision(&mut self, op: ClipboardOp, decision: ClipboardDecision) {
        self.last_decision = Some((op, decision, Instant::now()));
    }
}
