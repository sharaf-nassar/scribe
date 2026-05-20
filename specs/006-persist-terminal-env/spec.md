# Feature Specification: Persist & Restore Terminal Environment Across Cold Restart

**Feature Branch**: `006-persist-terminal-env`
**Created**: 2026-05-19
**Status**: Draft
**Input**: User description: "when scribe cold restarts, our terminal environments get reset. is it possible to cleanly detect and persist terminal env changes as they occur so we can restore them on a cold restart? do any research you need"

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Restore my environment after an unexpected cold restart (Priority: P1)

Over a work session a developer sets up several terminals: exported API endpoints and tokens, activated a language/tool version manager, set project-specific variables, and changed directories. Scribe then cold restarts (a crash, a forced kill, or an OS reboot relaunch). Today the windows, layout, and working directories return, but every terminal's environment is empty — the developer must manually re-run every export and re-activate every tool in every terminal before they can resume. With this feature enabled, each restored terminal comes back with the same user-set environment it had immediately before the restart, so work continues without manual re-setup.

**Why this priority**: This is the entire point of the feature and the only slice that delivers the core value. It directly removes repetitive, error-prone manual re-setup after every crash. Without it, nothing else matters.

**Independent Test**: With the feature enabled, in a terminal set several environment variables and change directory; force a cold restart; confirm the restored terminal reports the same variables and directory with no manual re-entry.

**Acceptance Scenarios**:

1. **Given** a terminal where the user has set environment variables, **When** a cold restart occurs and the terminal is restored, **Then** the restored terminal's environment contains those same variables with the same values.
2. **Given** a terminal where the user later removed (unset) a previously set variable, **When** a cold restart occurs and the terminal is restored, **Then** the removed variable is absent in the restored terminal (the restore reflects the latest state, not a stale one).
3. **Given** multiple terminals each with a different environment, **When** a cold restart occurs, **Then** each restored terminal receives only its own environment, never another terminal's.
4. **Given** a terminal, **When** the user closes it or quits Scribe cleanly (not a crash), **Then** no stale environment is later restored into a freshly opened terminal.

---

### User Story 2 - Changes are captured continuously and silently as I work (Priority: P2)

The developer never takes a snapshot or presses a "save environment" button. As they export variables and activate tools throughout the session, Scribe detects and records those changes automatically and unobtrusively, so whatever the environment is at the moment of a crash is what gets restored — not an old or partial version.

**Why this priority**: Determines the correctness and freshness of the restore. If P1 restored a stale or partial environment, trust would erode quickly. Continuous, silent capture makes the feature dependable. It is separable from P1: capture and freshness can be verified independently of the restore path.

**Independent Test**: Change environment variables at several points in a session; at each point verify the persisted record reflects the most recent change with no user action; verify the record survives an abrupt process kill.

**Acceptance Scenarios**:

1. **Given** an active terminal, **When** the user sets or changes an environment variable, **Then** the persisted record for that terminal updates to reflect the change with no user action.
2. **Given** rapid successive environment changes, **When** a crash occurs immediately after, **Then** the restored environment reflects the most recent change, allowing for a small bounded capture window.
3. **Given** environment capture is active, **When** the user runs ordinary commands, **Then** no perceptible delay is added to command execution or prompt responsiveness.

---

### User Story 3 - The feature is explicitly opt-in and sensitive values are protected (Priority: P3)

Environments routinely contain secrets (access tokens, API keys, passwords). The developer must explicitly opt in to environment persistence, and when they do, the persisted data is encrypted at rest using the operating system's secret store. If the secret store is unavailable, the developer is told clearly and the feature stays off rather than silently writing secrets in plaintext.

**Why this priority**: A security and privacy gate. The feature is only acceptable if it is off by default, deliberately enabled, and never writes secrets in plaintext. It builds on P1/P2, so it is sequenced after them, but it is independently testable via the setting and the preflight check.

**Independent Test**: Toggle the setting with the OS secret store present and absent; verify enabling is refused with a clear message when absent; with it present, verify persisted data is encrypted (not readable as plaintext) and restores correctly; with the feature disabled, verify nothing is captured, persisted, or restored.

**Acceptance Scenarios**:

1. **Given** the feature is disabled (the default), **When** environment changes occur, **Then** nothing is captured, persisted, or restored, and cold restart restores no environment (existing behavior preserved).
2. **Given** the user enables the setting in Settings → Terminal → General and the OS secret store is available and configured, **When** the environment is persisted, **Then** it is stored encrypted at rest and is not readable as plaintext on disk.
3. **Given** the user attempts to enable the setting, **When** the OS secret store is unavailable or misconfigured, **Then** enabling is refused with a clear, actionable message describing what is missing, and the feature stays disabled.
4. **Given** the feature was enabled and the OS secret store later becomes unavailable, **When** an environment change would be persisted, **Then** the system stops persisting rather than writing plaintext, the terminal keeps functioning, and the situation is surfaced non-intrusively.

---

### Edge Cases

- **Process- or host-specific variables**: variables only valid for the previous process or host (session/display identifiers, authentication sockets, terminal-multiplexer markers, variables embedding a process identifier) and Scribe's own internally-injected terminal/identity/integration-discovery variables MUST be excluded from restore so a restored terminal is not given stale or invalid values.
- **Conflict with the restored shell's own startup**: when a captured user-set variable differs from what the restored shell's startup files would set, the captured user-set value wins (the feature's purpose is to restore the user's working environment — see FR-008).
- **rc drift across sessions**: if the user's shell startup files changed between sessions, the reconstructed post-startup baseline differs; the captured delta is still layered on top, and any key conflict is resolved by the precedence rule. Minor drift is tolerated by design.
- **Unsupported shell or shell integration disabled**: capture may be unavailable; the feature degrades silently — the terminal still restores and functions normally, just without its environment, and no error is surfaced.
- **OS secret store unavailable at enable time**: enabling is refused with an actionable message; the feature stays off; no data is persisted.
- **OS secret store becomes unavailable after enable** (locked/removed): the system fails safe — it stops persisting (never falls back to plaintext), the terminal is unaffected, and the condition is surfaced non-intrusively.
- **Very large environments**: many variables or very large values must not grow persisted data without bound or degrade terminal responsiveness; oversized values degrade to being skipped rather than failing the terminal's restore.
- **Set → unset → set again before a crash**: restore reflects only the final state.
- **Nested non-local contexts**: environment changes inside a remote SSH session, terminal multiplexer, or container are out of scope — only the local shell session's environment is captured and restored.
- **Crash mid-change**: at most the last fully-observed state is restored; a partially-applied change is not restored.
- **Restored terminal that re-runs a launch command** (custom command or AI tool) instead of a plain shell: the environment is still applied to the underlying shell session so the relaunched program inherits it.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST detect changes (additions, modifications, removals) to a terminal session's exported environment variables automatically as they occur, without any explicit user action.
- **FR-002**: When the feature is enabled, the system MUST persist, per terminal session, the current user/session environment delta in durable storage that survives full termination of the application and its operating-system process (a cold restart).
- **FR-003**: On cold restart, when a terminal is restored, the system MUST apply that terminal's persisted environment delta on top of the freshly reconstructed post-startup baseline so that commands run in the restored terminal observe the restored variables.
- **FR-004**: The persisted environment for a terminal MUST represent the latest observed state relative to the baseline, including reflecting removals — not an append-only or first-seen snapshot.
- **FR-005**: The system MUST associate each persisted environment with the specific restored terminal it belongs to, so environments are never cross-applied between terminals.
- **FR-006**: The system MUST exclude from restoration variables that are Scribe-internal or inherently specific to the previous process/host/session and would be invalid or misleading in a new process.
- **FR-007**: When the feature is enabled, the system MUST encrypt persisted environment data at rest using the operating system's secret store (Linux and macOS). The system MUST NOT write persisted environment data in plaintext under any condition; if encryption cannot be performed, the data MUST NOT be written (fail safe).
- **FR-008**: When a captured user-set variable conflicts with a value the restored shell's startup would set, the captured user-set value MUST take precedence.
- **FR-009**: The feature MUST be disabled by default and controlled by a single user setting located in the Settings window under the Terminal → General section. When disabled, nothing is captured, persisted, or restored, and behavior is identical to today.
- **FR-010**: The system MUST degrade gracefully when environment-change detection is unavailable for a terminal (unsupported shell, disabled shell integration): the terminal continues to function and restore normally, simply without environment, with no error surfaced.
- **FR-011**: Persisted environment data MUST be afforded at least the same on-disk protection and lifecycle as Scribe's existing per-window crash-recovery restore data (restricted access, retained only to recover from a crash, cleared on clean shutdown/close), in addition to the encryption-at-rest requirement of FR-007.
- **FR-012**: The capture model MUST be delta-only: the system MUST capture a per-terminal baseline after the shell's startup/initialization completes, and persist/restore only the exported variables that differ from that baseline (added, changed, or removed). Shell aliases, functions, shell options, and command history are out of scope.
- **FR-013**: Environment capture MUST NOT add perceptible latency to interactive command execution or prompt responsiveness (see PR-001).
- **FR-014**: The system MUST bound the per-terminal and overall size of persisted environment data and define behavior when a bound is exceeded (skip the oversized item rather than fail the terminal's restore).
- **FR-015**: When the user attempts to enable the setting, the system MUST perform a preflight check of the operating system's secret store and any other prerequisites; if they are unavailable or misconfigured, the system MUST refuse to enable the feature and present a clear, actionable message describing what is missing, leaving the setting off.
- **FR-016**: If the operating system's secret store becomes unavailable after the feature was enabled, the system MUST fail safe — stop persisting without any plaintext fallback, leave the terminal fully functional, and surface the condition non-intrusively, consistent with existing Scribe notification patterns.

### Quality, UX, and Performance Requirements

- **QR-001**: Implementation MUST preserve existing architecture boundaries and reuse existing project abstractions (the existing automatic session-survival/restore experience, the existing change-detection channel, and the existing Settings window organization) unless the plan states why a divergence is required.
- **QR-002**: Each user story MUST name its independent verification path. New test code MUST be requested explicitly in this spec or deferred to manual quickstart verification with rationale.
- **UX-001**: The restore itself MUST remain silent and automatic, consistent with how Scribe already returns window layout, working directory, and AI/session state after a restart. The only new user-visible surface is a single setting in the existing Settings window (Terminal → General) plus the actionable message shown if its prerequisites are unmet.
- **PR-001**: Capture MUST add no statistically significant change to interactive prompt round-trip versus baseline; persistence writes MUST be debounced/coalesced rather than per-keystroke; restoring environment MUST add under 100 ms to per-terminal ready time. Targets to be refined in the plan.

### Key Entities

- **Terminal Environment Delta**: the set of exported variables (added/changed/removed) for one terminal/launch within a window that differ from that terminal's post-startup baseline; the unit that is persisted and later restored.
- **Startup Baseline**: the per-terminal environment captured after shell startup/initialization completes; the reference against which the delta is computed and re-applied.
- **Environment Change Event**: an observed set/modify/unset of a variable within a terminal session; drives delta updates.
- **Restore Association**: the linkage tying a persisted delta to the specific restored terminal (window + launch identity) so it is reapplied to the correct terminal.
- **Feature Setting**: the single opt-in toggle in Settings → Terminal → General, default off, that gates all capture, persistence, and restoration.
- **Keystore Preflight**: the prerequisite check performed when enabling the setting; determines whether the OS secret store is available and correctly configured.
- **Exclusion Set**: variable names/patterns never restored because they are Scribe-internal or process/host-specific.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After a cold restart, for supported shells with the feature enabled, 100% of eligible user-set environment variables present immediately before the restart are present with identical values in the corresponding restored terminal (eligible = differs from the startup baseline and not in the Exclusion Set).
- **SC-002**: Users perform zero manual environment re-setup steps (no re-running of exports or tool activations) in restored terminals for the supported, enabled case.
- **SC-003**: Enabling the feature produces no perceptible delay: interactive prompt responsiveness is indistinguishable from before, within measurement noise.
- **SC-004**: Zero restored terminals are left in an incorrect or broken state due to inappropriately restored process/host-specific or Scribe-internal variables.
- **SC-005**: 100% of persisted environment data is encrypted at rest via the OS secret store; zero bytes of environment data are ever written in plaintext (data is either encrypted or not written).
- **SC-006**: Environment restore works across the same set of shells Scribe shell integration already supports; any unsupported shell still restores normally without environment and surfaces zero errors.
- **SC-007**: Persisted environment data never exceeds the defined per-terminal/global size bounds; exceeding a single-value bound degrades to skipping that value rather than failing the terminal's restore.
- **SC-008**: When OS secret-store prerequisites are unmet, 100% of enable attempts are blocked with a clear actionable message and zero environment data is persisted.
- **SC-009**: With default settings the feature is off: zero environment data is captured, persisted, or restored, and there is no observable behavior change versus today.

## Clarifications

### Session 2026-05-19

- **Q1 — Sensitive value handling policy** → **Encrypt at rest + explicit opt-in.** Persisted environment data is encrypted at rest using the operating system's secret store (Linux/macOS). The feature is gated by a new setting in the Settings window under Terminal → General, **disabled by default**. Enabling triggers a preflight check of the OS secret store and any other prerequisites; if they are missing or misconfigured the system refuses to enable and shows a clear, actionable message. There is no plaintext fallback at any point (fail safe). Encoded in FR-007, FR-009, FR-011, FR-015, FR-016; SC-005, SC-008, SC-009; User Story 3.
- **Q2 — Capture model** → **Delta-only with a post-startup baseline.** The system captures a per-terminal baseline *after* shell startup/rc completes and persists/restores only the exported variables that differ from it. This is equivalent in end result to a full restore for the variables users actually re-type, because restored terminals re-run startup/rc the same way and regenerate rc-driven state automatically; it also avoids re-injecting stale host/session-specific values that a full snapshot would carry. Encoded in FR-002, FR-003, FR-004, FR-012; SC-001.

## Assumptions

- **Exported variables only**: shell aliases, functions, shell options, command history, and non-exported shell variables are out of scope; users continue to rely on their own shell startup files for those. Only exported variables are meaningfully and safely portable into a freshly spawned shell.
- **Cold restart is the in-scope scenario**: full application/process death (crash, forced kill, OS reboot relaunch). Scribe's existing zero-downtime upgrade/handoff path also currently drops environment; extending environment preservation to that path is a natural follow-on but is out of scope for this MVP and noted as a candidate extension.
- **Off by default, single opt-in setting**: the feature ships disabled and is controlled by one toggle in the existing Settings window under Terminal → General, consistent with existing settings organization.
- **Encryption is mandatory when enabled**: there is no plaintext fallback at enable time, at persist time, or after a runtime keystore failure — the system always fails safe.
- **Supported secret stores**: the platform-native OS secret store on Linux and macOS (Scribe's primary platforms). Other platforms are out of scope for this MVP.
- **Baseline is post-startup**: the delta reference is the environment after shell startup/rc completes; the restored shell re-runs startup/rc the same way to rebuild it, and the persisted delta is layered on top, so the final environment is equivalent. Minor rc drift across sessions is tolerated; key conflicts resolve via FR-008.
- **Applied before first prompt**: restored environment is applied to the freshly recreated terminal before the user's first interactive prompt, so the first command already observes it.
- **Default Exclusion Set**: Scribe's own injected terminal/identity/integration-discovery variables plus well-known process/host/session-specific variables (display, authentication sockets, terminal-multiplexer markers, variables embedding process identifiers). The exact list is defined during planning.
- **Storage reuse**: persisted environment data inherits the existing crash-recovery restore-data location and lifecycle (retained only to recover from a crash; cleared on clean shutdown/close), with encryption-at-rest added on top.
- **Supported shells**: the set Scribe shell integration already supports; others degrade gracefully.
- **Restore stays invisible**: aside from the single Settings toggle and the prerequisite-failure message, the feature introduces no new user-facing surface — the restore itself is silent and automatic like existing session survival.
