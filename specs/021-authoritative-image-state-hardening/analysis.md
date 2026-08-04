# Analysis: authoritative-image-state-hardening

The clarified specification and aligned plan are ready for dependency-wired bead materialization under `scribe-aq1.9`.

## Coverage Table

| User story / requirement | Covered by (plan section) | Status |
| --- | --- | --- |
| US1 — Bound hostile allocations before ownership | Architecture Approach; Data Model; Testing Strategy: accounting; Sequencing: storage accounting | full |
| US2 — Terminate incomplete transfers safely | Data Model; Testing Strategy: transfer lifecycle; Sequencing: incomplete transfers | full |
| US3 — Observe terminal chronology with Alacritty parity | Architecture Approach; Testing Strategy: observer parity; Sequencing: lifecycle observations | full |
| US4 — Preserve server-client convergence | API / Interface Changes; Testing Strategy: convergence/overflow; Sequencing: counter safety | full |
| US5 — Enforce fair mandatory scheduling | Data Model; API / Interface Changes; Testing Strategy: scheduling; Sequencing: decode scheduling | full |
| US6 — Commit image mutations transactionally | Architecture Approach; Testing Strategy: transactional mutations; Sequencing: mutations | full |
| US7 — Assemble the authoritative engine | Architecture Approach; Testing Strategy: assembly; Sequencing: final certification | full |
| Deterministic requested-storage accounting | Clarifications Q1; Architecture Approach; Data Model; accounting Docker gate | full |
| One shared production seam | Clarifications Q2; Architecture Approach; first sequencing work item | full |
| Reject-before-mutation exhaustion | Clarifications Q3; API / Interface Changes; convergence Docker gate | full |
| Wire compatibility | Clarifications Q4; Data Model; API / Interface Changes; IPC fixtures | full |
| Versioned final evidence manifest | Clarifications Q5; Data Model; Testing Strategy: assembly | full |
| Approved dependency order | Clarifications Q6; Sequencing DAG | full |
| Performance scope and measurement | Clarifications Q7; Constraints; Testing Strategy; Constitution alignment | full |
| Protocol-specific Kitty/Sixel lifecycle | Architecture Approach; Testing Strategy: observer/mutations; alignment fixes | full |
| No live fanout/write-back scope creep | Non-Goals; Affected Components; Target Epic boundary | full |

## Backlog Disposition

| Source P4 id | Plan work item(s) / non-goal | Disposition | Ready to resolve? |
| --- | --- | --- | --- |
| None | No P4 sources in hierarchy-plus-provenance closure | not applicable | yes |

Ready P4 is zero.

## Target Epic

`scribe-aq1.9` — Authoritative server image state.

The original task was converted in place to a nested epic, preserving `scribe-aq1.10` and `scribe-aq1.14` as downstream dependents of the final assembly result.

## Remaining Risks

- Shared Rust modules create contention. The dependency DAG serializes overlapping engine work; implement-ready must not dispatch dependent tasks together.
- Conservative storage accounting can reject work earlier than mature terminals. Evidence records requested and observed capacities so later policy changes remain measurable without weakening hard limits.
- Terminal observation can drift from Alacritty. The observer task compares production observations against real `Term` state, including both grids and negative boundary cases.
- Compatibility defaults can decode old messages while changing new bytes. Byte-for-byte legacy/current IPC fixtures are mandatory.
- Selective reuse from the preserved monolithic commit can carry hidden assumptions. Every reused hunk must be attributed to one child invariant and independently reviewed.
- Final assembly can expose interactions absent from child cases. The combined scenario and versioned manifest block epic closure and downstream integration.

## Unresolved Questions

None. The clarify gate resolved accounting, shared-seam, overflow, compatibility, evidence, sequencing, and performance decisions. Implementation-level choices remain bounded by each child acceptance contract.

## Constitution Check

1. **Clear Boundaries and Typed Failure — pass.** Eight narrow tasks own typed seams, reservations, tickets, lifecycle, mutations, convergence, and assembly.
2. **Session-Safe, Consistent UX — pass.** Server/client convergence and long-lived viewerless state are explicit gates.
3. **Explicit, Risk-Based Verification — pass.** Every story maps to a named production-path Docker command; mock-only evidence cannot close a child.
4. **Performance Budgets and Measurement — pass.** Live latency is explicitly inapplicable before `scribe-aq1.10`; work, allocation, queue, and deadline ceilings are measured now.
5. **Default-Safe Trust Boundaries — pass.** PTY bytes remain hostile, indirect transports remain excluded, and all growth is bounded before retention.
6. **Local-First Data Locality — pass.** Validation is offline and evidence is payload-free.
7. **Compatible, Documented, Operationally Safe Change — pass.** Primary protocol sources, stable wire fixtures, isolated worktrees, Docker-only runtime tests, and `lat.md` updates are required.

No constitution violation remains. The only tension is conservative accounting versus compatibility with large images; exact Docker evidence makes that tradeoff reviewable rather than implicit.

## Recommendation

**GO** — The monolithic failure mode has been replaced by a shared-seam-first DAG with eight independently testable P1 tasks, explicit protocol semantics, deterministic accounting, preserved wire compatibility, no unresolved backlog, and an objective final assembly manifest. Materialize the beads and run `$implement-ready` in dependency order.
