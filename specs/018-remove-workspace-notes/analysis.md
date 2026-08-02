# Analysis: remove-workspace-notes

Cross-check of `spec.md` (status CLARIFIED) against `plan.md` (post-alignment)
and `constitution.md`. This is a report gate: it records what is covered, what
is not, and whether the epic is fit to decompose into beads. It makes no
decisions and changes no scope. Where the spec body and `## Clarifications`
disagree, the clarification is treated as authoritative, per the spec's own
status note; where the spec and the plan disagree on a measured fact, the
plan's `## Alignment fixes applied` entry is treated as the later measurement.

## Coverage Table

Twenty-two items: the 7 Goals, the 6 User Stories, and the 9 Clarification
answers (Q1-Q7, Q-A, Q-B). "Covered by" names the plan section or the
`## Sequencing` work item that owns the item; work items are cited by their
plan title, since the plan deliberately uses no step codes.

| User story / requirement | Covered by (plan section) | Status |
|---|---|---|
| **Goal 1** — two-stage completion gate returns zero | `## Testing Strategy` (the `COMMON`/`A`/`ALLOW` block reproduced verbatim); work items *Run the quality gate* (GATE A + GATE B both zero) and *Remove the `justfile` recipe and the E2E script, and reword `tab-window-chords.sh`* | full |
| **Goal 2** — `just ready` clean, no new `allow`/`expect` | `## Testing Strategy` (`just ready`, `check-no-new-lint-suppressions.sh`); work items *Run the quality gate* and *Stage all code and gate-document edits and create the single atomic commit* | full |
| **Goal 3** — both CI count-gates recompute cleanly | `## Affected Components § CI gate documents`, `## API / Interface Changes § Gate-document interface changes`; work items *Recompute `tools/reachability-baseline.txt`* (64/64, 52/57, 36/36) and *Recompute the parity inventory and amend `US4-3`* (199 rows … 190 user-facing) | full |
| **Goal 4** — docs consistent, `lat check` passes | Work items *Delete the `lat.md` notes sections and fix the surrounding prose*, *Record the compatibility decision in `lat.md/protocol.md`*; `lat check` green is an acceptance criterion of *Run the quality gate* by deliberate relocation | full |
| **Goal 5** — titlebar visually correct, no dead gap | `## API / Interface Changes § User-facing surface changes`; work items *`titlebar.rs`, including the `e530da7` carry-forward* and *Manual client-launch verification* (screenshot evidence required) | full |
| **Goal 6** — `ctrl+shift+m` unbound, `OVERLAY_CHORDS` arity 5 -> 4 | `## API / Interface Changes` ("Compile-enforced detail"); work items *`keybindings.rs` chord and array arity*, *Edit down the shared hot spots in client `main.rs`* (`open_overlay_chord` has exactly four arms) | full |
| **Goal 7** — persisted data deleted, no backup, after restart | `## Data Model § Data destruction is an ordered operator step, not code`; work items *Rebuild, reinstall, and restart*, *Stale-file startup check on a dev daemon*, *Delete the note data* | full |
| **Story 1** — UI affordances are gone | Work items *`titlebar.rs`, including the `e530da7` carry-forward* (band lines only; `advance_move_arm` and `update_drag` preserved verbatim; no hard-coded pixel band survives) and *Manual client-launch verification*; `## Testing Strategy` per-story map row 1 | full |
| **Story 2** — `ctrl+shift+m` opens nothing | Work items *`keybindings.rs` chord and array arity*, *Edit down the shared hot spots in client `main.rs`*, *Manual client-launch verification* (chord press plus all four surviving overlay chords); `## Testing Strategy` per-story map row 2 | full |
| **Story 3** — stored note data deleted, no backup | `## Data Model`; work items *Rebuild, reinstall, and restart*, *Stale-file startup check on a dev daemon*, *Delete the note data*; `## Testing Strategy` per-story map row 3 (order is the criterion) | full |
| **Story 4** — no dead code left in any crate | Work items *Delete the protocol types and variants in `scribe-common`*, *Remove the server side*, *Delete the client notes modules…*, *Delete the notes-only surface in client `main.rs`*, *Edit down the shared hot spots…*, *`ipc_bridge.rs`*, *`scribe-test` daemon or-pattern edit*, *Run the quality gate*; retained-by-design items asserted in `## Affected Components § Must NOT be touched` | full |
| **Story 5** — `lat.md` reflects reality | Work items *Delete the `lat.md` notes sections and fix the surrounding prose* (including `architecture.md:16` and `:148`), *Record the compatibility decision in `lat.md/protocol.md`*, *Delete the client notes modules…* (the 16 `// @lat:` anchors leave with their files) | full |
| **Story 6** — live upgrade degrades to a recoverable blip | `## Architecture Approach`, `## API / Interface Changes § Breaking wire changes`, `## Risks` (mixed-version window); work item *Record the compatibility decision in `lat.md/protocol.md`*; `## Testing Strategy` per-story map row 6 (documentation plus a diff assertion that `REMOTE_PROTOCOL_VERSION` is untouched) | full |
| **Q1** — delete outright, single atomic change | `## Architecture Approach` (blip-not-data-loss argument, unanimous precedent) and `### Rejected alternatives` (a); atomicity enforced by *Stage all code and gate-document edits…* | full |
| **Q2** — `REMOTE_PROTOCOL_VERSION` stays `3` | `### Rejected alternatives` (b) with all three grounds; `## API / Interface Changes`; the constant is named as a diff assertion on Story 6 | full |
| **Q3** — both CI count-gates are mandatory in-scope work | `### The atomicity constraint is mechanical, not stylistic`; the two recompute work items; the `--staged` hook consequence carried onto the commit item | full |
| **Q4** — symbol names govern; `e530da7` carry-forwards | `## Affected Components` opening banner; `### Compiler-invisible hazards` items 2 and 3; work item *Re-verify the scope survey against the rebased tree*, which blocks every other item and emits `edit-list.md` | full |
| **Q5** — data deleted last, after rebuild/reinstall/restart | `## Data Model` 4-step ordering with step 3b inserted; the four terminal work items, split so the point of no return stands alone | full |
| **Q6** — leave `ctrl+shift+m` unbound, not swallowed, not reassigned | `## API / Interface Changes § User-facing surface changes` (the `0x0D` fall-through documented as behavior, not a defect); `## Affected Components` (`keybindings/tests.rs` not edited) | full |
| **Q7** — the two-stage `rg` gate is the "done" oracle | `## Testing Strategy`, block copied verbatim; plus the plan-time correction that GATE A cannot reach zero without the `tab-window-chords.sh` reword, and that neither gate sees the singular `workspace-note` | full |
| **Q-A** — server restart authorized, for step 3 only | `### Constitution check` P7; work item *Rebuild, reinstall, and restart* ("the one item authorized to restart the server"); every other item's verification is restart-free | full |
| **Q-B** — keep `specs/004` and `specs/007` | `## Affected Components § Must NOT be touched`; stale cross-references owned by the P3 work item *Scrub the stale `specs/**` cross-references* | full |

**Headline: 22 items — 22 full, 0 partial, 0 none.**

Four notes qualify that result. None of them is a missing owner; each is a
caveat an implementer should carry, and all four are already stated somewhere
in the plan rather than being discovered here.

1. **Goal 1's green gate does not by itself prove the docs are true.** The plan
   says so plainly in `## Testing Strategy`: GATE A's leading alternative
   requires the plural `notes`, and GATE B reduces every hit to its bare token
   and then strips anything ending in `:(notes?|noted|noting)`, so the singular
   `workspace-note` at `lat.md/architecture.md:16` is invisible to both stages.
   The compensating control is the `lat.md` item's line-by-line, diff-shaped
   acceptance, which names that line explicitly. Coverage is full because the
   compensating control has an owner, not because the gate is sufficient.
2. **GATE B's post-removal cleanliness rests on the spec's sandbox simulation,
   not on a plan-time re-measurement.** GATE A was re-measured at plan time
   (710 lines / 24 files, confirmed again against the working tree during this
   analysis); GATE B's 842-token baseline and its "zero false positives after
   deletion" claim were not re-run the same way. If a surviving `note`-bearing
   token turns up that the allowlist does not cover, it surfaces at the
   *Run the quality gate* item and needs an allowlist judgement there. Low
   severity, but it is the one number in the completion oracle that no one has
   re-checked post-rebase.
3. **Story 1 has no automated oracle after `workspace-notes.sh` is deleted.**
   `titlebar.sh` and `window-chrome-bands.sh` survive but assert nothing about
   the notes button. The plan states this honestly and mitigates with a manual
   client launch carrying screenshot evidence. That is permitted by constitution
   principle 3; it is still the weakest verification path in the epic.
4. **The manual-verification item deliberately runs a new client against the
   still-old server**, which is precisely the mixed-version window Q1 accepts.
   The plan names the hazard in advance so the resulting blip is not misread as
   a regression. This is a knowing acceptance, not an oversight.

## Backlog Disposition

None — this run originates from a direct user request, not from a P4 backlog
issue. No backlog inputs to refine, and none to resolve.

| Backlog input | Disposition | Status |
|---|---|---|
| *(none)* | No P4 backlog issue sourced this run; `spec.md` `## Backlog Inputs` and `plan.md` `## Backlog Refinement` both record "None" | n/a |

## Target Epic

No existing epic was supplied by the user and none could be inferred — there
are no backlog inputs to infer from. A **new feature epic will be created at
the `create-beads` step**, with the plan's twenty-three work items decomposed
into task beads beneath it.

The epic id is load-bearing for exactly one edit. The `US4-3` descope
annotation the plan writes into `specs/016-gpui-client-rebuild/spec.md` carries
an `<EPIC-ID>` placeholder that must be replaced with the real epic id:

```markdown
- **US4-3** *(descoped 2026-08-01, bead <EPIC-ID>: the workspace notes modal and
  hover preview are removed from the product)* Workspace system (accent colors,
  badges, workspace splits) works as today.
```

Per the alignment round this is carried as an **instruction on the
parity-inventory work item** — "replace `<EPIC-ID>` with this bead's parent
epic id" — and deliberately **not** as a DAG edge. The epic is minted by the
same `create-beads` step that mints the task beads, so an edge reading "blocked
by the epic existing" would point at a node the DAG does not contain. This is
**resolved, not ambiguous**: the substitution has a named owner, a named file,
and a named moment.

No existing beads from the `specs/004` or `specs/007` eras need closing or
re-parenting — the only `.beads/` hits for notes are two closed, immutable
history records, and no open bead references the feature.

## Remaining Risks

Carried forward from `plan.md § Risks` with their mitigations, stated as the
plan states them rather than softened.

**Mixed-version window during upgrade.** An old client emitting a deleted frame
at a new server severs the connection, and the trigger is **hover, not a
deliberate chord press** — `set_workspace_notes_preview` fires from the
titlebar's `on_mouse_move` band. The window is roughly 1-4 s on the packaged
`postinst` path but **indefinite** under `just restart-server` /
`just restart-server-release`, which do not touch clients at all, and in four
other `postinst` fallback branches. *Mitigation is operational, not code:*
restart the server and the clients together, which the Story 3 step 3 sequence
does anyway. *Residual severity:* a red status dot and one status line; PTYs,
scrollback, and typed input all survive, and `cx.quit()` is unreachable from a
connection failure.

**The parity-inventory recompute is eight interlocking hand-maintained
numbers.** Five rows, three headings, three footers, the roll-up Total, three
prose figures, and the `US4-3` coverage cell must all move together, and the
gate parses the file live against `protocol.rs` — a partial edit fails with
*"the 'Client messages' table names unknown entries."* *Mitigation:* run
`tools/check-parity-inventory.sh --working-tree` after **every** doc edit, not
at the end. It is instant and needs no build.

**Cold `target/` makes the first full gate slow.** This worktree has never been
built and GPUI compiles at `opt-level = 3` even in debug. *Decision recorded in
the plan rather than deferred: accept the cold gate in this worktree.* The
warm-primary-checkout alternative was considered and rejected because it means
carrying a ~64 GB `target/` and cross-checkout state over the very files under
surgery. *Mitigation:* front-load every script gate — both count-gates,
`lat check`, GATE A/B — so nothing waits on the build.

**Two compiler-invisible hazards.** First, `crates/scribe-test/src/daemon.rs`:
the `WorkspaceNotesSnapshot` / `WorkspaceNotesChanged` lines sit inside a
grouped or-pattern whose enclosing arm **survives** on `Error`, `SessionList`,
`SearchResults`, `PromptMark`, and `PromptReceived`; deleting the arm instead
of exactly two `|` lines silently reroutes those unrelated variants with no
compiler error. Second, `titlebar.rs`: the `e530da7` root `on_mouse_move`
closure **interleaves** `advance_move_arm`'s early return and the `update_drag`
call with the `WorkspaceNotesHover` hit band, and over-deleting breaks window
dragging on X11/Wayland with no compiler error, because
`WindowControlArea::Drag` is a no-op in the pinned GPUI revision.
*Mitigation:* both are explicit acceptance criteria on their own work items, and
the manual client launch checks dragging.

**Rollback, and the point of no return.** The code is fully revertible: a single
`git revert` restores every deleted module, the protocol variants, the chord,
the docs, and — because they land in the same commit — the `specs/016-*` gate
numbers, so the gates stay self-consistent in both directions. The revert is
safe **precisely because `REMOTE_PROTOCOL_VERSION` is untouched**: there is no
version a peer could have moved past and be stranded on. **The note data is not
revertible.** Story 3 step 4 is the point of no return — by explicit design
there is no backup, archive, or export to restore from. Everything before step
4 is undoable; nothing after it is.

**Two facts the alignment round corrected, both re-confirmed here against this
worktree.** GATE A returns **710 lines across 24 files**, not the 709 the spec
body records — the plan carries the measured figure. And
`.pre-commit-config.yaml` has **three** `--staged` hooks, not two:
`no-new-lint-suppressions`, `reachability-baseline`, and `parity-inventory`.
Both were re-run at analysis time and both hold. An implementer should treat the
plan's numbers as authoritative wherever the spec body disagrees.

## Unresolved Questions

**None.** The clarify gate is closed and the alignment round is applied.

All seven critical questions raised in `## Spec Review` are answered in
`## Clarifications` (Q1-Q7), together with the two user decisions (Q-A on the
server restart, Q-B on the historical `specs/` archives), and the spec's
`## Open Questions` section carries an explicit resolution map from each of the
five draft questions to the answer that closed it. The alignment round then
applied 5 must-fix and 16 should-fix findings, all enumerated in
`plan.md § Alignment fixes applied`; the must-fixes closed the only three
genuine coverage holes — the unowned `tab-window-chords.sh` GATE A hit, the
`lat.md`-before-files ordering that would have left 16 dangling `// @lat:`
anchors, and the missing step 3b stale-file check.

Three items look open on a fast read and are not. Each is a deliberate,
recorded position rather than a question:

- **The `<EPIC-ID>` placeholder** is unresolved *text*, but its resolution is
  fully specified — substituted at `create-beads` time, owned by the
  parity-inventory work item, deliberately not a DAG edge. See
  [Target Epic](#target-epic).
- **The release-note sentence** has no work item on purpose. Tagging is not part
  of this epic and the sentence cannot be written until a release is cut, so the
  decision is explicitly deferred to the user at release time and recorded in
  `plan.md § Risks` so it is a deliberate omission rather than a floating
  obligation.
- **The spec/plan divergences on 709-vs-710 and two-vs-three `--staged` hooks**
  are settled by document authority: the plan measured later, records the
  correction, and both figures were re-confirmed during this analysis.

## Constitution Check

All seven principles, checked against the plan rather than assumed.

1. **Clear Boundaries and Typed Failure — PASS.** The change only subtracts:
   no new dependency, no new abstraction, no new cross-cutting helper, and
   nothing crosses a crate boundary that did not already. Note failures already
   ride the generic `ServerMessage::Error` (there is no `ServerError` enum), so
   no error taxonomy is disturbed, and the retained `toml` dependency in
   `scribe-server` is explicitly protected because `lan/network.rs`,
   `lan/trust.rs`, and `env_store/gc.rs` still use it.

2. **Session-Safe, Consistent UX — PASS, with a noted tension.** A titlebar
   control disappears and a configurable-adjacent shortcut becomes unbound,
   which is a real UX contract change carried by Stories 1 and 2. It is
   justified because the feature itself is gone, and it is bounded: the notes
   button is a plain flex child so its siblings simply reflow, and
   `ctrl+shift+m` joins the eleven other unbound `ctrl+shift+<letter>` combos
   (`a e g h i j l o r s y`) that already fall through to the PTY today.
   Long-lived server-owned sessions are untouched — the store was a sidecar,
   never session state — and no user configuration can break, since
   `OverlayChord` sits outside `KeybindingsConfig` by design.

3. **Explicit, Risk-Based Verification — PASS.** Every story has an
   independent, user-reachable verification path in
   `plan.md § Testing Strategy`'s per-story map. **No new test code is written**,
   which the principle permits: none was requested, and the only coverage that
   changes is coverage leaving with its feature. The honest caveat is stated in
   the plan and repeated here — the deleted `tests/e2e/visual/workspace-notes.sh`
   was the only automated oracle for the notes UI, and `titlebar.sh` /
   `window-chrome-bands.sh` assert nothing about the notes button, so Story 1's
   replacement is the manual client launch (with screenshot evidence) plus the
   two-stage completion gate.

4. **Performance Budgets and Measurement — INAPPLICABLE, explicitly marked.**
   The principle requires the mark rather than silence, and both documents give
   it. This is a pure removal: no new hot path, no new allocation, no new render
   work, no new IO. The only measurable direction is downward — one fewer
   titlebar child, one fewer sync pass in `Render::render`, one fewer reader
   routing arm — and none of it warrants instrumentation.

5. **Default-Safe Trust Boundaries — PASS.** No capability is added and no
   capability surface changes shape. One data-adjacent surface is *removed*:
   the notes messages were routed with no `is_remote` gate and were absent from
   `requires_window_control`, so a remote controller could read and mutate
   workspace notes; deleting them shrinks the remote-reachable surface. The
   removal also deletes 0600 user-authored free text that the product will no
   longer have any UI to view, export, or delete — the privacy-correct
   direction, since "inert" is not "gone."

6. **Local-First Data Locality — PASS.** Nothing gains network behavior. A
   locally-persisted TOML file and its protocol messages are removed; no
   terminal contents or audio move anywhere they did not already, and no
   network access is introduced or made mandatory.

7. **Compatible, Documented, Operationally Safe Change — PASS.** The
   compatibility decision — four variants deleted outright, six types with
   them, `REMOTE_PROTOCOL_VERSION` deliberately left at `3`, and the reasoning
   for both halves — is recorded in `lat.md/protocol.md` by its own work item
   and in the spec's `## Clarifications` as the durable decision record.
   `lat.md` is kept synchronized, with `lat check` green enforced at the
   quality-gate join point. The standing rule against restarting the live
   server is honored: exactly one approval exists (Q-A, for step 3 of the
   data-deletion sequence), it is bound to a single work item, and Story 6 is
   verified by reading and documentation rather than by exercising a live
   upgrade. Worktree state is preserved — the baseline is this worktree rebased
   onto `cfcc84d`, clean except for the untracked spec directory.

No violations. One documented tension (P2) and one explicit inapplicability
mark (P4), both of which the constitution's own wording anticipates.

## Recommendation

**GO**

Every one of the 22 checked items — 7 Goals, 6 User Stories, 9 Clarification
answers — has a named owner in the plan, and each of the 23 work items carries
a diff-shaped acceptance criterion with no placeholder or TBD. The target epic
is resolved rather than ambiguous: a new feature epic is minted at
`create-beads`, and the single load-bearing consumer of its id (the `US4-3`
`<EPIC-ID>` placeholder) is carried as an instruction on the parity-inventory
work item rather than as a dangling DAG edge. There are no backlog inputs, so
none can be unresolved. No constitution principle is violated; P2's tension is
the intended product change and is bounded, and P4 carries the explicit
inapplicability mark the principle demands. The alignment round closed the
three real coverage holes — the unowned `tab-window-chords.sh` GATE A hit, the
`lat.md`-before-files ordering that would have stranded 16 `// @lat:` anchors,
and the absent step 3b stale-file check — and its two plan-time measurements
(710 GATE A lines, three `--staged` hooks) were re-confirmed against this
worktree during this analysis. The residual risks are known, quantified, and
mitigated: the mixed-version window is operational and self-healing, the
parity recompute is guarded by an instant `--working-tree` check after every
doc edit, the cold build is a scheduling cost with a recorded decision behind
it, and the two compiler-invisible hazards are explicit acceptance criteria on
their own items. The one irreversible step — deleting the note data — stands
alone as the last work item, behind the restart and the stale-file check, which
is exactly where it belongs. Proceed to `create-beads`.
