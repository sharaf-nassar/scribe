# Phase 0 Research: Keyboard Protocol & Command Awareness

All Technical Context items were resolvable from the codebase + verified external sources;
**no `NEEDS CLARIFICATION` remained**. Two parallel deep traces (Track A keyboard, Track B
command-awareness) produced the findings below.

## Verified External API Facts

These were read from actual source/docs, not memory (Constitution Engineering Constraint:
verify external library behavior before relying on it).

- **winit 0.30.13 `KeyEvent`** (Cargo.lock pinned; fields confirmed against live codebase
  usage and docs): `state: ElementState` (`Pressed`/`Released` — `Released` already used at
  `main.rs:5659`), `repeat: bool` (already used at `main.rs:104`), `logical_key: Key`,
  `physical_key: PhysicalKey`, `text: Option<SmolStr>` (the "associated text"),
  `key_without_modifiers()` (base/unshifted codepoint — already used at `input.rs:160,167`).
  → Press/repeat/release and alternate-key/associated-text data are **all already
  available**; Scribe currently discards them by early-returning on non-`Pressed`.
- **alacritty_terminal 0.26.0-rc1 `TermMode`** (Cargo.lock pinned; flag names proven to
  compile against this version via existing `main.rs` usage; semantics per the alacritty
  TermMode source, API-stable across 0.26.x): the five Kitty flags
  `DISAMBIGUATE_ESC_CODES` (`CSI = 1 u`), `REPORT_EVENT_TYPES` (`2`),
  `REPORT_ALTERNATE_KEYS` (`4`), `REPORT_ALL_KEYS_AS_ESC` (`8`),
  `REPORT_ASSOCIATED_TEXT` (`16`). The CSI `>`push / `<`pop / `?`query stack is maintained
  **inside `Term` per session**; `Term::mode()` always reflects the current top of stack.
  `kitty_keyboard: true` is already set on both the server `Term`
  (`session_manager.rs:106`) and the client pane `Term` (`pane.rs:178`), so negotiation is
  fully handled — only **client outbound key encoding** is missing.

## Decisions

### D1 — Replace the binary `KeyboardProtocol` enum with a five-flag `KittyFlags` struct
- **Decision**: New `KittyFlags { disambiguate, report_event_types, report_alternate_keys,
  report_all_keys, report_associated_text }` (all `bool`), built fresh each keystroke from
  the focused pane's `Term::mode()`. `focused_keyboard_protocol` returns it; the
  encoding functions accept it.
- **Rationale**: The protocol is progressive — apps negotiate *subsets* (Helix/Neovim
  commonly enable only flag 1). A two-state enum cannot gate per-flag behavior, which is the
  root cause of every audit symptom. Reading all five bits is the minimal correct model and
  satisfies FR-001/FR-003.
- **Alternatives rejected**: (a) Pass raw `TermMode` into `input.rs` — couples the encoder to
  an alacritty type, hurts isolation/testability. (b) Keep the enum, add ad-hoc flag params —
  perpetuates the lossy abstraction the audit flagged.

### D2 — Encoding lives at level 4 of the existing key-translation priority chain
- **Decision**: All CSI-u logic stays inside `translate_key` and its callees (level 4
  generic translation). Levels 1–3 (layout shortcuts, palette/settings/find, terminal
  shortcuts) are untouched and remain `Pressed`-only.
- **Rationale**: Preserves QR-001 architecture boundary and guarantees SC-003 (legacy apps
  unchanged) — the protocol branch only applies after configured shortcuts decline the key.
- **Alternatives rejected**: A pre-translation interceptor — would risk shortcut regressions
  and duplicate the dispatch chain.

### D3 — Source press/repeat/release from the existing winit event; relax the gate narrowly
- **Decision**: Event type = `Released`→3, `Pressed && repeat`→2, `Pressed && !repeat`→1.
  Widen the `ElementState::Pressed` gate **only on the terminal-key path and only when
  `report_event_types` is set**; blink-reset and shortcut checks keep their own `Pressed`
  guard.
- **Rationale**: Data already exists on the event; FR-002 requires release/repeat *only*
  when negotiated; scoping the relaxation prevents shortcut/overlay double-fire.
- **Alternatives rejected**: Global gate removal — would dispatch shortcuts on key release.

### D4 — `build_csi_u_seq` gains an optional event-type; modifier formula reused
- **Decision**: Extend to `build_csi_u_seq(codepoint, modifier_param, event_type:
  Option<u8>)`. The Kitty modifier value uses the same xterm formula already implemented
  (`1 + shift + alt + ctrl + super`); winit exposes no hyper/meta/caps/num, so `u8` cannot
  overflow — type unchanged. Add a `NamedKey → protocol codepoint` functional-key table.
- **Rationale**: Smallest change that produces conformant 2- and 3-parameter CSI-u forms;
  reuses the verified-correct modifier math.
- **Alternatives rejected**: A new encoder module — unnecessary; the existing builder is the
  right seam.

### D5 — Config opt-out is an additive defaulted `[terminal]` field
- **Decision**: `TerminalConfig.keyboard_protocol_enhanced: bool` with
  `#[serde(default = "default_true")]`. When `false`, `focused_keyboard_protocol` returns
  an all-false `KittyFlags` regardless of negotiation (pure legacy).
- **Rationale**: FR-006 + Constitution "compatibility decision": old configs load unchanged,
  no migration, no version bump.

### D6 — Single per-pane `command_records: Vec<CommandRecord>` replaces `prompt_marks`
- **Decision**: `CommandRecord { abs_pos: usize, status: CommandStatus }` with
  `CommandStatus { Success, Failure, Unknown }`; plus `Pane.last_command_status:
  Option<CommandStatus>`. `shift_absolute_marks_after_trim` retargets to shift
  `record.abs_pos`.
- **Rationale**: Jump nav already iterates the mark list; a parallel status vec would create
  index-sync bugs across prune/trim. One typed list reuses the proven absolute-position +
  trim-shift mechanism (FR-013/SC-007) and is the minimal model for FR-007/008/012.
- **Alternatives rejected**: Parallel `Vec<usize>` + `Vec<CommandStatus>` — fragile index
  coupling. Storing raw `i32` exit code — not needed for the spec (classification suffices);
  may be added later for an out-of-scope "show exit code" affordance.

### D7 — Status from the A→B→C→D state machine; anchor jumps on PromptStart (A)
- **Decision**: Open a record (`Unknown`) on `A`; on `D`, if the open record is still
  `Unknown`, set `0 → Success`, non-zero `→ Failure`, missing code `→ stays Unknown`. A new
  `A` before `D` leaves the prior record `Unknown`. Jump/scroll targets remain the `A` row
  (the visible prompt), preserving today's behavior; `C/D` only resolve status.
- **Rationale**: Directly satisfies FR-012/SC-006 (unreported ≠ failure) and keeps
  navigation muscle-memory identical (UX consistency).

### D8 — `jump_to_failure` is a new bindable action; reattach yields empty (non-misleading)
- **Decision**: Add `KeybindingsConfig.jump_to_failure` (distinct default) that reverse-scans
  `command_records` for the most recent `Failure`; no-op + non-disruptive signal when none
  (mirrors current top-of-scroll jump no-op). On reattach/handoff, `command_records` starts
  empty because `SessionReplay` reproduces cell content, not OSC 133 callbacks — historical
  rows show neutral/unknown, never fabricated status.
- **Rationale**: FR-010/FR-011 + FR-014. Absence is correct; fabricated history would be the
  only misleading outcome, and it is structurally impossible here.

## Resolved Risks (carried into the plan)

- **No IPC change**: `ClientMessage::KeyInput` is raw bytes; `ServerMessage::PromptMark`
  already carries `exit_code`. `UiEvent` is client-internal (not serialized) — adding
  `exit_code` is a Rust struct change, not a wire/version change.
- **Reattach/handoff**: keyboard flags re-derive from replayed bytes via the client `Term`;
  command records intentionally reset (ephemeral per-attach metadata).
- **Codex Alt+Enter override** (`main.rs:5071`): must keep firing before generic Kitty
  encoding; validate it stays coherent with the new encoder for the same key (no
  double-encode).
- **alacritty issue #8836** (Kitty flags enablement) — **RESOLVED (T002, verified against
  pinned source `~/.cargo/.../alacritty_terminal-0.26.0-rc1/src/term/mod.rs`)**: the five
  flags are `TermMode` bits 18–22 (`DISAMBIGUATE_ESC_CODES`=1<<18 … `REPORT_ASSOCIATED_TEXT`
  =1<<22). Push/pop is fully implemented upstream: `keyboard_mode_stack: Vec<KeyboardModes>`
  (:320), `set_keyboard_mode` (:1029), `report_keyboard_mode` reads `.last()` (:1275); a
  separate `inactive_keyboard_mode_stack` is swapped on alt-screen switch (:726) so
  alt-screen apps negotiate independently. **`Term::mode()` reflects the correct
  top-of-stack; no Scribe-side stack and no FR-003 fallback are required.** winit 0.30.13
  confirmed: `KeyEvent.location: KeyLocation` (event.rs:609) for left/right modifier
  disambiguation, `.repeat: bool` (:647), `key_without_modifiers()` platform-gated
  (modifier_supplement.rs:22 — "not available on all platforms"; encoder must degrade
  gracefully where absent).
