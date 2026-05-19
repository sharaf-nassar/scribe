# Implementation Plan: Keyboard Protocol & Command Awareness

**Branch**: `005-keyboard-command-awareness` | **Date**: 2026-05-18 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/005-keyboard-command-awareness/spec.md`

## Summary

Complete two shipped-but-unfinished terminal integrations, both entirely client-side:

1. **Kitty keyboard protocol** — replace the binary `KeyboardProtocol` enum with the five
   real progressive-enhancement flags read per-session from `Term::mode()`, and encode every
   key/modifier combination (plus key-release and key-repeat events) as protocol-conformant
   CSI-u when the focused application has negotiated it. Legacy encoding stays byte-identical
   when nothing is negotiated.
2. **Command awareness** — stop dropping the OSC 133 `exit_code` at the client `UiEvent`
   boundary, classify each command Success/Failure/Unknown, differentiate scrollbar marks by
   status, show the latest command's outcome in the status bar, and add jump-to-failure
   navigation alongside the existing prompt-jump shortcuts.

Research confirmed **zero IPC wire change** for either track: `ClientMessage::KeyInput`
already carries raw encoded bytes, and `ServerMessage::PromptMark` already carries
`exit_code: Option<i32>`. The work is the client-side "last hop" plus one config field per
track. No server restart, no protocol/persistence migration.

## Technical Context

**Language/Version**: Rust 1.87, edition 2024 (existing workspace)
**Primary Dependencies**: `alacritty_terminal` 0.26.0-rc1 (`TermMode` Kitty flag bits +
push/pop/query stack already handled internally per session), `winit` 0.30.13 (`KeyEvent`
exposes `state`, `repeat: bool`, `logical_key`, `physical_key`, `text: Option<SmolStr>`,
`key_without_modifiers()`), `wgpu` 29 (existing scrollbar/status-bar quad path), `serde` /
`rmp-serde` (config + IPC — no IPC change)
**Storage**: `~/.config/scribe/config.toml` — one new `[terminal]` key
(`keyboard_protocol_enhanced`, default `true`) and one new `[keybindings]` action
(`jump_to_failure`). No persisted-state/restore schema change (command records are ephemeral
per-attach, never written to the cold-restart snapshot).
**Testing**: Manual quickstart per user story (see `quickstart.md`). No new automated test
code is requested by the spec (QR-002); deferral rationale recorded in Constitution Check
below. The existing `scribe-test` input-simulation harness is noted as the surface a future
conformance suite would use if explicitly approved.
**Target Platform**: `scribe-client` on Linux (GTK) and macOS (tao); all changes client-side
**Project Type**: Desktop application — Rust client-server terminal workspace
**Performance Goals**: Added per-keystroke encoding cost well under one 60 fps frame (~16 ms);
realistic target is sub-microsecond added CPU (5 bitset reads + one `match`, zero added
allocation on the typed-text path). Scrollbar/status-bar render shows no measurable
frame-rate change at the 10,000-line scrollback cap.
**Constraints**: SC-003 — legacy (non-negotiating) applications must receive byte-identical
input before/after. Per-pane isolation (SC-008). Config opt-out default-enabled (FR-006).
Unknown/unreported exit status must never render as failure (FR-012/SC-006).
**Scale/Scope**: ~6 `scribe-client` source files + `scribe-common/src/config.rs` + 3
`lat.md` sections. Two independently shippable tracks. Hard scope boundary: no per-command
output folding, selection, or grouping (deferred per spec Assumptions).

## Constitution Check

*GATE: evaluated before Phase 0 and re-evaluated after Phase 1 design. Result: PASS (both).*

- **Code Quality**: **PASS**. Both tracks stay within existing crate boundaries
  (`scribe-client` encoding/UI + `scribe-common` config); no new crate, no new dependency.
  Typed contracts replace weaker ones: a five-field `KittyFlags` struct supersedes the
  lossy two-variant `KeyboardProtocol` enum; a `CommandStatus` enum + `CommandRecord`
  supersede the untyped `Vec<usize>` mark list. No duplicated protocol/config parsing
  (the OSC 133 parser and msgpack protocol are untouched and already complete). No
  unrelated refactor — the `prompt_marks → command_records` change is required by FR-008/
  FR-013, not opportunistic.
- **Testing Strategy**: **PASS (with documented deferral)**. Every user story has an
  independent manual verification path in `quickstart.md` (US1: byte-probe against a
  negotiating app; US2: pass-then-fail command; US3: jump in a seeded scrollback).
  Constitution II permits documented manual quickstart when tests are not explicitly
  requested; the spec (QR-002) explicitly does not request automated tests and the project
  rule is test-on-explicit-request only. Rationale recorded: keyboard-protocol conformance
  is a strong *future* automated-suite candidate (would extend the existing `scribe-test`
  input-simulation harness), but adding it now would violate the test-only-on-request
  constraint; it is called out as a recommended follow-up requiring explicit approval, not
  silently skipped.
- **User Experience Consistency**: **PASS**. New scrollbar status colors reuse the existing
  tick-instancing path and palette-slot pattern; the status-bar outcome indicator reuses the
  existing left-segment `connected_dot`/colored-glyph pattern; `jump_to_failure` is a normal
  configurable `KeybindingsConfig` action like `prompt_jump_*`; the opt-out sits in the
  existing `[terminal]` config section. Legacy input stays byte-identical, protecting muscle
  memory and not disrupting server-owned sessions.
- **Performance**: **PASS**. Measurable budgets stated above and per-change in `research.md`;
  hot path adds only bitset reads + a `match` with no new allocation on the common path;
  scrollbar loop stays O(marks) with one added enum compare. Final verification step names
  the manual measurement (frame pacing under a scripted keystroke flood) in `quickstart.md`.
- **Operational Safety**: **PASS**. No live server restart/upgrade/stop required (all changes
  client-side; server already negotiates and already forwards `exit_code`). Worktree
  preserved, no unrelated file churn. `lat.md` updates are planned and enumerated
  (`client.md` Key Translation Priority + Scrollbar Prompt Mark Indicators + IPC Client;
  `pty.md` OSC 133 cross-reference), with `lat check` to run before completion. Config change
  carries an explicit compatibility decision: additive `#[serde(default)]` field → old
  configs load unchanged, no migration, no protocol/persistence version bump.

No violations → **Complexity Tracking is intentionally empty**.

## Project Structure

### Documentation (this feature)

```text
specs/005-keyboard-command-awareness/
├── plan.md              # This file
├── research.md          # Phase 0 — decisions, verified APIs, alternatives
├── data-model.md        # Phase 1 — entities & state transitions
├── quickstart.md        # Phase 1 — per-story manual verification + perf check
├── contracts/           # Phase 1 — conformance / config / keybinding / event contracts
│   ├── kitty-keyboard-encoding.md
│   ├── config-and-keybindings.md
│   └── client-uievent-change.md
├── checklists/
│   └── requirements.md  # Spec quality checklist (from /speckit-specify)
└── tasks.md             # Phase 2 — created later by /speckit-tasks (NOT here)
```

### Source Code (repository root — existing workspace, no new crates/modules)

```text
crates/scribe-client/src/
├── input.rs        # Track A: replace KeyboardProtocol→KittyFlags; full CSI-u in
│                   #   translate_key/translate_named_with_modifiers/
│                   #   translate_character_with_modifiers; extend build_csi_u_seq
│                   #   with event-type; functional-key table; alt-key + assoc-text fields
├── main.rs         # Track A: focused_keyboard_protocol→KittyFlags from Term::mode();
│                   #   relax Pressed-only gate for terminal path when REPORT_EVENT_TYPES.
│                   # Track B: handle_prompt_mark state machine; handle_prompt_jump_*;
│                   #   new handle_jump_to_failure; trim call-site signature update;
│                   #   status-bar data feed
├── ipc_client.rs   # Track B: add exit_code to UiEvent::PromptMark; stop dropping it
├── pane.rs         # Track B: CommandStatus/CommandRecord; prompt_marks→command_records;
│                   #   shift_absolute_marks_after_trim signature; last_command_status
├── scrollbar.rs    # Track B: color tick by CommandStatus
└── status_bar.rs   # Track B: last_command_status field + left-segment indicator

crates/scribe-common/src/
└── config.rs       # Track A: TerminalConfig.keyboard_protocol_enhanced (default true)
                    # Track B: KeybindingsConfig.jump_to_failure (+ default)

lat.md/
├── client.md       # Key Translation Priority; Scrollbar Prompt Mark Indicators; IPC Client
└── pty.md          # OSC 133 cross-reference (exit-status now surfaced client-side)
```

**Structure Decision**: Single existing Rust workspace; no Option-style restructure. All
production change lands in `scribe-client` (six files) plus one `scribe-common` config file,
matching the crate responsibilities documented in `lat.md` (encoding/UI is client-owned;
config is common-owned). `scribe-pty`, `scribe-server`, and `scribe-common/src/protocol.rs`
are explicitly unchanged — research proved the data already reaches the client.

## Complexity Tracking

No constitution violations. Section intentionally empty.
