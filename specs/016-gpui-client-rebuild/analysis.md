# Analysis: gpui-client-rebuild

## Coverage Table

| User story / requirement | Covered by (plan section) | Status |
|---|---|---|
| G1 Full parity (46/57 msgs, subsystems) | parity-inventory.md artifact (Phase 0) + phases B–F + launch gate | full |
| G2 Modern UI (emoji, decorations, shadows, motion, titlebar, hover states) | Terminal core, Phase C animations/titlebar, Phase D overlays; emoji visual-E2E item; opacity spike-gated | full (opacity contingent on spike, sanctioned by Q7) |
| G3 Maximal Zed reuse | Architecture Approach (crib list incl. alacritty.rs glue, mappings/colors) | full |
| G4 Aggressive deletion | Phase H deletions + Phase I sweep (udeps, greps) | full |
| G5 Testing continuity | Testing Strategy (scribe-test frozen, visual E2E, gpui::test) | full |
| G6 GPL-3.0 relicense | Phase 0 step-0 bead + attribution | full |
| G7 De-risked sequencing | Phase A spikes + criteria-reconciliation bead | full |
| US1 Terminal parity (input/mouse/selection/search/URL/IME/sync-frames) | Phase B + golden harness + manual IME gate item | full |
| US2 Session lifecycle (replay, snapshot, restore, upgrade, abort path) | Phase B/E + lifecycle E2E + Vulkan stash/probe packaging test | full |
| US3 Visual polish | Phase C/D + perf gate (60fps) + spike-gated items | full (spikes may rewrite — by design) |
| US4 Differentiators (AI, command marks, workspaces, remote/LAN, status bar) | Phases C–E; 015-gated beads; provisional inventory rows + reconcile bead | full |
| US5 Clean tree (deletions, ported logic, lat.md, license) | Phases H/I + Affected Components | full |
| US6 Testing continuity (harnesses) | Testing Strategy + Phase G | full |
| Q1 Vulkan/updater safety | preinst stash + postinst probe/restore + deb Depends + packaging test | full |
| Q2 Parity oracle | Phase 0 inventory + golden fixtures + checklist verification column | full |
| Q3 Perf budget numbers | Testing Strategy perf gate + Phase 0 baselines | full |
| Q4 015 lands first | Phase E dependency edges + reconcile bead | full |
| Q5 Phasing (relicense step-0, cosmetics trail, deletion post-cutover) | Sequencing phases 0/H/I | full |
| Q6 Settings GPUI rebuild + macOS Linux-first | Phase F + macOS disable bead + follow-on stub | full |
| Q7 Spike gate with rewrite authority | Phase A + criteria-reconciliation bead | full |
| Non-Goals (protocol freeze, no images, no compat shims) | Affected Components freeze rows; CI freeze proof | full |
| Review observations (a11y/telemetry/i18n scoping, degraded states, dual-writer, geometry compat, docs ownership) | Data Model out-of-scope lines; failure-path tests; dual-writer rule; geometry normalization; Phase I docs bead | full |

No requirement mapped to "none". Two criteria are deliberately contingent
(opacity, ligatures) under Q7's spike-rewrite authority.

## Remaining Risks

1. **GPUI capability spikes may fail** — mitigated: run first, named
   fallbacks per spike, authority to rewrite criteria (Q7). Residual: if
   the *scaffold* spike itself fails (GPUI fundamentally unfit), the
   feature stops at Phase A with only Phase 0 sunk cost (relicense is the
   one irreversible artifact — see 5).
2. **Perf regression vs bespoke pipeline** — mitigated: launch-blocking
   perf gate with recorded baselines; Zed as existence proof. Residual:
   lavapipe (software Vulkan) users get a slower floor than the old
   GL-free pipeline; accepted per Q1.
3. **Parity tail length** (45k SLoC, remote/LAN/restore/notifications) —
   mitigated: countable inventory, parallelizable phases, correctness-only
   launch gate. Residual: calendar time; no scope cut identified.
4. **015 timing** — only Phase E remote/LAN beads + inventory
   reconciliation depend on it; explicit re-decision point if 015 stalls.
5. **GPL flip is irreversible in practice** once outside contributions
   arrive — front-loaded deliberately as step-0 per human decision;
   comms/attribution handled in the relicense bead.
6. **Pin staleness** — gpui pinned to v1.12.0; vendoring into
   `third_party/` is the contingency; pin moves are deliberate beads, never
   drive-by.

## Unresolved Questions

- OQ9 (zoom semantics), OQ10 (final removed-keys list), OQ12 (opacity) —
  all resolve inside Phase A/B beads (spike results + inventory
  removed-keys table); none block bead creation.
- Exact deb Depends package names per distro release (resolved in the
  packaging bead against the CI image).

## Constitution Check

No constitution.md — skipped. (Flagged at clarify gate; human chose to
proceed without one.)

## Recommendation

**GO.** Every goal, user story, clarification answer, and review
observation traces to a plan section with a verification method; the three
alignment must-fixes (postinst rollback mechanics, spike dependency edges,
settings packaging orphan) are applied; risks carry named mitigations and
the two irreversible artifacts (relicense, old-client deletion) are
correctly sequenced at opposite ends of the DAG with the deletion gated on
captured baselines and a green launch gate. The plan is ready to convert
into dependency-wired task beads.
