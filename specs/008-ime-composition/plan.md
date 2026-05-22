# Implementation Plan: IME Composition and Preedit Handling

**Branch**: `008-ime-composition` | **Date**: 2026-05-21 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/008-ime-composition/spec.md`

## Summary

Wire winit's `WindowEvent::Ime` pipeline into the existing Scribe client so
CJK input methods, X11 Compose, macOS Press-and-Hold, and dead keys all
deliver composed text into focused-pane PTYs. The change adds a per-focused-
pane preedit overlay rendered at the cursor cell, reports an OS-side cursor
rectangle so candidate popups follow the caret, and short-circuits the level-
4 key encoder when the OS reports the keystroke was consumed by the IME.

The work is **purely client-local**: no IPC, protocol, server, settings, or
config schema changes. The existing `KeyInput` write path carries committed
IME bytes as plain text — the encoder is *bypassed*, not modified.

## Technical Context

**Language/Version**: Rust (workspace edition per `Cargo.toml`)
**Primary Dependencies**:
  - `winit` — `Window::set_ime_allowed`, `Window::set_ime_cursor_area`,
    `WindowEvent::Ime(Ime::{Enabled, Preedit, Commit, Disabled})`.
  - `wgpu` — existing instanced-quad pipeline reused.
  - `cosmic-text` — existing glyph shaping/atlas reused for preedit text.
  - `x11rb` — existing `x11_focus.rs` focus guard reused.
**Storage**: N/A — preedit is transient, never persisted.
**Testing**: `cargo test --workspace` (existing 122-test input-pipeline suite
must remain green). No new automated tests requested in spec; manual quickstart
covers all three user stories.
**Target Platform**: macOS Cocoa IME (Press-and-Hold + system input methods),
Linux X11 (XIM / IBus / Fcitx), Linux Wayland (zwp_text_input_v3), Windows
IMM / TSF — all abstracted via winit.
**Project Type**: Desktop app, Rust workspace (`crates/scribe-{client,server,
pty,renderer,common,cli,settings,test}`).
**Performance Goals**:
  - Preedit visual update ≤16 ms (one 60Hz frame) from first composing
    keystroke.
  - IME cursor-rect update ≤16 ms after cursor cell change (PTY output,
    scroll, alt-screen redraw, resize, focus change).
  - Zero measurable regression in non-IME key encoder throughput (legacy and
    Kitty CSI-u paths byte-identical).
**Constraints**:
  - Must preserve the level-4 encoder boundary (`translate_key`,
    `translate_key_kitty`, `translate_numpad_app_keypad`) — IME adds a
    short-circuit, not a rewrite.
  - Must reuse the existing focus guard (`window_focused` + `x11_focus.rs`)
    as the IME activation gate.
  - No new config keys, no new protocol/IPC messages, no new persistence.
**Scale/Scope**: One IME state machine per focused pane; at most one
composition active at a time across the entire window.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **Code Quality**: **PASS**. Change is confined to `crates/scribe-client/`
  with at most a small additive helper in `crates/scribe-renderer/` for the
  preedit quad/text. Reuses existing typed bundles (`TerminalMode`),
  per-pane state (`Pane`), and the focus guard. No new cross-cutting
  abstractions, no new dependencies. The encoder boundary is preserved by
  adding an IME-active short-circuit at the *entry point* of key dispatch,
  not by modifying the encoder internals.

- **Testing Strategy**: **PASS**. Each user story has an independent manual
  quickstart path documented in `spec.md` and reproduced in
  `quickstart.md`. No new automated tests are requested in the spec because
  (a) IME requires a real OS input method to exercise meaningfully, which
  cargo-test cannot host, and (b) the existing 122-test input-pipeline
  suite already pins the non-IME byte path that this change must not
  regress. Constitution principle II permits this: the spec documents
  manual verification with rationale.

- **User Experience Consistency**: **PASS**. Preedit visual treatment
  (underline at cursor cell) follows established terminal convention
  (Alacritty/Kitty/WezTerm/Ghostty). Existing cursor styles (block/beam/
  underline) keep their current behavior at the composition anchor.
  Settings, shortcuts, selection, and server-session survival are
  untouched. No new keybindings.

- **Performance**: **PASS**. Measurable budgets stated: ≤16 ms preedit
  render, ≤16 ms cursor-rect update, zero non-IME-key throughput change.
  Hot path (legacy/Kitty encoders) is unchanged; the IME branch executes
  *before* the encoder dispatch and only when the OS reports a consumed
  keystroke. Preedit rendering reuses existing `cosmic-text` shaping and
  the wgpu instanced-quad pipeline — no new render passes.

- **Operational Safety**: **PASS**. No server restart, no IPC protocol
  change, no config migration, no persistence change. `lat.md` updates
  scoped to `client.md` (Input section + new Preedit subsection) and a
  short cross-reference in `rendering.md`. `lat check` will be run before
  completion.

No constitution violations. Complexity Tracking remains empty.

## Project Structure

### Documentation (this feature)

```text
specs/008-ime-composition/
├── plan.md              # This file
├── research.md          # Phase 0 output — winit IME contract, platform notes
├── data-model.md        # Phase 1 output — PreeditState + IME state machine
├── quickstart.md        # Phase 1 output — manual verification per user story
├── contracts/
│   └── ime-pipeline.md  # Phase 1 output — internal contract: WindowEvent → PTY
├── checklists/
│   └── requirements.md  # Spec quality checklist (already created)
└── tasks.md             # Phase 2 output (created by /speckit-tasks — not here)
```

### Source Code (repository root)

Touched files (Rust workspace; layout already established in `lat.md/architecture.md`):

```text
crates/scribe-client/src/
├── main.rs              # New WindowEvent::Ime arm; call set_ime_allowed /
│                        # set_ime_cursor_area on focus + cursor moves;
│                        # short-circuit ME-consumed keys; route Commit to PTY.
├── input.rs             # New `Ime` short-circuit at the entry of translate_key.
│                        # No changes to translate_key_kitty / legacy / numpad.
├── pane.rs              # New `preedit: Option<PreeditState>` field; helpers
│                        # set_preedit / clear_preedit / preedit_cursor_rect.
├── x11_focus.rs         # Reused unchanged — IME activation gated on existing
│                        # focus guard.
└── renderer overlay     # If a new file is needed, add
                          # `crates/scribe-client/src/preedit.rs` for the
                          # transient render request; otherwise extend
                          # `pane.rs` rendering path.

crates/scribe-renderer/src/
├── chrome.rs            # Reuse `solid_quad` for the preedit background /
│                        # underline. No new pipeline.
└── atlas.rs             # Reuse shaping + atlas upload paths for preedit
                          # glyphs (cold-cache cost acceptable; one-off).

lat.md/
├── client.md            # New subsection under Input: "IME Composition";
│                        # extend Key Translation Priority to mention IME gate.
└── rendering.md         # One-line cross-reference to client's preedit overlay.
```

**Structure Decision**: Pure additive change to `scribe-client` plus a
minimal documentation-only touch in `scribe-renderer` (reuses existing
helpers; no new render pipeline). No other crates change. This keeps the
blast radius identical to the recent Kitty CSI-u work and lets the change be
shipped behind no protocol or config rollout.

## Complexity Tracking

> No constitution violations recorded. This section intentionally empty.
