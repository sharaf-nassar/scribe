# Plan: authoritative-image-state-hardening

This plan replaces one monolithic implementation with a dependency-wired sequence of independently reviewable production invariants.

## Architecture Approach

Create one server-owned `SessionTerminal` seam first, with typed inputs, ordered raw/image outputs, and no live fanout. Every later task extends this seam rather than creating an alternate probe-only engine. The seam owns generation, sequence, active screen, framing, definitions, placements, pending transfer state, and handles to shared accounting and scheduling policy.

The seam preserves protocol chronology rather than normalizing Kitty and Sixel into identical lifecycle rules. Kitty chunks stay invisible until final validation and use the cursor observed at the final chunk; query replies remain FIFO. Kitty reset, alternate-screen creation, and ED2 clear visible graphics while other text erases do not. Sixel effects follow the frozen xterm DECSDM, margin-scrolling, and cursor modes.

Build deterministic storage accounting next. Quotas measure requested live storage and simultaneous replacement peaks, not process RSS. Each allocation path reserves before allocation, reconciles observed capacity before retention, and transfers or releases RAII ownership exactly once. Session and process budgets are independent; decode tickets bind both budgets and an issuing session.

After accounting, incomplete-transfer lifecycle and the mandatory decode scheduler can land without inventing ownership rules. Alacritty observer parity can proceed from the shared seam in parallel. Transactional mutation then builds on accounting, while convergence and overflow build on observer and mutation semantics. Final assembly runs only after all invariant tasks close and produces one versioned, payload-free evidence manifest.

Alternatives rejected:

- Cherry-picking `bb81878c7e69a13fb516365b3e559476500331f0` wholesale preserves unreviewed cross-invariant assumptions.
- Isolated per-task engines make green tests incomparable and recreate ordering drift.
- Allocator-observed RSS quotas are not portable; Rust permits capacity beyond an exact reservation request.
- Reset-on-counter-overflow complicates publication ordering; pre-mutation rejection preserves the last committed state.

## Affected Components

- `crates/scribe-pty/src/graphics_framing.rs` — reserve-before-copy event transfer and terminal-context boundaries.
- `crates/scribe-common/src/kitty_decode.rs` — exact chunk replacement ownership and decoded-buffer growth.
- `crates/scribe-common/src/terminal_images.rs` — existing typed limits, effects, definitions, placements, and compatibility defaults.
- `crates/scribe-server/src/terminal_image_state.rs` — shared `SessionTerminal` seam and invariant implementations, added incrementally rather than imported wholesale.
- `crates/scribe-server/src/lib.rs` — internal module exposure only after the seam compiles.
- `crates/scribe-client/src/terminal_image_scene.rs` — canonical client convergence and half-open placement effects.
- `crates/scribe-client/src/terminal.rs` — shared terminal-effect application where raw-derived and image-derived origins differ.
- `crates/scribe-test/src/terminal_image_server_state.rs` — production-path probes partitioned by invariant.
- `tests/e2e/terminal-image-*.sh` and `tests/e2e/fixtures/terminal-images/` — named functional gates and versioned evidence.
- `lat.md/{terminal-images,server,pty,test}.md` and `specs/020-terminal-images/` — behavior, ownership, protocol-source, and test-contract alignment.

Live `ipc_server` fanout and PTY reply write-back are not connected here; `scribe-aq1.10` owns that integration after this epic closes.

## Data Model

- `SessionTerminal` — authoritative ordered image state with one generation and output sequence cursor.
- `ImageStorageBudget` — deterministic session/process requested-storage accounting with current and peak counters.
- Per-image checked limits — dimensions, pixels, decoded bytes, and work are rejected before session/process reservation; they do not become a third mutable ownership ledger.
- `ImageReservation` — non-forgeable RAII token that can be transferred into retained state or released.
- `DecodeTicket` and `DecodePermit` — issuer-, session-, target-, generation-, and budget-bound scheduling capabilities.
- `PendingKittyTransfer` / `PendingSixel` — payload-free control metadata plus explicitly charged retained bytes.
- `TerminalGridObservation` — Alacritty-derived cursor, wrap, margins, screen, size, and image-relevant effects.
- Canonical definitions and placements — generation-tagged state keyed by protocol identity and screen; bounds are half-open.
- Evidence manifest — versioned JSON containing case ids, limits, counters, typed outcomes, and convergence hashes, never image payloads.

No persistent database schema or migration is introduced. Existing MessagePack bytes remain stable; any unavoidable field uses legacy-preserving defaults and is omitted at the default value.

## API / Interface Changes

- Add an internal constructor for the shared `SessionTerminal` seam with immutable `ImageLimits` and shared process policy.
- Framing completion transfers a charged event token instead of allocating an unreserved payload copy.
- Decode entry points require a scheduler-issued ticket/permit and cannot accept a foreign session or budget.
- Ordered processing returns typed raw spans, replies, definitions, placements, removals, and grid effects under one commit boundary.
- Raw-derived grid effects update image state without replaying text; image-derived effects update terminal and image state once.
- Overflow returns a typed rejection before mutation; it does not reuse an exhausted sequence or emit a partial reset.
- Internal inspection exposes payload-free current/peak counters and canonical hashes to `scribe-test`.

No public CLI/UI endpoint is added. No terminal-image wire-version bump is permitted by this epic.

## Testing Strategy

Every child adds one production-path functional Docker gate and payload-free evidence. Static helper tests may supplement but cannot replace these gates.

- Shared seam: `just e2e-func terminal-image-state-seam.sh` proves ordered raw/image boundaries, stable legacy serialization, and no test-only alternate engine.
- Accounting: `just e2e-func terminal-image-accounting.sh` proves pre-reservation, old-plus-new peaks, exact/max-plus-one outcomes, rollback, and zero release across Kitty and Sixel.
- Scheduling: `just e2e-func terminal-image-scheduler.sh` proves mandatory issuer-bound admission, FIFO/no barging, cancellation wake, bounded metadata, and independent sessions.
- Transfer lifecycle: `just e2e-func terminal-image-transfer-lifecycle.sh` proves partial APC/DCS/chunks retire on EOF/reset/close/cancel with typed replies and zero ownership.
- Observer parity: `just e2e-func terminal-image-observer-parity.sh` compares the production observer with real Alacritty state for wrap, cursor save/restore, margins, scrolling, ED2, 1049, both-grid resize, and split reads.
- Transactional mutations: `just e2e-func terminal-image-mutations.sh` proves atomic define/place, exact deletes, eviction order, screen scope, rollback, and protocol-specific erases: ED2/reset/1049 lifecycle, Kitty immunity to other text erases, and exact Sixel area/scroll clipping.
- Convergence/overflow: `just e2e-func terminal-image-convergence.sh` applies production updates to the client scene and proves generation, sequence, reset, resize, and overflow convergence.
- Assembly: `just e2e-func terminal-image-server-state.sh` runs one cross-invariant multi-session scenario and publishes `test-output/terminal-images/server-state-manifest.json` mapping every spec criterion to passing evidence.

Each task also runs relevant companion terminal-image suites, `pre-commit run --all-files`, and `lat check`. Runtime commands execute only through the Docker harness.

## Risks

- Shared files create integration contention. Mitigation: dependencies serialize overlapping state-engine work; only observer research may proceed beside accounting when file ownership is disjoint.
- Accounting can be safe but overly conservative. Mitigation: record requested and observed capacity, exact rejection category, and old/new ownership at every boundary.
- A second terminal parser can drift from Alacritty. Mitigation: derive observations from the production Alacritty path wherever possible and compare every effect to real `Term` state.
- Compatibility defaults can preserve decoding while changing encoded bytes. Mitigation: freeze legacy/current MessagePack fixtures and require byte equality for default values.
- Preserved code can smuggle monolithic assumptions into a child. Mitigation: every selectively re-derived hunk is named in review and must pass that child's independent evidence.
- Final assembly can reveal cross-invariant defects. Mitigation: the parent remains blocked until the combined scenario and versioned manifest pass; downstream fanout cannot start early.
- A child can regress main after passing its narrow case. Mitigation: each integration reruns relevant existing terminal-image suites, remains one revertable commit, and leaves the epic blocked; rollback is `git revert` of that child before downstream work begins.

Constitution alignment:

- Principle 1: typed seams, tickets, reservations, failures, and narrow child ownership.
- Principle 2: server-owned state and client convergence preserve long-lived session semantics.
- Principle 3: each story has an independent production-path Docker gate.
- Principle 4: numeric latency is explicitly inapplicable before live integration; work, allocation, queue, and deadline ceilings remain measured.
- Principle 5: all PTY bytes are hostile and indirect resource transports remain excluded.
- Principle 6: implementation and validation remain local/offline with payload-free evidence.
- Principle 7: external protocol sources, compatibility fixtures, worktrees, Docker isolation, and `lat.md` updates are mandatory.

## Sequencing

All work items are P1 children of `scribe-aq1.9`; dependency edges, not title prefixes, carry order.

| Work item | Depends on | Acceptance summary |
| --- | --- | --- |
| Establish shared terminal image state seam | Existing closed parser/decoder/IPC prerequisites | Internal ordered seam and `terminal-image-state-seam.sh` pass without live fanout. |
| Enforce exact image storage accounting | Shared seam | `terminal-image-accounting.sh` proves deterministic requested-storage peaks, rollback, and release. |
| Enforce mandatory decode scheduling | Storage accounting | `terminal-image-scheduler.sh` proves issuer-bound FIFO admission and cancellation. |
| Retire incomplete graphics transfers | Storage accounting; decode scheduling | `terminal-image-transfer-lifecycle.sh` proves EOF/reset/close/cancel retirement and replies. |
| Match Alacritty image lifecycle observations | Shared seam | `terminal-image-observer-parity.sh` proves cursor, wrap, margins, screen, erase, scroll, and both-grid resize parity. |
| Commit image mutations transactionally | Storage accounting; shared seam | `terminal-image-mutations.sh` proves atomic definitions, placements, deletes, protocol-specific erases, eviction, screen scope, and rollback. |
| Preserve client convergence and counter safety | Observer parity; transactional mutations; transfer lifecycle | `terminal-image-convergence.sh` proves client/server state and reject-before-mutation overflow. |
| Assemble and certify authoritative image state | Scheduling; transfer lifecycle; observer parity; transactional mutations; convergence | All child gates plus combined `terminal-image-server-state.sh` and versioned manifest pass. |

## Backlog Refinement

There are no P4 Backlog Inputs in the hierarchy-plus-provenance closure. No source issue requires refinement, supersession, or non-goal disposition. The existing monolithic task was converted in place to epic `scribe-aq1.9`; its preserved notes remain historical evidence rather than implementation input.

Ready P4 must remain zero throughout materialization and closeout.

## Target Epic

`scribe-aq1.9` — Authoritative server image state.

This existing nested epic keeps `scribe-aq1.10` and `scribe-aq1.14` blocked until every child and the final assembly gate close.

## Alignment fixes applied

- Spec↔plan must-fix: corrected resize semantics to match Alacritty resizing both active and inactive grids.
- Spec↔plan must-fix: separated Kitty text-erase behavior from Sixel/xterm image-grid effects and added final-chunk/FIFO chronology.
- Plan-quality must-fix: clarified per-image limits versus session/process ownership so no third mutable ledger is invented.
- Plan-quality should-fix: added child-commit rollback and regression-suite expectations.
- Plan-quality should-fix: tightened transactional-mutation acceptance around protocol-specific erase and screen scope.
