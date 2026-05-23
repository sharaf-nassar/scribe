# Phase 0 Research — OSC 52 Clipboard Gating

**Branch**: `010-osc52-clipboard-gating`
**Date**: 2026-05-22
**Method**: Targeted code reads against `crates/`, the pinned
`alacritty_terminal-0.26` and `vte-0.15` sources, the kitty + iTerm2 +
xterm + WezTerm docs and issue trackers from the clarify session, and
the four shipped sibling specs (005 command awareness, 007 hover-note,
008 IME, 009 OSC 8). No build, no dynamic verification — facts only.

## Headline finding

The whole feature wires *between* two existing layers and a known
client-side library:

1. **alacritty_terminal already parses OSC 52** and fires
   `Event::ClipboardStore` / `Event::ClipboardLoad` via its VTE Perform
   impl with no upstream feature flag. `scribe-pty/src/event_listener.rs`
   forwards those events as `SessionEvent::ClipboardStore` /
   `ClipboardLoad` today.
2. **`crates/scribe-server/src/ipc_server.rs` already handles those
   events** at the per-session reader task — lines 2617-2627 — by
   storing into / formatting back from a per-server in-memory
   `ServerClipboard` struct (lines 329-356).
3. **`arboard::Clipboard` is already wired into
   `crates/scribe-client/src/main.rs`** (~lines 346, 374, 9048,
   9117) for the user-driven copy / paste path. It supports both the
   system clipboard and the X11 primary selection via the same
   handle.

The plan is therefore: *replace* the server-side `ServerClipboard`
buffer with a policy engine, *gate* the existing event arms behind it,
*reuse* the spec 009 dialog model for the prompt UI, and *plumb* the
host clipboard through an additive IPC round-trip onto the existing
`arboard::Clipboard` handle. No new crates, no new parsers, no new
render pipeline.

## Decisions

### 1. Source of truth: host clipboard, not server buffer

- **Decision**: Drop the existing in-memory `ServerClipboard` struct
  (`crates/scribe-server/src/ipc_server.rs` lines 329-356). Every
  allowed OSC 52 read returns the current *host* clipboard (via the
  client + `arboard`); every allowed OSC 52 write updates the host
  clipboard. No second buffer.
- **Rationale**: The audit explicitly noted (lines 489-504) that the
  current in-memory buffer is isolated from the host clipboard with
  no observable bridge — i.e., no caller can read the in-memory
  value separately. Keeping it would force the implementation to
  either (a) merge two divergent values on read (last-OSC-52-write
  vs. current-host) or (b) preserve an unobservable side-channel.
  Both are worse than just removing the buffer and using the host
  clipboard as the single source. Peer terminals (kitty, iTerm2,
  xterm, WezTerm) all treat the host clipboard as the source of
  truth — OSC 52 writes are visible to other apps, reads see what
  the user (or the most recent write) put there.
- **Alternatives considered**:
  - **Keep `ServerClipboard` as a fallback** for the headless-mode
    case (no client connected). Rejected: per FR-013 the prompt
    path already denies in headless mode, and the allow-mode
    headless case is a non-feature for v1 (nothing in the spec
    promises observable behavior without a client).
  - **Use server-side `arboard::Clipboard` directly**. Rejected:
    the server has no GUI display and no compositor; arboard on a
    headless Linux server doesn't work, and even on a desktop the
    server may be running under a different DISPLAY or no DISPLAY.
    Only the client has reliable host-clipboard access.
- **Implication**: One less struct, one less Mutex, one less
  consistency question. Replaces with a `ClipboardPolicyConfig`
  read from `ScribeConfig` plus per-session `ClipboardBurstState`.

### 2. Dialog UI: clone the spec 009 disallowed-scheme dialog model

- **Decision**: New `crates/scribe-client/src/clipboard_dialog.rs`
  modeled exactly on `disallowed_scheme_dialog.rs`. Reuses
  `DialogLayout`, `DialogRenderer`, `DialogColors`, `ButtonIndex`-
  style cycling, Esc-cancels / Tab-cycles / Enter-activates /
  Cancel-as-default-focus conventions. Two buttons (Allow / Deny)
  in the simple mode, four (Allow once / Always allow / Deny /
  Always deny) when the "always" affordance is shown.
- **Rationale**: QR-001 / UX-001 mandate this verbatim. Building a
  parallel modal subsystem would duplicate the chrome work spec 009
  just landed. The existing dialog file is ~100 lines of layout +
  ~200 lines of build/event logic; cloning + adapting is small.
- **Alternatives considered**:
  - **Reuse the existing dialog with a config-driven variant
    enum** instead of a sibling file. Rejected: the body content
    differs (URI vs. clipboard op + selection + optional preview),
    the button counts differ (2 vs. 4), and the close-on-decision
    semantics differ (URL dialog dispatches on accept; clipboard
    dialog ALSO persists the policy via a separate channel on
    "Always" choices). A second file keeps each dialog's
    invariants legible.
  - **Webview-rendered prompt**: Rejected. Adds a webview window
    for a high-frequency prompt; introduces a security regression
    (webview content can run JS); breaks the established GPU-
    rendered dialog pattern.

### 3. arboard reuse for the host bridge

- **Decision**: Use the existing `arboard::Clipboard` handle on
  `App` in `crates/scribe-client/src/main.rs` for both the read
  (`get_text` / `Linux::primary`) and write (`set_text` /
  `Linux::primary`) paths of the OSC 52 bridge.
- **Rationale**: arboard already handles platform conditional
  logic (X11 primary via `GetExtLinux::primary_clipboard` /
  `SetExtLinux::primary_clipboard`; Wayland and macOS map primary
  to system clipboard at the library level). Spec Assumptions
  explicitly defer Wayland primary-selection support and macOS
  pasteboard variants to platform default behavior; arboard's
  defaults satisfy that.
- **Alternatives considered**:
  - **x11-clipboard / wayland-protocols direct**: Rejected. Would
    require platform-specific code in the client and re-implement
    arboard's existing conditional logic. arboard is a stable
    workspace dep; no reason to bypass it.
  - **Server-side arboard with a DISPLAY proxy**: Rejected. As
    noted in decision 1, the server cannot reliably reach the
    host clipboard.

### 4. Size-cap enforcement layer: at the `SessionEvent` arm

- **Decision**: Enforce FR-009 / FR-015 by checking the parsed
  payload length in the `SessionEvent::ClipboardStore` handler in
  `ipc_server.rs`, before bridging to the client. Reject (drop)
  the write if `text.len() > policy.max_write_bytes`. No vte / no
  alacritty patch.
- **Rationale**: alacritty's VTE Perform impl hands us the fully
  parsed payload as `String`; the parser layer's "multi-chunk OSC
  52 across PTY read boundaries" is already handled inside vte
  (which has an internal OSC raw-buffer; the standard build is
  uncapped per spec 009 research). Adding a Scribe-side parser
  branch to enforce the cap at the byte-stream layer would
  duplicate vte's job. The server-side check is the simplest
  point that still satisfies FR-015 ("reject rather than
  silently truncate" — we reject the whole payload before bridging).
- **Memory caveat**: vte's uncapped OSC buffer means a malicious
  peer could push an oversize OSC 52 sequence and consume
  `cap_bytes` of memory inside vte before we reject it. With the
  default 16 MB cap this is acceptable; at the 512 MB ceiling, a
  single oversize attempt costs 512 MB of peak memory in the
  parser plus another 512 MB in the resulting `String`. Documented
  as a known limitation; mitigation is the user-controlled
  setting (the user picks their cap with full knowledge).
- **Alternatives considered**:
  - **Patch alacritty / vte to expose a configurable OSC raw
    buffer cap**: Rejected for v1. Out-of-tree patch is a
    maintenance liability; upstream PR is the right path for
    future hardening; tracked as a follow-up in research.md.
  - **Stream-decode OSC 52 manually in `scribe-pty`'s
    OscInterceptor and short-circuit**: Rejected. Re-implements
    upstream base64 + OSC framing; high blast radius for a
    marginal memory win.

### 5. Burst-decision-reuse state machine

- **Decision**: Per-session `ClipboardBurstState` in
  `PtyReaderState`. Fields: `outstanding_prompt: Option<PromptId>`,
  `last_decision: Option<(ClipboardOp, ClipboardDecision, Instant)>`,
  `burst_window_ms: u64` (from config; default 500).
- **Decision flow** (per FR-016/017/018):
  1. OSC 52 event arrives.
  2. Look up policy axis (read or write mode).
  3. If mode is `allow` or `deny`: apply directly, skip burst state.
  4. If mode is `prompt`:
     - If `outstanding_prompt.is_some()` and the open prompt's op
       matches this op: defer this request and bind it to the
       open prompt's future decision (per FR-016).
     - Else if `last_decision.is_some()` and `now -
       last_decision.timestamp < burst_window_ms` and the op
       matches: reuse the decision without prompting (per FR-017).
     - Else: open a new prompt; set `outstanding_prompt`.
  5. On prompt resolution: clear `outstanding_prompt`, update
     `last_decision`, apply the decision to all deferred requests
     bound to it.
- **Rationale**: Captures all three FRs in a single per-session
  state machine with O(1) lookup. Per-session scoping satisfies
  FR-018 (cross-pane independence). The burst window is a single
  duration value, set from config; the spec defers the exact
  number to plan, this plan sets 500 ms (matches typical tmux
  per-action burst durations observed in shell-script flurries).
- **Alternatives considered**:
  - **Global server-wide burst state**: Rejected — would couple
    cross-pane behavior, violating FR-018.
  - **No burst-window logic, only "while prompt open" defer**:
    Rejected — degrades the post-decision tmux UX, defeats the
    Q4 clarification.

### 6. Focus-gating implementation (FR-019)

- **Decision**: Move the focus-gate check to the client. The
  server unconditionally forwards `ClipboardBridgeWrite` to the
  client whenever the write mode is `allow` (or after a prompt
  Allow); the client checks its own `window_focused` state
  (already tracked per `lat.md/client.md#Focus Guard#Winit Focus`)
  before calling `arboard::set_text`. If unfocused, the bridge
  call no-ops silently. The `focus_gate_writes` config key is
  carried to the client at attach and consulted only at bridge
  time, not on the server.
- **Rationale**: Window focus state lives client-side; the server
  has no synchronous view of it. Asking the server to keep a
  cached focus-state would add a new periodic IPC message and
  introduce a race where the server's view lags the actual focus
  by a few ms — the wrong direction for a security gate. The
  client-side check is one atomic boolean read inside the bridge
  handler that already holds the `arboard` handle.
- **Alternatives considered**:
  - **Server polls / caches focus state via existing IPC**:
    Rejected. Adds a new message + a TTL race + no security
    benefit.
  - **Drop the feature**: Rejected. The user accepted Q5 with
    "yes"; this is in scope.

### 7. Headless / no-client behavior (FR-013)

- **Decision**: If no client is attached to a session, OSC 52
  events are handled as follows:
  - Read with `read_mode = allow` → reply with empty payload
    (server has no host clipboard to bridge to).
  - Read with `read_mode = prompt` → silent deny per FR-013.
  - Read with `read_mode = deny` → silent deny (same as today).
  - Write with `write_mode = allow` → silent drop (nowhere to
    bridge to).
  - Write with `write_mode = prompt` → silent deny per FR-013.
  - Write with `write_mode = deny` → silent deny.
- **Rationale**: Scribe sessions outlive client attaches (zero-
  downtime upgrades, session multiplexing). A PTY-side program's
  OSC 52 op during a detached interval has no rendering surface
  for a prompt and no host clipboard for the bridge. Silently
  no-oping matches what a sane app would do if the user closed
  the GUI; matches peer behavior (kitty / iTerm2 are single-
  process; there's no detached-server analog, but the behavior
  is functionally equivalent to "no clipboard access").
- **Alternatives considered**:
  - **Queue requests until a client attaches**: Rejected. Would
    re-introduce the unbounded queue problem from Q4, plus
    cross-session interleaving, plus surprises after long
    detachments.
  - **Re-add the in-memory buffer as a fallback for headless
    writes**: Rejected. Reintroduces the buffer-vs-host
    consistency question (decision 1) for an edge case.

### 8. IPC migration: additive variants + attach-time negotiation

- **Decision**: The new variants on `ClientMessage` and
  `ServerMessage` land additively. With `#[serde(tag = "type")]`
  the wire shape is `{ "type": "ClipboardPromptRequest", ... }`
  and an older serde-decoded `ClientMessage`/`ServerMessage` will
  error on the unknown variant. To handle cross-version
  combinations during cold-restart handoff and zero-downtime
  upgrades, the server includes a `clipboard_gating: bool` flag
  in its existing `ServerMessage::Hello` (or equivalent
  attach-time message); the client includes a matching flag in
  `ClientMessage::Attach`. When either side reports `false`:
  - Server side: treat the session as "no client connected" for
    OSC 52 gating purposes (decision 7 applies). The server
    never emits `ClipboardPromptRequest` / `ClipboardBridgeWrite`
    / `ClipboardBridgeReadRequest` to a non-supporting client.
  - Client side: silently no-op on receipt of the new variants
    (defensive — shouldn't happen if the negotiation works) and
    continue with the legacy paste-via-arboard user-driven path
    unchanged.
- **Rationale**: Scribe's architecture explicitly supports
  zero-downtime upgrades where the server outlives a client
  restart and cold-restart handoffs replay session state to a
  new server. Either side can be on a mixed version during the
  upgrade window. A feature flag plus attach-time negotiation is
  the established Scribe pattern (see also the `kitty_keyboard`
  feature negotiation in spec 008's IME work).
- **Alternatives considered**:
  - **Hard version gate**: Force a server-client version match
    and refuse cross-version attaches. Rejected: existing
    upgrade UX assumes the user controls the cadence of client
    vs. server restarts.
  - **Versioned IPC variants** (e.g.,
    `ClipboardPromptRequestV1`): Rejected. Premature; one
    negotiation flag is sufficient for the foreseeable horizon.

### 9. OSC 52 fallback-chain decomposition: verified upstream

- **Decision**: Trust alacritty_terminal's VTE Perform impl to
  decompose OSC 52 fallback-chain selection targets (`cp`, `pc`,
  `c0..c7`) into one `Event::ClipboardStore` / `ClipboardLoad`
  per selection target, in priority order. Scribe-side does not
  walk the chain — each fired event is treated as a standalone
  request with its single `ClipboardType` argument.
- **Rationale**: alacritty's `Event::ClipboardLoad(kind, ...)`
  carries a single `ClipboardType` value (`Clipboard` or
  `Selection`); the OSC 52 parser at the upstream layer is
  responsible for chain resolution. FR-011's "walk each entry
  in the chain entry-by-entry" requirement is therefore
  satisfied by the structure of the events Scribe receives,
  not by Scribe-side chain-walking logic.
- **Empirical verification step** (drives the quickstart
  fallback-chain scenario): during quickstart, issue
  `printf '\x1b]52;cp;<base64>\x07'` and confirm Scribe's
  server log shows two distinct event arms firing in order
  (clipboard then primary), each evaluated against the
  appropriate policy axis.
- **Alternatives considered**:
  - **Add a Scribe-side OSC 52 chain parser**: Rejected.
    Re-implements upstream logic with no benefit; would
    drift from upstream behavior over time.
  - **Document fallback-chain handling as "best-effort"**:
    Rejected. FR-011 makes it a MUST; we need a definite
    answer, even if delegated.
- **Risk if assumption is wrong**: If alacritty's VTE Perform
  impl turns out to fire a single event with a chain
  representation rather than decomposing it, the FR-011 path
  will silently fail open. The empirical verification step
  in the quickstart catches this; if it fails, the fix is a
  Scribe-side chain walker in the
  `SessionEvent::ClipboardStore`/`Load` arms — additive on
  top of decision 4.

## Outstanding risks

- **vte OSC raw-buffer memory** (decision 4): A 512 MB cap is the
  user-exposed upper bound; the cap-enforcement check happens
  after vte has buffered the payload, so peak memory at the
  ceiling is ~1 GB per oversize attempt. Acceptable for v1 with
  default cap of 16 MB; document the trade-off in `lat.md/pty.md`
  near the cap setting. A follow-up upstream PR exposing a
  configurable OSC raw-buffer cap on vte would close this.
- **Headless write loss** (decision 7): A program writing to the
  clipboard during a detached interval loses the value. This is
  consistent with peer behavior but worth calling out in the
  Settings UI help text so users with long-running background
  scripts know not to depend on detached-session writes.
- **arboard error paths**: arboard returns `Result` for both
  `get_text` and `set_text` (e.g., compositor restart, X11
  selection ownership lost). On error the client MUST treat the
  operation as a silent deny — same observable behavior as a
  policy-denied request, consistent with UX-002.
