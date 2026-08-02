# Analysis: ai-tab-shell-env

Cross-artifact analysis of `specs/018-ai-tab-shell-env/spec.md` (clarified,
Q1-Q7 binding) against `specs/018-ai-tab-shell-env/plan.md` (post-alignment,
sequencing items 1, 2a, 2b, 3, 4a, 4b, 5, 6, 7a, 7b, 8) and
`constitution.md` (7 principles). This file is a speckit gate artifact
consumed at bead-creation time.

## Coverage Table

| User story / requirement | Covered by (plan section) | Status |
|---|---|---|
| US-1: full login environment in AI tabs (login+interactive `$SHELL`, profile-exported PATH/env reach the AI binary, server-side host resolution, per-shell startup order) | Architecture Approach (server-owned argv, passwd-first resolution); items 2b (argv/env builders, typed passwd → `$SHELL` → `sh` chain), 8 (US-1 sentinel + `readlink /proc/<pid>/exe` shim check) | full |
| US-2: shell-integration parity (preamble source via `SCRIBE_INTEGRATION_SCRIPT`, `SCRIBE_AI_TAB=1` gates, `--rcfile -l` conflict removed, delta sourced-once-then-deleted, no env leaks, moot-features documented, plain tabs unchanged) | Items 2a (script AI-mode gates), 2b (preamble build, `--rcfile` removal, `ENV=` skip, leak restores), 1 (launch_id/env-envelope identity preserved), 7b (moot-feature list in lat.md), 8 (staged-file exists/deleted, sourced-once sentinel, ZDOTDIR/XDG_DATA_DIRS leak checks) | full |
| US-3: `ai_tab_cwd` live (`pane` default, `project_root` chain, new `home`, typed fallback, fresh-creates-only, live settings reload, no dead UI options) | Items 4a (`Home` variant + settings UI incl. apply.rs arm), 4b (client cwd resolution, `TerminalView::reload_config` live read, end-to-end fallback enumeration), 8 (`readlink /proc/<pid>/cwd` per variant) | full |
| US-4: zsh/fish/nushell/unknown correctness (zsh ZDOTDIR chain, fish vendor conf.d + PATH, nushell documented no-integration + no staging, unknown-shell debug-log path, valid per-shell `-lic` syntax) | Architecture Approach per-shell builders (exact zsh/fish preamble syntax, nushell `nu -l -i -c` no-integration, PowerShell folded into Unknown, Unknown arm preserved); items 2b, 8 (US-4 matrix) | full |
| Goal: real login+interactive launch before the AI binary runs | Architecture Approach (Q3 invocation); items 2b, 8 | full |
| Goal: structured `ai_launch`, server-owned argv, dual-write compat, `REMOTE_PROTOCOL_VERSION` 3→4 | Architecture Approach + API/Interface Changes (dual-write matrix, v4 gates); items 1, 3 | full |
| Goal: integration attaches to AI tabs on bash/zsh/fish; nushell documented limitation | Items 2a, 2b; Architecture Approach per-shell table | full |
| Goal: restore delta applied post-login, delta wins (spec-006 FR-008) on bash/zsh/fish | Architecture Approach (Q4 mechanism split, sourced exactly once, deleted); items 2a, 2b, 7b (FR-008 contract comment + spec-006 amendment note), 8 (cold-restart marker check) | full |
| Goal: `ai_tab_cwd` live with `home` variant, client-resolved, server-guarded | Items 4a, 4b | full |
| Goal: ~1s perf budget with named `--ai-tab-only` command | Item 6 (owns measurement; stub-binary, no fixed sleeps), item 8 (verifies run + budget) | full |
| Goal: lat.md sync incl. correcting server.md:572 and client.md:1424 | Item 7b; Affected Components lat.md/server.md, client.md, common.md, settings.md bullets | full |
| Q1: server-owned argv via structured launch, dual-write, v4 bump, `launch_binding_for` rewire, lockstep argv sites | Items 1 (protocol + plumbing), 3 (client structured launch, binding rewire, replay lockstep), 2b (`ai_provider_hint` from structured field) | full |
| Q2: AI tabs only; plain tabs unchanged | Architecture Approach ("plain tabs untouched"), Non-Goals honored; follow-up unification bead in item 7a. Noted documented edge: `--rcfile` removal changes hand-typed custom `["bash"]` launches (Risks; spec-mandated) | full |
| Q3 (a-f): real login + source-preamble, AI-mode gate, `-i` kept, env-var path crossing, baseline decision (planning), bashrc consequence documented, dead `--rcfile` removed | Architecture Approach (all six amendments; Q3d decided: DROP baseline for AI tabs with rationale); items 2a, 2b | full |
| Q4: enforce FR-008 for AI tabs; track plain-tab zsh/fish separately | Architecture Approach (Q4 paragraph); items 2a, 2b, 7a (later fixed by scribe-ebz), 7b (server/spec-006 correction) | full |
| Q5: `home` variant, no migration, client-resolved cwd, server guard, fresh-creates-only, own bead | Items 4a, 4b (ships as own beads within epic); Data Model (`AiTabCwd::Home`, serde default stays `Pane`) | full |
| Q6: soft ~1s budget + named `--ai-tab-only` command, no escape hatch | Item 6; Testing Strategy perf entry; Risks (latency variance: measured, not mitigated) | full |
| Q7: full env + delta into AI CLIs recorded as deliberate; `SHELL` (+ ENV/ZDOTDIR/XDG_DATA_DIRS/SCRIBE_* vars) to EXCLUSION_SET; verify via `scribe-dev` flavor | Architecture Approach constitution-check paragraph (decision recorded); item 5 (EXCLUSION_SET + tests + plain-tab-capture sentence); Testing Strategy (dev-flavor commands, never `just restart-server`) | full |
| lat.md doc-sync obligations (server.md, client.md, common.md, settings.md, spec-006 amendment, FR-008 code comment) | Item 7b (blocked by 2b/3/4b so it documents final behavior); Affected Components doc bullets | full |

Coverage headline: **19 full / 0 partial / 0 none.**

## Backlog Disposition

None — spec Backlog Inputs is "None"; no P4 sources.

## Target Epic

New epic (`ai-tab-shell-env`) to be created at bead-creation time. No
existing epic covers this work (spec and plan agree).

## Remaining Risks

Honest consolidation of the plan's Risks section plus analysis findings:

1. **Old-live-server window / dual-write retirement deferred.** The live
   server cannot be restarted; the new pipeline activates only after an
   approved upgrade. Until the retirement bead (item 7a) lands, TWO argv
   builders and the token-sniffing fallback coexist indefinitely — a
   drift surface if either is edited alone. Mitigated by the lockstep
   requirement (item 3) and the explicit retirement trigger, but the
   window's length is user-controlled, not plan-controlled.
2. **Login-profile latency variance.** nvm/conda/mise chains can blow the
   ~1s budget on other machines; Q6 forbids an escape hatch, so a miss is
   a finding to surface, not a mitigation path. Budget is scoped to this
   machine's profile only.
3. **Remote v3↔v4 refusal.** After the bump, mixed-version remote pairs
   refuse loudly with `IncompatibleVersion` until both ends update —
   intentional and precedented, but a real interop outage for any
   long-lived remote pairing.
4. **Verification depends on the dev-flavor install being current.** The
   entire manual gate (item 8) runs on `scribe-dev`; a stale dev install
   silently verifies old code. The plan names `just ready` +
   `just install-dev` before verification but has no freshness assertion
   (e.g. build-stamp check) — verifier discipline required.
5. **nushell no-integration.** Documented limitation (validated on nu
   0.114.1); residual risk is user surprise, mitigated only by docs.
6. **Double-sourcing regression.** If the `SCRIBE_AI_TAB=1` gate in
   scribe.bash regresses, /etc/profile + profile run twice (verified
   failure mode); covered by US-2's sourced-once sentinel but only in
   manual verification, not automated tests (Principle 3 trade-off).
7. **Preamble quoting.** `conversation_id` is the one interpolated user
   datum, moving server-side; must reuse `shell_single_quote` discipline
   with per-shell syntax variants — a classic quoting-bug surface.
8. **`--rcfile` removal fallout.** Hand-typed custom `["bash"]` commands
   lose integration injection; spec-mandated, documented, tiny blast
   radius — but it is a plain-launch behavior delta under a "plain tabs
   unchanged" feature.
9. **Cold-restart regression.** Un-rewired `launch_binding_for` restores
   AI tabs as plain shells (validated); rewire ships in item 3 with the
   US-2 cold-restart check.
10. **apply.rs string-match arm** (:424-430) is the one
    non-compile-checked settings spot; a typo ships a dead `home` option.
    Named in the plan; still a manual-vigilance point.
11. **Flavor-split PATH crossing** inside AI tabs; absolute
    `SCRIBE_HOOK_HELPER` shields hook scripts — documented, residual
    confusion risk for users running both flavors.
12. **zsh ZDOTDIR daemon-env read** (shell_integration.rs:181):
    pre-existing bug class explicitly out of scope; carried as a noted
    lat.md correction + follow-up candidate, not fixed.

## Unresolved Questions

Near-empty, as expected after Clarifications:

- **OQ9 (error surfacing when the AI binary is missing): addressed —
  verified present in the plan.** Item 2b carries both halves: the typed
  shell-resolution fallback chain (passwd → daemon `$SHELL` → `sh`, chosen
  tier logged) and the "documented consequence sentence for
  AI-binary-not-found (shell prints command-not-found and the tab exits)";
  alignment log entry B11 records the fix. No richer client-side typed
  error ships — that is a recorded scope decision, not an open question.
- **Baseline emission (Q3d)** was delegated to planning and IS decided:
  drop for AI tabs, gated on `SCRIBE_AI_TAB=1` only. Closed.
- Genuinely open: none blocking. Two watch items, not questions: the exact
  timing of dual-write retirement (depends on the user approving a live
  server upgrade — item 7a trigger is defined, the date is not), and
  whether `AiProvider`/serde derives are reused on the wire vs mirrored
  (plan item on ai_state.rs leaves "confirm or mirror" to implementation;
  bounded either way).

## Constitution Check

| Principle | Verdict | Evidence |
|---|---|---|
| 1. Clear Boundaries and Typed Failure | pass | Typed `AiLaunchSpec` wire struct; typed, logged shell-resolution fallback chain; explicit cwd fallback tiers; server retains `is_dir → $HOME` guard. |
| 2. Session-Safe, Consistent UX | **tension** | Q3e: bashrc-only bash users (profile does not chain `~/.bashrc`) see different env in AI tabs vs plain tabs — documented consequence of real login semantics per the user's explicit redesign directive; plan records it as "documented, not mitigated". Minor same-family edge: `--rcfile` removal for hand-typed custom bash commands. Otherwise plain tabs untouched (Q2) and AI tabs gain parity. |
| 3. Explicit, Risk-Based Verification | pass | Per-story named manual verification commands on `scribe-dev`; no new test files; only two justified existing-coverage extensions (protocol round-trip, exclusion tests). |
| 4. Performance Budgets and Measurement | pass | ~1s tab-open budget with named `run-perf-ab.sh --ai-tab-only` command; stub-binary methodology excludes AI CLI startup; item 6 owns the number. |
| 5. Default-Safe Trust Boundaries | pass | Q7 decision explicitly recorded (full env + persisted delta into AI CLIs is deliberate user intent); `SHELL` + control-flow vars join EXCLUSION_SET; script/delta paths cross as env vars, never string-interpolated. |
| 6. Local-First Data Locality | pass | No network-touching change; everything is local launch plumbing. |
| 7. Compatible, Documented, Operationally Safe Change | pass | Dual-write compat matrix documented; v4 bump with loud refusal consistent with v2/v3 precedent; live server never restarted (dev-flavor verification); lat.md sync is its own sequenced item (7b) including corrections to existing false text. |

## Recommendation

**GO.**

Every user story, every Goal, and all seven binding Clarification decisions
map to specific plan sequencing items with verifiable acceptance criteria
(19 full / 0 partial / 0 none), the dependency graph is explicit and
acyclic, and the alignment log shows both audit passes already folded in —
including the previously open OQ9, now closed via item 2b's typed fallback
chain and documented command-not-found consequence. The one constitution
tension (Principle 2 vs Q3e) is a knowingly accepted, documented outcome of
the user's own redesign directive rather than an oversight, and the
riskiest surfaces (dual-write compat window, cold-restart binding rewire,
double-sourcing) each have a named mitigation and a named manual check in
the item-8 gate. Remaining risks are execution-discipline items (dev-flavor
freshness, apply.rs string arm, retirement-bead follow-through), none of
which blocks bead creation.
