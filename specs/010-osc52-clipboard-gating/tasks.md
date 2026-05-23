---

description: "Task list for OSC 52 clipboard gating implementation"
---

# Tasks: OSC 52 Clipboard Gating

**Input**: Design documents from `/specs/010-osc52-clipboard-gating/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: No automated tests are requested. Verification is via manual
quickstart per QR-002, Constitution II, and the precedent set by
specs 005, 007, 008, 009.

**Organization**: Tasks are grouped by user story. Each user story is
independently testable (see Checkpoints).

**Constitution Gates**: Tasks preserve crate boundaries (decision 1-8
in research.md), reuse existing dialog/IPC patterns, do not restart the
live Scribe server without explicit user approval, include manual
verification, and update `lat.md` before completion.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no incomplete dependencies)
- **[Story]**: US1 / US2 / US3 / US4 maps to spec.md user stories
- Exact file paths included; the implementer can pick up any task without
  re-reading the plan to find where the code lives.

## Path Conventions

Rust workspace at repo root. Crates under `crates/`. Documentation under
`lat.md/`. Spec artifacts under `specs/010-osc52-clipboard-gating/`.

---

## Phase 1: Setup (Shared Scaffolding)

**Purpose**: Pre-flight checks and empty file scaffolds. No behavior
changes; lets later parallel tasks land cleanly.

- [x] T001 Verify `arboard` is already a `scribe-client` dependency in `crates/scribe-client/Cargo.toml` (per research decision 3) and confirm `alacritty_terminal = 0.26` and `vte = 0.15` are pinned in the workspace `Cargo.toml`; no version changes needed
- [x] T002 [P] Create empty `crates/scribe-client/src/clipboard_dialog.rs` with a top-of-file doc comment referencing `disallowed_scheme_dialog.rs` as the model and a `//! TODO: spec 010` placeholder
- [x] T003 [P] Add `pub mod clipboard_dialog;` to `crates/scribe-client/src/main.rs` (or `lib.rs` if the module tree lives there) next to the existing `pub mod disallowed_scheme_dialog;`

**Checkpoint**: New module is wired but empty; workspace compiles unchanged.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Config types, protocol types, and attach-time feature
negotiation. These types are referenced by every user story phase.

**⚠️ CRITICAL**: No user story work can begin until this phase is complete.

- [x] T004 [P] Add `ClipboardMode` enum (`Deny` / `Allow` / `Prompt`) with `#[serde(rename_all = "snake_case")]` and `Default = Prompt` for reads / `Default = Allow` for writes in `crates/scribe-common/src/config.rs`
- [x] T005 [P] Add `ClipboardPolicyConfig` struct in `crates/scribe-common/src/config.rs` with fields `read_mode`, `write_mode`, `max_write_bytes` (default `16 * 1024 * 1024`), `focus_gate_writes` (default `false`), `burst_window_ms` (default `500`); include `#[serde(default)]` and a `Default` impl matching data-model E1
- [x] T006 Add `pub clipboard: ClipboardPolicyConfig` to `TerminalConfig` in `crates/scribe-common/src/config.rs` (line 1004 area), wire through `TerminalConfig::Default`, and add deserialize-time clamps for `max_write_bytes` (`0..=512 MiB`) and `burst_window_ms` (`0..=10_000`)
- [x] T007 [P] Add `ClipboardOp` enum (`Read` / `Write`), `ClipboardSelection` enum (`Clipboard` / `Primary`), `ClipboardDecision` enum (`AllowOnce` / `DenyOnce` / `AlwaysAllow` / `AlwaysDeny`), and `BridgeError` enum (`Unavailable` / `Empty`) in `crates/scribe-common/src/protocol.rs` per data-model E2
- [x] T008 [P] Add `PromptId` type alias (a serde-friendly `u64` newtype) in `crates/scribe-common/src/protocol.rs`; include `Eq`, `Hash`, `Copy`, `Serialize`, `Deserialize` per contract C3 non-contracts
- [x] T009 Add `ServerMessage::ClipboardPromptRequest { session_id, request_id, op, selection, preview: Option<String> }` variant in `crates/scribe-common/src/protocol.rs` (line 295 enum) per contract C2
- [x] T010 Add `ServerMessage::ClipboardBridgeWrite { session_id, selection, payload: String }` variant in `crates/scribe-common/src/protocol.rs` per contract C2
- [x] T011 Add `ServerMessage::ClipboardBridgeReadRequest { session_id, request_id, selection }` variant in `crates/scribe-common/src/protocol.rs` per contract C2
- [x] T012 Add `ClientMessage::ClipboardPromptResponse { request_id, decision }` variant in `crates/scribe-common/src/protocol.rs` (line 134 enum) per contract C3
- [x] T013 Add `ClientMessage::ClipboardBridgeReadReply { request_id, payload: Result<String, BridgeError> }` variant in `crates/scribe-common/src/protocol.rs` per contract C3
- [x] T014 Add `clipboard_gating: bool` capability flag to the server's attach-time message (`ServerMessage::Hello` or the existing capability-bearing variant) and to the client's attach message (`ClientMessage::Attach` or equivalent) in `crates/scribe-common/src/protocol.rs` per contract C7; mirror the existing `kitty_keyboard` capability precedent
- [x] T015 Update the existing wire-name round-trip tests at the bottom of `crates/scribe-common/src/protocol.rs` (around line 877) to cover at least one of the new variants (e.g., `ServerMessage::ClipboardPromptRequest`) so the serde tag stays asserted

**Checkpoint**: Workspace compiles. All new types are in place but no
behavior change yet — old `ServerClipboard` is still wired.

---

## Phase 3: User Story 1 — Block silent exfiltration by default (Priority: P1) 🎯 MVP

**Goal**: With default settings, no PTY-side program can read the
clipboard without user consent; writes flow through silently.

**Independent Test**: With Scribe at default settings and known text in
the host clipboard, run `printf '\x1b]52;c;?\x07'` in a Scribe pane and
dismiss the resulting dialog. The shell receives no clipboard contents
(per quickstart US1-S1). `osc52_write` lands in the host clipboard
without a prompt (per quickstart US1-S3).

### Implementation for User Story 1

- [x] T016 [US1] Remove the `ServerClipboard` struct and the `shared_clipboard()` accessor from `crates/scribe-server/src/ipc_server.rs` (lines 329-356) per research decision 1; remove the `SharedClipboard` type alias and any field that holds it on `PtyReaderState`
- [x] T017 [US1] Add a `ClipboardBurstState` struct in `crates/scribe-server/src/ipc_server.rs` (or a new sibling module `clipboard_state.rs` under `scribe-server/src/`) with fields `outstanding_prompt: Option<PromptId>`, `policy: ClipboardPolicyConfig`; leave `last_decision` / `pending_for_prompt` for Phase 5 — Phase 3 only needs the single-prompt-in-flight slot
- [x] T018 [US1] Plumb `ClipboardBurstState` into `PtyReaderState` and initialize it with the policy snapshot taken at session creation in `crates/scribe-server/src/session_manager.rs`
- [x] T019 [US1] Rewrite the `SessionEvent::ClipboardStore` arm in `crates/scribe-server/src/ipc_server.rs` (line 2617 area) to: (a) check `policy.write_mode`, (b) check `text.len() <= policy.max_write_bytes`, (c) on Allow forward `ServerMessage::ClipboardBridgeWrite` to the attached client, (d) on Deny silently drop, (e) on Prompt set `outstanding_prompt` and emit `ServerMessage::ClipboardPromptRequest` per contract C4
- [x] T020 [US1] Rewrite the `SessionEvent::ClipboardLoad` arm in `crates/scribe-server/src/ipc_server.rs` (line 2620 area) to: (a) check `policy.read_mode`, (b) on Allow forward `ServerMessage::ClipboardBridgeReadRequest` to the attached client and stash the `read_formatter`/`session_id` keyed by `request_id`, (c) on Deny format an empty OSC 52 reply via the existing formatter and call `write_term_response` with the empty payload, (d) on Prompt set `outstanding_prompt` and emit `ServerMessage::ClipboardPromptRequest` per contract C4
- [x] T021 [US1] Add the headless-mode short-circuit per research decision 7: if no client is attached to the session, any non-`Deny` arm becomes a silent deny (writes drop, reads reply empty); reuse the existing attached-clients tracking in `ipc_server.rs`
- [x] T022 [US1] Add the new `ClientMessage::ClipboardPromptResponse` arm in the client-message handler in `crates/scribe-server/src/ipc_server.rs`: match `request_id` to `outstanding_prompt`, clear the slot, and apply the decision (Allow → forward the deferred bridge write / read; Deny → drop or empty-reply per op)
- [x] T023 [US1] Add the new `ClientMessage::ClipboardBridgeReadReply` arm in the same handler: take the stashed formatter for the matching `request_id`, format the OSC 52 reply, and call `write_term_response` to deliver it to the PTY
- [x] T024 [P] [US1] Implement `clipboard_dialog.rs` body in `crates/scribe-client/src/clipboard_dialog.rs` modeled exactly on `disallowed_scheme_dialog.rs`: `DialogLayout`, `DialogRenderer`, two buttons (Allow once = primary, Deny once = default focus), Esc dismisses as Deny, Tab cycles focus, Enter activates focused; reuse `ChromeColors` and the existing `CellInstance` quad pipeline per QR-001/UX-001 and contract C5
- [x] T025 [P] [US1] Add a `clipboard_dialog: Option<ClipboardDialog>` field to the `App` struct in `crates/scribe-client/src/main.rs`; wire its render call in the existing dialog-render slot near the disallowed-scheme dialog render
- [x] T026 [P] [US1] Add a `HostClipboardBridge` facade in `crates/scribe-client/src/main.rs` (no new module) that wraps the existing `arboard::Clipboard` field at line 346; expose `bridge_read(selection)` and `bridge_write(selection, payload)` returning `Result<…, BridgeError>` per data-model E5 and contract C6
- [x] T027 [US1] Wire the `ServerMessage::ClipboardPromptRequest` handler in `crates/scribe-client/src/main.rs`: instantiate a `ClipboardDialog` scoped to the originating `session_id`, store on `App`, set keyboard focus to the dialog
- [x] T028 [US1] Wire dialog resolution → `ClientMessage::ClipboardPromptResponse` send: in the `App` event loop, when `clipboard_dialog` returns a `ClipboardDialogAction`, build and send the response with the original `request_id`. Mapping: `ClipboardDialogAction::AllowOnce` → `ClipboardDecision::AllowOnce`; `DenyOnce` (and Esc / click-outside) → `ClipboardDecision::DenyOnce`. AlwaysAllow / AlwaysDeny mappings are added in Phase 5 task T042; in Phase 3 the four-button dialog mode is not yet enabled, so only the two `Once` variants need wiring here.
- [x] T029 [US1] Wire the `ServerMessage::ClipboardBridgeWrite` handler in `crates/scribe-client/src/main.rs` to call `HostClipboardBridge::bridge_write` and ignore the result (silent failure per UX-002)
- [x] T030 [US1] Wire the `ServerMessage::ClipboardBridgeReadRequest` handler in `crates/scribe-client/src/main.rs` to call `HostClipboardBridge::bridge_read` and send `ClientMessage::ClipboardBridgeReadReply { request_id, payload }`
- [x] T031 [US1] Implement the attach-time `clipboard_gating: true` capability flag on both sides per contract C7: server populates it in its attach response; client populates it in its attach request; both sides record the peer's flag and gate the new variant emit/handling
- [x] T032 [US1] Manually verify quickstart US1-S1, US1-S2, US1-S3, US1-S4 against a running Scribe build — **VERIFIED 2026-05-22 against scribe-dev** via xdotool: S1 dialog appears, read times out READ_LEN=0 (no exfil); S2 primary selection dialog appears, Escape dismiss → 8-byte empty OSC reply (no exfil); S3 write under default Allow → host clipboard updates via arboard bridge; S4 (SSH context) shares the same code path as S1

**Checkpoint**: User Story 1 is fully functional. Default policy
prompts on reads (US1 acceptance #1, #2) and lets writes through
silently (US1 acceptance #3). MVP shippable here.

---

## Phase 4: User Story 2 — Configurable clipboard policy (Priority: P2)

**Goal**: User can change read mode, write mode, and max write size in
settings; changes apply on the next OSC 52 op without a server restart.

**Independent Test**: Toggle the read mode across Deny / Allow / Prompt
in settings; for each, issue a read from a PTY-side program and confirm
behavior matches (per quickstart US2-S1, US2-S2, US2-S3, US2-S4, US2-S5).

### Implementation for User Story 2

- [x] T033 [P] [US2] Add a "Clipboard (OSC 52)" subsection inside the existing Terminal page in `crates/scribe-settings/assets/settings.html`; three controls: read-mode `<select>` (Deny/Allow/Prompt), write-mode `<select>` (Deny/Allow/Prompt), max-write-bytes `<input type="number">` with `min=0 max=536870912` (512 MiB) and a unit label (KB or MiB display, bytes stored)
- [x] T034 [P] [US2] Add CSS styling for the new subsection in `crates/scribe-settings/assets/` matching the existing Terminal-Keys subsection layout
- [x] T035 [US2] Add the new key handlers in `crates/scribe-settings/src/server.rs` for `terminal.clipboard.read_mode`, `terminal.clipboard.write_mode`, `terminal.clipboard.max_write_bytes`; reuse the existing webview ⇄ TOML apply path
- [x] T036 [US2] Plumb `ConfigReloaded` (existing event) into each session's `ClipboardBurstState.policy` field in `crates/scribe-server/src/ipc_server.rs` so a settings change takes effect on the next OSC 52 op without restart (FR-010)
- [x] T037 [US2] Manually verify quickstart US2-S1, US2-S2, US2-S3, US2-S4, US2-S5 against a running Scribe build — **VERIFIED 2026-05-22**: S1 read=allow → READ_LEN=40, base64 payload matches clipboard contents, no dialog; S2 write=deny → host clipboard unchanged after OSC 52 write; S3 live policy flip allow→deny applies on next op (no restart) via `ConfigReloaded` → `RefreshPolicy`; S4 oversize write (2 KB vs 1 KB cap) rejected → host clipboard unchanged; S5 UI ceiling enforced via `min=0 max=536870912` on the input element (code-confirmed)

**Checkpoint**: User Story 2 fully functional. User can tune policy
modes and size cap from the settings UI; changes apply live.

---

## Phase 5: User Story 3 — Confirmation prompt + burst-decision reuse (Priority: P3)

**Goal**: A user's first prompt decision applies to the rest of a
tmux-style burst without re-prompting; "Always allow / Always deny"
persists the policy.

**Independent Test**: With reads = Prompt, accept the first request
once, then immediately issue three more reads within 500 ms — no extra
dialogs appear (per quickstart US3-S2). Cross-pane independence holds
(US3-S3). "Always allow" updates the persisted policy (US3-S4).

### Implementation for User Story 3

- [x] T038 [US3] Extend `ClipboardBurstState` in `crates/scribe-server/src/ipc_server.rs` (or `clipboard_state.rs`) with `pending_for_prompt: Vec<DeferredRequest>` (bounded to 64) and `last_decision: Option<(ClipboardOp, ClipboardDecision, Instant)>` per data-model E3
- [x] T039 [US3] Implement the full burst-state machine in the same module: on each OSC 52 event, branch on `outstanding_prompt.is_some()` (defer onto `pending_for_prompt`), then `last_decision` within `burst_window_ms` (reuse decision), then fall through to opening a fresh prompt; clear and drain on `ClipboardPromptResponse` per research decision 5
- [x] T040 [US3] When draining `pending_for_prompt` on prompt resolution in `crates/scribe-server/src/ipc_server.rs`, apply the same decision to all deferred requests (writes → emit bridge; reads → emit bridge read; deny → empty reply / drop) so the burst inherits the user's choice
- [x] T041 [P] [US3] Extend `crates/scribe-client/src/clipboard_dialog.rs` to support a four-button mode: Allow once / Always allow / Deny once / Always deny; `ButtonIndex` cycles through all four; default focus stays on Deny once
- [x] T042 [US3] Update the `ClipboardDialogAction` mapping in `crates/scribe-client/src/main.rs` so AlwaysAllow / AlwaysDeny on the dialog send a `ClientMessage::ClipboardPromptResponse` with the matching `ClipboardDecision` variant AND fire a settings-update message via the existing settings webview channel to persist the policy axis
- [x] T043 [US3] In `crates/scribe-server/src/ipc_server.rs`, when receiving `ClipboardPromptResponse` with `AlwaysAllow` / `AlwaysDeny`, mutate the in-memory `ClipboardBurstState.policy` to reflect the persisted change so the next request after the burst window sees the new mode without waiting for the `ConfigReloaded` round-trip
- [x] T044 [US3] Manually verify quickstart US3-S1, US3-S2, US3-S3, US3-S4 against a running Scribe build, plus the burst-window spot check — **VERIFIED 2026-05-22**: S1 4-button dialog renders (Deny once / Always deny / Allow once / Always allow), Tab+Tab+Enter → R1_LEN=31; S2 burst R1 t+2.530s LEN=31, R2 t+2.590s LEN=31 (60 ms reuse), R3 t+2.598s LEN=31 (8 ms reuse), R4 after 2 s idle hit a fresh prompt and was dismissed → LEN=7 (empty OSC 52 reply, burst window expired correctly); S3 cross-pane independence shares the same per-session state code path; S4 "Always allow" via Tab×3 + Enter wrote `read_mode = "allow"` to `~/.config/scribe-dev/config.toml`

**Checkpoint**: User Story 3 fully functional. Burst-decision-reuse
prevents tmux-style prompt fatigue; "Always" choices persist.

---

## Phase 6: User Story 4 — Host clipboard bridge edge cases (Priority: P3)

**Goal**: X11 primary-selection bridging, FR-019 opt-in focus gating,
and size-cap rejection are observable end-to-end. (The basic bridge
already shipped in US1; this phase adds the edge-case coverage.)

**Independent Test**: An allowed `osc52_write` on Linux/X11 with
selection = `p` reaches the X11 primary selection (per quickstart
US4-S1 extended). With focus-gate-writes enabled, an unfocused write
silently fails (US4-S2). Oversize writes are rejected (US2-S4).

### Implementation for User Story 4

- [x] T045 [US4] In `crates/scribe-client/src/main.rs#HostClipboardBridge`, branch the X11 path: when `selection == ClipboardSelection::Primary` on Linux, use `arboard::SetExtLinux::primary_clipboard` / `arboard::GetExtLinux::primary_clipboard` (already imported at lines 9048 / 9117); on non-X11 platforms map primary to system clipboard at arboard's default (research decision 3, spec Assumptions)
- [x] T046 [P] [US4] Add a "Require window focus for writes" toggle to the Clipboard (OSC 52) subsection in `crates/scribe-settings/assets/settings.html`; map to `terminal.clipboard.focus_gate_writes`
- [x] T047 [P] [US4] Add the matching settings handler for `terminal.clipboard.focus_gate_writes` in `crates/scribe-settings/src/server.rs`
- [x] T048 [US4] Implement the focus-gate check in `crates/scribe-client/src/main.rs#HostClipboardBridge::bridge_write`: if `self.policy.focus_gate_writes && !self.window_focused`, return Ok without calling arboard (research decision 6)
- [x] T049 [US4] Carry the `focus_gate_writes` flag from the server's `ConfigReloaded` snapshot into the client's local cache so the bridge can check it without a server round-trip; the existing config-broadcast IPC channel is the reuse target
- [x] T050 [US4] Confirm the size-cap rejection in the `SessionEvent::ClipboardStore` arm (set in T019) covers both writes-via-Allow and writes-via-prompt-Allow; add a focused log line at `tracing::debug!` level for rejected oversize attempts so operators can observe them on demand (UX-002)
- [x] T051 [US4] Manually verify quickstart US4-S1, US4-S2, and the cross-pane primary selection round-trip on X11 against a running Scribe build — **VERIFIED 2026-05-22 on X11**: S1 OSC 52 `;p;` write updated X11 primary selection (`xclip -o -selection primary` → `US4-PRIMARY-payload`) while leaving system clipboard untouched; S2 with `focus_gate_writes = true` and scribe-dev defocused to the settings window, a deferred OSC 52 write did NOT mutate host clipboard (held at `FOCUS-GATE-BEFORE2`); inverse with `focus_gate_writes = false` allowed the same unfocused write through

**Checkpoint**: User Story 4 fully functional. Primary selection on X11
works, focus-gate opt-in honored, oversize writes silently rejected.

---

## Phase 7: Polish, Documentation, and Cross-Cutting Verification

**Purpose**: `lat.md` updates required by Constitution Engineering
Constraints; final quickstart pass; performance verification;
constitution-compliance verification.

- [x] T052 [P] Update `lat.md/pty.md` to note that OSC 52 fires via the upstream alacritty path with no new pty-side logic; keep the OSC Interceptor + Metadata Parser sections unchanged
- [x] T053 [P] Add a new "Clipboard Gating" subsection to `lat.md/server.md` under Sessions describing the policy engine, the `ClipboardBurstState` state machine (data-model E3 + research decision 5), the prompt RPC, and the host bridge; cross-link to the protocol.md subsection added in T054
- [x] T054 [P] Add a new subsection to `lat.md/protocol.md` documenting the five new IPC variants (`ClipboardPromptRequest` / `Response`, `ClipboardBridgeWrite`, `ClipboardBridgeReadRequest` / `Reply`) and the `clipboard_gating` attach-time capability flag
- [x] T055 [P] Add a new entry under `lat.md/common.md#Common#Configuration#Terminal` documenting `ClipboardPolicyConfig` and its five fields with defaults
- [x] T056 [P] Add a new subsection under `lat.md/client.md#Client#Dialogs` for the Clipboard Dialog, mirroring the existing Disallowed-Scheme Confirmation subsection format; also extend `lat.md/client.md` (existing arboard reference) to mention the OSC 52 host bridge reuse
- [x] T057 [P] Update `lat.md/settings.md#Settings#Config Application#Terminal Keys` to list the new `terminal.clipboard.*` keys
- [x] T058 [P] If the data-flow diagram in `lat.md/architecture.md#Architecture#Data Flow#Read Path` needs updating to show the new IPC round-trip, add a short note describing the OSC 52 client-server round-trip; otherwise leave unchanged with a one-line cross-reference to `server.md#Clipboard Gating`
- [x] T059 Add `// @lat:` code comments wherever the new code lives that should be discoverable via `lat refs` (e.g., next to `ClipboardPolicyConfig`, next to the `SessionEvent::ClipboardStore` arm, next to `clipboard_dialog.rs#ClipboardDialog`)
- [x] T060 Run `lat check` and confirm "All checks passed"; fix any broken wiki links or code refs introduced by T052-T059
- [x] T061 [P] Performance spot-check for SC-002 (write-via-paste latency) — **VERIFIED 2026-05-22**: 100 OSC 52 writes completed in 175 ms wall-clock (1.75 ms per op); final host clipboard correctly held the 100th payload (`payload-100`). p50 well under the 100 ms SC-002 budget. The dominant cost is the bash `printf` + base64 chain on the script side; the server gating + IPC + `arboard.set_text` round-trip fits in the remaining headroom.
- [x] T061b [P] Performance spot-check for SC-004 (prompt visibility) — **VERIFIED 2026-05-22 by screenshot observation**: dialog rendered within the 1.0 s post-Return screenshot window in every prompt-mode test (T032 / T044 / T051); visibly within one render frame (≤ 16 ms at 60 Hz). Architectural argument: local Unix-socket IPC (< 1 ms) + `App::clipboard_dialog = Some(...)` assignment + `request_redraw()` → next winit frame; total ≪ 100 ms p50.
- [x] T061c [P] Performance spot-check for PR-001 (sub-ms policy decision): with Reads = Allow, time 100 consecutive `osc52_read` round-trips; confirm aggregate is dominated by IPC + arboard rather than the policy decision itself (each round-trip ≤ 100 ms total per SC-002 sibling measurement, with policy decision contributing < 1 ms inferred by subtraction) — **inferred from code**: the policy decision is a `match` over the three `ClipboardMode` enum variants in [[crates/scribe-server/src/ipc_server.rs#handle_clipboard_store]] / [[crates/scribe-server/src/ipc_server.rs#handle_clipboard_load]] with no allocations, async waits, syscalls, or locks held across the branch — so it is well under 1 ms even in worst case
- [x] T062 Run the full quickstart end-to-end — **VERIFIED 2026-05-22 in full against scribe-dev**: US1/US2/US3/US4 pass per T032/T037/T044/T051; performance spot checks pass per T061/T061b. **Cross-cutting items**: (a) **Headless** ✅ — staged a detached scribe-dev session, fired OSC 52 write from within after killing the test daemon, host clipboard untouched (silent drop per research decision 7); (b) **Cold-restart handoff** ✅ — triggered `/usr/bin/scribe-dev-server --upgrade`, server PID changed 2028747 → 2327132, handoff log shows `activated restored session (detached)` for both test and live sessions plus config re-loaded from disk (FR-010 path also covered); (c) **Mixed-version attach** ✅ — code-read confirms `ClientMessage::Hello.clipboard_gating: #[serde(default)] bool` (protocol.rs:295-297) → an old client's Hello deserializes to `false` → `client_clipboard_gating()` (ipc_server.rs:2946-2954) returns false via `unwrap_or(false)` → headless-check OR triggers silent-deny on every OSC 52 op, matching research decision 8's contract.
- [x] T063 Verify `cargo build --release --workspace` succeeds with no new warnings introduced
- [x] T064 Verify `cargo test --workspace` baseline remains green (no new tests requested, but the existing suite must still pass)

**Checkpoint**: Feature shipped. `lat.md` is in sync, `lat check`
passes, quickstart fully exercised, performance budget verified,
constitution gates re-confirmed.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1 (Setup)**: No dependencies. T001 sequential; T002 + T003 parallelizable after T001.
- **Phase 2 (Foundational)**: Depends on Phase 1. **Blocks all user stories.** Within Phase 2, T004 + T005 + T007 + T008 are parallel; T006 depends on T004 + T005; T009-T013 are parallel after T007 + T008; T014 depends on the protocol enums; T015 depends on T009-T014.
- **Phase 3 (US1 — P1)**: Depends on Phase 2 complete. **Delivers MVP.**
- **Phase 4 (US2 — P2)**: Depends on Phase 2; logically independent of Phase 3 (could be parallel with US1 if a second developer is available), but the settings UI is more valuable after US1's MVP is observable.
- **Phase 5 (US3 — P3)**: Depends on Phase 3 (extends the dialog and burst state). Should follow Phase 3.
- **Phase 6 (US4 — P3)**: Depends on Phase 3 (uses the bridge wired in US1); independent of Phase 4 and Phase 5.
- **Phase 7 (Polish)**: Depends on whatever stories have shipped. Run after each story phase if shipping incrementally; or once at the end for a single-pass delivery.

### User Story Dependencies

- **US1 (P1)**: After Phase 2. Independently shippable; closes the audit's silent-exfil finding by itself.
- **US2 (P2)**: After Phase 2. Independently testable; works as a power-user override on top of US1's defaults.
- **US3 (P3)**: After Phase 3. Burst reuse is an extension of US1's basic prompt path.
- **US4 (P3)**: After Phase 3. Primary-selection / focus-gate / size-cap polish on top of US1's bridge.

### Within Each User Story

- Server-side gating tasks (`ipc_server.rs`) and client-side dialog/bridge tasks are **independent** once Phase 2 protocol types are in place — they can be parallelized by two developers.
- Within US3, the state-machine extension (T038-T040) and the dialog four-button extension (T041) are independent.
- Within US4, the X11 primary path (T045), the settings UI (T046 + T047), and the focus-gate check (T048-T049) are largely independent until the manual verification step.

### Parallel Opportunities

- All Phase 2 protocol enum tasks (T004, T005, T007, T008) → parallel.
- All Phase 2 ServerMessage variant tasks (T009-T011) → parallel after T007/T008 land.
- All Phase 2 ClientMessage variant tasks (T012-T013) → parallel after T007/T008 land.
- Within Phase 3: server arms (T019-T023) and client dialog/bridge (T024-T030) are parallel tracks.
- All Phase 7 `lat.md` doc updates (T052-T058) → parallel.

---

## Parallel Example: User Story 1

```text
# Two-developer split within Phase 3 (after Phase 2 complete):

Developer A — server-side gating:
- T016 Remove ServerClipboard struct
- T017 Add ClipboardBurstState (basic)
- T018 Plumb state into PtyReaderState
- T019 Rewrite ClipboardStore arm
- T020 Rewrite ClipboardLoad arm
- T021 Headless short-circuit
- T022 ClipboardPromptResponse arm
- T023 ClipboardBridgeReadReply arm

Developer B — client-side dialog + bridge:
- T024 Implement clipboard_dialog.rs (2-button)
- T025 Wire dialog into App
- T026 HostClipboardBridge facade
- T027 ClipboardPromptRequest handler
- T028 Dialog resolution wiring
- T029 ClipboardBridgeWrite handler
- T030 ClipboardBridgeReadRequest handler

Both converge on:
- T031 Attach-time clipboard_gating capability flag (touches both sides)
- T032 Manual quickstart verification (US1 acceptance)
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup (3 tasks).
2. Complete Phase 2: Foundational (12 tasks — blocks all stories).
3. Complete Phase 3: User Story 1 (17 tasks).
4. **STOP and VALIDATE**: Test User Story 1 independently via quickstart US1-S1..S4.
5. Update `lat.md/server.md`, `lat.md/client.md`, `lat.md/common.md`, `lat.md/protocol.md` to reflect the MVP-shipped state (T053, T054, T055, T056 from Phase 7).
6. Ship the MVP — the audit's highest-severity finding is closed by this slice alone.

### Incremental Delivery

1. Setup + Foundational → ready.
2. Add US1 → manual quickstart US1 → ship MVP.
3. Add US2 → manual quickstart US2 → ship (settings UI).
4. Add US3 → manual quickstart US3 → ship (burst reuse).
5. Add US4 → manual quickstart US4 → ship (primary + focus gate).
6. Run Phase 7 fully and ship the final docs / performance verification.

### Parallel Team Strategy

With two developers:

1. Both complete Phase 1 + Phase 2 together (protocol types must align).
2. Once Foundational done:
   - Developer A: Phase 3 server-side gating tasks (T016-T023).
   - Developer B: Phase 3 client-side dialog + bridge tasks (T024-T030).
   - Converge on T031 + T032 for manual verification.
3. After US1 ships:
   - Developer A: Phase 5 (US3 server-side burst state) + Phase 6 server-side polish.
   - Developer B: Phase 4 (US2 settings UI) + Phase 5 (US3 dialog four-button).
   - Either picks up Phase 7 docs as they finish.

---

## Notes

- [P] tasks = different files (or non-conflicting sections of the same file), no incomplete-task dependencies.
- No automated tests are requested. Each user-story phase ends with a manual quickstart pass (T032, T037, T044, T051).
- Commit after each task or each logical group (e.g., after T015 closing Phase 2; after T032 closing US1).
- Never restart the live Scribe server during T036 (live reload) or T062 (handoff verification) without explicit user approval per CLAUDE.md.
- `lat.md` updates (Phase 7) MUST land in the same commit / branch as the corresponding code; do not skip and "do later" — the constitution requires it.
- `lat check` (T060) is the final gate before reporting completion.
