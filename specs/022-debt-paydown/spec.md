# Spec: debt-paydown

## Problem Statement

Two deliberate `ponytail:` shortcuts were left in the tree with named upgrade
triggers, and both are now on the ledger as open P4 chores. This run refines
them into actionable work — it is a debt paydown, not a feature.

**(1) `scribe-6vw` — unbounded `upgrade.log`.**
`dist/debian/postinst:685` (`spawn_upgrade_server`) points the successor
server's stdout/stderr at a fixed `${STATE_DIR}/upgrade.log`, truncated at
spawn (`: > "${UPGRADE_LOG}"`). The successor inherits that fd for its entire
life, so the file grows for as long as that server runs and is only reclaimed
when the *next* upgrade truncates it. `cleanup_upgrade_state` deliberately
does not remove it, precisely because the server is still writing.

**(2) `scribe-233` — display-only settings scrollbar.**
`crates/scribe-client/src/settings/window.rs:525`
(`tick_content_scrollbar`) computes a thumb purely for painting. The settings
content pane has no hover widen, no click-to-jump, and no drag, so the thumb
is a page-length *hint* rather than a control — even though the pure
`crate::scrollbar` module already exports every primitive needed and the
terminal pane already wires all three behaviors.

**Why now:** both carry explicit upgrade triggers ("if unbounded growth ever
matters", "if grabbing the settings thumb is ever wanted"). This run decides
whether those triggers have fired, and refines or retires each accordingly.

### Correction to the ledger's premise (material)

`scribe-6vw`'s description asserts that "the durable state-dir `server.log`
already accepts" the same unbounded-growth ceiling. **That is no longer true,
and was already false when the bead was filed.**

`crates/scribe-server/src/main.rs` caps the state-dir log at
`SERVER_LOG_MAX_BYTES = 8 MiB` and rotates it once to `server.log.1` at
startup via `rotate_if_oversized`, bounding it at ~2x the cap.

| event | commit | when |
|---|---|---|
| `server.log` rotation landed | `e50c2c3` | 2026-08-07 01:07 PDT |
| `scribe-6vw` source-commit | `a2a7441` | 2026-08-07 14:09 PDT |
| `scribe-6vw` filed | — | 2026-08-07 14:45 PDT |

So the "add rotation to both together" instruction is half-satisfied already.
Only `upgrade.log` is unbounded, and it carries the *same tracing stream* that
the already-rotated `server.log` mirrors (see the `--upgrade` comment block at
`crates/scribe-server/src/main.rs:96-101`). This materially shrinks — and may
eliminate — the remaining work. It must be settled at the clarify gate.

## Goals

1. Retire the `upgrade.log` rotation ceiling as an explicitly approved
   non-goal, and replace it with the one piece of real work it leaves behind:
   correcting the stale `ponytail:` comment so it records that `server.log` is
   already capped and rotated, and that `upgrade.log` duplicates that stream.
   *(Settled at the clarify gate — see Clarifications Q1.)*
2. Make the settings content-pane scrollbar thumb a real control — hover
   widen, click-to-jump, and drag, all three — behaving identically to the
   terminal pane's overlay scrollbar (constitution #2).
3. Reuse `crate::scrollbar`'s existing primitives rather than adding new
   geometry or hit-testing helpers (constitution #1).
4. Leave the ledger clean: every source P4 refined in place to P0–P3,
   superseded after replacement coverage exists, or retired with explicit
   human approval. No open P4 remains in the closure.
5. Remove both `ponytail:` comments, or rewrite them to state the newly
   accepted ceiling and its (revised) upgrade trigger.

### Measurable where possible

- Settings thumb hit zone and widen match the terminal's `3x`-width band —
  same constants, not re-derived.
- No new code in `dist/debian/postinst`; Story A's diff is comment-only.

## Non-Goals

- **Rotation or any size bound for `upgrade.log`** — explicitly approved as a
  non-goal at the clarify gate. `server.log` is already capped at 8 MiB with
  single rotation and receives the same tracing stream, so a second bound buys
  nothing and would put a behavior change into a root-run install script on
  the live upgrade path.
- Inline text editing in settings. It is the *other* half of `scribe-19v` and
  is actively in flight in the working tree (see Constraints); this run does
  not touch it.
- A general logging framework, log levels, or a `logrotate` dependency.
- Rotation for any log at all. `server.log` is already done; nothing else is
  in scope.
- Changing the postinst readiness protocol (the `IPC server listening` grep)
  or the detached-spawn mechanism.
- Measuring real-world `upgrade.log` growth. The redundancy argument settles
  the question without measurement.
- Scrollbar behavior anywhere other than the settings content pane.
- Reworking the `scrollbar` pure module's API. It already exposes what is
  needed; consuming it is the job.

## Backlog Inputs

Both sources are unparented by design so they stay out of feature-run
closures. Neither has dependencies, acceptance criteria, design, or notes —
the description is the whole record.

### `scribe-6vw` — P4 chore, open

- **Current intent:** add rotation to the postinst `upgrade.log` and the
  state-dir `server.log` together, triggered "if unbounded growth of either
  log ever matters".
- **Backtrack:** source-task `scribe-oij` (closed P3 bug: postinst redirected
  the upgraded server's stdout to a *deleted* temp file, leaving an unlinked
  tmpfs inode); source-commit `a2a7441`.
- **Missing decisions:** (a) the premise is stale — `server.log` rotation
  already exists, so half the stated scope is done; (b) has the trigger
  actually fired? No measurement of real `upgrade.log` growth exists; (c) is
  `upgrade.log` even needed past the readiness grep, given the successor
  mirrors the same stream into the rotated `server.log`?
- **Expected refinement:** P3 if a bound is wanted, retired-as-non-goal (with
  the `ponytail:` comment rewritten to record the corrected, already-mitigated
  state) if the duplication argument holds. Human decides at the gate.

### `scribe-233` — P4 chore, open

- **Current intent:** wire drag and click-to-jump into the settings scrollbar
  thumb, triggered "if grabbing the settings thumb is ever wanted".
- **Backtrack:** source-task `scribe-19v` (closed P3 task: settings window
  polish follow-ups — scroll affordance and inline text editing);
  source-commit `b2f1b20`.
- **Missing decisions:** (a) does hover widen belong too, or only the two
  behaviors in the title? The comment names all three; (b) verification path
  under constitution #3 — the existing `tests/e2e/visual/scrollbar.sh` is the
  terminal oracle, and there is no settings-window equivalent; (c) sequencing
  against the in-flight `window.rs` work.
- **Expected refinement:** P3 task. This is straightforward reuse with strong
  prior art, so the trigger firing is the only real question.

### Prior art available for `scribe-233` (all already in-tree)

| primitive | location |
|---|---|
| `hit_test_scrollbar` | `crates/scribe-client/src/scrollbar.rs:563` |
| `hit_test_thumb`, `offset_from_track_click` | `crates/scribe-client/src/scrollbar.rs` |
| `ScrollbarDrag` | `crates/scribe-client/src/scrollbar.rs:242` |
| hover wiring (`update_scrollbar_hover`) | `crates/scribe-client/src/main.rs:~5947` |
| press wiring (`press_scrollbar`) | `crates/scribe-client/src/main.rs:~5981` |
| settings thumb computation | `settings/window.rs:501` (`tick_content_scrollbar`) |
| settings state | `settings/window.rs:306` (`content_scrollbar: ScrollbarState`) |

## Target Epic

No epic exists and none can be inferred. Both sources are unparented with no
`discovered-from` provenance, which is intentional for `ponytail-debt` beads.
**This run will create a new epic** and file the refined tasks under it.

## User Stories

### Story A — correct the stale upgrade-log ceiling

> As a maintainer reading `dist/debian/postinst`, I want the `ponytail:`
> comment to state the true current situation, so that the next person does
> not implement rotation that the codebase already made unnecessary.

**Acceptance Criteria**

- The `ponytail:` comment at `dist/debian/postinst:682-685` no longer claims
  that "the durable state-dir `server.log` already accepts" an unbounded
  ceiling.
- The replacement text records the facts: `server.log` is capped at
  `SERVER_LOG_MAX_BYTES` (8 MiB) and rotated once to `server.log.1` at
  startup; the `--upgrade` server mirrors its tracing there, so `upgrade.log`
  duplicates an already-bounded stream and is truncated at each upgrade spawn.
- The comment either drops the `ponytail:` prefix (the ceiling is accepted,
  not deferred) or states a revised, honest upgrade trigger. It must not leave
  a debt marker that re-enters the ledger describing work now ruled a non-goal.
- **No behavioral change.** The diff touches comments only — no change to
  `UPGRADE_LOG`, the redirect, the readiness grep, `cleanup_upgrade_state`, or
  the `mktemp` fallback.
- `scribe-6vw` is superseded by this task, not silently dropped.
- Constitution #4: performance budget explicitly inapplicable — comment-only
  change.
- Verification is reading the diff; no harness run is required for a
  comment-only change. If any behavioral change creeps in, it must instead be
  verified inside the Docker E2E harness only.

### Story B — grabbable settings scrollbar

> As someone reading a long settings page, I want to grab and drag the
> scrollbar thumb and click the track to jump, so that navigating settings
> works the same way as navigating terminal scrollback.

**Acceptance Criteria**

- Pointer over the settings content-pane hit zone widens the thumb and pins
  the overlay open; leaving the zone resumes the idle fade.
- Pressing the thumb starts a drag; moving the pointer scrolls the content
  pane proportionally; releasing ends the drag.
- Pressing the track outside the thumb jumps the viewport to that position.
- A drag holds the overlay open — the thumb does not fade out from under the
  pointer mid-drag (matches `press_scrollbar`'s `fade_start = None`).
- Hit zone geometry and widen behavior use `crate::scrollbar`'s existing
  primitives; no duplicated geometry math (constitution #1).
- Behavior is indistinguishable from the terminal pane's scrollbar
  (constitution #2).
- Scroll position stays clamped to the existing whole-pixel scroll-unit
  ceiling (`settings/window.rs:103`).
- When the page does not overflow (no thumb), a press in the hit zone is **not**
  consumed — it falls through to the settings controls underneath, mirroring
  `press_scrollbar`'s return contract.
- A user-reachable verification path exists (constitution #3), run in the
  Docker harness.
- Constitution #4: performance budget explicitly inapplicable — pointer
  handling on an existing render path, no hot-path change.
- **Does not start until the in-flight `settings/window.rs` work is committed
  to `main`** (Clarifications Q4).

## Constraints

- **HARD REPO RULE:** all validation, testing, and debugging runs *only*
  inside the Docker E2E harness via `just` recipes (`just e2e-func`,
  `just e2e-visual`, ...). Never against the host server. The developer works
  inside Scribe all day; a host invocation targets the live socket. Never
  restart the host server.
- **Story B is gated on the in-flight settings work landing** (Clarifications
  Q4). Its tasks must not enter the dispatchable ready frontier until the
  uncommitted `settings/window.rs` changes are committed to `main`. This is
  wired as a real bd dependency so `/implement-ready` cannot pick it up early.
- **Live collision risk (Story B).** The working tree has ~521 uncommitted
  lines in `crates/scribe-client/src/settings/window.rs`, plus changes to
  `settings/mod.rs`, `settings/model.rs`, `main.rs`, and `titlebar.rs`, and a
  new untracked `tests/e2e/visual/settings-keybindings.sh`. The new
  `inline_commit_value` helper indicates the *inline text editing* half of
  `scribe-19v` is being implemented right now, in the same file and near the
  same regions. Story B will conflict if it starts from the current `main`.
  Sequencing must be decided at the gate.
- Both ceilings are independent and can be paid down separately, in either
  order, by different sessions.
- `ponytail-debt` beads are unparented by design; the refined tasks land under
  a newly created epic, and the ledger label should be preserved for traceability.
- Constitution #7: `lat.md` must stay synchronized, packaging changes must be
  documented, and the live server must not be disrupted.
- Constitution #3: add test code only when explicitly requested or when
  existing coverage must change; otherwise document and run manual
  verification. `tests/e2e/visual/scrollbar.sh` is the existing terminal
  oracle and may need to change rather than be duplicated.
- Constitution #4: performance budgets are likely *inapplicable* to both
  stories, but that must be stated explicitly rather than omitted.
- Story A touches Debian packaging (`dist/debian/postinst`), which runs as
  root during install and handles the live upgrade path — the highest-blast-
  radius file in either story. Failure there breaks upgrades for real users.

## Open Questions

1. **Has Story A's trigger actually fired?** No measurement of real
   `upgrade.log` growth exists. Without evidence, the honest answer may be
   "no", making retirement-as-non-goal the correct disposition.
2. **Is `upgrade.log` redundant?** The successor mirrors its tracing into the
   rotated `server.log`. If the content is genuinely duplicated, the lazy fix
   is *no rotation at all* — just correct the stale comment. This is the
   single highest-leverage question in this spec.
3. If a bound is wanted, what is the mechanism? The successor holds the fd for
   life, so postinst-side truncation would corrupt the writer's offset. Plausible
   options: a size constant checked at spawn, the server dropping/reopening its
   stdio after signalling readiness, or redirecting to the rotated `server.log`
   path directly. Each has a different blast radius.
4. **Story B scope:** all three behaviors (hover widen, click-to-jump, drag),
   or only the two named in the bead title? The `ponytail:` comment names three.
5. **Story B sequencing:** wait for the in-flight `window.rs` work to land,
   rebase onto it, or proceed and absorb the conflict?
6. What is the user-reachable verification path for Story B? Extend
   `tests/e2e/visual/scrollbar.sh`, add a settings-specific visual script, or
   document manual verification under constitution #3?
7. Does Story A need any harness coverage at all, given the postinst upgrade
   path's existing E2E coverage? Which recipe exercises it?
8. Should the refined tasks keep the `ponytail-debt` label once they are P3
   under a real epic, or does the label only mark unrefined ledger entries?

## Spec Review

One self-review pass (depth: quick) covering requirements, gaps, ambiguity,
feasibility, scope, and stakeholders. Cross-dimension hits are ranked highest.

### Critical Questions (answer before planning)

1. **Does `upgrade.log` need bounding at all, given `server.log` is already
   rotated and receives the same stream?** — This is the whole of Story A. The
   ledger entry's stated premise is factually stale: `server.log` gained an
   8 MiB cap with single rotation in `e50c2c3`, ~13 hours before `scribe-6vw`
   was filed, and `crates/scribe-server/src/main.rs:96-101` states the
   `--upgrade` server mirrors its tracing into that same rotated file
   precisely because postinst stdio is not durable. If the content is
   genuinely duplicated, the correct paydown is *delete the stale comment and
   retire the bead*, and any rotation work is invented scope. Story A cannot
   become concrete P0–P3 work until this is settled — its acceptance criteria
   are currently written as a disjunction ("bound it, **or** record why not"),
   which is not a testable outcome.
   *Flagged by: requirements, scope, gaps, feasibility.*

2. **If `upgrade.log` is bounded, what protects upgrade-failure diagnostics?**
   — The log's real consumer is a human debugging a failed upgrade, and the
   postinst watchdog itself greps it for `IPC server listening`. Any bound
   introduces a window where the evidence of *why* an upgrade failed can be
   discarded. Compounding this: the successor holds the inherited fd for its
   whole life, so postinst-side truncation or rename would leave the writer
   appending at a stale offset — the exact class of bug `scribe-oij` fixed.
   The `mktemp` fallback branch (`dist/debian/postinst:686`) needs the same
   answer. A "redirect into `server.log` instead" option additionally risks
   two servers appending to one file during handoff overlap.
   *Flagged by: gaps, stakeholders, feasibility.*

3. **Story B scope: all three behaviors, or only the two in the bead title?**
   — `scribe-233`'s title names drag and click-to-jump; the `ponytail:`
   comment names hover widen as well. Hover widen is not cosmetic here: the
   terminal's `update_scrollbar_hover` doc states the resting 6 px thumb is a
   hint and the widen is "what makes it grabbable". Shipping drag without
   widen produces a control that is technically present but hard to hit,
   which is phase-2 work in disguise and violates constitution #2's
   consistency requirement against the terminal pane.
   *Flagged by: scope, requirements, ambiguity.*

4. **Story B sequencing against in-flight work.** — `settings/window.rs` has
   ~521 uncommitted lines in the working tree, including a new
   `inline_commit_value` helper, i.e. the *inline text editing* half of the
   same source task `scribe-19v` is being written right now in the same file.
   Starting Story B from current `main` guarantees a conflict of unknown size.
   Decide: block Story B on that work landing, rebase onto it, or accept the
   conflict.
   *Flagged by: feasibility, scope.*

### Non-Blocking Observations

- **Unit mapping is already solved.** `tick_content_scrollbar` documents "a
  pixel scroller is that same shape with one pixel as the unit" and builds
  `ScrollMetrics` with `display_offset = overflow - scrolled`, matching the
  terminal's count-from-bottom convention. `offset_from_drag` and
  `offset_from_track_click` therefore apply unchanged; the only new code is
  inverting the result back (`scrolled = overflow - display_offset`) onto
  `scroll_handle`. This removes the main feasibility risk from Story B.
- **Do not swallow clicks when the page fits.** `tick_content_scrollbar`
  returns `None` when opacity is zero or the page does not overflow. The new
  mouse handler must mirror `press_scrollbar`'s contract and return "not
  consumed" in that case, or the invisible overlay will eat presses meant for
  the settings controls underneath.
- **Constitution #4 (performance budgets)** is almost certainly inapplicable
  to both stories, but the constitution requires that be stated explicitly,
  not omitted. Plan should carry one line marking it inapplicable and why.
- **Constitution #3 verification path** is undecided for Story B: extend the
  existing `tests/e2e/visual/scrollbar.sh` terminal oracle, add a
  settings-specific visual script, or document manual verification. Note a new
  untracked `tests/e2e/visual/settings-keybindings.sh` already exists, so a
  settings visual-test pattern is being established concurrently — prefer
  matching it over inventing a second shape.
- Story A's blast radius is disproportionate to its value: `dist/debian/postinst`
  runs as root during install on the live upgrade path. This asymmetry is an
  argument for the retire-and-correct-the-comment disposition in Q1.
- The two stories are fully independent and can be worked by different
  sessions in either order.
- `ponytail-debt` label retention on refined tasks (Open Question 8) is a
  bookkeeping convention with no functional impact; pick one and be consistent.
- The `UNIT_CAP` whole-pixel ceiling (`settings/window.rs:103`) already clamps
  scroll units, so drag targets inherit clamping for free — no new bounds
  logic needed.

## Clarifications

Answered by the human at the clarify gate. All four answers changed the spec
body above.

**Q1: Does `upgrade.log` need bounding at all, given `server.log` is already
rotated and receives the same stream?**

A: **No — retire it and fix the comment.** `scribe-6vw` is an explicitly
approved non-goal. The rotation work is invented scope: `server.log` was
already capped and rotated ~13 hours before the bead was filed, and the
`--upgrade` server mirrors its tracing into that file precisely because
postinst stdio is not durable. Replacement coverage is the comment correction
(Story A), so `scribe-6vw` is superseded rather than bare-retired. Reflected
in Goals 1, Non-Goals, and Story A.

**Q2: If bounded, what protects upgrade-failure diagnostics and the live
writer's fd?**

A: **N/A — retiring per Q1.** No mechanism is needed. The fd-offset hazard and
the failed-upgrade diagnostics cost are recorded in the Spec Review as the
reasons the mechanism options were rejected, not as deferred work.

**Q3: Story B scope — which scrollbar behaviors ship?**

A: **All three** — hover widen, click-to-jump, and drag. Widen is load-bearing,
not cosmetic: the terminal's own `update_scrollbar_hover` doc states the
resting 6 px thumb is a hint and the widen is what makes it grabbable.
Shipping drag without widen would violate constitution #2's consistency
requirement. Reflected in Goal 2 and Story B's criteria.

**Q4: How should Story B sequence against the ~521 uncommitted lines in
`settings/window.rs`?**

A: **Block on the in-flight work.** Story B's implementation task gets a real
bd dependency on a precondition task that confirms the inline-text-editing
changes are committed to `main`. This keeps Story B out of the dispatchable
ready frontier until the conflict risk is gone, at the cost of latency.
Reflected in Constraints and Story B's criteria.

### Backlog disposition (settled)

| bead | disposition | replacement coverage |
|---|---|---|
| `scribe-6vw` | superseded — approved non-goal | Story A comment-correction task |
| `scribe-233` | refined in place to P3 | Story B tasks (all three behaviors) |

No source P4 is silently dropped, and none remains open in the closure.
