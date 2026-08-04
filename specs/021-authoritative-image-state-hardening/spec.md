# Spec: authoritative-image-state-hardening

This specification decomposes authoritative terminal-image state into independently reviewable invariants with production-path Docker evidence.

## Problem Statement

`scribe-aq1.9` combines hostile-stream framing, allocation accounting, decode scheduling, terminal observation, image mutation, and client synchronization in one server engine. Two monolithic `$implement-ready` runs passed their test suites but failed production-path review because safety and convergence defects remained at invariant boundaries.

The work must be decomposed into small, dependency-wired tasks whose acceptance tests exercise user-reachable production paths. Commit `bb81878c7e69a13fb516365b3e559476500331f0` may be inspected or selectively used as source material, but must never be integrated wholesale.

## Goals

- Reserve memory before allocation and account for exact Kitty chunk replacement peaks.
- Release incomplete transfer state on EOF, reset, and close without publishing partial images.
- Match Alacritty cursor, wrap, screen, erase, scroll, and resize observation semantics.
- Keep server and client image generations convergent, including sequence overflow.
- Enforce image, session, and process quotas through one mandatory decode scheduler.
- Make define, place, delete, erase, eviction, and placement mutations transactional.
- Validate each invariant through an independent Docker test using production ingestion and state paths.
- Establish one shared production engine seam before invariant implementations branch, so every child extends the same ordering and ownership path.
- Assemble the verified invariants into one authoritative server engine with cross-invariant Docker evidence.
- Publish a versioned payload-free evidence manifest that maps every child invariant and one combined scenario to passing Docker results.
- Preserve typed boundaries and specific failures so protocol errors remain distinguishable from internal defects.

## Non-Goals

- Live PTY fanout or protocol reply write-back, which remains downstream work under `scribe-aq1.10`.
- Wholesale integration, rebasing, or squashing of preserved commit `bb81878c7e69a13fb516365b3e559476500331f0`.
- Host execution of `scribe-server`, `scribe-client`, `scribe-test`, or another Scribe runtime.
- New image protocols, renderer behavior, GPU behavior, or visual fidelity changes.
- A terminal-image IPC version bump; hardening must preserve existing wire bytes through internal-only state or defaulted, skipped fields.
- Native macOS Metal validation, except where a downstream GPUI or release gate separately requires it.
- Replacing Alacritty terminal semantics with an image-specific cursor or screen model.
- Network services or online dependencies for implementation or validation.

## Backlog Inputs

None. Scope epic `scribe-aq1.9` has no child backlog provenance and no open P4 inputs.

## Target Epic

`scribe-aq1.9` is the target epic. It remains a nested epic so existing downstream blockers continue to depend on completion of the assembled authoritative-state engine rather than on any single hardening task.

## User Stories

**US1 — Bound hostile allocations before ownership**

As a server operator, I need image bytes charged before allocation so a hostile PTY cannot exceed configured memory while the server is still parsing or replacing a transfer.

Acceptance criteria:

- Production Kitty ingestion reserves capacity before candidate, chunk, frame, decode, canonical-image, or replacement storage is allocated.
- Replacing a Kitty chunk charges the simultaneous old-and-new peak before releasing old ownership.
- Rejected reservations do not grow retained state, mutate an image definition, create a placement, or consume a generation.
- Exact-boundary and one-byte-over-boundary cases produce typed, deterministic outcomes.
- A functional Docker case drives the real ingestion path and reports process and session ownership before, during, and after replacement.

**US2 — Terminate incomplete transfers safely**

As a session owner, I need incomplete image transfers discarded on stream termination so stale bytes and reservations cannot survive a disconnected producer.

Acceptance criteria:

- EOF, parser reset, session reset, and close clear active Kitty and Sixel framing state.
- Every path releases retained bytes, decode admissions, scheduler tickets, and pending mutation state exactly once.
- An incomplete transfer never publishes an image, placement, generation, or client operation.
- Repeated reset or close is idempotent and cannot underflow accounting.
- A functional Docker case covers partial headers, partial payloads, compressed payloads, split terminators, EOF, reset, and close through production framing.

**US3 — Observe terminal chronology with Alacritty parity**

As a terminal user, I need images anchored to the terminal state that existed when their sequence began so text, wrapping, scrolling, and screen changes remain chronologically correct.

Acceptance criteria:

- Raw terminal bytes and normalized grid effects are applied before later image events from the same PTY read observe cursor or screen state.
- Split DCS and APC sequences retain their begin-time terminal context.
- Pending wrap, cursor movement, line wrapping, scroll regions, ED2, primary/alternate screen changes, and resize use Alacritty-compatible semantics.
- A terminal resize clips placements on both active and inactive screens according to each Alacritty grid; later screen switches cannot resurrect out-of-bounds placements.
- A functional Docker case compares image observations with the production terminal observer across same-read text/image sequences, split sequences, screen swaps, erasure, scrolling, and resize.

**US4 — Preserve server-client convergence**

As a connected client, I need every published image operation to be applicable in order so server and client state cannot diverge after lifecycle changes or counter exhaustion.

Acceptance criteria:

- Image definitions, placements, removals, screen ownership, and generations converge after define, replace, delete, erase, scroll, reset, and resize.
- Sequence or generation exhaustion is preflighted before server mutation and publication.
- Overflow either yields a typed rejection with no state change or a complete resynchronization operation; it never publishes a partial lifecycle.
- Failed publication releases provisional ownership and leaves both sides at the prior committed generation.
- A functional Docker case applies production operations to the client state model and compares canonical server/client state, including maximum-counter boundaries.

**US5 — Enforce fair mandatory scheduling**

As a multi-session server operator, I need all decode work admitted through shared quotas so one session or alternate caller cannot bypass process limits or starve queued work.

Acceptance criteria:

- Decode admission uses immutable process-owned image, byte, and concurrency ceilings.
- All production decode entry points require an admission object that cannot be constructed outside the scheduler.
- Image, session, and process reservations remain distinct and reconcile after success, failure, cancellation, reset, and close.
- FIFO waiters cannot barge; cancellation wakes eligible successors promptly.
- Abandoned or weak tickets are pruned and remain bounded.
- Independent tickets cancel only work for their own transfer or target image.
- A functional Docker case uses multiple sessions to verify mixed ceilings, FIFO ordering, cancellation wake-up, bypass prevention, and final zero ownership.

**US6 — Commit image mutations transactionally**

As a terminal user, I need image commands applied atomically so quota failures, malformed operands, or eviction cannot leave half-applied definitions and placements.

Acceptance criteria:

- Compound define-and-place commits both mutations or neither.
- Placement identity and command matching preserve all protocol-significant identifiers.
- Omitted Kitty delete operands remain distinguishable from explicit zero values and cannot become unintended wildcards.
- Delete, erase, scroll, screen reset, and eviction mutate only their specified screen and target scope.
- Eviction publishes authoritative client removals before later dependent operations.
- Protocol failures map to exact supported Kitty replies, including unsupported operation, invalid input, size limit, and capacity exhaustion.
- A functional Docker case exercises success and every rollback point, then compares accounting, server state, replies, and client state.

**US7 — Assemble the authoritative engine**

As a maintainer, I need the independently verified invariants assembled behind one production interface so downstream PTY fanout can depend on a stable server-owned state engine.

Acceptance criteria:

- Dependency wiring prevents assembly until allocation, lifecycle, observer, convergence, scheduling, and transaction tasks are complete.
- The assembled path uses the verified production seams rather than parallel test-only implementations.
- Existing Kitty and Sixel framing, decoder, IPC, contract, and client-scene functional Docker suites remain green.
- A final functional Docker scenario combines multiple sessions, partial transfers, replacement, screen changes, deletion, quota pressure, cancellation, eviction, and counter boundaries.
- Evidence records bounded counters, ordered lifecycle operations, typed failures, and canonical convergence without embedding image payloads.
- `lat.md/` documents ownership, ordering, failure, and verification boundaries, and `lat check` passes.

## Constraints

- Treat PTY bytes as hostile input. No trust may be inferred from escape-sequence syntax, declared sizes, compression metadata, or prior chunks.
- Preserve protocol-specific lifecycle semantics: Kitty displays only after the final validated chunk, uses the final-chunk cursor, clears visible images on reset/1049 alternate creation/ED2, and leaves other text erases independent of Kitty graphics; Sixel follows the frozen xterm DECSDM, scrolling, margin, and cursor rules.
- Define quotas over deterministic requested live storage rather than process RSS: reserve the simultaneous old-plus-requested-new peak before allocation, use fallible exact reservation, then reconcile allocator-returned capacity before retaining the buffer. Evidence must record requested and observed capacity without claiming byte-exact RSS.
- Use typed structures at framing, scheduling, accounting, terminal-observation, mutation, and client-publication boundaries.
- Catch only expected protocol or resource failures; unexpected internal defects must surface.
- Allocation, ownership, queue order, mutation order, and convergence must be measurable in Docker evidence.
- Sequence or generation exhaustion must reject before mutation and preserve the prior committed state; it must not synthesize a partial reset or stale-sequence notification.
- Preserve existing serialized bytes. Any unavoidable field must deserialize from a default and be omitted when at that default, with legacy/current fixture coverage.
- Each child task must have a narrow invariant, explicit dependencies, a bounded review surface, and its own user-reachable functional Docker acceptance.
- Mock-only, constructor-only, or isolated helper tests cannot satisfy a task's primary acceptance gate.
- Runtime validation must use repository `just` Docker recipes. Never invoke Scribe runtimes on the host or restart the live server.
- Implementation and validation must remain local and offline.
- Preserve worktree isolation and Beads ownership rules during `$implement-ready`.
- Update relevant `lat.md/` sections for behavior, architecture, and verification changes; run `lat check` for every task.
- Use Rust 1.95.0 or newer and existing repository protocol and IPC types where they express required semantics.
- Preserve downstream dependency meaning: `scribe-aq1.10` remains blocked until final engine assembly, not merely until foundational hardening lands.
- Use this dependency DAG: accounting foundation; scheduler and incomplete-transfer framing may then proceed; observer parity and transactional mutations follow their shared prerequisites; convergence/overflow follows observer and mutation semantics; assembly and the versioned manifest are last.
- Numeric latency goals are inapplicable before `scribe-aq1.10` connects the live PTY path. Every child must still enforce numeric security ceilings and record work/allocation measurements through a named Docker command.
- The authoritative protocol references are the [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) and [xterm control sequences](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html). Terminal-state parity follows [Alacritty's `Term`](https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/src/term/mod.rs); allocation and compatibility assumptions follow the Rust [`Vec`](https://doc.rust-lang.org/std/vec/struct.Vec.html#method.try_reserve_exact) and [Serde field attribute](https://serde.rs/field-attrs.html) documentation.

## Open Questions

No human-decision blockers remain. Planning must map every selectively re-derived hunk from `bb81878c7e69a13fb516365b3e559476500331f0` to one child invariant and require independent production-path evidence before that hunk can land.

## Clarifications

**Q1: What byte-ownership metric governs each cap?**

A: Use a documented conservative live-storage bound. Reserve old retained storage plus the requested replacement capacity before allocation, then reconcile observed capacity before retention. Rust permits allocator over-allocation even for `try_reserve_exact`, so the contract is deterministic storage quota—not byte-exact RSS.

**Q2: How should child tasks share production code?**

A: Establish one shared production engine seam first. Children must extend that seam instead of creating parallel test-only engines.

**Q3: What happens on sequence or generation exhaustion?**

A: Reject before mutation and preserve prior committed server/client state.

**Q4: What wire-compatibility rule applies?**

A: Preserve existing wire bytes. New fields are allowed only when missing values default to legacy behavior and the default value is omitted from serialization; otherwise keep the change internal.

**Q5: What closes `scribe-aq1.9`?**

A: Every child Docker case, one combined cross-invariant Docker scenario, and a versioned payload-free evidence manifest must pass.

**Q6: What dependency order should the plan use?**

A: Accounting first; scheduler and framing lifecycle next; observer parity and transactional mutations next; convergence/overflow after those semantics; final assembly last.

**Q7: What performance requirement applies before live integration?**

A: Numeric latency goals are inapplicable until `scribe-aq1.10` connects the live path. Hard security ceilings and Docker work/allocation measurements remain mandatory now.

## Spec Review

### Critical Questions (answer before planning)

1. What exact byte-ownership metric governs each cap: logical length, `Vec` capacity, allocator-observed bytes, or a documented conservative replacement bound? — Without one rule, exact/max-plus-one tests and reserve-before-allocation behavior will disagree; flagged by: requirements, ambiguity, feasibility.
2. Must every child build through one shared production engine seam from the first task, or may children land isolated production components that assembly connects later? — Parallel test-only seams recreated the monolithic review failures and would violate clear ownership boundaries; flagged by: feasibility, scope, requirements.
3. What is the one sequence/generation exhaustion policy: reject before mutation with prior state intact, or publish a complete reset/resynchronization boundary? — Both appear in the draft, but clients and accounting need one deterministic contract; flagged by: ambiguity, gaps, stakeholders.
4. Which IPC/wire records may change during hardening, and what backward-compatibility rule applies to new origin, failure, or accounting metadata? — Existing clients and fixtures require explicit defaults or versioning before task boundaries can be safe; flagged by: gaps, stakeholders, requirements.
5. What exact final assembly evidence closes `scribe-aq1.9`: all child Docker cases plus one cross-invariant scenario, or a versioned evidence manifest consumed by `scribe-aq1.10`? — The downstream unblock decision must be objective and auditable; flagged by: requirements, scope, stakeholders.
6. Which dependency order is mandatory among accounting, incomplete-transfer lifecycle, observer parity, convergence, scheduling, and transactional mutations? — Shared types and ownership tokens can otherwise force broad rebases or duplicate implementations; flagged by: feasibility, scope, ambiguity.
7. What measurable hot-path performance constraint must the engine preserve, or should performance be explicitly marked inapplicable until downstream integration? — Constitution principle 4 requires a named measurement or an explicit inapplicability decision; flagged by: requirements, gaps, stakeholders.

### Non-Blocking Observations

- There are no P4 backlog inputs or epic-selection ambiguity; `scribe-aq1.9` is the fixed target.
- Authentication, accessibility, i18n, and admin UI are not relevant to this server-state engine; typed payload-free diagnostics remain relevant downstream.
- Live PTY fanout, application reply write-back, GPU rendering, and native Metal remain explicit downstream or out-of-scope work.
- The preserved monolithic commit is useful as an adversarial review corpus, but child implementations need independent ownership and evidence.
