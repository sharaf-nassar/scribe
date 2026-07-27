# Constitution

The engineering principles governing this repository. Read by the `speckit`
formula's spec-review, plan, and analyze gates — every feature is checked
against these.

## Principles

1. **Clear Boundaries and Typed Failure** — Preserve documented crate
   responsibilities and data flow; use typed structures, specific errors, and
   existing abstractions before adding dependencies or cross-cutting helpers.
   _Rationale:_ Focused ownership keeps terminal-session changes reviewable and
   prevents cascading failures.
2. **Session-Safe, Consistent UX** — Follow established interaction, settings,
   language, and visual patterns while preserving configurable shortcuts and
   long-lived server-owned sessions.
   _Rationale:_ Consistency protects user muscle memory and Scribe's defining
   session-continuity guarantee.
3. **Explicit, Risk-Based Verification** — Give every user story an independent,
   user-reachable verification path; add test code only when explicitly
   requested or when existing coverage must change, otherwise document and run
   manual verification.
   _Rationale:_ Green lower-level suites do not prove users can reach promised
   behavior.
4. **Performance Budgets and Measurement** — State measurable performance
   goals—or explicitly mark them inapplicable—and verify hot-path behavior with
   a named command, harness, or manual measurement.
   _Rationale:_ Terminal latency, frame stability, and uninterrupted upgrades
   are product requirements.
5. **Default-Safe Trust Boundaries** — Treat PTY programs as untrusted and gate
   capabilities that exfiltrate data, inject input, or invoke host actions
   behind safe defaults, explicit policy, and confirmation where warranted.
   _Rationale:_ Clipboard, hyperlink, paste, and related terminal capabilities
   cross meaningful security boundaries.
6. **Local-First Data Locality** — Keep core terminal and on-device AI
   functionality usable offline; make network access optional and never
   transmit terminal contents or microphone audio without explicit opt-in.
   _Rationale:_ Scribe handles sensitive command output and audio, so processing
   should remain local by default.
7. **Compatible, Documented, Operationally Safe Change** — Verify external APIs,
   preserve worktree state, document compatibility decisions, keep `lat.md`
   synchronized, and never disrupt the live server or publish state without
   explicit authority.
   _Rationale:_ Evidence-backed delivery protects user work and keeps protocol,
   configuration, persistence, and packaging evolution deliberate.
