# Plan: debt-paydown

## Architecture Approach

Two independent paydowns with deliberately asymmetric effort, matching the
clarify-gate answers.

**Story A is a comment-only correction.** The clarify gate ruled `upgrade.log`
rotation an approved non-goal, so the entire remaining change is rewriting a
stale `ponytail:` block in `dist/debian/postinst` to record the true state:
`server.log` is already capped at `SERVER_LOG_MAX_BYTES` (8 MiB) and rotated
once to `server.log.1`, and the `--upgrade` server mirrors its tracing there,
so `upgrade.log` duplicates an already-bounded stream. No shell logic changes.

*Alternatives rejected (at the clarify gate, recorded here for traceability):*
size-cap at spawn — bounds nothing between upgrades and adds logic to a
root-run install script; server manages its own stdio after readiness —
couples the server startup path to the postinst readiness contract for no
user-visible gain; redirect into `server.log` — risks predecessor and
successor appending to one file during handoff overlap.

**Story B is pure consumption of an existing pure module.** `crate::scrollbar`
already exports every primitive (`hit_test_scrollbar`, `hit_test_thumb`,
`offset_from_track_click`, `offset_from_drag`, `ScrollbarDrag`,
`ScrollbarState::{on_hover_enter, on_hover_leave, current_width}`), and
`crates/scribe-client/src/main.rs` already demonstrates the exact wiring for
the terminal pane. The settings window needs no new geometry, no new state,
and no new module.

Two facts make this smaller than it looks:

1. **State already exists.** `ScrollbarState` carries `pub drag:
   Option<ScrollbarDrag>` (`scrollbar.rs:225`), and the settings window
   already owns a `content_scrollbar: ScrollbarState` (`window.rs:306`). No
   new fields.
2. **Units already reconcile.** `tick_content_scrollbar` documents "a pixel
   scroller is that same shape with one pixel as the unit" and builds
   `ScrollMetrics` with `display_offset = overflow - scrolled`, the same
   count-from-bottom convention the terminal uses. So `offset_from_drag` and
   `offset_from_track_click` apply unchanged; only the inverse mapping back
   onto `scroll_handle` is new.

The one structural change: `tick_content_scrollbar` currently builds a
`ScrollbarLayout` locally and discards it. Extract that construction into a
small `content_scrollbar_layout(&self) -> Option<ScrollbarLayout>` so the
render tick and the three mouse handlers share one definition of the geometry
rather than each re-deriving it (constitution #1).

*Alternative rejected:* caching the layout in a field. It is cheap to compute,
derived entirely from `scroll_handle` state, and a cached copy would go stale
against the live scroller — a correctness hazard for zero measurable gain.

### Constitution check

| # | principle | status |
|---|---|---|
| 1 | Clear Boundaries and Typed Failure | **Honored.** Zero new dependencies, zero new abstractions; one extracted helper so geometry has a single definition. Existing `Option` returns carry the "no thumb" case. |
| 2 | Session-Safe, Consistent UX | **Honored — this is the point of Story B.** Settings scrollbar becomes behaviorally identical to the terminal's. |
| 3 | Explicit, Risk-Based Verification | **Honored with noted tension** — see Testing Strategy. New test code is justified because existing coverage must change: the behavior is new and unit tests cannot prove a real pointer drag reaches the handler. |
| 4 | Performance Budgets and Measurement | **Explicitly inapplicable, both stories.** Story A is comments. Story B adds pointer handling to an existing render path with no hot-path or per-frame cost change. Stated rather than omitted, as the principle requires. |
| 5 | Default-Safe Trust Boundaries | **Not applicable.** Neither story touches PTY input, clipboard, hyperlinks, paste, or any capability that exfiltrates data or invokes host actions. |
| 6 | Local-First Data Locality | **Not applicable.** No network, no telemetry, no audio, no terminal-content transmission. |
| 7 | Compatible, Documented, Operationally Safe Change | **Honored.** `lat.md` sync is an explicit acceptance criterion; Story A's comment-only scope keeps the live upgrade path untouched; all validation is Docker-harness-only; the host server is never restarted. |

## Affected Components

| file | change | story |
|---|---|---|
| `dist/debian/postinst` | Rewrite the `ponytail:` block above `UPGRADE_LOG=` (~lines 682-685). Comments only. | A |
| `crates/scribe-client/src/settings/window.rs` | Extract `content_scrollbar_layout`; add hover / press / drag-move / release handlers on the `settings-scroll-track` wrapper; delete the `ponytail:` comment at ~line 525. | B |
| `crates/scribe-client/src/scrollbar.rs` | **No change.** Consumed as-is. Listed to make the no-change explicit. | B |
| `tests/e2e/visual/settings-scrollbar.sh` | New scripted visual E2E. | B |
| `justfile` | New `e2e-visual-settings-scrollbar` recipe plus registration in the script-to-recipe map (~line 571 block). | B |
| `lat.md/client.md` | Update `Client#Scrollbar` and `Client#Input#Mouse Handling` to state the settings pane now has an interactive scrollbar. | B |
| `lat.md/test.md` | New test-spec section for the visual script (`require-code-mention` frontmatter needs a `# @lat:` ref from the script). | B |

Attach point for Story B is the existing
`div().id("settings-scroll-track").relative()` wrapper (`window.rs:~2348`) —
the same element the thumb is already positioned against, so hit-test
coordinates and paint coordinates share an origin by construction. The
settings window already uses `on_mouse_down` / `on_mouse_move` /
`on_mouse_up` with `cx.listener` elsewhere in the file (~lines 2064-2128),
so there is in-file precedent for the handler shape.

## Data Model

**No changes.** No new entities, no schema, no storage, no migrations.

- Story A touches no data at all.
- Story B reuses `ScrollbarState` (including its existing `drag` field) and
  `ScrollMetrics`, both already owned by the settings window. Scroll position
  continues to live in GPUI's `scroll_handle`; nothing is persisted, and no
  config key is added.

## API / Interface Changes

**No breaking changes. No public API changes.**

- `crate::scrollbar`'s public surface is unchanged — this plan only adds a
  consumer.
- One new private method on `SettingsWindow`:
  `fn content_scrollbar_layout(&self) -> Option<ScrollbarLayout>`.
- Private handler methods on `SettingsWindow` mirroring the terminal's names
  so the parallel is obvious to a reader: hover tracking, press claiming
  (returning whether the press was consumed), drag-move, and release.
- **User-facing surface change (intended):** the settings content-pane
  scrollbar becomes interactive — hover widens the thumb and pins the overlay,
  the thumb drags, and the track click-jumps. No keyboard, config, or CLI
  surface changes; wheel scrolling behaves exactly as before.
- No IPC, protocol, or persistence-format change, so no client/server version
  pairing concern.

## Testing Strategy

**Story A — no automated test.** The change is comments in a shell script.
Verification is reading the diff and confirming no non-comment line moved
(`git diff --stat` shows only `dist/debian/postinst`, and the diff contains no
executable-line change). Adding harness coverage for a comment would violate
constitution #3's instruction not to add test code that is not required.

**Story B — one scripted visual E2E**, following the established settings
pattern (`settings-trust.sh`, `settings-entry.sh`, `settings-keybindings.sh`),
each with its own `just e2e-visual-settings-*` recipe.

Constitution #3 tension, stated explicitly: the principle says add test code
only when explicitly requested or when existing coverage must change. Here
existing coverage *must* change — the behavior is new, and the pure-module
unit tests in `scrollbar.rs` already prove the geometry (`hit_test_scrollbar`,
`hit_test_thumb`, `offset_from_drag` all have tests) but cannot prove that a
real pointer press at a real X server reaches the settings overlay's handler
rather than the controls underneath. That gap is exactly what the visual script
covers, and it is the user-reachable path the principle demands.

Phases the script must assert:

1. Open the settings window on a page long enough to overflow; confirm a thumb
   paints.
2. Move the pointer into the hit zone; assert the thumb widens and the overlay
   stops fading.
3. Press the thumb and drag; assert the content offset tracks the pointer.
4. Assert the overlay does **not** fade out mid-drag.
5. Release, then click the track away from the thumb; assert the viewport jumps
   to that position.
6. Switch to a page that does not overflow; press where the hit zone would be
   and assert the press reaches the control underneath — the regression guard
   for the invisible-overlay-swallows-clicks failure.

Reuse `tests/e2e/visual/scrollbar.sh` (the terminal oracle) as the structural
model; do not duplicate its terminal assertions.

Regression coverage: the existing `scrollbar.rs` unit suite is unchanged and
keeps guarding the shared geometry both panes now depend on, which is the
regression net for the reuse.

All runs are Docker-harness-only via `just e2e-visual-settings-scrollbar`.
Never against the host server, never a host `scribe-*` invocation.

## Risks

| risk | severity | mitigation |
|---|---|---|
| **Merge conflict with in-flight `settings/window.rs` work** (~521 uncommitted lines, same file, overlapping regions). | High | The clarify gate chose to block: a precondition work item gates all Story B items on that work reaching `main`, wired as a real bd dependency so `/implement-ready` cannot start early. |
| **Invisible overlay swallows clicks** when the page does not overflow, breaking settings controls. | High | Handlers mirror `press_scrollbar`'s contract and return not-consumed when `content_scrollbar_layout` yields `None` or opacity is zero. Explicit acceptance criterion and a dedicated E2E phase (6). |
| Drag inversion applied with the wrong sign, so dragging scrolls backwards. | Medium | `display_offset` counts from the bottom in both panes; invert once via `scrolled = overflow - display_offset`. E2E phase 3 asserts direction, not just movement. |
| Thumb fades out from under the pointer mid-drag. | Medium | Mirror `press_scrollbar`: set `opacity = 1.0` and clear `fade_start` on drag start. E2E phase 4. |
| Story A's comment rewrite accidentally alters shell logic in a root-run install script. | Medium (high blast radius) | Comment-only acceptance criterion; reviewer confirms the diff contains no executable-line change. This risk is the reason the mechanism options were rejected at the gate. |
| Rewritten Story A comment re-enters the `ponytail-debt` ledger and re-files itself as a future P4. | Low | Acceptance criterion requires dropping the `ponytail:` prefix (or stating an honest revised trigger), since the ceiling is now accepted rather than deferred. |
| `lat.md` drifts from the new behavior. | Low | `lat check` is an acceptance criterion on the Story B items; `test.md`'s `require-code-mention` will fail the check if the new script has no `# @lat:` ref. |
| Reduced-motion path regresses. | Low | `tick_content_scrollbar` already pins opacity under `cx.reduce_motion()`; handlers must not clear that. Covered by not touching the branch. |

Rollback: both stories are independent and self-contained. Story A reverts as a
comment revert with zero behavioral surface. Story B reverts by removing the
handlers; the extracted layout helper is inert on its own.

No spikes needed — every primitive Story B depends on is already in-tree with
a working caller.

## Sequencing

Story A is fully independent and can be picked up immediately by any session.
Story B is a chain gated on the in-flight settings work landing.

- **Correct the stale upgrade-log comment** *(Story A)* — no dependencies.
  Ready immediately.
- **Confirm the in-flight settings-window work has landed** *(Story B
  precondition)* — no dependencies. Ready immediately, but it is a
  **verification gate, not implementation**: whoever claims it does *not*
  write or finish the inline-text-editing work (that is an explicit Non-Goal
  of this run). They only confirm it has landed, then close the item.
  *Acceptance:* `git status --porcelain crates/scribe-client/src/settings/window.rs`
  reports no uncommitted changes; the inline-editing changes (the
  `inline_commit_value` helper and its callers) are present on `main`; and
  `just check` passes on `main`. If the work has been abandoned rather than
  landed, close this item with that as the stated reason — Story B then
  proceeds from current `main` with no conflict to avoid.
- **Wire the settings scrollbar handlers** *(Story B implementation)* —
  blocked by the precondition item. Includes deleting the `ponytail:` comment
  and the `lat.md/client.md` sync.
- **Add the settings-scrollbar visual E2E** *(Story B verification)* — blocked
  by the implementation item, since the script asserts against the behavior it
  adds. Includes the `justfile` recipe, its registration, and the
  `lat.md/test.md` spec section plus `# @lat:` ref.

Ordering is expressed purely through these dependency edges. The two Story B
work items are deliberately separate rather than one bead: the implementation
is Rust in a conflict-prone file, the verification is a shell script plus
`justfile` plumbing, and splitting them lets the E2E be written against landed
behavior.

## Backlog Refinement

| source | disposition | work item(s) | target priority | acceptance criteria |
|---|---|---|---|---|
| `scribe-6vw` | `approved-non-goal` **+** `split-and-supersede` | Correct the stale upgrade-log comment | P3 | The `ponytail:` block no longer claims `server.log` accepts an unbounded ceiling; replacement text records the 8 MiB cap, the single rotation to `server.log.1`, and that the `--upgrade` server mirrors tracing there so `upgrade.log` duplicates a bounded stream; no `ponytail:` marker survives that would re-enter the ledger for now-rejected work; diff is comment-only — no change to `UPGRADE_LOG`, the redirect, the readiness grep, `cleanup_upgrade_state`, or the `mktemp` fallback. |
| `scribe-233` | `refine-in-place` | Wire the settings scrollbar handlers; Add the settings-scrollbar visual E2E | P3 | Hover widens the thumb and pins the overlay; leaving resumes the idle fade. Thumb press starts a drag that scrolls proportionally and in the correct direction; release ends it. Track press jumps the viewport. Overlay does not fade mid-drag. No press is consumed when the page does not overflow. Geometry comes from `crate::scrollbar` with no duplicated math, and the resulting behavior is indistinguishable from the terminal pane's scrollbar (constitution #2). Offsets stay clamped by the existing `UNIT_CAP`. `ponytail:` comment removed. `lat.md/client.md` and `lat.md/test.md` updated, the new script carries its `# @lat:` ref, and `lat check` passes. The `justfile` gains an `e2e-visual-settings-scrollbar` recipe **and** its entry in the script-to-recipe map. Verified via `just e2e-visual-settings-scrollbar` in the Docker harness. |

**Rotation rationale for `scribe-6vw`'s dual disposition:** the *rotation work*
is an approved non-goal (human-approved at the clarify gate — Clarifications
Q1), but the bead is not bare-retired: it is superseded by the comment
correction, which is real replacement coverage. This keeps the ledger honest
rather than closing a P4 with nothing behind it.

Neither source is silently dropped or duplicated. No source is closed or
superseded before its replacement task exists. After materialization the
closure contains no open P4 and the ready subset contains zero P4 items.

The `ponytail-debt` label is carried onto the replacement tasks for
traceability back to the ledger (Open Question 8 — a bookkeeping convention
with no functional impact; consistency is the only requirement).

## Target Epic

**A new epic will be created.** No existing epic applies and none can be
inferred: both sources are unparented with no `discovered-from` provenance,
which is intentional for `ponytail-debt` beads so they stay out of feature-run
closures. This was confirmed unambiguous at the clarify gate — there were no
epic candidates to choose between.

The four work items above are filed as children of that new epic.

## Alignment fixes applied

Quick pass — one self-check round covering spec↔plan coverage and plan quality
(no subagent dispatch, per depth=quick).

**Coverage walk result:** all 5 Goals, all 9 Non-Goals, both User Stories with
every acceptance criterion, both Backlog Inputs, all 4 Clarification answers,
and all 8 spec Open Questions trace to something in the plan. No plan work item
violates a Non-Goal.

Fixes applied:

- **must-fix (quality/testability)** — The Story B precondition work item
  appeared in Sequencing but had acceptance criteria nowhere: it maps to no
  backlog source, so the Backlog Refinement table (correctly) did not cover it,
  leaving it unable to become a task with verifiable criteria. Added explicit
  acceptance criteria inline in Sequencing, including the abandoned-work escape
  hatch so the item cannot deadlock Story B if the in-flight changes are
  dropped rather than committed.
- **must-fix (scope creep guard)** — The same precondition item could be read
  as "finish the inline-text-editing work", which is an explicit Non-Goal.
  Reworded to state it is verification-only and that the claimer must not write
  that work.
- **should-fix (coverage/PARTIAL)** — `scribe-233`'s acceptance criteria listed
  the individual behaviors but omitted the spec's constitution #2 requirement
  that the result be *indistinguishable from the terminal pane's scrollbar*.
  Added, so a reviewer checks parity rather than only per-behavior presence.
- **should-fix (completeness)** — The `justfile` script-to-recipe map
  registration was named in Affected Components but not required by any
  acceptance criterion, so a recipe could land unregistered. Added to
  `scribe-233`'s criteria, along with explicit naming of both `lat.md` files
  and the new script's `# @lat:` ref.

Nothing else required a fix. Sequencing has no circular or hidden dependencies
and no false serialization: Story A is fully independent, and the single Story B
chain (precondition → implementation → verification) reflects real
prerequisites — the E2E script asserts against behavior the implementation item
adds. Target Epic is resolved (new epic; no candidates existed). No P4 work item
and no placeholder criteria were introduced.
