# Implementation Plan: Smart Selection

**Branch**: `002-smart-selection` | **Date**: 2026-05-12 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `specs/002-smart-selection/spec.md`

## Summary

Add global, iTerm2-style Smart Selection for terminal text. The feature adds configurable semantic-selection rules and explicit rule actions, exposes them in a dedicated Terminal settings section, and routes double-click or quad-click selection through the configured smart-selection matcher while preserving existing word, line, copy-on-select, URL hover, and mouse-reporting behavior.

## Technical Context

**Language/Version**: Rust 2024, workspace rust-version 1.87; embedded HTML/CSS/JavaScript for settings UI  
**Primary Dependencies**: `alacritty_terminal` for terminal grid state, existing workspace `regex` dependency for rule matching, `serde`/`toml`/`serde_json` for persisted settings and webview payloads, existing `winit` mouse events  
**Storage**: User config TOML via `scribe_common::config::ScribeConfig`, with settings webview JSON updates applied by `scribe-settings`  
**Testing**: Planning only in this phase; implementation verification should use focused `cargo test` package filters and existing functional/visual e2e patterns where appropriate  
**Target Platform**: Scribe desktop terminal on Linux and macOS  
**Project Type**: Desktop terminal emulator with Rust client, shared config crate, and embedded settings webview  
**Performance Goals**: Smart Selection should produce a visible selection within 100 ms on typical visible terminal content and avoid regex recompilation during normal click handling  
**Constraints**: Preserve copy-on-select, preserve Shift mouse-selection bypass for mouse-reporting applications, avoid automatic execution of configured actions, and keep command-like actions explicit  
**Scale/Scope**: Global feature across panes and windows, no profile-specific configuration, default rule set plus user-managed rules and actions

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

Initial gate status: PASS.

- Code Quality: PASS. The plan keeps persisted data in `scribe-common`, matching
  logic in a focused client module, and settings updates in the existing
  `scribe-settings::apply` path.
- Testing Strategy: PASS. The plan names focused `cargo test` package filters
  and the feature quickstart as verification paths while respecting the project
  instruction not to add test code unless explicitly requested.
- User Experience Consistency: PASS. The plan preserves mouse reporting,
  copy-on-select, existing selection behavior, and the settings webview
  structure.
- Performance: PASS. The plan sets a 100 ms visible-selection target and avoids
  regex recompilation during click handling.
- Operational Safety: PASS. The plan does not restart the server, is grounded in
  relevant `lat.md` sections, and reserves `lat.md` updates for implementation.

## Project Structure

### Documentation (this feature)

```text
specs/002-smart-selection/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/
│   ├── config-smart-selection.md
│   └── settings-ui-smart-selection.md
└── tasks.md
```

### Source Code (repository root)

```text
crates/scribe-common/src/
├── config.rs                 # persisted Smart Selection settings and defaults
└── lib.rs

crates/scribe-client/src/
├── main.rs                   # click dispatch, selection state, context menu integration
├── mouse_state.rs            # click counting extended to quad click
├── selection.rs              # selection ranges, text extraction, smart selection range support
└── smart_selection.rs        # matcher, candidate scoring, action parameter expansion

crates/scribe-settings/src/
├── apply.rs                  # apply Smart Selection config updates
└── assets/
    ├── settings.html         # Terminal page Smart Selection section
    ├── settings.js           # rule editor state, validation, save/reset/test actions
    └── settings.css          # rule table/editor styling

lat.md/
├── client.md                 # document smart selection behavior after implementation
├── settings.md               # document settings surface after implementation
└── test.md                   # document test specs if implementation adds tests
```

**Structure Decision**: Keep matching and action expansion in a focused client module because it depends on terminal grid text and click coordinates. Keep persisted data in `scribe-common` so the client and settings process share one schema. Keep UI-specific rule editing in the settings assets and route durable changes through the existing `scribe-settings::apply` path.

## Phase 0 Research Summary

Research decisions are recorded in [research.md](./research.md).

Resolved decisions:
- Use iTerm2's user-facing rule model: named regex rules, precision levels, default recognizers, and explicit actions.
- Use Scribe's existing `regex` dependency for matching to preserve linear-time behavior and avoid a new native regex engine.
- Store Smart Selection as global terminal configuration, not profile-specific configuration.
- Extend click classification to support quad click while preserving existing single/double/triple behavior.
- Generate context-menu actions from matching Smart Selection rules without running actions during selection.

## Phase 1 Design Summary

Design artifacts are recorded in [data-model.md](./data-model.md), [contracts/config-smart-selection.md](./contracts/config-smart-selection.md), [contracts/settings-ui-smart-selection.md](./contracts/settings-ui-smart-selection.md), and [quickstart.md](./quickstart.md).

Key design points:
- `SmartSelectionConfig` owns activation gesture, rule list, and defaults/reset behavior.
- `SmartSelectionRule` owns matching metadata and action list.
- `SmartSelectionCandidate` is transient and must preserve match range plus capture groups for selection and parameter expansion.
- Rule matching operates over visible terminal logical text with a row/column map back to grid coordinates.
- Context-menu action execution is explicit and uses existing open/copy/send-command pathways where those already exist.

## Constitution Check

*GATE: Re-check after Phase 1 design.*

Post-design gate status: PASS. The design remains scoped to existing crates,
keeps Smart Selection actions explicit, preserves current mouse and selection
behavior, keeps performance-sensitive matching out of repeated regex
compilation, avoids server restart, and keeps implementation documentation
updates for the implementation phase.

## Complexity Tracking

No constitution violations or complexity exceptions are required.
