# pi-ai-integration

## Problem Statement

Scribe launches Pi through `new_pi_tab`, but treats it as an untracked `ShellTool`. Pi sessions therefore lack the state, prompt, task-label, context, indicator, clipboard, detection, and packaging behavior available to Claude Code and Codex. Pi users need the same first-class AI integration without adding tab resume.

## Goals

- Add Pi as a user-visible `AiProvider` with provider id, display name, binary name, enable toggle, launch support, ambient process detection, and shared AI-session behavior.
- Keep `new_pi_tab` as the Pi launch action, changing its internals as needed to create a tracked Pi AI session. Do not add a Pi resume action.
- Ship a Scribe-owned Pi extension that uses Pi's documented extension lifecycle events to emit structured hook-channel events through `SCRIBE_HOOK_HELPER`, `SCRIBE_HOOK_SOCK`, and `SCRIBE_SESSION_ID`.
- Default Pi integration to enabled and install or activate the Pi extension when enabled. Keep it user-scoped, idempotent, upgradeable, safe outside Scribe, and non-conflicting with project-local or existing user extensions.
- Track Processing, WaitingForInput or IdlePrompt, Error, and cleared session state with the same server and client pipeline used by other providers. Pi does not emit `PermissionPrompt` because it has no built-in permission request lifecycle.
- Feed prompt text, a concise task label, context-window fill percentage, and provider identity into the existing prompt bar, tab label, context badge, pane border, tab indicator, scrollback, and clipboard behavior.
- Preserve offline operation. The extension must not transmit terminal or conversation content anywhere except Scribe's local hook socket.
- Add provider, extension, installer, packaging, settings, unit, functional, and visual coverage sufficient to prove a fresh enabled install and a live Pi session work on supported package layouts.

## Non-Goals

- Pi tab resume shortcuts, targeted conversation resume, or restoration of a previous Pi conversation.
- Changes to Pi itself or dependence on undocumented Pi internals when a documented extension event supplies the required signal.
- A project-local `.pi/extensions` installation. Scribe must not create duplicate global and project extension registrations.
- New AI indicator styles or a Pi-specific copy-cleanup pipeline.
- Network services, telemetry, or hosted integration components.
- A Scribe-owned Pi permission gate or `PermissionPrompt` state. Pi intentionally has no built-in permission popups, and tool execution must not be reinterpreted as a permission request.
- Exact parity for any other provider state that Pi cannot expose reliably through its public extension API. Such a gap must be documented and must degrade without false UI state.

## Backlog Inputs

None.

## Target Epic

This run will create a new feature epic.

## User Stories

### First-class Pi launch

As a Scribe user, I want the existing Pi tab action to launch a tracked Pi provider, so that Pi receives the same Scribe AI chrome as Claude Code and Codex.

Acceptance Criteria:

- `new_pi_tab` remains configurable and launches `pi` through the server-owned shell startup path.
- The launched session is identified as Pi before its first extension event and uses the configured AI-tab working-directory policy, including project root as the default.
- Cold restore relaunches a fresh tracked Pi session with no conversation-resume intent.
- No `new_pi_resume_tab` setting, action, menu item, or launch mode exists.

### Automatic extension setup

As a Scribe user who enables Pi integration, I want Scribe to install the matching Pi extension safely, so that state tracking works without manual Pi configuration.

Acceptance Criteria:

- Pi integration defaults to enabled. A fresh enabled install creates exactly one Scribe-owned user-scope extension through Pi's documented global auto-discovery path.
- Repeated setup and package upgrades are idempotent and update stale Scribe-owned registration without deleting unrelated Pi settings or extensions.
- Disabling Pi integration stops visible Scribe chrome for Pi but leaves the Scribe-owned extension installed for safe re-enable and stable/development coexistence; bounded local hook events may still be emitted like existing Claude/Codex hooks.
- The extension exits or no-ops silently when Scribe hook discovery variables are absent or the helper is unavailable.
- Debian and macOS packaging include the extension and setup path, with development and stable install layouts isolated consistently with existing hook setup.

### Live state and attention signals

As a Scribe user running Pi, I want the tab and pane to show whether Pi is working or waiting for me, so that I can switch tasks without repeatedly checking the terminal.

Acceptance Criteria:

- A submitted user prompt moves the session to Processing and records the real user prompt in Scribe.
- Settled Pi work emits enough final assistant text for the server's provider-independent stop classifier to choose IdlePrompt or WaitingForInput.
- Session shutdown clears retained Pi state.
- Error state is emitted only from documented Pi message data that unambiguously reports an error.
- Pi never emits `PermissionPrompt`; tool calls and terminal bytes are not treated as permission signals.
- Machine-injected or extension-originated turns do not replace the user's prompt bar or task label.

### Prompt, task, and context metadata

As a Scribe user, I want Pi prompt, task, model-context pressure, and provider metadata displayed in existing Scribe surfaces, so that Pi sessions are as legible as other AI sessions.

Acceptance Criteria:

- The first non-empty, non-command line of a real user prompt supplies a bounded task label unless Pi supplies a better documented session name signal.
- Context fill uses Pi's documented context-usage API, is clamped to 0-100, and refreshes at defined lifecycle edges without polling or blocking the agent loop.
- Prompt and task-label payloads retain existing server caps and machine-injected filtering.
- Pi context percentage uses the existing warn and danger thresholds in both tab and prompt-bar displays.

### Shared Scribe behavior

As a Scribe user, I want provider-neutral Scribe features to include Pi automatically, so that integration behavior stays consistent.

Acceptance Criteria:

- Provider enable gating, AI indicator styles, preserved AI scrollback, scroll pin eligibility, clipboard cleanup, reconnect metadata, and ambient binary detection recognize Pi where their existing provider-neutral rules apply.
- Pi does not inherit the Claude-only picker workaround.
- Existing Claude Code and Codex behavior and configuration compatibility remain unchanged.

## Constraints

- Follow all constitution principles, including user-reachable verification and a named no-polling responsiveness check in addition to typed provider boundaries, established UI behavior, local-only transport, safe failure, verified Pi APIs, packaging compatibility, and synchronized `lat.md` documentation.
- Use the existing hook-channel wire schema and `scribe-hook-helper`; do not add a second transport.
- The Pi extension must use documented extension events. Current candidate mappings are `input` or `before_agent_start` for prompt capture, `agent_start` for Processing, `agent_settled` for stop classification, `session_shutdown` for clear, and `ctx.getContextUsage()` for context fill. The plan must verify exact event ordering and payload access against installed Pi docs and examples.
- Extension placement must avoid the recorded duplicate-extension failure in `docs/solutions/environment/duplicate-pi-extension-blocks-fresh-workers.md`: install at one scope only and never pair a user-scope copy with a project-local copy.
- Hook emissions must never block or break Pi. Helper invocation must be bounded, silent, best-effort, and safe when Pi runs outside Scribe, over SSH, in CI, or as a subagent.
- Do not expose prompt or assistant text in argv, logs, world-readable files, or network requests. Use stdin payloads for value-bearing data.
- Keep the smallest provider-specific layer in the extension and reuse provider-neutral Rust paths.
- State transitions must account for strict lifecycle effects such as reload, session replacement, nested agent activity, queued follow-ups, auto-retry, and compaction.
- Performance goal: extension event handling adds no polling loop and no synchronous work proportional to transcript size. Named smoke or functional checks must verify prompt submission and tool execution remain responsive.

## Open Questions

None. The plan resolves final-message capture through `message_end`, derives task labels from real submitted prompts, and suppresses known child processes through `PI_SUBAGENT_CHILD=1`.

## Spec Review

### Critical Questions (answer before planning)

1. Should Pi integration default to enabled, matching Claude Code and Codex, or require explicit opt-in before Scribe installs a first-party extension into `~/.pi/agent/extensions`? This controls the trust and fresh-install experience; flagged by: gaps, stakeholders.
2. Pi intentionally has no built-in permission popups. Should the first release omit `PermissionPrompt`, or expand scope by adding a Scribe-owned Pi permission-gate workflow solely to create that state? This changes both security behavior and product scope; flagged by: requirements, gaps, feasibility, scope.
3. Should `new_pi_tab` adopt the shared AI-tab working-directory policy, including project-root default and configured fallback, or preserve its current focused-pane CWD behavior? Users will get different project context from the same shortcut; flagged by: ambiguity, scope, stakeholders.
4. On Scribe cold restore, should a prior Pi tab launch a fresh tracked Pi session, or return to a plain shell because targeted resume is excluded? This determines whether Pi remains first-class after restart without implying conversation resume; flagged by: requirements, ambiguity, scope.

### Technical Decisions (self-resolved; veto at the gate to override)

- Install one marked Scribe-owned user-scope file at `~/.pi/agent/extensions/scribe-ai-integration.ts` using atomic replacement. Refuse to overwrite an unmarked collision, do not edit Pi's `settings.json`, and do not create a project-local extension. A process-global symbol guard makes accidental duplicate loading emit only once.
- Leave the extension installed when integration is disabled. The Scribe provider toggle gates visible behavior, and retaining the flavor-neutral file avoids stable/dev ownership conflicts. Like existing Claude/Codex hooks, a loaded disabled integration may still send bounded local events that no visible surface renders.
- Use documented Pi events only. Real `input` enqueues Processing, prompt, and prompt-derived task label in that order with `source === "extension"` excluded; `agent_start` supplies Processing for retries or command-triggered runs without a captured input; `message_end` retains final assistant/error data; `agent_settled` emits stop classification and context; `session_shutdown` clears.
- Read context percentage from `ctx.getContextUsage().percent`, round and clamp it to 0-100, and emit only at lifecycle edges. Do not poll or parse Pi session files.
- Treat Pi permission state as unsupported. `tool_call` is not a permission request, and inferring one would display false attention state.
- Ignore known child-agent processes when `PI_SUBAGENT_CHILD=1` so Pi subagents that inherit `SCRIBE_SESSION_ID` cannot overwrite foreground state. Add an extension-level root-session isolation fixture and document the limit for third-party child launchers that expose no child marker.
- Invoke `scribe-hook-helper` directly from TypeScript with fixed argv selectors and JSON payloads on stdin. A bounded serial queue preserves order without blocking callbacks; a generation token cancels stale pending events, caps backlog at 32, and lets shutdown flush the active event plus clear within 250 ms. Never place prompt or assistant text in argv, logs, or files.
- Negotiate Pi support additively on local IPC. New `Hello` and `Welcome` capability booleans default false; a new client uses legacy `ShellTool::Pi` against an old server, and a new server omits Pi enum values and presents legacy shell-tool metadata to an old client. Exact-match remote protocol and handoff versions advance where serialized Pi provider state can cross a version boundary.
- Install or repair the extension only when Pi integration is enabled. A setup failure must leave Scribe and Pi usable, report a visible but non-blocking settings/startup notice, and retry on the next enable or packaged startup repair.
- Reuse provider-neutral Rust paths for indicators, prompt bar, context thresholds, clipboard cleanup, scrollback, scroll pin, ambient binary detection, and reconnect metadata. Add Pi-specific code only for provider identity, no-resume launch rules, extension setup, packaging, and event translation.
- First release scope is provider identity and toggle, tracked new-tab launch, extension setup, state/prompt/task/context emission, child isolation, shared chrome, packaging, tests, and docs. Do not add a custom permission system, conversation resume, Pi UI customization, or network services unless the clarification gate changes scope.

### Non-Blocking Observations

- Existing acceptance criteria need exact failure fixtures for missing helper, malformed payload, absent Scribe env, extension setup failure, abrupt Pi death, reload, compaction, queued follow-up, and package upgrade.
- Settings and setup notices need keyboard-readable labels and must not rely on color alone; existing AI chrome already supplies shared visual behavior.
- Stable and development packages share the same global Pi extension location. The source must therefore be flavor-neutral and resolve the active helper only through Scribe-injected environment variables.
- Setup diagnostics may record paths and result codes, but must never log prompt, assistant, model-context, or session content.
- Constitution principles 3 and 4 also apply: each user story needs a reachable functional check, and the plan must name a no-polling responsiveness measurement.

## Clarifications

**Q1: Should Pi integration default to enabled, matching Claude Code and Codex, or require explicit opt-in?**

A: Enabled by default. Fresh enabled installs create the Scribe-owned global Pi extension automatically.

**Q2: Should the first release omit `PermissionPrompt`, or add a Scribe-owned permission-gate workflow?**

A: Omit `PermissionPrompt`. Do not add a custom permission system or infer permission state from tool calls.

**Q3: Should `new_pi_tab` use the shared AI-tab working-directory policy or preserve focused-pane CWD?**

A: Use the shared AI-tab policy, including project root as the default and the existing configured fallbacks.

**Q4: Should cold restore launch a fresh tracked Pi session or return to a plain shell?**

A: Launch a fresh tracked Pi session. Preserve provider identity and Scribe chrome, but do not resume a prior Pi conversation.

## Architecture Approach

Promote Pi from a newly-created `ShellTool` launch into a first-class `AiProvider`, while retaining the old `ShellTool::Pi` wire and restore representation only for backward compatibility. New Pi tabs send structured `AiLaunchSpec` with `AiResumeMode::New`; cold restore also sends `New`, never targeted or generic resume. A provider capability method owns this distinction so restore, retained binding, and server argv construction cannot drift.

A Scribe-owned TypeScript extension at `~/.pi/agent/extensions/scribe-ai-integration.ts` translates documented Pi lifecycle events into the existing `scribe-hook-helper` CLI. It uses Node's standard child-process API, fixed selector arguments, JSON stdin payloads, a bounded serial emission queue, and no network or transcript-file parsing. A process-global guard prevents duplicate registration. The extension caches the latest finalized assistant message because `agent_settled` has no message payload, emits stop classification only after Pi is fully settled, reads context from `ctx.getContextUsage()`, and suppresses known child agents via `PI_SUBAGENT_CHILD=1`.

Scribe installs the extension by atomically copying a marked packaged source file into Pi's documented global auto-discovery directory. It updates only a Scribe-owned target and reports an unowned collision instead of overwriting it. The existing AI integration toggle pattern gains a Pi row that defaults enabled. Startup repair and a false-to-true settings transition run the same setup path; disabling leaves the file installed while client provider gating hides Pi chrome. Fresh Pi processes load it automatically, and setup failure remains visible but non-blocking.

Local mixed-version operation uses additive `Hello`/`Welcome` Pi capability fields. A new client falls back to the existing `ShellTool::Pi` request until a server advertises support, while a new server with an old client suppresses Pi enum values and returns legacy shell-tool metadata. Remote peers continue to use exact protocol matching, and serialized handoff state advances its version. This prevents the new enum variant from breaking the running old GPUI client during a server-only hot upgrade.

Alternatives rejected:

- Terminal-output parsing: brittle, cannot distinguish machine turns, and violates the structured hook-channel design.
- A new transport or direct socket implementation in TypeScript: duplicates the helper's framing, credential checks, timeout, and silent-failure contract.
- A project-local extension: reproduces the documented duplicate-extension startup failure and misses non-project Pi sessions.
- Editing Pi `settings.json` or registering a local package path: leaves path ownership and stale-package cleanup in user configuration when auto-discovery needs only one managed file.
- A custom permission gate: changes Pi's security model and was explicitly excluded at clarification.

The approach satisfies constitution boundaries by keeping provider identity in common types, launch and session ownership in existing client/server paths, translation in one Pi extension, local transport only, and failures typed or silently bounded at the correct boundary.

## Affected Components

- `crates/scribe-common/src/ai_state.rs`: add `AiProvider::Pi`, provider metadata, and a typed resume-capability decision shared by launch and restore code.
- `crates/scribe-common/src/config.rs`: add the default-enabled `pi_integration` toggle and include Pi in `ai_provider_enabled`.
- `crates/scribe-common/src/protocol.rs`: retain `ShellTool::Pi` for backward decoding, add default-false local Pi capability fields, bump exact remote compatibility where required, and let new requests use `AiLaunchSpec` only after negotiation.
- `crates/scribe-hook-helper/src/main.rs`: accept `--provider=pi` through the expanded provider enum and update provider-contract tests/docs.
- `crates/scribe-client/src/main.rs`: route `NewPiTab` through the AI launch path, use shared AI-tab CWD policy, prevent retained Pi metadata from becoming resume intent, and restore fresh tracked Pi sessions.
- `crates/scribe-client/src/restore_replay.rs`, `restore_state.rs`: map new Pi bindings and legacy `ShellTool::Pi` snapshots to `AiResumeMode::New` without conversation ids.
- `crates/scribe-client/src/settings/{model,apply,values,window}.rs`: expose, persist, categorize, and apply `terminal.pi_integration`; enabling triggers best-effort extension setup with an accessible notice.
- `crates/scribe-client/src/hook_setup.rs`: expand packaged AI setup repair to install Pi's extension on supported platforms when enabled, using one shared resource-location and setup execution path.
- `crates/scribe-client/src/ai_indicator.rs` and provider-aware clipboard/scroll-pin paths: add Pi gating tests; implementation should otherwise remain provider-neutral.
- `crates/scribe-server/src/session_manager.rs`: construct Pi launch argv with no resume args, preserve legacy shell-tool compatibility, and seed Pi provider hints before the first hook edge.
- `crates/scribe-server/src/ipc_server.rs`, `handoff.rs`, `handoff_tests.rs`, and `hook_ingress.rs`: store each client's Pi capability, downgrade Pi metadata/events for old local clients, version serialized handoff state, and keep generic ingress/filter behavior.
- `dist/pi-extension.ts`: new lifecycle adapter with no third-party runtime dependency.
- `dist/setup-pi-extension.sh`: idempotent, atomic, user-scope installer for the packaged extension.
- `dist/debian/postinst`, `dist/macos/build-dmg.sh`, and `crates/scribe-server/Cargo.toml`: package and repair the extension/setup assets for stable and development layouts.
- `tests/install/postinst-regressions.sh`, `tests/e2e/func/ai-launch-smoke.sh`, new `tests/e2e/func/pi-extension-harness.mjs` and `pi-ai-lifecycle.sh`, `tests/e2e/visual/{ai-indicator,settings-entry,multi-window-restore}.sh`, `tests/e2e/bin/pi`, and `justfile`: prove installation, launch, lifecycle events, isolation, failure behavior, shared chrome, and no-resume restore.
- `README.md` and `lat.md/{common,client,server,settings,test}.md`: document configuration, architecture, compatibility, and test intent.

## Data Model

- Add `AiProvider::Pi` with id `pi`, display name `Pi`, binary name `pi`, and no resume arguments.
- Expand the user-visible provider set from two to three. `AiProvider::System` remains excluded.
- Add a provider capability such as `supports_resume() -> bool`; Claude Code and Codex return true, Pi and System return false. Callers use it instead of inferring capability from an empty argument list.
- Add `TerminalAiIntegrationConfig.pi`, serialized as `terminal.pi_integration`, defaulting to true.
- Live Pi launch bindings use `LaunchKind::Ai { provider: Pi, resume_mode: New, conversation_id: None }` after capability negotiation.
- `LaunchKind::ShellTool { tool: Pi }` and `CreateSession.shell_tool = Pi` remain the compatibility representation for unsupported local peers and persisted restore snapshots. New replay upgrades that representation in memory to a fresh Pi AI launch.
- The extension keeps only ephemeral in-process state: latest real prompt, latest finalized assistant text/error marker, and a serial emission promise. It writes no conversation state or cache.
- Add default-false `pi_provider` capability fields to local `Hello` and `Welcome`. Unknown additive fields remain ignorable by older peers.
- New clients persist Pi restore records in the legacy `ShellTool::Pi` representation, then promote them in memory to fresh Pi AI launches. This keeps on-disk restore readable by older clients while live bindings remain provider-aware.
- Bump the exact remote protocol version because remote session metadata may now contain `AiProvider::Pi`.
- Bump the server handoff format version because live session state may serialize `AiProvider::Pi`; forward upgrade remains supported, while downgrade handoff fails safely and leaves the current server running.
- Hook payloads reuse existing `HookEventKind` variants and caps. No database or network-service migration is required.

## API / Interface Changes

- New config key and settings control: `terminal.pi_integration = true` / "Pi integration".
- Existing keybinding `new_pi_tab` remains the only Pi launch shortcut. Its behavior changes from focused-CWD shell tool to project-root-default AI launch.
- No `new_pi_resume_tab`, Pi resume menu action, conversation id, or resume CLI argument is added.
- `AiProvider::all()` includes Pi; `id`, `from_id`, `display_name`, `binary_name`, and resume capability become exhaustive for Pi.
- `scribe-hook-helper --provider=pi` accepts the existing events: `state_changed`, `session_stopped`, `state_cleared`, `prompt_received`, `task_label_changed`, `task_label_cleared`, and `context_changed`.
- `dist/setup-pi-extension.sh --extension-source <dir>` installs `<dir>/pi-extension.ts` as `~/.pi/agent/extensions/scribe-ai-integration.ts`. Re-running with identical content performs no write; an existing unmarked target produces a non-blocking collision error.
- Local `Hello.pi_provider` and `Welcome.pi_provider` negotiate structured Pi enum use. Without mutual support, Pi launch and session listing use `ShellTool::Pi` and omit Pi AI state/provider fields.
- The extension registers documented Pi handlers:
  - `session_start`: initialize root-session state and report idle only for a real Scribe root Pi process.
  - `input`: for `interactive` and `rpc` input, enqueue Processing, bounded prompt, and first non-empty non-slash task label in order; ignore `extension` input.
  - `agent_start`: emit Processing only when needed for a run without a captured input edge or as retry liveness.
  - `message_end`: retain the latest assistant text and unambiguous error stop reason without emitting user content.
  - `agent_settled`: emit Error or `session_stopped`, then a rounded context percentage when available.
  - `session_shutdown`: invalidate stale queued events, wait at most 250 ms for the active helper plus clear, and handle reload without letting an old generation overwrite the restarted runtime.
- Pi emits no `PermissionPrompt`. Tool events are not permission events.
- Disabling the provider is live for Scribe chrome. First-time extension installation applies to newly started Pi processes; the settings notice states this rather than claiming hot injection into an existing process.
- Existing config readers remain backward compatible. Local mixed versions negotiate Pi support, old restore files remain readable, remote mixed versions receive the existing typed incompatibility refusal, and no existing Claude Code or Codex keys change.

## Testing Strategy

- Common unit tests verify Pi provider metadata, iteration, serde ids, default-enabled config, provider gating, `supports_resume == false`, additive local capability defaults, exact remote-version change, and unchanged Claude/Codex behavior.
- Launch/restore unit tests verify a negotiated `new_pi_tab` creates `AiProvider::Pi` with `New`, project-root CWD, no conversation id, and no shell-tool field; an old-server capability fallback sends `ShellTool::Pi`; persisted Pi restore snapshots stay legacy-readable and upgrade to the same fresh launch; retained Pi edges never become `Resume`.
- Server argv tests verify fresh and restored Pi execute exactly `pi` with no resume argument, retain normal shell startup, and close the tab when Pi exits.
- Hook-helper unit/functional tests verify `--provider=pi` reaches generic ingress, prompt/task/context caps and machine-injected filtering remain active, and Pi never enters the Claude picker-only filter.
- A Node extension harness imports `dist/pi-extension.ts` with a fake `ExtensionAPI` and fake helper. It drives startup, duplicate load, interactive/rpc/extension input, Processing order, normal settle, question settle, error settle, context rounding/clamp, reload, bounded shutdown, missing env/helper, malformed assistant content, queue saturation, and `PI_SUBAGENT_CHILD=1`. It asserts exact fixed argv plus JSON stdin event order and zero permission events.
- The same harness proves responsiveness by making the fake helper sleep beyond the hook budget while extension callbacks return without waiting; it proves stale generations skip pending work, backlog never exceeds 32, and shutdown completes within the 250 ms bound without a post-clear event.
- `tests/install/postinst-regressions.sh` and setup-script fixtures verify first install, default enabled behavior, ownership marker, refusal to overwrite an unmarked target, identical second run, atomic stale managed-file replacement, unrelated extension preservation, disabled startup skip, stable/development source neutrality, permissions, and non-blocking failure.
- `tests/e2e/func/ai-launch-smoke.sh` gains a Pi stub phase proving zero args, requested project-root CWD, helper env injection, provider hint, and tab exit with the CLI.
- A Pi lifecycle functional test runs the packaged extension harness against a disposable Scribe server and verifies Processing, prompt, task label, WaitingForInput classification, Error, context percentage, clear, and no PermissionPrompt through `scribe-test ai-chrome`.
- A Pi child-isolation fixture sets `PI_SUBAGENT_CHILD=1` and proves inherited `SCRIBE_SESSION_ID` cannot overwrite root state, matching the existing Codex isolation test's intent.
- Local mixed-version tests serialize old/new `Hello` and `Welcome`, prove new-client/old-server launch fallback, prove new-server/old-client Pi metadata downgrading, and verify a server-only hot upgrade cannot send an unknown Pi enum to the running old client. Handoff tests cover forward transfer and safe downgrade refusal.
- Visual AI-indicator and settings tests add Pi phases for provider toggle off/on, integration row keyboard access, tab/pane color, context suffix, prompt bar, and cold restore into a fresh tracked Pi session.
- Run targeted Rust tests, setup/install regressions, Pi extension harness, `just e2e-func-ai-launch-smoke`, the new Pi lifecycle target, relevant visual targets, and `lat check`. The implementation plan must record exact command names added to `justfile`.

## Risks

- Global extension trust and ownership: Pi extensions run with full user permissions, and a generic filename could collide with user code. Mitigation: ship a small readable first-party file, use only Node stdlib, perform no network or arbitrary shell command, require a Scribe ownership marker before replacement, refuse unmarked collisions, and guard duplicate runtime registration.
- Pi API drift: lifecycle event names or message shapes may change. Mitigation: code against documented exported types, test with the installed Pi package, keep shape guards around message data, and fail silently rather than breaking Pi.
- Event ordering and shutdown: fire-and-forget helper processes could reorder events or emit after clear. Mitigation: one bounded serial queue, a 32-event cap, generation invalidation, redundant state/context coalescing, and a 250 ms shutdown flush preserve order without unbounded memory or post-clear writes.
- Stop classification input: `agent_settled` carries no messages. Mitigation: retain the last finalized assistant message from `message_end`, clear it at the next real prompt, and test retry, compaction, follow-up, abort, and error paths.
- Root/subagent confusion: child Pi processes inherit Scribe env. Mitigation: suppress the known `PI_SUBAGENT_CHILD=1` contract and test it; document that third-party launchers without a marker cannot be distinguished by Scribe's inherited env alone.
- Mixed-version compatibility: `AiProvider::Pi` is an unknown enum to old local peers and old restore readers. Mitigation: additive local capability negotiation, legacy shell-tool launch/list fallback, legacy-compatible Pi restore serialization, exact remote version bump, and handoff-version tests. A downgrade handoff with live Pi state may refuse safely; the current server remains running until Pi sessions close or a cold restart is chosen.
- Stable/development ownership: both flavors target one global file. Mitigation: make extension content flavor-neutral and resolve only the per-session `SCRIBE_HOOK_HELPER`; identical setup is idempotent.
- Setup lifecycle: enabling cannot inject code into an already-running Pi process that never loaded the extension. Mitigation: state that setup applies to new Pi sessions, retain the extension on disable, and make future re-enable live for loaded processes.
- Abrupt process death skips `session_shutdown`. Mitigation: rely on existing session teardown and stale-Processing cleanup, with a functional abrupt-exit regression.
- Packaging or setup failure must not block Scribe or Pi. Mitigation: best-effort execution, bounded diagnostics without content, retry on enabled startup, and explicit install tests.

## Sequencing

- **Define Pi provider and compatibility negotiation** (P0). Add provider identity, default-enabled config toggle, typed no-resume capability, local `Hello`/`Welcome` capability fields, remote/handoff version updates, legacy fallback helpers, and hook-helper acceptance. Acceptance: old local messages still decode, unsupported peers never receive `AiProvider::Pi`, remote peers refuse mismatches, and every consumer can branch on one shared capability. This blocks every other item.
- **Promote Pi launch and restore to tracked AI** (P1). Change `new_pi_tab`, project-root CWD, server argv, provider hinting, retained binding, negotiated legacy fallback, cold replay, and legacy-compatible restore serialization. Acceptance: supported peers launch fresh tracked Pi with zero resume args; old peers still launch Pi as `ShellTool`; cold restore is fresh and readable by the old restore schema. Depends on provider/negotiation.
- **Build the Pi lifecycle extension and harness** (P1). Implement the marked TypeScript adapter, duplicate-load guard, 32-event ordered queue, generation cancellation, 250 ms shutdown bound, prompt/task/context/error/stop behavior, no permission state, and child suppression. Acceptance: the harness proves exact event order, no blocking or polling, no post-clear writes, no child emissions, and silent failure. Depends on provider/negotiation; files are disjoint from launch work after that item lands.
- **Install, repair, and package the extension** (P1). Add ownership-safe atomic setup, unmarked-collision refusal, settings/startup trigger, accessible failure notice, Debian/DMG assets, and install regressions. Acceptance: fresh enabled install works, an identical rerun is a no-op, stale managed content updates atomically, unrelated files survive, and setup failure never blocks Scribe or Pi. Depends on lifecycle extension and provider toggle.
- **Verify provider-neutral Scribe behavior for Pi** (P1). Add gating, clipboard, scroll pin, indicator, hook-ingress, machine-injected filtering, mixed-version output downgrading, and Claude-only-filter regressions. Acceptance: Pi receives every shared feature when enabled, none of the visible chrome when disabled, no Claude-only workaround, and no unknown enum reaches an old client. Depends on provider/negotiation and tracked launch.
- **Prove Pi launch and lifecycle end to end** (P2). Extend AI launch fixtures and add lifecycle, context, error, abrupt-exit, root/subagent isolation, cold-restore, settings, mixed-version hot-upgrade, and visual checks. Acceptance: named functional and visual commands exercise each user story through a disposable server and real packaged paths, with `PermissionPrompt` absent. Depends on launch, extension, setup/package, and shared behavior.
- **Document Pi integration and compatibility** (P2). Update README and `lat.md` architecture/settings/test sections for default enablement, no permission state, fresh restore, local capability fallback, remote/handoff versioning, legacy restore representation, child-marker limit, setup ownership, rollback, and named verification commands. Acceptance: every changed behavior has one design section and one test-spec reference, README config matches defaults, and `lat check` passes. Depends on final behavior from all implementation items and blocks feature completion.

Items may run in parallel only after provider/negotiation lands. Tracked launch and lifecycle extension share no new primitive after that point and may proceed concurrently. Shared-behavior verification depends on tracked launch because both touch provider-aware client/server paths. Setup/package depends on the extension artifact; end-to-end testing depends on all behavior-producing items. Documentation follows settled behavior rather than guessing ahead.

## Backlog Refinement

None. This feature has no P4 backlog inputs to refine, supersede, cover, or classify as a non-goal.

## Alignment fixes applied

- Must-fix from plan-quality review: added additive local Pi capability negotiation, legacy launch/list fallback, remote protocol versioning, handoff versioning, and legacy-compatible restore serialization so `AiProvider::Pi` cannot break a mixed old/new local pair.
- Must-fix from plan-quality review: changed the global extension target to the marked `scribe-ai-integration.ts`, added unowned-collision refusal, and added a process-global duplicate-load guard.
- Must-fix from plan-quality review: bounded the ordered event queue to 32 entries, added generation cancellation, and set a 250 ms shutdown flush contract so stale events cannot appear after clear.
- Must-fix from plan-quality review: gave every sequencing item a concrete acceptance contract and corrected dependencies for shared provider/client-server paths.
- Must-fix from alignment self-check: resolved all remaining technical open questions, ordered Processing before prompt/task events, and made the disabled-provider behavior match existing local hook semantics.
- Should-fix from alignment self-check: added rollback, ownership-marker, mixed-version hot-upgrade, queue saturation, duplicate-load, and no-post-clear verification.
