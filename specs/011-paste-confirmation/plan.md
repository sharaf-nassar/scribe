# Implementation Plan: Paste Confirmation (Multiline / Control-Character)

**Branch**: `011-paste-confirmation` | **Date**: 2026-05-28 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/011-paste-confirmation/spec.md`

## Summary

An opt-in, client-only confirmation gate that intercepts a paste before its
bytes reach the PTY and — only when the focused app has NOT enabled bracketed
paste AND the content contains a line break or a non-tab control/escape byte —
pops a two-button GPU dialog (**Cancel** default / **Paste**) that states why
the paste was flagged and shows a caret-escaped preview. Disabled by default
via a new `terminal.paste_confirmation` boolean.

Technical approach: reuse the existing disallowed-scheme dialog chrome verbatim
(spec 009 precedent), add one pure classifier, and **unify the two divergent
paste code paths** — the keybinding / context-menu path
(`send_paste_data`) and the middle-click path (`perform_primary_paste`) —
behind a single gated sender so no entry point can bypass the gate. Ride the
existing terminal-config ⇄ settings-webview round-trip for live reload. **No
protocol, IPC, or server change**; the paste content and the bracketed-paste
signal are both already client-side.

## Technical Context

**Language/Version**: Rust (existing `scribe` Cargo workspace; edition per
workspace)
**Primary Dependencies**: existing only — `alacritty_terminal` 0.26
(`TermMode::BRACKETED_PASTE`), the GPU dialog overlay infra (`wgpu`/`winit`),
`arboard` (clipboard / X11 primary selection), the `wry` settings webview, and
`scribe_common::config`. **No new crates.**
**Storage**: user config TOML — one additive key `terminal.paste_confirmation`
(`#[serde(default)]` → `false`).
**Testing**: manual quickstart per user story (QR-002 / Constitution II); the
pure `classify_paste` function is the unit-test seam if tests are later
explicitly requested (out of scope now).
**Target Platform**: Linux + macOS desktop. Note: middle-click
primary-selection paste (`perform_primary_paste`) is Linux-only; the gate must
cover it where it exists.
**Project Type**: desktop application (GPU terminal, client-server) — the work
is entirely in the GUI client + common config + settings webview.
**Performance Goals**: classification is O(n) in paste byte length; the dialog
appears with no user-perceptible delay for typical pastes; **zero** added work
on the paste path when the setting is disabled; no added per-frame render cost
beyond the existing dialog overlay.
**Constraints**: byte-identical delivery on confirm (no transform); parked
content released on resolve (no retained-clipboard growth); live config reload
without restart; client-only (no server round-trip).
**Scale/Scope**: small. One new client module
(`paste_confirmation_dialog.rs`: classifier + caret-escape helper + dialog),
one config bool, settings wiring (apply.rs + settings.html + settings.js), a
targeted unification of two paste functions in `main.rs`, plus `lat.md` sync.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design.*

### Initial Check (pre-Phase 0) — PASS

- **Code Quality**: **PASS** — Reuses existing abstractions (dialog overlay,
  paste pipeline, config/settings round-trip); adds no new dependency; uses a
  typed `PasteRisk` and a pure classifier. The unification of
  `perform_primary_paste` behind `send_paste_data` is a *targeted improvement
  to code directly being modified to gate it* (both paths must be gated), not
  an unrelated refactor — and it removes existing duplication plus a latent
  >4 KiB primary-paste chunking bug. Permitted under Principle I.
- **Testing Strategy**: **PASS** — Each of the three user stories has an
  independent manual quickstart path (QR-002). No automated tests added
  (test-only-on-explicit-request); the pure classifier is documented as the
  unit-test seam for later.
- **User Experience Consistency**: **PASS** — Dialog clones the
  disallowed-scheme dialog chrome, keyboard model (Esc=Cancel, Enter=focused,
  Tab cycle), and default-focus-on-safe-action convention; the settings toggle
  matches existing terminal toggles; plain-language helper text explains the
  bracketed-paste deferral so the feature never reads as "broken".
- **Performance**: **PASS** — O(n) classify only when enabled; skipped entirely
  when disabled; parked content freed on resolve; measurable budget stated
  (no perceptible paste→dialog latency; zero added latency when off).
- **Operational Safety**: **PASS** — No server restart; client-only; additive
  config key is backward compatible (absent key → `false` → today's behavior),
  so the compatibility decision is "no migration required" (recorded in
  research.md R7). `lat.md` updates are planned (client/common/settings) with
  `lat check` before completion.

### Post-Design Check (after Phase 1) — PASS

Re-evaluated against research.md + data-model.md + contracts/. No new
dependencies, no protocol/persistence migration, no new cross-cutting helper
beyond the one feature-local module. All gates remain **PASS**; Complexity
Tracking is empty.

## Project Structure

### Documentation (this feature)

```text
specs/011-paste-confirmation/
├── plan.md              # This file
├── research.md          # Phase 0 — decisions R1..R8
├── data-model.md        # Phase 1 — entities
├── quickstart.md        # Phase 1 — manual verification per user story
├── contracts/
│   └── paste-confirmation.md   # config key, settings data-key, dialog action contract, decision table
└── tasks.md             # Phase 2 — created by /speckit-tasks (NOT here)
```

### Source Code (repository root)

```text
crates/scribe-client/src/
├── paste_confirmation_dialog.rs   # NEW — classify_paste() (pure) + PasteRisk
│                                   #       + caret-escape preview helper
│                                   #       + PasteConfirmationDialog / PasteConfirmationAction
│                                   #       (cloned from disallowed_scheme_dialog.rs)
└── main.rs                         # gate inside send_paste_data() (after prepare_paste_target);
                                    # refactor perform_primary_paste() to fetch text then call send_paste_data();
                                    # App.paste_confirmation_dialog field + init;
                                    # render call (~5395 sibling) + window-event guard (~1723 sibling);
                                    # handle_paste_confirmation_action() resume/drop

crates/scribe-common/src/
└── config.rs                       # TerminalConfig.paste_confirmation: bool (#[serde(default)] → false) + Default impl

crates/scribe-settings/src/
├── apply.rs                        # add "terminal.paste_confirmation" to apply_terminal_key match → apply_terminal_behavior_key
└── assets/
    ├── settings.html               # Terminal-page toggle block (data-key="terminal.paste_confirmation")
    └── settings.js                 # setToggleValue("terminal.paste_confirmation", …); click dispatch is automatic

lat.md/
├── client.md                       # Dialogs → new "Paste Confirmation Dialog"; Input → paste gate note
├── common.md                       # Configuration#Terminal — add paste_confirmation to TerminalConfig
└── settings.md                     # Config Application#Terminal Keys — add the toggle + apply.rs ref
```

**Structure Decision**: Single-client-crate feature with config + settings
wiring. The dialog follows the one-file-per-dialog convention
(`close_dialog.rs`, `update_dialog.rs`, `clipboard_dialog.rs`,
`disallowed_scheme_dialog.rs`); the pure classifier + caret helper live in the
same new module (cohesive with their only consumer, independently unit-testable
as free functions). No new crate, no protocol crate touched.

## Complexity Tracking

> No constitution violations. Table intentionally empty.

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|--------------------------------------|
| (none)    | —          | —                                    |
