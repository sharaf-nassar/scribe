# Implementation Plan: Add Note From Hover Preview

**Branch**: `007-add-note-from-hover` | **Date**: 2026-05-20 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/007-add-note-from-hover/spec.md`

## Summary

Add an inline note-creation affordance to the existing workspace-notes hover preview. The preview gains a `~2 col × 1 row` bordered "+" cell in its bottom-right corner that, when clicked, transitions the preview into an inline editor row pre-populated with the workspace's saved draft. The editor uses a hardcoded keymap — **Enter saves, Ctrl+Enter inserts a newline, Escape cancels** — which is also retrofitted onto the existing modal editor for mental-model consistency, replacing the modal's prior Ctrl+Enter-saves binding. A pre-existing modal spacebar bug (silently dropped on `NamedKey::Space`) is fixed in the same change.

Technical approach: extend the existing `workspace_notes_preview.rs` GPU-overlay with affordance hit-rects and a transient "adding note" row; add a per-workspace `AddingNoteState` map to `App` so editor state persists across hover gaps and across split panes; reuse the existing `WorkspaceNotesMutation` protocol unchanged (`SaveDraft` for debounced drafts, `CreateActiveNote` for commit); reuse the existing `ScrollbarState` overlay-scrollbar pattern from `scrollbar.rs` for the editor's internal overflow scroll. Swap the Enter/Ctrl+Enter arms in `handle_workspace_notes_keyboard` and add a `NamedKey::Space` arm to fix the spacebar bug.

## Technical Context

**Language/Version**: Rust 2021 (Scribe workspace)
**Primary Dependencies**: existing — `winit` (key events incl. `NamedKey`), `wgpu` via `scribe-renderer` (GPU cell rendering), `alacritty_terminal` (`TermMode` for Kitty keyboard protocol). **No new dependencies.**
**Storage**: server-owned `$XDG_STATE_HOME/<flavor>/workspace_notes.toml` (unchanged); client cache via `WorkspaceNotesStore`; no new client persistence.
**Testing**: Manual quickstart verification per QR-002 and constitution principle II. No new automated tests requested in the spec.
**Target Platform**: macOS and Linux desktop (existing Scribe targets).
**Project Type**: Rust workspace, desktop terminal application (multi-crate: `scribe-client`, `scribe-server`, `scribe-common`, `scribe-renderer`, etc.).
**Performance Goals**:
- PR-001: "+" click → caret-ready editor MUST complete in a single render tick at Scribe's target frame rate; editor row grows without measurable stutter at typical note lengths.
- PR-002: per-keystroke server bandwidth bounded by the existing `SaveDraft` debounce (`WORKSPACE_NOTES_DEBOUNCE`), not by raw keystroke frequency.
- PR-003: zero added per-frame work or long-lived allocations in the read-only hover preview path for users who never click the affordance.

**Constraints**:
- No new protocol message (`WorkspaceNotesMutation` variants are reused as-is).
- No new persistent file/format on disk (client or server).
- No live server restart — all changes are pure client-side.
- Modal keymap flip is a deliberate breaking change with **no legacy keymap toggle** (Assumption #8).
- `lat.md/client.md` MUST be updated to reflect the new behavior, and `lat check` MUST pass before completion.

**Scale/Scope**: per-workspace state for typically <20 workspaces visible at once; preview's outer height ≤ 3/4 of one focused pane's vertical extent; editor draft length unbounded but expected to be short (single user notes).

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

### Initial check (pre-research)

| Gate | Status | Notes |
|---|---|---|
| **Code Quality** | **PASS** | Extends existing `workspace_notes_preview.rs`, `workspace_notes_modal.rs`, `workspace_notes.rs`, and `main.rs` (`handle_workspace_notes_*`, `apply_workspace_notes_preview_overlay`). Reuses existing `WorkspaceNotesMutation` variants. New per-workspace `AddingNoteState` is a typed struct held on `App` parallel to `workspace_notes_save_pending`. No cross-crate refactors or new abstractions. No new dependencies. |
| **Testing Strategy** | **PASS** | QR-002 in spec calls for manual quickstart verification. Each user story (US1 inline capture, US2 dismiss-suppression, US3 cancel/abandon-empty) has a named independent test path in the spec. No new automated tests requested; existing test harnesses don't already cover this behavior, so manual verification is documented in `quickstart.md`. |
| **User Experience Consistency** | **PASS** | Modal keymap flip (FR-017) and inline editor share one keymap → preserves muscle memory across both surfaces. Spacebar fix (FR-018) restores expected text-input behavior. All other modal behavior unchanged. Read-only hover behavior unchanged for users who never engage the affordance (SC-003). |
| **Performance Budgets** | **PASS** | PR-001/002/003 give measurable budgets (single render tick activation, inherited `SaveDraft` debounce window, zero added per-frame cost in read-only path). Manual verification of the budgets is in `quickstart.md`. |
| **Operational Safety** | **PASS** | Pure client changes — no server restart needed; no protocol change; no config migration. `lat.md/client.md` Workspace Notes section update is the only `lat.md` write; `lat check` runs before completion (post-task checklist). Worktree convention preserved. |

**Migration / compatibility decision**: The modal keymap flip (Enter↔Ctrl+Enter) is a **breaking UX change** affecting users who have learned the prior modal binding. No config migration is needed because these bindings are hardcoded modal-internal keys, not entries in `KeybindingsConfig`. Users will encounter the new keymap on first modal open after the upgrade; the spec's Assumption #8 explicitly accepts this tradeoff for mental-model consistency with the new inline editor.

**Result: PASS, no Complexity Tracking entries needed.**

### Post-design check (post-Phase 1)

(Re-evaluated after the design artifacts are written below — see end of plan.)

## Project Structure

### Documentation (this feature)

```text
specs/007-add-note-from-hover/
├── plan.md              # This file (/speckit-plan command output)
├── spec.md              # Feature specification (already complete)
├── checklists/
│   └── requirements.md  # Spec quality checklist (already complete)
├── research.md          # Phase 0 output (this command)
├── data-model.md        # Phase 1 output (this command)
├── quickstart.md        # Phase 1 output (this command)
├── contracts/           # Phase 1 output (this command — sparse; no new protocol)
│   └── README.md
└── tasks.md             # Phase 2 output (/speckit-tasks — NOT created by /speckit-plan)
```

### Source code (touch points)

```text
crates/scribe-client/src/
├── workspace_notes_preview.rs       # EXTEND: affordance bordered cell, editor row, hit-rects,
│                                    #         per-row scroll, overlay scrollbar integration
├── workspace_notes_modal.rs         # MINOR: ensure modal still consumes the same keys
│                                    #        correctly after main.rs keymap flip (no API change)
├── workspace_notes.rs               # POSSIBLY: helper for editor-state-from-store snapshot
├── scrollbar.rs                     # REUSE: existing ScrollbarState pattern, no edits
└── main.rs                          # EXTEND App: AddingNoteState map per-workspace,
                                     #             handle_workspace_notes_keyboard keymap flip,
                                     #             NamedKey::Space arm (FR-018),
                                     #             preview hover/click routing for affordance,
                                     #             inline-editor keyboard capture before PTY,
                                     #             higher-priority-overlay handoff,
                                     #             draft-pristine policy for inline editor

lat.md/
└── client.md                        # UPDATE: Workspace Notes section reflects new behavior

specs/007-add-note-from-hover/       # plan + research + data-model + quickstart artifacts
```

**Structure Decision**: This is a single-crate touch within the existing `scribe-client` plus a docs update. No new crates, no new modules, no protocol/server work. The Rust workspace layout from `Cargo.toml` is preserved unchanged.

## Phase 0: Research

See [research.md](./research.md) for the consolidated decisions. Summary:

- **No NEEDS CLARIFICATION items remain** — all five `/speckit-clarify` questions resolved on 2026-05-19 (see spec's `## Clarifications` section). Phase 0 work in this plan is therefore a consolidation of the existing-pattern reuse decisions, not a divergent research effort.
- Existing patterns captured: GPU cell rendering via `CellInstance`, `ChromeColors` theming, `ScrollbarState` overlay-scrollbar, `workspace_badge_hit_rect` for click routing, the four-level Key Translation Priority chain (modal keys consumed at level 2, before PTY translation), the existing `SaveDraft` debounce window (`WORKSPACE_NOTES_DEBOUNCE`).
- No external library research required (no new deps).

## Phase 1: Design & Contracts

See:

- [data-model.md](./data-model.md) — new transient client state (`AddingNoteState` per workspace) + clarifies how it composes with the existing `WorkspaceNotesStore` and `workspace_notes_save_pending` field.
- [contracts/](./contracts/) — explicit "no new protocol" decision (with rationale + a thin README listing the existing `WorkspaceNotesMutation` variants the feature exercises).
- [quickstart.md](./quickstart.md) — manual verification recipe for US1, US2, US3 plus a probe for the spacebar fix and the modal keymap flip.

### Agent context update

The CLAUDE.md SPECKIT marker is updated to point at this plan (see Phase 1 step 3).

## Constitution Check (post-design)

After writing the design artifacts:

| Gate | Status | Notes |
|---|---|---|
| **Code Quality** | **PASS** | Data model confirms no new persistent entities; contracts confirm no new protocol; touch list confirms no cross-crate ripple. |
| **Testing Strategy** | **PASS** | quickstart.md provides the named manual verifications referenced by QR-002 and the constitution. |
| **User Experience Consistency** | **PASS** | Design preserves modal layout, hover-preview rendering math, click-to-archive behavior, and `SaveDraft` semantics; only adds an opt-in inline editor and harmonizes keys. |
| **Performance Budgets** | **PASS** | PR-001/002/003 budgets unchanged after design; the per-workspace `AddingNoteState` map is `BTreeMap<WorkspaceId, …>` and only allocated lazily on first "+" click per workspace (preserves PR-003). |
| **Operational Safety** | **PASS** | Design confirms client-only changes; lat.md update plan unchanged; the `lat check` step is part of the post-task checklist. |

**Result: PASS, ready for `/speckit-tasks`.**

## Complexity Tracking

No constitution violations to justify. This section intentionally empty.
