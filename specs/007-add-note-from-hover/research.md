# Phase 0 Research: Add Note From Hover Preview

**Date**: 2026-05-20
**Spec**: [spec.md](./spec.md) | **Plan**: [plan.md](./plan.md)

## Status

All `NEEDS CLARIFICATION` items from the spec were resolved during `/speckit-clarify` on 2026-05-19 (see spec `## Clarifications`). Phase 0 here is therefore a **consolidation** of the existing-codebase patterns this feature will inherit rather than divergent research.

## Decisions

### D-001: Reuse `CellInstance` GPU rendering for the affordance and editor row

- **Decision**: The "+" bordered cell and the inline editor row are rendered as `CellInstance` quads in `workspace_notes_preview.rs`, alongside the existing preview cells. No new render path, shader, or canvas surface is introduced.
- **Rationale**: The hover preview is already a `CellInstance` overlay built by `build_workspace_notes_preview`. Reusing the same path means: (a) same DPI / cell-size math; (b) same theme integration via `ChromeColors`; (c) no new GPU resource lifetime concerns.
- **Alternatives considered**: Rendering the affordance as a separate `tooltip.rs`-style overlay → rejected because it splits the hit-rect routing across two surfaces and complicates the bottom-right anchor math.

### D-002: Reuse the `ScrollbarState` overlay-scrollbar pattern

- **Decision**: When the editor exceeds the 3/4-pane growth cap (FR-019), an overlay scrollbar inside the editor row follows the existing `crates/scribe-client/src/scrollbar.rs#ScrollbarState` pattern: 1.5 s idle fade, 0.3 s fade-out, hover-expand width via lerp, 3× visible-width hit zone, drag-to-scroll computing offset from mouse delta. Per FR-022.
- **Rationale**: Scribe already has exactly one overlay-scrollbar convention; users have learned it. Reusing the pattern avoids inventing a second scroll affordance.
- **Alternatives considered**: A static slim scrollbar always visible → rejected for the same screen-real-estate reasons that motivated the read-only preview's compact size budget.

### D-003: Reuse `WorkspaceNotesMutation::{SaveDraft, CreateActiveNote}` unchanged

- **Decision**: The inline editor sends the same `SaveDraft` (debounced) and `CreateActiveNote` (on commit) mutations the modal already sends. No new protocol variant, no new server-side branch.
- **Rationale**: Spec assumptions explicitly require no new protocol message (Assumption #2). The shared draft buffer (FR-020) is a *client-side framing* — both editors hold a pointer into the same buffer rather than the protocol caring about two editors.
- **Alternatives considered**: A new `CreateActiveNoteInline` variant → rejected; adds protocol surface for zero behavioral benefit.

### D-004: Per-workspace `AddingNoteState` map on `App`

- **Decision**: Per-workspace inline-editor state lives in a `BTreeMap<WorkspaceId, AddingNoteState>` on `App`, parallel to the existing `workspace_notes_save_pending: Option<Instant>` field. The map's entry is created lazily on first "+" click for a given workspace; absent entries mean the workspace is in read-only preview mode.
- **Rationale**: Matches FR-021 (per-workspace state isolation). Lazy creation preserves PR-003 (zero added per-frame cost for non-engaging users). `BTreeMap` (not `HashMap`) matches the codebase's preference for deterministic iteration order seen in `WorkspaceNotesStore` (`collections: BTreeMap<String, WorkspaceNotesCollection>`).
- **Alternatives considered**: Singleton `Option<AddingNoteState>` on `App` → rejected, doesn't support per-workspace isolation. Holding the state inside `WorkspaceNotesStore` → rejected, that struct is a *snapshot cache* of server data; transient client UI state has no place there.

### D-005: Keyboard capture before PTY translation, gated by per-workspace state

- **Decision**: When ANY workspace has an active `AddingNoteState`, the focused-workspace key event flow checks whether the current hover-preview workspace has an editor open and, if so, routes the key to the inline editor instead of the PTY (matching the modal's level-2 "Special commands" placement in the four-level Key Translation Priority chain).
- **Rationale**: The existing modal already runs key handling before PTY translation; FR-005 requires the inline editor to do the same. Routing through the same priority layer keeps the chain a single source of truth.
- **Alternatives considered**: A separate top-level key router for hover-preview editors → rejected as a second mental model for keymap precedence.
- **Important nuance**: The "focused" workspace and the workspace whose preview currently holds the editor may differ. The router uses the **hover-target workspace** (whose preview is rendered) as the editor owner; if no preview is rendered at the moment of key event (e.g., user mouse-moved off the badge and the preview is hidden but state is preserved per FR-021), keys fall through to PTY normally until pointer returns.

### D-006: Modal keymap flip via swapping two match arms

- **Decision**: `handle_workspace_notes_keyboard` at `crates/scribe-client/src/main.rs:9701` swaps the bodies of the two Enter arms — `Key::Named(NamedKey::Enter) if self.modifiers.control_key()` becomes the **newline-insertion** branch (currently the save branch), and `Key::Named(NamedKey::Enter)` (no modifier) becomes the **save** branch (currently the newline-insertion branch). This is a minimal, atomic change.
- **Rationale**: Direct expression of FR-017. No new state, no new helper.
- **Alternatives considered**: Adding a config flag to toggle between old and new keymaps → explicitly rejected by Assumption #8 ("No 'legacy keymap' toggle is offered").

### D-007: Spacebar fix is a new `NamedKey::Space` arm

- **Decision**: Add `Key::Named(NamedKey::Space)` as an explicit match arm in `handle_workspace_notes_keyboard` that pushes a `' '` character to the modal draft and calls `sync_workspace_notes_draft()` / `request_redraw()`. The existing `Key::Character(text)` arm (which filters control characters but accepts `' '`) is presumably never hit for the bare spacebar because winit reports it as `NamedKey::Space` on this platform.
- **Rationale**: Direct expression of FR-018. Targeted fix without touching the existing `Key::Character` arm. Implementation verifies that the new arm is reached on a quickstart probe (Q: press spacebar → see space in draft).
- **Alternatives considered**: Removing the `is_control()` filter in the `Key::Character` arm → rejected as a wider change with unclear motivation; the filter is presumably there to drop CR/LF/Tab from text events.

### D-008: Affordance hit-rect routing via the existing preview interaction structure

- **Decision**: Extend `WorkspaceNotesPreviewInteraction` to carry an optional "affordance" rect alongside the existing `note_targets: Vec<WorkspaceNotesPreviewNoteTarget>`. The hit-test path in `apply_workspace_notes_preview_overlay` checks the affordance rect first; if hit, it transitions the workspace's state to "adding note" instead of archiving a row.
- **Rationale**: Keeps hit-rect routing in the same place the existing click-to-archive routing lives — no parallel routing surface.
- **Alternatives considered**: A separate top-level click handler → rejected; would split workspace-preview routing across two places.

### D-009: Higher-priority-overlay handoff reuses `flush_workspace_notes_now`

- **Decision**: When a higher-priority overlay opens (notes modal, context menu, command palette, search overlay, close dialog, update dialog), the existing `flush_workspace_notes_now` (or its `flush_workspace_notes_if_due` counterpart) is called to flush all in-flight `AddingNoteState` drafts via `SaveDraft` before transferring focus. Per FR-010.
- **Rationale**: Reuses the existing flush path; doesn't introduce a new "preview drains" code path. The flush already handles the modal's draft; extending to iterate over all `AddingNoteState` entries is a small addition.
- **Alternatives considered**: A per-overlay flush callback → rejected; same flush semantics for every higher-priority overlay can share one implementation.

### D-010: Manual quickstart verification (no new automated tests)

- **Decision**: Verification is via `quickstart.md` covering US1 (inline capture), US2 (dismiss-suppression), US3 (cancel/abandon-empty), plus probes for the modal keymap flip (D-006) and spacebar fix (D-007).
- **Rationale**: Constitution principle II + spec QR-002. The existing test harness in the workspace doesn't cover GPU-overlay interaction; building new harness for one feature contradicts the constitution's "narrower reason to diverge" engineering constraint.
- **Alternatives considered**: Adding an in-process integration test for `WorkspaceNotesMutation` flows → would be redundant with the existing `004-workspace-notes` integration tests, which already verify the protocol round-trip; this feature changes UI, not protocol.

## Existing-pattern catalog (informational)

The following existing patterns are inherited unchanged and listed here only so the implementation plan and the eventual `/speckit-tasks` output have a single reference point:

| Pattern | Source | Used for |
|---|---|---|
| `CellInstance` GPU quads | `crates/scribe-renderer/src/types.rs` | Preview + editor + affordance rendering |
| `ChromeColors` palette | `crates/scribe-common/src/theme.rs` | Affordance idle/hover/pressed/disabled colors |
| `workspace_badge_hit_rect` | `crates/scribe-client/src/tab_bar.rs:351` | Anchor for the preview |
| `PreviewLayout::new` | `crates/scribe-client/src/workspace_notes_preview.rs:79` | Viewport clamping + above/below flip |
| `WorkspaceNotesMutation::SaveDraft` | `crates/scribe-common/src/protocol.rs:122` | Debounced inline draft writes |
| `WorkspaceNotesMutation::CreateActiveNote` | `crates/scribe-common/src/protocol.rs:122` | Commit on Enter |
| `WORKSPACE_NOTES_DEBOUNCE` constant | `crates/scribe-client/src/main.rs` | Per-keystroke bandwidth bound |
| `ScrollbarState` overlay-scrollbar | `crates/scribe-client/src/scrollbar.rs:70` | Editor overflow scroll affordance |
| Key Translation Priority chain (level 2) | `crates/scribe-client/src/main.rs` keyboard dispatch | Modal & inline-editor key capture before PTY |
| `flush_workspace_notes_now` / `_if_due` | `crates/scribe-client/src/main.rs:9095` / `9104` | Higher-priority-overlay handoff flushing |
| Modal pristine-draft policy | `crates/scribe-client/src/workspace_notes_modal.rs` (draft_dirty flag) | Late `WorkspaceNotesChanged` snapshot resolution |

## Open risks

- **Spacebar arm placement**: The fix in D-007 assumes winit delivers the spacebar as `NamedKey::Space` rather than `Key::Character(" ")`. If it's actually the latter on this platform/version, the existing `Key::Character` arm's `is_control()` filter should already pass through `' '`. Manual verification (quickstart probe Q4) will confirm the actual event shape and the implementation may need to handle both arms.
- **`AddingNoteState` memory growth**: An indefinite per-workspace map could grow if a user opens editors in many workspaces without committing/cancelling. Mitigation: state is cleared on commit, cancel, and higher-priority-overlay handoff (FR-006/008/010). In the cold-restart path the map starts empty.
- **Preview height vs pane resize**: FR-019's 3/4-pane cap requires `apply_workspace_notes_preview_overlay` to recompute the cap on each render tick when in editing state. The compute is O(1) per render so this is a budget question, not an algorithmic one — observable via existing render-tick instrumentation.
