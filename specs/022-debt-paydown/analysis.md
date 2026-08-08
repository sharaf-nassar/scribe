# Analysis: debt-paydown

Report only — abbreviated per `depth=quick`. No changes were made to `spec.md`
or `plan.md` while producing this.

## Coverage Table

| User story / requirement | Covered by (plan section) | Status |
|---|---|---|
| Goal 1 — retire `upgrade.log` rotation, correct the stale comment | Architecture Approach (Story A); Backlog Refinement row `scribe-6vw` | full |
| Goal 2 — settings scrollbar: hover widen + click-to-jump + drag | Architecture Approach (Story B); Sequencing (implementation item); Backlog Refinement row `scribe-233` | full |
| Goal 3 — reuse `crate::scrollbar`, add no new geometry helpers | Architecture Approach; Affected Components (explicit "no change" row); Constitution check #1 | full |
| Goal 4 — ledger left clean, no open P4 in closure | Backlog Refinement (dispositions + closing invariant paragraph) | full |
| Goal 5 — both `ponytail:` comments removed or rewritten | Affected Components (both files); acceptance criteria on both work items | full |
| Story A — comment states the true state (8 MiB cap, single rotation, mirrored tracing) | Backlog Refinement row `scribe-6vw` | full |
| Story A — no `ponytail:` marker survives that would re-enter the ledger | Backlog Refinement row `scribe-6vw`; Risks (re-entry row) | full |
| Story A — diff is comment-only, no behavioral change | Backlog Refinement row `scribe-6vw`; Testing Strategy; Risks (blast-radius row) | full |
| Story A — `scribe-6vw` superseded, not silently dropped | Backlog Refinement (dual disposition + rationale) | full |
| Story B — hover widens and pins overlay; leaving resumes fade | Backlog Refinement row `scribe-233`; Testing Strategy phase 2 | full |
| Story B — thumb drag scrolls proportionally, correct direction | Backlog Refinement row `scribe-233`; Testing Strategy phase 3; Risks (sign-inversion row) | full |
| Story B — track press jumps viewport | Backlog Refinement row `scribe-233`; Testing Strategy phase 5 | full |
| Story B — overlay does not fade mid-drag | Backlog Refinement row `scribe-233`; Testing Strategy phase 4; Risks | full |
| Story B — no press consumed when page does not overflow | Backlog Refinement row `scribe-233`; Testing Strategy phase 6; Risks (top-severity row) | full |
| Story B — offsets clamped by existing `UNIT_CAP` | Backlog Refinement row `scribe-233` | full |
| Story B — behavior indistinguishable from terminal pane (constitution #2) | Backlog Refinement row `scribe-233` (added in alignment); Constitution check #2 | full |
| Story B — user-reachable verification path (constitution #3) | Testing Strategy (6-phase visual E2E + tension statement) | full |
| Story B — `lat.md` synced, `lat check` passes | Affected Components; Backlog Refinement row `scribe-233`; Sequencing (split across both B items) | full |
| Story B — does not start until in-flight work committed (Q4) | Sequencing (precondition item + dependency edge); Risks (top row) | full |
| Constitution #4 — perf budget explicitly stated inapplicable | Constitution check table (both stories) | full |
| Clarification Q1 — retire + fix comment | Architecture Approach; Non-Goals; Backlog Refinement | full |
| Clarification Q2 — N/A, no mechanism | Architecture Approach (alternatives rejected, with reasons) | full |
| Clarification Q3 — all three behaviors | Goal 2; Backlog Refinement row `scribe-233` | full |
| Clarification Q4 — block on in-flight work | Sequencing (real dependency edge, not a prose note) | full |
| Spec Open Questions 1–8 | Q1–Q5 by Clarifications; Q6–Q7 by Testing Strategy; Q8 by Backlog Refinement | full |

No requirement is partial or uncovered. No plan work item violates a Non-Goal.

## Backlog Disposition

| Source P4 id | Plan work item(s) / non-goal | Disposition | Ready to resolve? |
|---|---|---|---|
| `scribe-6vw` | Rotation ruled a non-goal (human-approved, Clarifications Q1); replacement coverage = "Correct the stale upgrade-log comment" | `approved-non-goal` + `split-and-supersede` | yes |
| `scribe-233` | "Wire the settings scrollbar handlers" + "Add the settings-scrollbar visual E2E" | `refine-in-place` | yes |

Both sources have concrete, verifiable acceptance criteria. Neither is dropped
or duplicated. `scribe-6vw` is superseded only after its replacement task
exists, so no P4 is closed with nothing behind it. After materialization the
closure contains no open P4, and its ready subset contains zero P4 items.

## Target Epic

**New epic.** No existing epic applies and none could be inferred — both
sources are unparented with no `discovered-from` provenance, which is
intentional for `ponytail-debt` beads so they stay out of feature-run closures.
There were no candidates to disambiguate, so this was never ambiguous.

## Remaining Risks

- **In-flight `settings/window.rs` work (~521 uncommitted lines) may not land
  promptly.** Story B's three-item chain stays blocked behind the precondition
  item until it does. *Mitigation:* the precondition item carries an explicit
  abandoned-work escape hatch — if the changes are dropped rather than
  committed, the item closes with that as the reason and Story B proceeds from
  current `main`. So the chain cannot deadlock. Story A is unaffected and
  dispatchable immediately.
- **Invisible-overlay click swallowing** is the highest-severity implementation
  hazard: if the new handlers consume presses when the page does not overflow,
  settings controls break. *Mitigation:* mirrors `press_scrollbar`'s
  not-consumed contract; carried as both an acceptance criterion and a
  dedicated E2E phase.
- **Story A edits a root-run install script.** Blast radius is high even though
  the change is comments. *Mitigation:* comment-only acceptance criterion;
  reviewer confirms no executable line moved. This asymmetry is precisely why
  the rotation mechanism was rejected at the gate.

## Constitution Check

| # | Principle | Result |
|---|---|---|
| 1 | Clear Boundaries and Typed Failure | **Pass.** No new dependencies or abstractions; one extracted layout helper so geometry has a single definition; existing `Option` returns carry the no-thumb case. |
| 2 | Session-Safe, Consistent UX | **Pass.** Story B's entire purpose is making the settings scrollbar match the terminal's established interaction. No shortcut or session behavior changes. |
| 3 | Explicit, Risk-Based Verification | **Pass, with a stated tension.** The principle limits new test code; here existing coverage must change because the behavior is new, and the `scrollbar.rs` unit tests cannot prove a real pointer press reaches the settings overlay rather than the controls underneath. The tension is named in Testing Strategy rather than glossed. |
| 4 | Performance Budgets and Measurement | **Pass.** Explicitly marked inapplicable for both stories with reasons, which is what the principle requires — not omitted. |
| 5 | Default-Safe Trust Boundaries | **Pass (not applicable).** Neither story touches PTY input, clipboard, hyperlinks, paste, or any host-action capability. |
| 6 | Local-First Data Locality | **Pass (not applicable).** No network, telemetry, audio, or terminal-content transmission. |
| 7 | Compatible, Documented, Operationally Safe Change | **Pass.** `lat.md` sync is an acceptance criterion; Story A's comment-only scope leaves the live upgrade path untouched; validation is Docker-harness-only; the host server is never restarted. |

No violations. One acknowledged tension (#3), resolved in favor of the
user-reachable verification the principle itself demands.

## Recommendation

**GO.**

Every spec requirement, user story, backlog input, and clarification answer
traces to concrete plan work with verifiable acceptance criteria; nothing is
partial and nothing is unresolved after the clarify gate. Both backlog sources
have settled dispositions backed by replacement coverage, the target epic is
unambiguous, and no constitution principle is violated. The run is also
notably smaller than the ledger implied: the highest-value finding was that
`scribe-6vw`'s premise was already stale when filed — `server.log` gained its
8 MiB cap and single rotation ~13 hours earlier — which converted a packaging
change on the live upgrade path into a comment correction. The remaining real
work is one well-scoped reuse task in the client plus its E2E, correctly
sequenced behind the in-flight settings changes so it cannot collide.
