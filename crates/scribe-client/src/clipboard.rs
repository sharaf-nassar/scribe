//! Host clipboard bridge for the GPUI client — arboard handle, OSC 52
//! read/write bridging, and Linux primary-selection routing.
//!
//! The OSC 52 flow (spec 010) is a two-hop bridge. The server decides policy
//! and, when a read or write is allowed, forwards it to the client's host
//! clipboard through [`ServerMessage::ClipboardBridgeReadRequest`] /
//! [`ServerMessage::ClipboardBridgeWrite`]. This module owns the client half:
//! the live `arboard` handle plus the pure routing that maps a
//! [`ClipboardSelection`] onto the system pasteboard or the X11 primary
//! selection, and that turns a read into the outbound
//! [`ClientMessage::ClipboardBridgeReadReply`].
//!
//! The transport is decoupled from the live GPUI app the same way the rest of
//! the crate defers runtime wiring: the bridge speaks to a
//! [`ClipboardBackend`] trait so the routing, focus-gate, and reply-message
//! construction are exercised by an in-memory fake in tests, while the shipped
//! [`ArboardClipboard`] backend performs the real X11 / Wayland / pasteboard
//! I/O. Ported byte-for-byte from the legacy client's `App::bridge_read`,
//! `App::bridge_write`, `App::perform_primary_paste`, and
//! `App::set_primary_selection`.
//!
//! [`ServerMessage::ClipboardBridgeReadRequest`]: scribe_common::protocol::ServerMessage
//! [`ServerMessage::ClipboardBridgeWrite`]: scribe_common::protocol::ServerMessage

use std::collections::VecDeque;

use scribe_common::protocol::{
    BridgeError, ClientMessage, ClipboardDecision, ClipboardOp, ClipboardSelection, PromptId,
};

use crate::clipboard_cleanup::{self, CopyTextOptions};

/// Upper bound on server-initiated bridge jobs parked for the foreground.
///
/// The reader thread cannot touch the host clipboard itself (arboard is not
/// thread-safe and the FR-019 focus gate only has a meaning on the window
/// thread), so every bridge message is queued for the next foreground tick. A
/// PTY program can emit OSC 52 far faster than that tick drains, so the queue
/// is bounded and the oldest job is dropped once it is full — the same
/// bounded-queue posture the server takes with `MAX_PENDING_FOR_PROMPT`.
pub const MAX_PENDING_BRIDGE_JOBS: usize = 64;

/// One server-initiated host clipboard job the IPC reader parked for the GPUI
/// foreground to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeJob {
    /// `ServerMessage::ClipboardBridgeWrite` — overwrite `selection` with
    /// `payload`. No reply is expected (OSC 52 has no write ack).
    Write {
        /// Which pasteboard the write targets.
        selection: ClipboardSelection,
        /// Text the PTY-side program asked to place on the clipboard.
        payload: String,
    },
    /// `ServerMessage::ClipboardBridgeReadRequest` — read `selection` and
    /// answer with `ClientMessage::ClipboardBridgeReadReply`.
    Read {
        /// Correlation id the reply must echo.
        request_id: PromptId,
        /// Which pasteboard to read.
        selection: ClipboardSelection,
    },
}

/// A parked `ServerMessage::ClipboardPromptRequest` awaiting a confirmation
/// overlay, kept whole so the raised dialog and its answer share one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardPrompt {
    /// Correlation id the `ClipboardPromptResponse` must echo.
    pub request_id: PromptId,
    /// Whether the PTY program asked to read or to write.
    pub op: ClipboardOp,
    /// Which pasteboard the request targets.
    pub selection: ClipboardSelection,
    /// Head-and-tail truncated write payload preview; always `None` for reads.
    pub preview: Option<String>,
}

/// Cross-thread OSC 52 state shared by the IPC reader and the GPUI window.
///
/// The reader owns none of the surfaces an OSC 52 message needs — the modal is
/// a GPUI entity and the host clipboard is a window-thread resource — so it
/// folds every clipboard message into this aggregate and the foreground tick
/// drains it. `gating` mirrors the `clipboard_gating` bit the server echoed in
/// `Welcome`: until it is on, the server has not agreed to route OSC 52 through
/// this client at all, and any clipboard message that still arrives is dropped
/// rather than acted on (the winit client's identical guard).
#[derive(Debug, Default)]
pub struct ClipboardBridge {
    gating: bool,
    prompt: Option<ClipboardPrompt>,
    jobs: VecDeque<BridgeJob>,
}

impl ClipboardBridge {
    /// Record the `clipboard_gating` capability the server echoed in `Welcome`.
    pub const fn set_gating(&mut self, gating: bool) {
        self.gating = gating;
    }

    /// Whether the server agreed to gate OSC 52 through this client.
    #[must_use]
    pub const fn gating(&self) -> bool {
        self.gating
    }

    /// Park a confirmation request for the window to raise.
    ///
    /// A second prompt cannot be outstanding — the server keeps exactly one
    /// per session and defers the rest — so a replacement simply overwrites,
    /// which also means a prompt that arrived while the window was tearing down
    /// can never wedge the slot.
    pub fn park_prompt(&mut self, prompt: ClipboardPrompt) {
        self.prompt = Some(prompt);
    }

    /// Take the parked confirmation request, if any.
    pub fn take_prompt(&mut self) -> Option<ClipboardPrompt> {
        self.prompt.take()
    }

    /// Queue a host clipboard job for the foreground, dropping the oldest when
    /// the bound is reached. Returns `true` when a job was evicted.
    pub fn push_job(&mut self, job: BridgeJob) -> bool {
        let evicted = self.jobs.len() >= MAX_PENDING_BRIDGE_JOBS;
        if evicted {
            self.jobs.pop_front();
        }
        self.jobs.push_back(job);
        evicted
    }

    /// Take every queued job in arrival order, leaving the queue empty.
    pub fn drain_jobs(&mut self) -> Vec<BridgeJob> {
        self.jobs.drain(..).collect()
    }
}

/// Host clipboard I/O abstraction behind the OSC 52 bridge.
///
/// Implementors map a [`ClipboardSelection`] onto a concrete pasteboard. On
/// Linux `Primary` targets the X11 primary selection; on every other platform
/// (and on Wayland, per spec Assumptions) it collapses onto the system
/// clipboard. Every fallible call collapses onto [`BridgeError`] so the server
/// can map failures onto an empty OSC 52 reply per UX-002.
pub trait ClipboardBackend {
    /// Read the requested selection's text payload.
    fn read(&mut self, selection: ClipboardSelection) -> Result<String, BridgeError>;
    /// Overwrite the requested selection with `payload`.
    fn write(&mut self, selection: ClipboardSelection, payload: String) -> Result<(), BridgeError>;
}

/// Spec 010 E5 / C6 — host clipboard bridge read.
///
/// The bridge does NOT consult `policy.read_mode`; that decision lives
/// server-side per research decision 6. Errors collapse onto
/// [`BridgeError::Unavailable`] via the backend.
pub fn bridge_read<B: ClipboardBackend>(
    backend: &mut B,
    selection: ClipboardSelection,
) -> Result<String, BridgeError> {
    backend.read(selection)
}

/// FR-019 focus gate inputs for an OSC 52 write.
///
/// Bundles the `terminal.clipboard.focus_gate_writes` config flag with the live
/// window-focus state so the gate decision stays a single pure value rather
/// than two loose bools threaded through [`bridge_write`].
#[derive(Debug, Clone, Copy)]
pub struct FocusGate {
    /// The `terminal.clipboard.focus_gate_writes` config flag.
    pub focus_gate_writes: bool,
    /// Whether the client window currently holds focus.
    pub window_focused: bool,
}

impl FocusGate {
    /// Whether an OSC 52 write must be silently dropped.
    ///
    /// When `focus_gate_writes` is enabled AND the window is unfocused, a
    /// background PTY-side program must not be allowed to hijack the host
    /// clipboard while another app holds focus. The check lives client-side
    /// (research decision 6) because window-focus state has no synchronous
    /// server-side view.
    #[must_use]
    pub fn drops_write(self) -> bool {
        self.focus_gate_writes && !self.window_focused
    }
}

/// Spec 010 E5 / C6 — host clipboard bridge write.
///
/// Honours the FR-019 opt-in focus gate before touching the backend: a gated
/// write on an unfocused window succeeds without mutating any clipboard (silent
/// no-op, no error reply). Otherwise the payload is written to the routed
/// selection.
pub fn bridge_write<B: ClipboardBackend>(
    backend: &mut B,
    selection: ClipboardSelection,
    payload: String,
    gate: FocusGate,
) -> Result<(), BridgeError> {
    if gate.drops_write() {
        tracing::debug!(
            "OSC 52 bridge write dropped: focus_gate_writes enabled and window unfocused"
        );
        return Ok(());
    }
    backend.write(selection, payload)
}

/// Build the [`ClientMessage::ClipboardBridgeReadReply`] for a server read
/// request: perform the host read and wrap its `Result` under the matching
/// `request_id`. The reply is sent verbatim whether the read succeeded or
/// collapsed onto a [`BridgeError`] (mapped server-side onto an empty OSC 52
/// reply).
pub fn read_reply<B: ClipboardBackend>(
    backend: &mut B,
    request_id: PromptId,
    selection: ClipboardSelection,
) -> ClientMessage {
    ClientMessage::ClipboardBridgeReadReply { request_id, payload: bridge_read(backend, selection) }
}

/// Build the [`ClientMessage::ClipboardPromptResponse`] for a resolved OSC 52
/// confirmation overlay, echoing the originating `request_id`.
#[must_use]
pub fn prompt_response(request_id: PromptId, decision: ClipboardDecision) -> ClientMessage {
    ClientMessage::ClipboardPromptResponse { request_id, decision }
}

/// Spec 010 T042 — persist the policy axis an `Always*` decision settled.
///
/// `AllowOnce` / `DenyOnce` are a no-op: only the sticky choices write config.
/// The server flips its own in-memory snapshot the moment the response lands,
/// so this write is what makes the choice survive a restart — and the config
/// watcher's `ConfigReloaded` round trip re-applies the identical value, which
/// is why a failed save is a warning rather than an error.
pub fn persist_policy_axis(op: ClipboardOp, decision: ClipboardDecision) {
    use scribe_common::config::{ClipboardMode, load_config, save_config};

    let new_mode = match decision {
        ClipboardDecision::AlwaysAllow => ClipboardMode::Allow,
        ClipboardDecision::AlwaysDeny => ClipboardMode::Deny,
        ClipboardDecision::AllowOnce | ClipboardDecision::DenyOnce => return,
    };
    let mut config = match load_config() {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "clipboard policy not persisted: config unreadable");
            return;
        }
    };
    match op {
        ClipboardOp::Read => config.terminal.clipboard_policy.read_mode = new_mode,
        ClipboardOp::Write => config.terminal.clipboard_policy.write_mode = new_mode,
    }
    if let Err(error) = save_config(&config) {
        tracing::warn!(%error, "clipboard policy not persisted: config unwritable");
    } else {
        tracing::info!(?op, ?new_mode, "persisted the clipboard policy axis");
    }
}

/// Read the Linux primary selection for a middle-click paste.
///
/// Returns `None` when the backend is unavailable or the selection is empty,
/// so the caller skips the paste rather than pasting stale text. The primary
/// selection maps to the system clipboard on non-Linux platforms (arboard
/// layer), matching the legacy `perform_primary_paste` fallback.
pub fn read_primary<B: ClipboardBackend>(backend: &mut B) -> Option<String> {
    match backend.read(ClipboardSelection::Primary) {
        Ok(text) if !text.is_empty() => Some(text),
        Ok(_) => None,
        Err(e) => {
            tracing::debug!("primary selection read failed: {e:?}");
            None
        }
    }
}

/// Write the current terminal selection to the Linux primary selection buffer,
/// applying the AI copy-cleanup transforms first.
///
/// `raw` is the extracted selection text; empty input is a no-op. The cleaned
/// text (dedent / blockquote / decoration / unwrap when `options` enables it)
/// is written to [`ClipboardSelection::Primary`]. Mirrors the legacy
/// `set_primary_selection`.
pub fn set_primary<B: ClipboardBackend>(backend: &mut B, raw: &str, options: CopyTextOptions) {
    if raw.is_empty() {
        return;
    }
    let text = clipboard_cleanup::prepare_copy_text(raw, options);
    if let Err(e) = backend.write(ClipboardSelection::Primary, text) {
        tracing::debug!("primary selection write failed: {e:?}");
    }
}

/// Live `arboard`-backed clipboard used by the shipped client.
///
/// Holds an `Option<arboard::Clipboard>` because handle creation can fail
/// (no display server, compositor restart); a `None` handle reports
/// [`BridgeError::Unavailable`] for every operation. On Linux the `Primary`
/// selection routes through arboard's `GetExtLinux` / `SetExtLinux` primary
/// target; elsewhere it collapses onto the default pasteboard.
pub struct ArboardClipboard {
    inner: Option<arboard::Clipboard>,
}

impl ArboardClipboard {
    /// Create a live clipboard handle, logging and degrading to an unavailable
    /// backend when arboard cannot initialise.
    #[must_use]
    pub fn new() -> Self {
        let inner = arboard::Clipboard::new()
            .map_err(|error| tracing::warn!("clipboard unavailable: {error}"))
            .ok();
        Self { inner }
    }

    /// Whether a live arboard handle is present.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.inner.is_some()
    }
}

impl Default for ArboardClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardBackend for ArboardClipboard {
    fn read(&mut self, selection: ClipboardSelection) -> Result<String, BridgeError> {
        let cb = self.inner.as_mut().ok_or(BridgeError::Unavailable)?;
        let result = match selection {
            #[cfg(target_os = "linux")]
            ClipboardSelection::Primary => {
                use arboard::{GetExtLinux, LinuxClipboardKind};
                cb.get().clipboard(LinuxClipboardKind::Primary).text()
            }
            _ => cb.get_text(),
        };
        result.map_err(|err| {
            tracing::debug!("OSC 52 bridge read failed: {err}");
            BridgeError::Unavailable
        })
    }

    fn write(&mut self, selection: ClipboardSelection, payload: String) -> Result<(), BridgeError> {
        let cb = self.inner.as_mut().ok_or(BridgeError::Unavailable)?;
        let result = match selection {
            #[cfg(target_os = "linux")]
            ClipboardSelection::Primary => {
                use arboard::{LinuxClipboardKind, SetExtLinux};
                cb.set().clipboard(LinuxClipboardKind::Primary).text(payload)
            }
            _ => cb.set_text(payload),
        };
        result.map_err(|err| {
            tracing::debug!("OSC 52 bridge write failed: {err}");
            BridgeError::Unavailable
        })
    }
}

#[cfg(test)]
mod tests;
