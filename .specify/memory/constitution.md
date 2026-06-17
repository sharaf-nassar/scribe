<!--
Sync Impact Report
Version change: 1.0.0 -> 1.1.0
Modified principles: none reworded
Added sections:
- V. Security and Trust Boundaries
- VI. Local-First Data Locality
Removed sections: none
Templates requiring updates:
- ✅ no change needed: .specify/templates/plan-template.md (Constitution Check derives gates from this file)
- ✅ no change needed: .specify/templates/spec-template.md (no enumerated principle list)
- ✅ no change needed: .specify/templates/tasks-template.md (no enumerated principle list)
- ✅ checked: README.md, AGENTS.md, CLAUDE.md (no principle references to change)
Follow-up TODOs:
- Add lat.md design-intent sections for the local voice engine and the OSC 52 / paste policy surface so Principles V and VI trace to documented architecture.
-->
# Scribe Constitution

## Core Principles

### I. Code Quality and Clear Boundaries
Production changes MUST preserve the crate responsibilities and data-flow
boundaries documented in `lat.md`. Code MUST use typed data structures,
specific error types, focused modules, and existing project abstractions before
adding new dependencies or cross-cutting helpers. Unrelated refactors, blanket
error handling, and duplicated protocol/config parsing are prohibited.

Rationale: Scribe spans PTY ownership, IPC, GPU rendering, settings, packaging,
and AI process awareness. Clear ownership keeps changes reviewable and prevents
small features from destabilizing terminal sessions.

### II. Explicit, Risk-Based Testing
Every feature spec and implementation plan MUST define how each user story will
be verified independently. Automated tests MUST be planned when the feature
specification or user explicitly requests test code, or when an existing test
harness already covers the changed behavior without adding new test code.
Otherwise, the plan MUST document manual quickstart verification and the reason
automated coverage is not being added.

Rationale: The project requires deliberate test creation while still requiring
every behavior change to have an auditable verification path.

### III. Consistent User Experience
User-facing changes MUST preserve established Scribe interaction patterns:
terminal shortcuts remain configurable, mouse and selection behavior remains
predictable, settings changes use the existing webview structure, and client
workflows must not disrupt long-lived server-owned sessions. New UI text,
controls, states, and error handling MUST match nearby product language and
visual hierarchy.

Rationale: Scribe is a daily-use terminal. Consistency protects muscle memory,
avoids surprising agent/session behavior, and keeps settings comprehensible.

### IV. Performance Budgets and Measurement
Plans for rendering, input, IPC, PTY processing, settings application, startup,
or packaging work MUST state measurable performance goals or explicitly mark
performance as not applicable. Hot paths MUST avoid avoidable allocation,
repeated parsing, blocking I/O, and recompilation of reusable matchers. Final
verification MUST include the command, harness, or manual measurement used to
check the stated budget.

Rationale: Terminal latency, frame stability, and upgrade behavior are product
requirements, not polish. Regressions are visible immediately during normal use.

### V. Security and Trust Boundaries
PTY-side programs are untrusted. Any escape sequence or terminal capability that
can move data to the host, inject input, or trigger a host action — clipboard
access (OSC 52), hyperlink and URL opening (OSC 8), paste injection, and
equivalents — MUST be gated behind explicit policy and, for risky operations,
user confirmation. Capabilities that can exfiltrate or inject MUST default to
the safe posture (off or deny) and act only after the configured policy or a
confirmation dialog approves them. New escape-sequence or capability handling
MUST classify whether the sequence can exfiltrate, inject, or invoke a host
action and route privileged cases through the existing confirmation/policy
path; confirmation chrome MUST stay consistent with the established dialogs.

Rationale: A terminal renders output from arbitrary, possibly hostile,
programs. Silent clipboard reads, auto-opened URLs, and injected pastes are
real attack vectors, so privileged terminal capabilities must be deliberate,
visible, and default-safe.

### VI. Local-First Data Locality
Core terminal operation and on-device AI features MUST function without network
access. Speech and other AI processing run locally through the vendored engines
(for example `pocket-tts` and `sherpa-onnx`); models are fetched once and
inference stays on-device. Any network access — auto-update checks, the release
catalog, and equivalents — MUST be user-configurable and MUST NOT be required
for core terminal or AI functionality. Terminal contents and captured
microphone audio MUST NOT leave the device unless the user explicitly opts in.

Rationale: Scribe is a daily-use terminal that handles sensitive command output
and, with voice enabled, microphone audio. Local-by-default processing protects
that data and keeps the terminal fully usable offline.

## Engineering Constraints

Scribe development MUST follow these standing constraints:

- Use the existing Rust workspace, crate boundaries, configuration model, IPC
  framing, settings apply path, and test harnesses unless the plan documents a
  narrower reason to diverge.
- Verify current external library and CLI behavior before relying on new APIs,
  flags, CSS tokens, or framework features.
- Preserve user worktree changes and avoid unrelated file churn.
- Do not restart, upgrade, stop, or replace the live Scribe server unless the
  user explicitly approves that operation.
- Update `lat.md` when functionality, architecture, test specs, or behavior
  changes, then run `lat check` before reporting completion.
- Protocol, config, persistence, and package-install changes MUST include a
  migration or compatibility decision.

## Development Workflow

Spec Kit artifacts MUST make constitution compliance visible:

1. Specifications MUST include prioritized user stories, independent
   verification notes, measurable success criteria, and any UX or performance
   requirements needed to evaluate the feature.
2. Implementation plans MUST pass the Constitution Check before research and
   again after design. Each check MUST cover code quality, testing strategy,
   user experience consistency, performance, and operational safety.
3. Task lists MUST preserve independently deliverable user-story slices, include
   verification tasks, and include test-writing tasks only when tests are
   explicitly requested or already required by the accepted plan.
4. Completion reports MUST name the verification commands or manual scenarios
   that were run, plus any remaining risk.

## Governance

This constitution governs Spec Kit artifacts and delivery standards for Scribe.
When it conflicts with direct user instructions or repository runtime guidance
in `AGENTS.md`, follow the stricter operational constraint and document the
decision in the relevant plan or completion report.

Amendments MUST include a rationale, a Sync Impact Report, a semantic version
bump, and validation of dependent templates. MAJOR versions remove or redefine
principles in a backward-incompatible way. MINOR versions add principles,
sections, or materially expanded guidance. PATCH versions clarify wording
without changing compliance obligations.

Every implementation plan, task list, and code review MUST check compliance
with the current constitution. Violations are allowed only when documented in
Complexity Tracking with the reason, rejected simpler alternative, mitigation,
and user-visible impact.

**Version**: 1.1.0 | **Ratified**: 2026-05-15 | **Last Amended**: 2026-06-16
