# Implementation Plan: OSC 52 Clipboard Gating

**Branch**: `010-osc52-clipboard-gating` | **Date**: 2026-05-22 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/010-osc52-clipboard-gating/spec.md`

## Summary

Replace the unconditional in-memory `ServerClipboard` with a gated
client-server clipboard pipeline. Policy decisions live server-side in
the existing `SessionEvent::ClipboardStore`/`ClipboardLoad` arms of
`crates/scribe-server/src/ipc_server.rs`; the prompt UI and the host
clipboard bridge live client-side, reusing the
`crates/scribe-client/src/disallowed_scheme_dialog.rs` pattern from
spec 009 and the existing `arboard::Clipboard` already wired into
`crates/scribe-client/src/main.rs`. Two new additive IPC variant pairs
carry the prompt RPC and the host bridge (`ClipboardPromptRequest` /
`ClipboardPromptResponse`; `ClipboardBridgeWrite` /
`ClipboardBridgeReadRequest` / `ClipboardBridgeReadReply`). Five new
keys land on `TerminalConfig` for read mode, write mode, max write
size, focus-gate toggle, and burst-window duration; the Scribe
settings webview gets a "Clipboard (OSC 52)" subsection on the
existing Terminal page. The in-memory `ServerClipboard` buffer is
removed — every allowed OSC 52 operation flows through the host
clipboard via the client, matching peer-terminal behavior where the
host clipboard is the single source of truth.

See [research.md](./research.md) for the eight decisions that drove
this shape (notably the host-as-source-of-truth vs. server-buffer
trade-off and the vte/alacritty_terminal size-cap enforcement layer),
[data-model.md](./data-model.md) for the five new entities
(ClipboardPolicyConfig, ClipboardRequest, ClipboardBurstState,
ClipboardDialog, HostClipboardBridge),
[contracts/internal-osc52-pipeline.md](./contracts/internal-osc52-pipeline.md)
for the seven internal contracts spanning pty → server → client, and
[quickstart.md](./quickstart.md) for per-user-story manual
verification.

## Technical Context

**Language/Version**: Rust (workspace edition per `Cargo.toml`).
**Primary Dependencies**:

  - `alacritty_terminal` 0.26 — `Event::ClipboardStore` /
    `Event::ClipboardLoad` are already fired by upstream's VTE Perform
    impl for OSC 52; no upstream patch required. See
    [`crates/scribe-pty/src/event_listener.rs`](../../crates/scribe-pty/src/event_listener.rs).
  - `vte` 0.15 — standard build is uncapped on OSC raw-buffer length;
    Scribe enforces FR-009 / FR-015 at the `SessionEvent::ClipboardStore`
    arm by checking the parsed `String` length before storing or
    bridging. Multi-chunk accumulation across PTY reads is already
    handled inside vte; the cap enforcement is Scribe-side, see
    research.md decision 4.
  - `arboard` — already a workspace dependency, already used by
    `crates/scribe-client/src/main.rs` for the user-driven paste path.
    Reused verbatim for the OSC 52 host bridge with no new crate.
  - `wgpu` + `cosmic-text` — existing GPU dialog/atlas pipeline reused
    for the new clipboard prompt dialog, identical to the spec 009
    `disallowed_scheme_dialog.rs` model.
  - `serde` / MessagePack framing — existing IPC layer extended with
    additive `ClientMessage` / `ServerMessage` variants (see contracts).

  No new crate dependencies.

**Storage**:

  - New config keys on `TerminalConfig` (per-installation TOML config
    via `ScribeConfig`). No new persistence files.
  - The in-memory `ServerClipboard` struct on the server is **removed**
    (research decision 1). Per-session `ClipboardBurstState` replaces it
    as the only server-side OSC 52 state.

**Testing**:

  - `cargo test --workspace` baseline must remain green.
  - No new automated tests requested in the spec (per QR-002 and
    Constitution II). Manual `quickstart.md` covers all four user
    stories. Rationale: OSC 52 verification requires a live PTY, a GUI
    client to drive the prompt, and an out-of-process host clipboard
    inspection — none of which the current Rust test harness models
    end-to-end. The same precedent was used by specs 005, 007, 008, 009.

**Target Platform**:

  - macOS, Linux X11/Wayland, Windows — same scope as the rest of
    `scribe-server` and `scribe-client`.
  - Primary-selection bridging is X11-only per spec Assumptions;
    Wayland and macOS use their platform-default behavior, which on
    those platforms means the primary-selection axis silently no-ops
    on writes and returns the system clipboard on reads (matches
    `arboard`'s existing platform-conditional implementation).

**Project Type**: Desktop app, Rust workspace
(`crates/scribe-{client,server,pty,renderer,common,cli,settings,test}`).

**Performance Goals**:

  - Server-side policy decision: sub-millisecond per OSC 52 event
    (per PR-001). The decision path is a `match` on the per-session
    `ClipboardPolicyConfig` + a length check against the configured
    cap — no allocation, no async wait outside the prompt path.
  - Host clipboard bridge round-trip (server → client → arboard →
    client → server): ≤100 ms p50 per SC-002, dominated by IPC
    framing + arboard's platform-specific clipboard call.
  - Prompt visible to user: ≤100 ms after OSC 52 byte arrival per
    SC-004. Dominated by the same IPC + one GPU frame.
  - Burst-decision-reuse: a single timestamp + decision tuple per
    session; lookup is O(1).
  - Size cap enforcement: short-circuit at the
    `SessionEvent::ClipboardStore` arm, before any host bridge call.
    A rejected payload incurs one `len()` check; nothing is allocated
    beyond the already-parsed `String` that alacritty hands us.

**Constraints**:

  - MUST preserve the existing `SessionEvent::ClipboardStore` /
    `ClipboardLoad` channel shape — only the *handler* changes, not
    the upstream event surface.
  - MUST NOT introduce a new VTE parser, OSC interceptor branch, or
    duplicate alacritty's OSC 52 handling. The MetadataParser /
    OscInterceptor in `scribe-pty` is **not** touched; OSC 52 reaches
    the server via the upstream VTE Perform path same as today.
  - MUST reuse the `disallowed_scheme_dialog.rs` dialog model verbatim
    for the new clipboard prompt — same `DialogLayout`/`DialogRenderer`
    pattern, same Esc-cancels / Tab-cycles / Enter-activates
    conventions, same default-focus-on-cancel-path discipline.
  - MUST reuse the existing client-side `arboard::Clipboard` for the
    host bridge. No second clipboard backend.
  - The new IPC variants MUST be additive serde-tagged variants. With
    `#[serde(tag = "type")]`, older peers will fail to deserialize the
    new variants. Migration plan: server gates the new variants behind
    a feature negotiation step on session attach (research decision 8).
  - `lat.md` updates MUST be co-shipped with the code change — pty,
    server, client, protocol, settings, and architecture all touch
    this feature.

**Scale/Scope**:

  - One `ClipboardPolicyConfig` per installation (shared across all
    sessions).
  - One `ClipboardBurstState` per session (max 256 per server, the
    existing per-server session cap).
  - One `ClipboardDialog` per client window. Multiple windows can show
    independent dialogs.
  - One outstanding `ClipboardPromptRequest` per session at any time —
    burst-decision-reuse (FR-016) collapses concurrent same-pane
    requests onto the open prompt.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

### Pre-research check

- **Code Quality**: **PASS**. Change uses existing typed surfaces
  (`SessionEvent`, `ClientMessage`/`ServerMessage` with serde tags,
  `TerminalConfig`, `arboard::Clipboard`). New types are introduced
  in their home crates (config in `scribe-common`, dialog in
  `scribe-client`, burst state in `scribe-server`). No cross-cutting
  helper crate. No new dependency. The MetadataParser /
  OscInterceptor in `scribe-pty` is deliberately not touched —
  preserves the protocol-layer split. The `ServerClipboard` struct
  removal is a deliberate simplification, documented in research.md
  decision 1.
- **Testing Strategy**: **PASS**. Each user story has an independent
  manual quickstart path. No new automated tests are requested; the
  existing `cargo test --workspace` baseline must remain green.
  Constitution II permits this when the spec documents the manual
  verification with rationale. The rationale here is the same as
  spec 009: OSC 52 behavior is inherently host-clipboard-mediated
  (verification requires reading the OS clipboard from outside
  Scribe), and the dialog UI is GPU-rendered with no headless harness.
- **User Experience Consistency**: **PASS**. The clipboard prompt
  dialog reuses the `disallowed_scheme_dialog.rs` chrome verbatim,
  including Esc-cancels / Tab-cycles / Enter-activates / Cancel-is-
  default conventions. The settings webview gets a new subsection
  under the existing Terminal page, mirroring the placement of the
  Keyboard Protocol and Persist-Environment toggles. No new
  keybindings, no new top-level settings page.
- **Performance**: **PASS**. Measurable budgets stated above for
  policy decision (sub-ms), bridge round-trip (≤100 ms p50), prompt
  visibility (≤100 ms), and burst-state lookup (O(1)). Hot paths
  (the `SessionEvent::ClipboardStore`/`Load` handler, the existing
  client-side `arboard` call, the IPC framing) are preserved
  verbatim for the non-prompt case.
- **Operational Safety**: **PASS** with one note. New config keys
  default to kitty-equivalent values — `read = prompt`,
  `write = allow`, `max_write_bytes = 16 MB`, `focus_gate_writes =
  false`, `burst_window_ms = 500` — so an upgrade in place changes
  behavior (OSC 52 reads start prompting where they were
  previously silently denied / fell through). This is the *intended*
  audit-closure behavior and the user-facing contract of the feature.
  The new IPC variants land additively; the cross-version handoff
  scenario (running server, new-version client; or vice versa) is
  handled by research decision 8's negotiation step. `lat.md`
  updates are scoped to `pty.md`, `server.md`, `protocol.md`,
  `common.md`, `client.md`, `settings.md`, and `architecture.md`.
  `lat check` MUST be run before completion.

No constitution violations. Complexity Tracking is empty (see below).

### Post-Phase 1 re-check

- **Code Quality**: **PASS**. Phase 1 design confirms five touched
  crates (`scribe-common`, `scribe-pty`, `scribe-server`,
  `scribe-client`, `scribe-settings`) and one logically untouched
  crate (`scribe-renderer` — reused as-is). No new module outside
  the existing crate roots. The Phase 1 contracts (C1..C7) are all
  additive on existing typed surfaces.
- **Testing Strategy**: **PASS**. Quickstart covers US1 (3
  scenarios), US2 (5 scenarios), US3 (4 scenarios + burst-window
  spot-check), US4 (3 scenarios + the X11-primary-selection caveat),
  plus a performance spot-check on the policy decision and a
  cold-restart-handoff scenario for the migration path.
- **User Experience Consistency**: **PASS**. The Phase 1
  data-model.md confirms the new dialog reuses the spec-009
  layout struct unchanged; only the body text and the button labels
  differ. The settings UI section copy is drafted to match the
  Notification / Terminal-Keys section's voice and option-list layout.
- **Performance**: **PASS**. Phase 1 contracts state the policy
  decision is a non-async `match` on `ClipboardPolicyConfig` with no
  Mutex contention (the policy struct is read-only after attach).
  The burst-state update is a single field write on a per-session
  state struct already held in `PtyReaderState`. The host bridge
  uses one ServerMessage + one ClientMessage round-trip with the
  same MessagePack framing as the rest of the protocol.
- **Operational Safety**: **PASS**. Phase 1 contracts document
  the IPC negotiation step (C7) and the cross-version fallback
  behavior (older client → server silently treats the session as
  "no client connected" for OSC 52 prompt purposes; older server →
  newer client cannot enable the bridge and falls back to the
  pre-feature paste-via-arboard path for user-driven copies).
  Quickstart includes a cold-restart-handoff verification step
  covering both directions.

No constitution violations after Phase 1. Complexity Tracking
remains empty.

## Project Structure

### Documentation (this feature)

```text
specs/010-osc52-clipboard-gating/
├── plan.md              # This file (/speckit-plan command output)
├── research.md          # Phase 0 output — 8 decisions
├── data-model.md        # Phase 1 output — 5 entities (E1..E5)
├── quickstart.md        # Phase 1 output — manual verification per US
├── contracts/
│   └── internal-osc52-pipeline.md   # Phase 1 output — C1..C7
├── checklists/
│   └── requirements.md  # Spec quality checklist (already created)
├── spec.md              # Feature spec with clarifications
└── tasks.md             # Phase 2 output (created by /speckit-tasks — not here)
```

### Source Code (repository root)

Touched files (Rust workspace; layout per `lat.md/architecture.md`):

```text
crates/scribe-common/src/
├── config.rs                        # Add ClipboardPolicyConfig
│                                    # struct with read_mode,
│                                    # write_mode, max_write_bytes,
│                                    # focus_gate_writes,
│                                    # burst_window_ms. Hang it off
│                                    # TerminalConfig as
│                                    # `clipboard: ClipboardPolicyConfig`.
│                                    # Default impl encodes the
│                                    # kitty-equivalent defaults.
└── protocol.rs                      # Add additive variants to
                                     # ClientMessage:
                                     # - ClipboardPromptResponse
                                     # - ClipboardBridgeReadReply
                                     # And to ServerMessage:
                                     # - ClipboardPromptRequest
                                     # - ClipboardBridgeWrite
                                     # - ClipboardBridgeReadRequest
                                     # Plus the supporting
                                     # ClipboardOp / ClipboardDecision
                                     # enums.

crates/scribe-server/src/
├── ipc_server.rs                    # Replace ServerClipboard struct
│                                    # and its arms (lines 329-356,
│                                    # 2617-2627) with the new
│                                    # gated dispatcher. Add
│                                    # ClipboardBurstState in
│                                    # PtyReaderState. Add
│                                    # ClipboardPromptResponse and
│                                    # ClipboardBridgeReadReply
│                                    # arms in the ClientMessage
│                                    # handler.
└── session_manager.rs               # Plumb ClipboardPolicyConfig
                                     # from ScribeConfig into each
                                     # session's PtyReaderState.

crates/scribe-pty/                   # NO source changes.
                                     # The Event::ClipboardStore /
                                     # ClipboardLoad event surface in
                                     # event_listener.rs is preserved
                                     # verbatim — only its handler
                                     # in ipc_server.rs changes.

crates/scribe-client/src/
├── clipboard_dialog.rs              # NEW — modelled after
│                                    # disallowed_scheme_dialog.rs.
│                                    # DialogLayout, DialogRenderer,
│                                    # DialogColors reused. Two
│                                    # buttons in basic mode (Allow
│                                    # / Deny, Deny default), four
│                                    # buttons in "always" mode
│                                    # (Allow once / Always allow /
│                                    # Deny / Always deny). Write
│                                    # variant carries a head-tail
│                                    # truncated payload preview.
├── main.rs                          # Wire ClipboardPromptRequest
│                                    # to dialog instantiation.
│                                    # Wire dialog resolution to
│                                    # ClientMessage::
│                                    # ClipboardPromptResponse. Wire
│                                    # ClipboardBridgeWrite /
│                                    # ClipboardBridgeReadRequest
│                                    # through the existing
│                                    # arboard::Clipboard handle.
│                                    # Apply FR-019 focus check
│                                    # before calling arboard::set.
│                                    # On "Always allow / Always
│                                    # deny", send the persisted
│                                    # policy update via the
│                                    # settings webview channel.
└── (other client files unchanged)

crates/scribe-renderer/              # NO source changes (dialog
                                     # reuses spec 009's existing
                                     # chrome / atlas helpers).

crates/scribe-settings/src/
├── assets/settings.html             # Add a Clipboard (OSC 52)
│                                    # subsection inside the
│                                    # existing Terminal page.
│                                    # Three controls: read-mode
│                                    # dropdown, write-mode
│                                    # dropdown, max-write-bytes
│                                    # numeric input with the
│                                    # 16 MB default and 512 MB
│                                    # upper bound enforced
│                                    # client-side via min/max
│                                    # attributes. One toggle for
│                                    # focus-gate-writes (FR-019).
│                                    # The burst_window_ms is a
│                                    # plan-phase tunable, not
│                                    # exposed in the UI for v1.
└── server.rs                        # Plumb the new keys through
                                     # the existing webview ⇄ TOML
                                     # apply path.

lat.md/
├── pty.md                           # Update OSC Interceptor +
│                                    # event_listener subsections
│                                    # to note that OSC 52 fires
│                                    # via the upstream alacritty
│                                    # path with no new pty-side
│                                    # logic.
├── server.md                        # New subsection "Clipboard
│                                    # Gating" under Sessions
│                                    # describing the policy
│                                    # engine, burst-state machine,
│                                    # prompt RPC, host bridge.
├── protocol.md                      # New subsection documenting
│                                    # ClipboardPromptRequest /
│                                    # ClipboardPromptResponse +
│                                    # the three bridge messages.
├── common.md                        # Document
│                                    # ClipboardPolicyConfig on
│                                    # TerminalConfig.
├── client.md                        # New subsection under
│                                    # Dialogs for the Clipboard
│                                    # Dialog. Mention the
│                                    # arboard reuse for the host
│                                    # bridge.
├── settings.md                      # Update Terminal Keys
│                                    # section to list the new
│                                    # clipboard keys.
└── architecture.md                  # If the data-flow diagram
                                     # changes (it does — new
                                     # IPC round-trip), update
                                     # the Data Flow Read Path.
```

**Structure Decision**: Cross-crate change touching five crates
(`scribe-common`, `scribe-server`, `scribe-client`, `scribe-settings`,
plus `lat.md/` doc updates). The blast radius is larger than
spec 009 (which was client-only) but each crate's diff is bounded
and uses existing typed surfaces. The
`MetadataParser`/`OscInterceptor` layer in `scribe-pty` is the
clear *don't touch* boundary — alacritty_terminal already handles
OSC 52 parsing and fires the events we need, and Scribe-side parsing
would re-implement upstream logic. The other clear *don't touch*
boundary is `scribe-renderer`: the dialog reuses spec 009's
existing chrome helpers without change.

## Complexity Tracking

> No constitution violations recorded. This section intentionally empty.

The two judgment calls below are noted for visibility, not as
violations:

1. **`ServerClipboard` removal**: The in-memory buffer is removed
   in favor of the host clipboard bridge. The audit explicitly
   noted the buffer is isolated from the GUI clipboard with no
   observable side-channel; no caller depends on the in-memory
   value being readable separately from the host clipboard.
   Behavior change is documented in research.md decision 1 and
   covered by quickstart's "headless-mode read denial" scenario.

2. **Protocol additive variants without an opt-in feature flag**:
   The new ClientMessage / ServerMessage variants land directly,
   relying on the existing serde-tagged enum behavior and the
   feature negotiation step in research decision 8 to handle
   cross-version client/server combinations during cold-restart
   handoff. A feature flag would add a year of stale config
   debt for a one-time migration; rejected as unnecessary.
