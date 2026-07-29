# Analysis: audit-findings-triage

Report-only pre-bead analysis of the clarified spec and aligned plan.
Artifacts: `spec.md` (82-finding disposition, 9 user stories, 7
clarifications), `plan.md` (48 work items in 3 waves, full 82-finding
traceability, per-item lat.md targets, alignment fixes applied).

## Coverage Table

| User story / requirement | Covered by (plan section) | Status |
|--------------------------|---------------------------|--------|
| US1 server survives misbehaving children/clients (5,6,7,8,9,11,12,13,21,58/59) | Sequencing Wave 1: reader cancellation → PtyGuard teardown → child-exit watcher → close-path lock protocol → sink state machine; Architecture (a)-(d) | full |
| US2 attach/replay lossless and bounded (3,4,17,18,19,22,23) | Wave 1: sink state machine, streamed decompression (64 MiB), fan-out dedup/cap, off-thread encode, client inflate off-runtime, fresh-session skip + real geometry; scribe-test replay-support item | full |
| US3 client IPC bounded and paced (14,15,62/63,64,65/66,68) | Wave 2: bounded queues (Q6 policies), #68 blocks #62 pacing wire-in, intermediate-frame skip, batch byte cap | full |
| US4 env persistence works end to end (26,27,28,29,30,31,32,33,34,35,72) | Wave 1: helper resolution/packaging, per-shell restore rendering, envelope bootstrap+GC (3 create paths), spawn-time semantics (#32 corrected: server-side disable already live); Wave 2: gate var, linear diffs, nu XDG cleanup | full |
| US5 hook pipeline lean, no secret leaks (37,38+40,41,42,43,44,45,46) | Wave 1: payload transport off argv (dual-accept), #46 slots+timeout (moved to Wave 1); Wave 2: adapter consolidation, equality guard | full |
| US6 metadata dedup/off hot path (48/50,49/51,52,54,55,56,57,73/74,75/76,82) | Wave 2: single-source title/bell, OSC 7 suppression, branch cache, ListSessions off-guards, empty-frame guard, config/theme cache, launch caching (#82 ratified at gate) | full |
| US7 admission and handoff correct (1,2,10,24,25) | Wave 1: cap counts live sessions atomically (256, #1+#2 together), PID-identity check; Wave 2: viewport debounce, ≤4 applies/sec (+ scripted resize verification) | full |
| US8 search doesn't stall the session (69,70) | Wave 1: scan outside Term lock; debounce 150 ms + overlay snapshot reuse (Q5) | full |
| US9 installer atomic and minimal (78,79+80) | Wave 1: atomic writes; single read-modify-write, idempotent | full |
| Goal: nothing silently dropped | plan Traceability (82/82 mapped) + "Not fixed (with rationale)" list | full |
| Wave 0 P4 baselines (constitution P4) | 4 baseline items (hook execs, search/attach lock+alloc, metadata/batch stats, per-prompt shell cost) → `baselines.md` | full |
| Q1-Q7 clarifications | threaded through Architecture, API, Sequencing (verified by alignment pass) | full |
| Rollout/compat (Q2) | API/Interface: additive serde, helper dual-transport, handoff-inherited exemptions, HANDOFF_VERSION 6 | full |

## Backlog Disposition

| Source P4 id | Plan work item(s) / non-goal | Disposition | Ready to resolve? |
|--------------|------------------------------|-------------|-------------------|
| (none) | No open P4 sources exist in the tracker; Backlog Inputs = None | n/a | yes |

## Target Epic

New epic — will be created at the create-beads step. No existing epic
was provided, inferred, or found (tracker holds one unrelated open
issue, `scribe-142`).

## Remaining Risks

- Sink state machine reworks the hottest path; mitigated by e2e-visual
  suite + revert criteria (restore install-after-replay order behind a
  flag on any ordering corruption).
- Handoff upgrade is one-shot against the live server; mitigated by
  additive `#[serde(default)]` fields, dev-identity upgrade rehearsal,
  and revert criteria on the HandoffState field.
- Helper transport migration window: pre-upgrade shells keep old argv;
  mitigated by dual-accept for one release (Q2).
- Decompression ceiling change on a LAN-reachable path; mitigated by
  the 64 MiB streamed absolute cap (Q7, constitution P5).
- Codex installer trusted-hash invalidation when adapter commands
  change; mitigated by installer migration step in the consolidation
  item.
- 48 items across 3 crates + 5 shells: shell-script items serialized
  (#26 → #27-29 → #42-45 → Wave 2 shell items) to avoid conflicting
  edits.

## Unresolved Questions

- (resolved at gate) #82 launch-dir caching ratified: packaged builds
  only, dev hot-swap preserved, #81 folded in, optional P3 under US6.

## Constitution Check

| Principle | Verdict |
|-----------|---------|
| P1 Clear Boundaries and Typed Failure | pass — fixes stay in owning crates; PtyGuard wraps (not forks) alacritty |
| P2 Session-Safe, Consistent UX | pass with resolved tension — Q6 forbids silent input drops; output overflow resyncs via existing RequestSnapshot |
| P3 Explicit, Risk-Based Verification | pass — per-story paths via dev-identity server, docker e2e, named manual scenarios; no unrequested test code |
| P4 Performance Budgets and Measurement | pass — Wave 0 baselines recorded in baselines.md before fixes merge; numeric targets from Q7 |
| P5 Default-Safe Trust Boundaries | pass — argv secrets closed, PID identity checked, fan-out capped, decompression hard-capped |
| P6 Local-First Data Locality | pass — no new network paths; env data stays local/encrypted; 30-day orphan GC |
| P7 Compatible, Documented, Operationally Safe Change | pass with resolved tension — live server never restarted; additive wire changes; per-item lat.md targets gate every bead |

## Recommendation

**GO** — every user story and clarification is fully covered by the
48-item plan; the 82-finding traceability is complete with no silent
drops; there are no backlog P4 sources to disposition; the target epic
is unambiguous (new); constitution principles all pass with tensions
explicitly resolved. The single open point (#82 ratification) is an
optional P3 item that does not block bead creation — it needs only a
yes/no recorded at this gate.
