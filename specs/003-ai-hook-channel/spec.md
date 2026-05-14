# Feature Specification: AI Hook Channel

**Feature Branch**: `003-ai-hook-channel`
**Created**: 2026-05-13
**Status**: Draft
**Input**: User description: "let's build the correct long term replacement for how we handle scribe hooks to fix this issue. we don't need any backward compatibility or fallbacks. ensure to do any research and investigation you need"

## Background

Scribe installs hooks into each supported AI coding tool (Claude Code, Codex, Auggie) so the AI session's state — running, waiting for input, encountering an error, current prompt, task label, context-window fill — can be reflected in Scribe's tab indicators and prompt bar.

Today every hook signals state by emitting an OSC 1337 escape sequence to `/dev/tty`, where Scribe's PTY interceptor reads it off the wire. This worked while every hook subprocess had a controlling terminal.

On 2026-05-11, Claude Code v2.1.139 shipped this change in its release notes:

> "Fixed a bug where a hook writing to the terminal could corrupt an on-screen interactive prompt; hooks now run without terminal access."
> — https://github.com/anthropics/claude-code/releases/tag/v2.1.139

The same direction of travel is plausible for Codex and Auggie. The OSC-over-`/dev/tty` channel is a coincidence of geometry, not a documented contract; it is now dead for Claude Code on every surface, and silently failing on cloud / headless surfaces for the other providers.

The user-visible regressions on Claude Code today are:

- `AskUserQuestion` is blocked outright. Scribe's `PreToolUse:AskUserQuestion` hook fails when its `/dev/tty` redirect cannot open, exits non-zero, and Claude Code denies the tool call.
- Tab state indicators do not update on `permission_prompt`, `error`, or AskUserQuestion transitions.
- The Stop-hook state classifier (`detect-claude-question.sh`) aborts under `set -euo pipefail` before it can decide whether Claude is idle or awaiting input — tab indicator stuck on "processing".
- Context-window fill from the statusline is silently dropped.
- Submitted prompt text intermittently does not reach the prompt bar.

This specification covers the long-term replacement of the entire AI-tool-originated state signaling path. There is **no backward compatibility and no fallback**: the new channel replaces the old one outright.

## Clarifications

### Session 2026-05-13

- Q: Should the Claude statusline subprocess migrate alongside hook subprocesses, or stay on its current `/dev/tty` path? → A: Keep statusline in scope. The statusline shares the same architectural pattern and migrates in this feature (current FR-021 stands).
- Q: Where does the Stop-hook `idle_prompt` vs `waiting_for_input` classification live — in each provider's adapter (current per-provider shell heuristic) or centrally in the Scribe server? → A: Centralize in the server. Adapters emit raw "session stopped" plus last-message text; the server runs one provider-independent classifier (FR-005 and new FR-013a).
- Remediation (post-`/speckit-analyze`): FR-015 ordering requirement softened to **best-effort** to match the one-connection-per-event architecture chosen in plan.md / research.md (Decision 2). The previous "MUST preserve ordering within a single session/provider stream" was not achievable without per-event sequence numbers, which were rejected because tab-indicator UI is eventually consistent and does not need strict FIFO. Companion change: a polish-phase task added to tasks.md (T053) measures hook-fire-to-`AiStateChanged` latency against the SC-002 200 ms p95 budget; final-grep task extended to cover SC-008 zero-duplication; US4 tests extended with subshell-propagation cases for FR-024.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Claude Code feature parity restored (Priority: P1)

A user running Claude Code inside Scribe gets the same tab-indicator and prompt-bar behavior they had before CC v2.1.139, and `AskUserQuestion` works without being blocked. This is the regression that motivated the work.

**Why this priority**: This is a live, user-visible breakage on a currently-shipping CC version. Every Scribe + CC user is affected today.

**Independent Test**: Launch Claude Code in a Scribe tab. (a) Trigger an interaction that calls `AskUserQuestion` — verify the picker renders and a selection round-trips. (b) Submit a prompt — verify the prompt bar updates immediately and the tab indicator transitions through `processing` → (the right post-Stop state). (c) Trigger a permission prompt or error — verify the tab indicator reflects it within 200 ms.

**Acceptance Scenarios**:

1. **Given** a Scribe user on Claude Code ≥ 2.1.139, **When** the assistant calls `AskUserQuestion`, **Then** the picker renders, the tab indicator switches to `waiting_for_input`, and the user's selection is received by the model with no error.
2. **Given** Claude Code is processing a prompt, **When** the Stop hook fires, **Then** Scribe's tab indicator transitions to `waiting_for_input` if the response ended on a question and to `idle_prompt` otherwise, with no spurious "processing" sticky state.
3. **Given** any Scribe + CC session, **When** the session emits state, prompt text, task label, or context-fill updates, **Then** no bytes from those signals appear in the user's visible terminal output, in scrollback, or in the model's conversation context.
4. **Given** any Scribe + CC session, **When** any hook fires, **Then** the hook subprocess exits with status 0 and produces no stderr.

---

### User Story 2 — Codex and Auggie surface parity (Priority: P2)

Users running Codex or Auggie inside Scribe get the same state, prompt, and task-label indicators they have today, on a channel that does not depend on `/dev/tty`. This preempts the same breakage if either tool tightens its hook subprocess environment.

**Why this priority**: Codex and Auggie hooks currently silently no-op on any TTY-less surface (their `2>/dev/null || true` pattern). The behavior happens to look "fine" today on a desktop terminal but is the same architectural bug; the user has no way to tell when an update is silently dropped. Solving it for Claude Code only would leave the codebase in a permanently mixed state.

**Independent Test**: Launch Codex in a Scribe tab. (a) Submit a prompt — verify the prompt bar updates and a task label appears in the tab strip. (b) Trigger an error or permission prompt — verify the tab indicator updates. Repeat for Auggie. All without `/dev/tty`-mediated signals.

**Acceptance Scenarios**:

1. **Given** a Scribe user on Codex, **When** Codex emits a state transition (processing, waiting_for_input, error, permission_prompt, inactive), **Then** the matching Scribe tab indicator updates within 200 ms.
2. **Given** a Scribe user on Codex or Auggie, **When** the assistant submits a task label, **Then** the tab strip shows the sanitized label and clears it when the task completes.
3. **Given** a Scribe user on any supported AI tool, **When** the tool emits a context-window percentage, **Then** Scribe's context indicator reflects the percentage within 200 ms.

---

### User Story 3 — Adding a new AI tool provider (Priority: P3)

A Scribe maintainer adds support for a new AI tool by writing one small adapter script. No changes to the transport, the shared event emitter, the env-var injection, the server-side consumer, or any other provider's adapter are required.

**Why this priority**: This is the long-term consistency dividend. The current per-provider duplication (each script inlines its own `printf '\e]1337;<Prefix>State=...\a' > /dev/tty`, each with subtly different error handling) is the root cause of the original asymmetry. The new architecture should make adding a fourth provider trivial; if it does not, the unification was incomplete.

**Independent Test**: Add a stub provider "Foo" with one adapter script. Register it via the existing per-provider setup script pattern. Verify that emitting state, prompt, task-label, and context events from the adapter results in Scribe's tab indicator and prompt bar updating identically to existing providers, with no changes to the transport, the helper, the server consumer, or the env-var injection.

**Acceptance Scenarios**:

1. **Given** the AI-Hook-Channel transport, helper, and server consumer are in place, **When** a maintainer adds a new provider by writing a single adapter that emits the standard event vocabulary, **Then** the provider's state, prompt, task-label, and context updates appear in Scribe with no other code changes.
2. **Given** the standard event vocabulary, **When** any provider emits an event whose `provider` field is unknown to the current Scribe build, **Then** the event is dropped silently on the server side without affecting other providers.

---

### User Story 4 — Hooks run safely outside Scribe (Priority: P2)

A Scribe-installed hook script that ends up running outside a Scribe terminal — cloud Claude Code, an SSH'd workstation without Scribe, a subagent inheriting global hooks, a CI environment — silently no-ops with no error, no stderr noise, and no impact on the AI tool.

**Why this priority**: Scribe-installed hooks live in the user's global AI-tool settings and follow the user to every surface those tools run on. Hooks must not break the AI tool anywhere. This is the contract that the `> /dev/tty` pattern violated.

**Independent Test**: Run any supported AI tool in an environment where `SCRIBE_HOOK_SOCK` is not set (or points to a non-existent socket). Trigger every event type the hooks listen for. Verify no hook subprocess fails, none writes to stderr, none blocks a tool call, and the AI tool runs as if the hooks were not installed.

**Acceptance Scenarios**:

1. **Given** Claude Code, Codex, or Auggie is running in a context where `SCRIBE_HOOK_SOCK` is unset, **When** any hook event fires, **Then** the hook subprocess exits with status 0, writes nothing to stdout or stderr, and does not block the AI tool.
2. **Given** the same context, **When** the hook is a blocking event type (e.g. `PreToolUse:AskUserQuestion`), **Then** the tool call is **not** denied by the hook.
3. **Given** `SCRIBE_HOOK_SOCK` is set but the socket file has been removed (server crash, package replacement mid-session), **When** any hook event fires, **Then** the hook subprocess still exits 0 with no noise, the AI tool is unaffected, and Scribe reconnects on the next event after the socket reappears.

---

### Edge Cases

- A hook fires before `scribe-server` has begun listening on the hook-event endpoint (e.g. immediately after a cold restart while a CC session is reattaching). The hook must no-op and exit 0; the missed event is acceptable as long as the next event lands.
- Multiple hook subprocesses fire concurrently from the same session (e.g. `PostToolUse` and `UserPromptSubmit` overlapping). The server must accept concurrent connections and route them to the right pane without dropping events.
- Multiple AI sessions run in parallel in one Scribe instance (different tabs). Each hook event carries enough identity for the server to route it to the right pane without ambiguity.
- A hook payload exceeds a reasonable per-event size (e.g. a 64 KiB prompt). The event is truncated at well-defined caps (prompts and task labels at 256 chars, matching the current OSC parser's caps), and the truncated payload is delivered.
- A hook subprocess inherits an `SCRIBE_HOOK_SOCK` env var from a parent process that has since exited (orphaned env). The hook attempts the connection, the connect fails, and the hook exits 0 silently. The same applies if the env var points at a Scribe instance that has been killed.
- A subagent dispatched by the AI tool inherits the parent session's `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID`. Events from the subagent are correctly attributed to the parent session's pane (the subagent has no separate Scribe pane).
- A hook subprocess takes a long time to send its event (slow disk, blocked process). The event emission has an explicit short timeout so it cannot stall the AI tool's hook pipeline; on timeout the helper exits 0 and the event is dropped.
- An AI tool the user installed has the Scribe hooks registered but is no longer one of the supported providers (e.g. the user disabled it in Scribe settings, or the Scribe server build does not recognize the provider). Events from that provider are accepted and dropped server-side; the hook still exits 0.

## Requirements *(mandatory)*

### Functional Requirements

**Transport and discovery**

- **FR-001**: Scribe MUST expose a dedicated hook-event ingress on the Scribe server that AI tool hook subprocesses can connect to from any process running under a Scribe-owned PTY (or any descendant thereof that inherits the PTY's environment).
- **FR-002**: Scribe MUST inject two environment variables into every PTY it spawns: one identifying the hook-event endpoint, and one identifying the Scribe pane/session the PTY belongs to. These environment variables are the sole discovery mechanism.
- **FR-003**: Hook subprocesses MUST no-op silently — with exit code 0 and no stdout or stderr output — whenever the Scribe-injected environment variable identifying the endpoint is unset.
- **FR-004**: Hook subprocesses MUST no-op silently — with exit code 0 and no stdout or stderr output — whenever the endpoint is set but unreachable (socket file missing, connection refused, server not yet listening, write timeout).

**Event vocabulary**

- **FR-005**: The hook channel MUST support the following event types, with provider-independent semantics:
  - state transition (one of: `idle_prompt`, `processing`, `waiting_for_input`, `permission_prompt`, `error`, `inactive`) — emitted by adapters that observe a structured state transition directly (e.g. `PreToolUse:AskUserQuestion` → `waiting_for_input`; `Notification:permission_prompt` → `permission_prompt`).
  - session-stopped event carrying the assistant's last-message text (truncated at the documented payload cap). Adapters MUST NOT classify this themselves; classification into `idle_prompt` or `waiting_for_input` is the server's responsibility (see FR-013a).
  - prompt text submitted by the user (truncated at 256 characters)
  - task label (sanitized, truncated at 256 characters)
  - task label cleared
  - context-window fill percentage (0–100)
- **FR-006**: Every event MUST identify (a) the AI provider, (b) the Scribe session/pane identity, and (c) optionally a stable conversation/session identifier from the AI tool itself, for forwarding into the session-state record.

**Hook subprocess contract**

- **FR-007**: Hook subprocesses MUST exit with status code 0 for every hook event type, regardless of whether the channel was reachable or whether the event was delivered.
- **FR-008**: Hook subprocesses MUST NOT write to stdout. (Several AI tools forward hook stdout into the model's conversation context; any non-empty stdout is a contamination vector.)
- **FR-009**: Hook subprocesses MUST NOT write to stderr. (Several AI tools surface hook stderr to the user; any non-empty stderr is user-visible noise.)
- **FR-010**: Hook subprocesses MUST NOT open or write to `/dev/tty`.
- **FR-011**: Hook subprocesses MUST NOT emit OSC bytes into the AI tool's standard streams.
- **FR-012**: The emission step (connect, write, close) MUST complete within a bounded short time budget; on timeout the helper MUST abandon the event and exit 0.

**Server-side handling**

- **FR-013**: The Scribe server MUST route an incoming hook event to the pane identified by the event's session/pane identifier, and update that pane's state, prompt-bar text, task label, or context-fill value as appropriate.
- **FR-013a**: The Scribe server MUST run a single, provider-independent classifier on incoming session-stopped events to map them to `idle_prompt` or `waiting_for_input` based on the assistant's last-message text. The classifier lives only in the server (not duplicated in adapters), is testable in isolation, and is shared by every supported provider. Today's per-provider shell heuristic (`dist/detect-claude-question.sh`, `dist/detect-codex-question.sh`) is replaced by this single classifier and removed from the adapter scripts.
- **FR-014**: The Scribe server MUST drop events whose provider is not recognized by the current build, without affecting other in-flight events or returning a hard error to the hook subprocess.
- **FR-015**: The Scribe server MUST accept concurrent hook-event connections without dropping events. Hook-event ordering across the channel is **best-effort**: each event travels through its own helper process and its own connection, so the wire-arrival order at the server may differ from the AI tool's emission order under sub-millisecond burst scheduling jitter. The downstream UI surfaces (tab indicators, prompt bar, context fill) are eventually consistent and tolerate this reordering. Strict per-session FIFO is **not** a guarantee of this channel.
- **FR-016**: The Scribe server MUST cap inbound message size to bound resource use; events exceeding the cap MUST be truncated to the documented per-field caps (256 chars for prompt and task label) rather than rejected.

**Provider symmetry and extensibility**

- **FR-017**: The same transport, the same shared emitter helper, and the same server consumer MUST handle all supported AI tool providers.
- **FR-018**: Adding a new AI tool provider MUST require only a new provider-specific adapter script that translates that tool's hook event JSON into a call to the shared emitter; no changes to the transport, the helper, the env-var injection, the server consumer, or other providers' adapters.
- **FR-019**: The system MUST support, at minimum, Claude Code, Codex, and Auggie at parity with today's OSC-based pipeline.

**Removals (no fallbacks)**

- **FR-020**: All Scribe-installed AI tool hook commands that currently write to `/dev/tty` MUST be removed and replaced with adapter scripts that use the new channel.
- **FR-021**: The Scribe-Claude statusline emitter, which currently writes context-fill OSC bytes to `/dev/tty`, MUST be migrated to use the new channel.
- **FR-022**: OSC 1337 parsing for AI-tool-originated state, prompt-text, task-label, and context-refresh events MUST be removed from Scribe's PTY metadata pipeline, because no Scribe-installed emitter will produce those bytes after this feature ships.
- **FR-023**: OSC 1337 parsing for the shell preexec pre-arm sentinel (`ScribeAiLaunch=<provider_id>`) MUST be retained. The pre-arm sentinel is emitted by the user's interactive shell, not by a hook subprocess; it has a controlling TTY by construction and is unaffected by the upstream change that motivated this feature.

**Compatibility constraints**

- **FR-024**: The new channel MUST function identically when the AI tool is launched in a subshell or wrapper of the Scribe-owned PTY, provided the environment variables propagate to the child.
- **FR-025**: Hooks MUST run safely in cloud sessions, in AI-tool-internal subagents, and in any other context where the AI tool runs but Scribe does not. The expected behavior in these contexts is `FR-003`-style silent no-op.

### Key Entities

- **Hook event**: A single structured message emitted by an AI tool hook subprocess. Carries: provider id, Scribe session/pane id, event type, and the event's payload (state name, prompt text, task label, context percentage, or a clear-marker).
- **Hook channel endpoint**: A Scribe-server-owned IPC endpoint that hook subprocesses connect to. Its address is injected as an environment variable into every PTY Scribe spawns. Lifetime is tied to the server.
- **Environment variables**: A small, well-defined set Scribe injects into every PTY it owns. At minimum: an endpoint locator and a session/pane identifier. Absence of either signals "not under Scribe; no-op".
- **Provider adapter**: A small per-AI-tool script registered as the AI tool's hook command. Its sole job is to translate that tool's hook stdin payload into a call to the shared emitter helper. Adapters carry zero transport logic.
- **Shared emitter helper**: A single program invoked by every provider adapter to perform the channel write. Centralizes endpoint discovery, error handling, timeout, exit-code policy, and the "no-op when unset" behavior. New providers reuse this without modification.
- **Server-side consumer**: The component in `scribe-server` that listens on the endpoint, deserializes events, validates and routes them by session id, and feeds them into the existing session-state record and downstream UI update path.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: In Scribe sessions on Claude Code ≥ 2.1.139, `AskUserQuestion` succeeds in 100% of invocations that would have succeeded on the same Claude Code version outside Scribe. (Concretely: zero `PreToolUse` hook denials originating from Scribe-installed hooks.)
- **SC-002**: A state, prompt, task-label, or context-window event emitted by an AI tool hook reaches the Scribe UI within 200 ms at the 95th percentile, measured from hook fire to UI repaint.
- **SC-003**: Across all supported AI tool providers, zero bytes originating from Scribe-installed hooks or statusline scripts appear in the AI tool's stdout, stderr, or conversation context, when measured by inspecting both the rendered PTY output and the AI tool's conversation transcript.
- **SC-004**: Adding a new AI tool provider, given the AI tool exposes a hook system, requires authoring only a single adapter script (or equivalent provider-specific entry point); no changes to the shared emitter, the env-var injection, the server consumer, or any other provider's adapter are needed.
- **SC-005**: When the AI tool is run in any context without Scribe (cloud session, subagent, SSH, CI, non-Scribe terminal), Scribe-installed hooks exit 0 with no stderr output in 100% of invocations.
- **SC-006**: 100% of state, prompt, task-label, and context-window updates that the OSC-based pipeline delivers today are delivered identically through the new channel, producing identical UI outcomes. (Verified by side-by-side scenario tests, not bytewise OSC inspection.)
- **SC-007**: No `> /dev/tty` redirect appears in any Scribe-installed AI tool hook command or in any Scribe-shipped script invoked from a hook context after this feature ships. (Excludes `tools/release-me/release.sh` and shell preexec / shell integration scripts, which run with a controlling TTY by construction and are out of scope.)
- **SC-008**: Source duplication of state-emission logic across providers, measured as inlined `<Provider>State=…` / `<Provider>Prompt=…` / `<Provider>TaskLabel=…` byte-emitting code paths in Scribe's hook scripts, drops to zero. All providers go through the shared emitter helper.

## Assumptions

- Hooks for all three currently supported AI tools run with the parent process's environment (so an environment variable injected by Scribe into the PTY reaches the hook subprocess). This is the documented Claude Code hook contract and matches the current behavior of the Codex and Auggie hook integrations Scribe already ships.
- The Scribe server's existing IPC primitive (a Unix domain socket served by `scribe-server`) is the right substrate for the new hook-event ingress. Whether the ingress is a separate socket or a new message variant on the existing one is an implementation decision deferred to planning.
- OSC 1337 parsing for the shell preexec pre-arm sentinel (`ScribeAiLaunch=<provider_id>`) stays in place. The pre-arm sentinel is fundamentally not a hook subprocess; it is emitted from the user's interactive shell, which has a controlling TTY by construction and is unaffected by Claude Code v2.1.139's hook hardening.
- Claude Code's "Channels" feature (released in v2.1.80) is the long-term upstream channel for state observation. It is currently research-preview and the protocol may change. The replacement built by this feature is Scribe-private and ships now; migrating to Channels (or layering Channels on top of the Scribe-private channel for two-way capabilities like permission-prompt relay) is tracked as a future direction, not a v1 deliverable.
- The user's "no backward compatibility or fallbacks" instruction is interpreted as: the new channel replaces the old hook scripts wholesale (FR-020 through FR-022), with no coexistence window. Existing installs receive the new hook script set via the existing install-time hook-rewriting mechanism (`is_scribe_hook` overwrite) when the user upgrades Scribe. Users who have not upgraded Scribe will continue to run the old broken hooks, which is the existing pre-fix state, not a regression caused by this feature.
- The new channel does not attempt to support arbitrary AI tools that have no hook system. Each supported provider is one Scribe knows about and ships an adapter for; "open-world" provider support is out of scope.
- Performance budget for hook emission is generous because hooks already run in a subprocess with non-trivial startup overhead (process fork, interpreter or binary load). A 200 ms p95 budget is comfortably above the underlying transport cost of a local Unix-domain-socket round-trip.
