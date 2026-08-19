# llm-control-surface

## Problem Statement

Scribe has no supported way for an external LLM or AI agent to interact with it
programmatically. Today the AI relationship is one-directional: `scribe-hook-helper`
and the provider adapters in `dist/` push AI *state* **into** Scribe (spec 003, spec
025), and Scribe renders it. Nothing lets an agent read Scribe's world or act on it.

An agent already running inside a Scribe pane cannot answer "what is on the screen
in my sibling pane?", "which of my sessions is blocked on a permission prompt?",
"open a new tab in this workspace and run the build there", or "what did the command
I ran two minutes ago print?" — even though the server holds authoritative answers to
all four. The user must copy/paste terminal content into the agent by hand, which is
exactly the manual step Scribe's AI awareness was built to remove.

The capability already exists inside the process boundary. `scribe-server` owns every
PTY session, every screen snapshot, workspace structure, AI process state, and the
`ListWindows` / `DispatchAction` automation pair. What is missing is a *supported,
gated, agent-shaped* surface onto it.

Two candidate surfaces were on the table. **Decision: the plain local API.** Scribe
exposes additive one-shot agent requests on its existing Unix socket, surfaced
through `scribe-cli` as a documented JSON contract. An MCP facade is deferred, not
rejected — see [Clarifications](#clarifications) Q1 for the evidence.

## Goals

- Resolve MCP vs local API with a written, evidence-backed decision record naming the
  losing option and why, so the choice survives review and is not relitigated.
- Ship one supported surface through which an external agent can, at minimum:
  - enumerate windows, workspaces, and sessions with their identity and status,
    server-wide;
  - read the current screen snapshot / recent scrollback of any named session;
  - read AI process state (provider, state, task label, context fill) per session;
  - invoke the existing `AutomationAction` set (new tab, split, focus session,
    profile switch, …);
  - write input into a named session.
- Every capability is individually gated by a default-safe policy with Allow / Deny /
  Prompt semantics, reusing the established `terminal.clipboard.*` policy shape rather
  than inventing a second gating vocabulary (constitution 5).
- Scribe opens no network connection and initiates no outbound transfer as a
  consequence of this feature. The surface binds UDS only. Granting a capability
  authorizes disclosure to the local agent, which may independently transmit content
  to its own model provider; that is the user's explicit opt-in (constitution 6).
- While an agent is actively using the surface against a session, the tab shows an
  agent indicator icon, so agent access is never invisible.
- Reuse the existing IPC protocol as the authoritative data source; the new surface is
  an adapter over `ClientMessage`/`ServerMessage`, never a second source of truth
  (constitution 1).
- A first-party consumer proves the surface end to end — the agent in a Scribe pane can
  read its sibling pane's screen without the user pasting anything.
- Measurable: a capability call returns within a stated budget on a warm server, and
  the surface adds no measurable cost to the PTY hot path when idle (constitution 4).

## Non-Goals

- Exposing the surface over the network, the tailnet, or LAN. Feature 013/014/015
  remote paths are out of scope; this is loopback/UDS only.
- Letting a remote or untrusted party drive Scribe. The consumer is a local agent
  running as the same user.
- Model hosting, inference, prompt construction, or any LLM logic inside Scribe.
- Replacing or deprecating the hook channel (spec 003 / 025). That is the inbound
  direction and stays as-is.
- Changing the frozen local IPC protocol's existing semantics. Additive variants are
  fine; renames and behavior changes to existing messages are not.
- A general third-party plugin/extension system for Scribe.
- Multi-agent arbitration or per-agent quotas.
- Per-agent cryptographic identity, agent tokens, or authorization on the raw IPC
  path. Same-UID trust is accepted (Q3).
- Restricting consumers to on-device models, or preventing a granted agent from
  forwarding content to its model provider.
- Terminal content in the audit record. The audit is metadata only.
- An MCP server, HTTP transport, or any stable third-party contract at the *raw IPC or
  network wire* level in v1. The `scribe agent --json` CLI output is a stable contract and
  is explicitly in scope.

## Backlog Inputs

None. No `source_backlog`, `epic`, or `backlog` ids were supplied and no P4 closure was
computed.

## Target Epic

Resolved: no `epic` or `epic_candidates` were supplied and the P4 closure is empty, so
this run creates a new feature epic. Not ambiguous.

## User Stories

### Agent reads a sibling pane

As an AI agent running inside a Scribe pane, I want to read the screen contents of
another session in the same window, so that I can act on build output or a failing
test without the user copy-pasting it to me.

Acceptance Criteria:

- Given a Scribe window with two sessions, an agent that names the sibling session
  receives that session's current screen content as text.
- The response identifies the session (id, title, cwd) so the agent cannot confuse two
  panes.
- A session id that does not exist, or belongs to another user's server, returns a
  typed error and never partial content.
- With the capability policy set to `Deny` (the default until the user opts in), the same
  call returns `AgentError::Denied` and no terminal bytes. All-`Deny` is the off state;
  there is no separate master switch producing a different refusal.

### Agent enumerates what Scribe is running

As an AI agent, I want to list windows, workspaces, and sessions with their AI state,
so that I can tell the user which of their agents is blocked and on what.

Acceptance Criteria:

- One call returns every window with its workspace name, session count, and connection
  status, matching what `ListWindows` reports.
- Each session carries provider, `AiProcessState`, task label, and context-fill
  percentage where the server has them, and omits (not fakes) them where it does not.
- The listing is consistent with what the Scribe UI shows at the same instant.

### Agent drives Scribe

As an AI agent, I want to open a tab, split a pane, focus a session, or type into a
session, so that I can set up the user's work instead of narrating instructions.

Acceptance Criteria:

- The full existing `AutomationAction` set is reachable and behaves identically to the
  keyboard/palette path — no second implementation of the actions.
- Writing input to a session requires its own capability policy, separate from read.
- With exactly one connected window an action may omit the target; with several, an
  ambiguous action is refused with a typed error rather than guessed.
- A denied or prompted action is refused/parked without side effects.

### Agent access is visible

As a Scribe user, I want to see when an agent is using the API against a session, so
that agent access is never silent.

Acceptance Criteria:

- While an agent is actively using the surface against a session, that session's tab
  shows an agent indicator icon at the start of the tab label.
- The indicator appears for read, action-dispatch, and input capabilities alike.
- The indicator clears when agent activity stops; it does not latch permanently after
  a single call.
- The indicator does not displace or truncate the existing AI-state indicator; the two
  coexist legibly.
- A metadata-only audit record captures agent, capability, target session, decision,
  and byte count for every call. It never contains terminal content.

### User controls what agents may do

As a Scribe user, I want per-capability control over what agents can see and do, so
that a compromised or careless agent cannot exfiltrate my terminal or type into my
shell.

Acceptance Criteria:

- Configuration exposes at least read-content, read-metadata, dispatch-action, and
  write-input as independently settable Allow / Deny / Prompt.
- Defaults are safe with no configuration present; the user opts in explicitly.
- A `Prompt` capability raises a Scribe-owned confirmation naming the agent and the
  requested capability, and the decision is honored (including its burst/always
  semantics, consistent with clipboard gating).
- Changing the policy takes effect without restarting the server.
- No capability transmits terminal content off the machine.

### Agent integrates through the documented CLI

As the author of an agent, I want a documented local contract to call, so that I can
reach Scribe without reverse-engineering its socket.

Acceptance Criteria:

- `scribe-cli` exposes the capability set as subcommands with stable `--json` output.
- The contract is documented well enough that an agent with shell access — including
  Pi, which has no MCP in core — can use it with no Scribe-specific adapter code.
- Scribe installs a provider-specific affordance so an agent *discovers* the contract
  without the user pasting instructions. The affordance text is **generated by the binary**
  (`scribe agent skill`) from the command tree and the live policy, never hand-authored,
  so it cannot drift from the contract it documents.
- Installation reuses the existing idempotent, ownership-marked setup scripts, which
  regenerate the file at startup and refuse to clobber a foreign one.
- The generated text reflects current policy: a capability the user has not granted is
  reported as unavailable with the settings path, rather than inviting the agent to
  attempt it and consume a turn on a refusal.
- Pi receives typed tools via `pi.registerTool()` in the Scribe-owned extension rather
  than prose.
- The affordance gates on `SCRIBE_SESSION_ID`, so an agent running outside Scribe is
  never told to call a command that cannot work.
- Reading a sibling pane is a single call: `scribe agent siblings` resolves the caller's
  window from `SCRIBE_SESSION_ID` with no id plumbing.
- A version/capability query reports what this build supports, and an unsupported
  request fails with a typed `unsupported` rather than hanging or guessing.

## Constraints

- **The MCP-vs-API decision is settled: plain local API, MCP deferred** (Q1). The
  losing option and its evidence are recorded in Clarifications. No MCP dependency,
  HTTP transport, or second listener enters v1.
- **Same-UID is the only authentication** (Q3). Capability policy constrains
  cooperative callers of the supported surface; it is explicitly not a sandbox against
  arbitrary same-UID processes, which retain their existing raw-IPC access — including
  `Hello { takeover: true }` and the ordinary attached-session snapshot and input
  paths. The spec must not claim otherwise.
- **Read and mutation scope is server-wide** (Q2, Q5): every window, every session,
  the full `AutomationAction` set, and input injection. This makes the Q6 tab
  indicator load-bearing rather than decorative — it is the only thing that makes a
  broad, weakly-authenticated capability observable.
- **Existing prior art must be reused, not duplicated.** The IPC protocol already
  carries the automation surface (`ClientMessage::ListWindows` / `DispatchAction` with
  `AutomationAction`; `ServerMessage::WindowList` / `ActionDispatched`) over the
  server's Unix socket, with local-CLI pre-`Hello` semantics that enumerate without
  registering an ephemeral window. Sessions, snapshots, subscribe, and AI state are all
  already protocol-reachable. The new surface adapts these; it does not reimplement them.
- The local IPC protocol is frozen: additive `ClientMessage`/`ServerMessage` variants
  are acceptable, semantic changes to existing ones are not (constitution 1, 7).
- `scribe-cli` is a thin launcher today; making it the agent entry point is a real
  option but changes its charter.
- `scribe-hook-helper` and the `dist/ai-hook-*.sh` adapters define the *inbound* AI
  channel. The new surface is the outbound direction and must not disturb them.
- Feature 013's Tailscale-gated TCP listener exists but is explicitly not the transport
  here — a network-reachable agent surface contradicts the non-goals.
- Constitution 5 (default-safe trust boundaries): a capability that reads terminal
  content or injects input is exactly the class of capability that must be gated behind
  safe defaults and confirmation.
- Constitution 6 (local-first data locality): terminal content must never be
  transmitted without explicit opt-in. Any transport choice must keep this true by
  construction, not by convention.
- Constitution 3: each user story needs an independent, user-reachable verification
  path; test code is added only where existing coverage must change.
- lat.md/ must be updated for whatever surface lands (constitution 7).
- The repo has no MCP dependency, code, or prior art today — greenfield either way.

## Open Questions

Q1–Q11 of the original draft are resolved: 1, 3, 4, 7, 8, 11 by the Clarifications
below; 2, 5, 6, 9, 10 by the Technical Decisions in Spec Review. Remaining:

All resolved during planning:

1. *Indicator trigger and dwell* — resolved: a reference-counted per-session activity
   lease held for the duration of each call, with a 1500 ms dwell after the last release
   so overlapping calls cannot clear each other's indicator.
2. *Audit destination* — resolved: structured `tracing` only, target
   `scribe::agent_api`, event `agent_call`. No queryable file.
3. *Extra write-input risk check* — resolved: none. The `WriteInput` capability plus the
   `max_input_bytes` cap checked before any prompt is the gate; a bounded UTF-8 payload
   with an explicit `submit` flag is not the arbitrary-control-byte hazard spec 011
   guards against.

## Spec Review

Six parallel review passes (requirements, gaps, ambiguity, feasibility, scope,
stakeholders) plus one dedicated MCP-vs-API research pass. Findings below are
ordered by how many dimensions independently flagged them; cross-dimension hits are
higher-confidence.

### Critical Questions (answer before planning)

1. **Which surface ships, and does MCP ship at all in v1?** — flagged by: scope,
   stakeholders, requirements, gaps, ambiguity, research.
   The research recommends a hybrid: Scribe's existing UDS protocol stays the
   authoritative capability layer, with an optional thin client-launched `stdio` MCP
   facade over it. But the scope pass argues MCP is not v1 at all — ship a JSON CLI
   over additive IPC first and add the facade after demonstrated demand. Evidence:
   MCP shipped four backwards-incompatible spec revisions in 18 months (one every
   ~5.4 months; the 2026-07-28 revision removed `initialize`, removed protocol
   sessions, added mandatory `server/discover`); major clients still document
   2025-06-18/2025-11-25 behavior; and Pi — a first-class Scribe provider (spec 025)
   — has no MCP in core by design. Counterweight: MCP is real reach (Claude Code,
   Codex, Cursor, Zed, VS Code, Continue, Gemini CLI, Copilot CLI, Cline, Windsurf,
   Goose) and requires no network access or phone-home in the default path, so it
   does not violate principle 6 by construction.

2. **What is the v1 capability cut — read-only, or read plus mutation?** — flagged
   by: scope, stakeholders, ambiguity, feasibility, gaps.
   The draft bundles five products: surface selection, discovery, content export,
   automation/input, and a consent UI. Every mutation path has an unresolved
   sub-problem: `AutomationAction` includes destructive `ClosePane`/`CloseTab` and
   the unrelated `OpenUpdateDialog`; `ActionDispatched` confirms *routing*, not
   execution, and the client's 16-entry action queue can discard an acknowledged
   action, so "open a tab and run the build there" is not safely composable without
   new correlated completion results; and `KeyInput` is raw bytes capped at 4 KiB
   that silently drops several failure classes. Read-only v1 removes all of that.

3. **Do we accept same-UID trust, or is per-agent identity in scope?** — flagged by:
   ambiguity, gaps, stakeholders, feasibility, requirements.
   The draft promises these policies stop a "compromised or careless agent." They
   cannot. Local UDS admission authenticates only the UID
   (`ipc_server.rs:4749-4764`); `SCRIBE_SESSION_ID` is explicitly a routing key that
   every PTY descendant inherits and any process can set
   (`specs/003-ai-hook-channel/contracts/env-vars.md:73-74`); and a same-UID process
   can already send `Hello { takeover: true }`, claim a window, and use the ordinary
   attached-session snapshot and input paths, entirely outside whatever gate this
   feature adds. Either narrow the promise to "constrains cooperative callers of the
   supported surface; not a sandbox against arbitrary same-UID processes" — or
   expand scope to server-issued agent tokens and authorization on raw IPC, which is
   a materially larger feature.

4. **How do we state the data-locality guarantee honestly?** — flagged by: all six
   dimensions.
   "Terminal content never leaves the machine" is not enforceable. Scribe hands text
   to a local agent; a cloud-backed agent puts it straight into model context and
   ships it to its provider. Constitution 6 permits this only with explicit opt-in,
   so the promise must be restated as what Scribe controls ("Scribe opens no network
   connection and initiates no outbound transfer") plus disclosure at the consent
   point naming the recipient and the egress risk. The alternative — supporting only
   on-device model consumers — is a real but much narrower product.

5. **What is the read scope: the caller's own window, or every window?** — flagged
   by: ambiguity, scope, stakeholders, requirements.
   The Goals section enumerates every window and session; the primary user story
   only needs a sibling pane in the caller's own window; Open Question 4 leaves it
   unresolved. This is a privacy decision, not a technical one — an agent in one
   project's window reading another project's window is a different product. The
   reviews converged on own-window-only for v1 with cross-window as a separate,
   higher-friction capability.

6. **Does an agent's access have to be visible, and is any audit record required?** —
   flagged by: gaps, stakeholders, scope.
   The draft lists audit logging as a non-goal "unless the analysis shows otherwise."
   The analysis shows otherwise is at least arguable: feature 013 already requires a
   persistent controller indicator and lifecycle audit for a *remote* party holding a
   window, and a local agent silently reading every pane is a comparable capability.
   Either accept invisible, unlogged reads explicitly, or fund a minimal
   metadata-only record (agent, capability, target, decision, byte count — never
   content) plus an attached-agent indicator.

### Technical Decisions (self-resolved — veto at the gate to override)

- **Additive one-shot agent variants on the existing `server.sock`; no second
  listener.** The pre-`Hello` transient path does not generalize: any non-`Hello`
  first frame that is not a known transient becomes a legacy client and registers a
  fresh window (`ipc_server.rs:4977-5064,5110-5130`). A new listener would cost
  path/permission handling, independent admission, upgrade takeover, and stale
  cleanup for no benefit. Follows constitution 1.
- **Never expose `SessionInfo` or `AiProcessState` on the wire.** They carry
  `launch_id`, retained prompt text, `conversation_id`, and model/tool/agent fields
  (`protocol.rs:1779-1840`, `ai_state.rs:123-133`). The external response is an
  explicit allowlisted DTO. Constitution 5 and 6.
- **Bounded text extraction straight from `Term`, not via `RequestSnapshot`.** That
  path requires attachment and returns a display-oriented compressed `SessionReplay`
  (`ipc_server.rs:8906-8946`), and full snapshots run 27–55 MiB at 200×50 with 10k
  scrollback. Copy only the selected rows under the terminal lock, format after
  releasing it, and gate concurrent extractions behind a dedicated semaphore.
- **Typed error enum with stable codes**, since the existing wire `Error` is a bare
  `String` (`protocol.rs:1153-1155`): `disabled`, `denied`, `not_found`,
  `out_of_scope`, `ambiguous_target`, `unsupported`, `too_large`, `busy`,
  `version_mismatch`, `internal`. Policy is evaluated *before* target lookup so an
  unauthorized target and a non-existent one are indistinguishable.
- **Performance budgets (constitution 4 requires numbers):** warm-server metadata
  p95 ≤50 ms, viewport read p95 ≤100 ms, response hard cap 256 KiB with an explicit
  `truncated` flag. With the surface disabled, no new listener, task, or timer
  exists, so idle cost is structurally zero rather than measured.
- **Content normalization:** join soft-wrapped rows, preserve hard breaks, trim
  trailing blank cells, drop styles/colors, emit `[image omitted]` for terminal
  images, keep OSC 8 link text but drop the URI.
- **Config is one additive `#[serde(default)]` table, default-Deny**, live-reloaded
  through the existing `ConfigReloaded` path. Do not inherit clipboard's defaults —
  `ClipboardMode` defaults to Prompt and clipboard writes default to Allow, which is
  the wrong posture here.
- **Concurrent reads are stateless and need no arbitration**, admitted under the
  existing transient pool bounds. Single-agent serialization is unnecessary for a
  read-only v1.
- **If MCP ships: `stdio` facade only, never new HTTP+SSE** (formally deprecated
  since 2025-03-26). Pin the spec revision, keep MCP types out of PTY/session
  modules, and prefer `rmcp` (official, Tier 2, 3.1.3) over hand-rolling because
  client-version lag makes dual-era compatibility the real cost. Loopback Streamable
  HTTP, if ever added, requires `127.0.0.1` binding, Origin validation, and
  authentication — "loopback" alone is not a boundary, per the spec's own DNS-rebinding
  warning.
- **If a `Prompt` policy ships, mirror spec 010 exactly:** headless denies, 60 s
  timeout denies, burst window 500 ms keyed by (agent, capability, target), queue cap
  64, `Always` mutates only that capability's persisted mode.

### Non-Blocking Observations

- Prior art favors the hybrid shape: Chrome DevTools keeps CDP and wraps it in a
  separate MCP server; VS Code documents native tool APIs for deep integration and
  MCP for cross-tool reuse; iTerm2 and WezTerm stayed CLI/native-API-first while
  third parties wrote the MCP adapters. Figma Desktop is the counter-example, serving
  MCP directly over loopback HTTP.
- iTerm2 is the closest security precedent: its Python API is off by default, uses an
  authenticated UDS with a per-script random cookie, and requires OS-level automation
  consent — justified explicitly because terminal data compromises local *and* SSH-reachable
  remote hosts.
- Add stable requirement IDs to acceptance criteria and map each to its verification
  path (constitution 3 traceability).
- `WindowInfo` carries `workspace_names: Vec<String>`, `connected`, controller,
  sharing mode, and participant count — the draft's singular "workspace name" and
  undefined "status" should match the real shape.
- Packaging fallout is real once surface ownership is decided: Debian stable/dev use
  separate binaries and share roots, and macOS stages and signs helpers under
  `Contents/MacOS`.
- Uninstall/deprecation matters only if install registers Scribe in external agent
  config files — an MCP facade would; a CLI would not.
- Day-after requests to expect: "read the last N lines", "tail this pane", "run a
  command and wait for exit", human-friendly target names instead of UUIDs, and MCP
  support if v1 ships CLI-only.

## Clarifications

**Q1: Which surface ships, and does MCP ship in v1?**

A: **Local API now, MCP facade deferred.** Additive one-shot agent requests on the
existing `server.sock`, exposed through `scribe-cli` as a documented JSON contract.
No MCP dependency, no HTTP transport, no second listener in v1.

Evidence for the losing option, recorded so this is not relitigated: MCP has real
reach (Claude Code, Codex, Cursor, Zed, VS Code, Continue, Gemini CLI, Copilot CLI,
Cline, Windsurf, Goose) and requires no network access or phone-home in its default
path, so it would not have violated principle 6. It lost on three counts. First,
churn: four backwards-incompatible spec revisions in 18 months — one every ~5.4 months
— with the 2026-07-28 revision removing `initialize`, removing protocol sessions, and
adding a mandatory `server/discover`; major clients still document 2025-06-18 and
2025-11-25 behavior, so a production facade must span two lifecycle eras. Second,
transport shape: canonical stdio is one server process per client host, so MCP does
not hand Scribe a shared-daemon model for free — a shared endpoint means loopback
Streamable HTTP, which the spec itself says needs Origin validation and
authentication because loopback is not a boundary under DNS rebinding. Third, MCP
supplies almost none of what Scribe actually needs: the Tools spec explicitly does not
mandate a user-interaction model, tool annotations are hints a client may ignore, and
the OAuth profile is HTTP-scoped, so per-capability consent, session authorization,
and terminal-data release policy would still be entirely Scribe's to build. Pi — a
first-class Scribe provider under spec 025 — has no MCP in core by design, so an
MCP-only surface would not reach it. Prior art favours the deferred-facade shape:
Chrome DevTools kept CDP and wrapped it in a separate MCP server; VS Code documents
native tool APIs for deep integration and MCP for cross-tool reuse; iTerm2 and WezTerm
stayed native-first while third parties wrote the adapters. Figma Desktop is the
counter-example, serving MCP directly over loopback HTTP.

**Q2: What is the v1 capability cut?**

A: **Everything in the draft, including write-input.** Read, metadata, the full
`AutomationAction` set, and input injection all ship in v1. This means the unresolved
mutation sub-problems become planning work rather than deferred scope: correlated
completion results for session-creating actions (since `ActionDispatched` confirms
routing only, and the client's 16-entry action queue can discard an acknowledged
action), and a bounded input contract, since `KeyInput` is raw bytes capped at 4 KiB
that silently drops several failure classes and spec 011's paste gate is client-side
and cannot cover programmatic injection.

**Q3: Do we accept same-UID trust, or is real agent identity in scope?**

A: **Accept same-UID trust and narrow the promise.** The spec states plainly that
capability policy constrains cooperative callers of the supported surface and is not a
sandbox against arbitrary same-UID processes. No server-issued agent tokens, no
authorization changes on the raw IPC path.

**Q4: How do we state the data-locality guarantee?**

A: **Restate the goal; no settings-copy change.** The goal now reads "Scribe opens no
network connection and initiates no outbound transfer", with granting a capability
being the explicit opt-in to disclosure toward the local agent, which may forward
content to its own model provider. Restricting consumers to on-device models was
considered and rejected — it would rule out Claude Code and Codex.

**Q5: Read scope?**

A: **All windows and sessions, server-wide.** No own-window restriction, no separate
cross-window capability.

**Q6: Must agent access be visible, and is any audit record required?**

A: **Yes to both, plus a specific UI requirement:** show an agent icon indicator at the
start of the tab label while that session is being used through the API, alongside a
metadata-only audit record (agent, capability, target, decision, byte count — never
content). Given Q2, Q3, and Q5 together grant broad, weakly-authenticated access, the
indicator is load-bearing rather than decorative: it is the only surface that makes
agent activity observable to the user.


## Architecture Approach

Scribe's server already owns every fact an agent wants. The surface is therefore an
**additive request/reply family on the existing `server.sock`**, dispatched by a new
`agent_api` module inside `scribe-server`, with `scribe-cli` as the documented consumer
contract. No new listener, no new daemon, no HTTP, no MCP dependency.

The mechanism already exists and is the natural extension point:
`establish_local_first_frame` routes a non-`Hello` first frame through
`is_transient_first_frame` (`ipc_server.rs:5110-5131`), which charges the connection to
the transient admission pool, dispatches, and closes without registering a window or
attaching a session. The existing `ListWindows` / `DispatchAction` pair already rides
it. One new variant — `ClientMessage::AgentRequest(AgentRequest)` — joins that list, so
agent traffic inherits bounded admission, never claims a `WindowId`, never attaches a
`ClientWriter`, and cannot resize a PTY.

**One wire variant, not five.** `AgentRequest` / `AgentResponse` are tagged enums in
`scribe-common`, so adding a capability later is a variant on an existing message rather
than another `ClientMessage` arm and another `is_transient_first_frame` entry. This also
keeps the whole external contract reviewable in one file, and — critically for
sequencing — means the `ipc_server.rs` edit is a *single* dispatch hook landed once by a
foundational item, after which each handler lands in its own `agent_api` submodule
instead of every handler contending for the same 13k-line file.

**Old-client compatibility is a real constraint, not a formality.** The client matches
`ServerMessage` exhaustively (`main.rs:12746-12811`), so broadcasting an unknown
`AgentActivity` to a client built before this feature breaks it. This is exactly the
problem spec 010 solved with the attach-time `clipboard_gating` capability bit on
`Hello` / `Welcome`. Reuse that shape: a new `Hello.agent_api: bool` recorded on the
window's participant entry; activity and prompt messages are sent only to participants
that advertised it, and a prompt with no capable local client takes the headless-deny
path.

Four deliberate rejections:

- **Rejected: reuse `RequestSnapshot` for reads.** It requires an attached session and
  returns a display-oriented compressed `SessionReplay` (`ipc_server.rs:8906-8946`);
  full snapshots run 27–55 MiB at 200×50 with 10k scrollback. Instead a bounded
  extractor reads rows directly from `Term` under the lock, copies only the requested
  range, and formats after releasing it.
- **Rejected: a long-lived agent connection with an `AgentHello`.** Every operation is
  one-shot request/reply. A persistent agent session would need its own admission pool,
  reconnect contract, and upgrade-takeover handling for no benefit.
- **Rejected: reuse `ListSessions` / `SessionInfo` on the wire.** `ListSessions` is
  post-`Hello` and window-scoped (`ipc_server.rs:8981-9059`), and `SessionInfo` carries
  `launch_id`, retained prompt text, `conversation_id`, and model/tool/agent metadata
  (`protocol.rs:1779-1840`). Agent replies use narrow allowlisted DTOs instead.
- **Rejected: a separate `enabled` master switch.** All-`Deny` capability defaults are
  already the off switch, and a second flag creates two indistinguishable refusal
  semantics (`Disabled` vs `Denied`) for the same user intent. There is no
  `AgentError::Disabled`; a capability the user has not granted returns `Denied`.

The one genuinely new subsystem is the **capability policy engine**: a typed
`AgentCapability` checked against a config-driven `Allow`/`Deny`/`Prompt` mode before any
target lookup. It reuses spec 010's *vocabulary and state-machine shape* — headless
denies, 500 ms burst reuse, `Always` mutates persisted mode, live reload via
`ConfigReloaded` — but not its code, which is specialised per PTY reader
(`clipboard_state.rs:61-140`). Defaults are all `Deny`; clipboard's Prompt/Allow defaults
are the wrong posture here.

Because Q2, Q3, and Q5 grant broad, same-UID-only, server-wide access, **observability is
a first-class component, not polish**: a tab-leading agent icon while a session is in use,
plus a metadata-only audit record. This is the concession that keeps the feature inside
constitution 5.

**Constitution check.** *1 (clear boundaries, typed failure):* external contract in
`scribe-common/src/agent.rs`; policy, extraction, and dispatch in
`scribe-server/src/agent_api/`; rendering in `scribe-client`; a typed `AgentError`
replaces the bare-string `ServerMessage::Error` for this family only, leaving frozen error
semantics untouched. *2 (session-safe UX):* agent requests never claim a window, attach,
or resize; the new indicator composes with the existing AI indicator. *3 (risk-based
verification):* every story maps to a named runnable path including denial paths.
*4 (performance budgets):* numbers and a named command in Testing Strategy; the extractor
holds the terminal lock only for the row copy. *5 (default-safe trust boundaries):* every
capability defaults `Deny`; destructive actions and write-input are separate capabilities;
the surface's limits are stated honestly rather than overclaimed. *6 (local-first):* no
network code, no new listener, no dependency. *7 (documented, operationally safe):*
`lat.md/` plus user-facing docs and packaging; no existing message semantics change.

**Learnings store.** `docs/solutions/` contains nothing on agent surfaces or protocol
design. `environment/beads-hooks-fire-in-linked-worktrees.md` governs this run's worktree
hygiene, already honored, and does not constrain the design.

## Affected Components

- **`crates/scribe-common/src/agent.rs`** (new) — the entire external contract:
  `AgentRequest`, `AgentResponse`, `AgentCapability`, `AgentPolicyMode`, `AgentError`, and
  the DTOs. Declared in `scribe-common/src/lib.rs`.
- **`crates/scribe-common/src/protocol.rs`** — additive
  `ClientMessage::AgentRequest` / `AgentPromptResponse`, `ServerMessage::AgentResponse` /
  `AgentPromptRequest` / `AgentActivity` / `RunActionCorrelated`,
  `ClientMessage::ActionCompleted`, and the `agent_api: bool` capability bit on `Hello`
  and `Welcome`. No existing variant changes.
- **`crates/scribe-common/src/config.rs`** — additive `#[serde(default)]` `AgentApiConfig`
  under `TerminalConfig`'s sibling scope (`config.rs:1095` is the analogous anchor).
- **`crates/scribe-server/src/agent_api/mod.rs`** (new) — the dispatcher: one entry point
  called from `ipc_server.rs`, routing each `AgentRequest` variant to its handler,
  evaluating policy first, emitting audit, and managing activity leases.
- **`crates/scribe-server/src/agent_api/policy.rs`** (new) — mode resolution, prompt
  correlation and routing, burst reuse, timeout, headless deny, live refresh.
- **`crates/scribe-server/src/agent_api/text.rs`** (new) — bounded `Term` → text extractor.
- **`crates/scribe-server/src/agent_api/world.rs`** (new) — server-wide aggregation from
  `IpcServerState.live_sessions`, `workspace_manager`, and `window_shares`. **Not**
  `session_manager.rs`, whose map is a creation-time staging area cleared immediately
  (`session_manager.rs:442-450,624-625`).
- **`crates/scribe-server/src/agent_api/activity.rs`** (new) — reference-counted
  per-session activity leases with dwell.
- **`crates/scribe-server/src/ipc_server.rs`** — exactly three edits, all in the
  foundational dispatcher item: extend `is_transient_first_frame`, add the dispatch arm,
  and add `agent_api` state to `IpcServerState` (`ipc_server.rs:1131`). Handlers do not
  touch this file.
- **`crates/scribe-server/src/main.rs`**, **`lib.rs`**, **`config.rs`** — module
  declarations, `agent_api` state init (`main.rs:444`), config projection and
  `ConfigReloaded` refresh.
- **`crates/scribe-cli/src/main.rs`** — a new `Agent` subcommand tree with stable JSON on
  stdout, diagnostics on stderr, and documented exit codes.
- **`dist/setup-claude-hooks.sh`**, **`dist/setup-codex-hooks.sh`**,
  **`dist/pi-extension.ts`** — the discovery layer. No new authored document exists: each
  script runs `scribe agent skill` and writes the output to
  `~/.claude/skills/scribe-terminal/SKILL.md` and
  `~/.codex/skills/scribe-terminal/SKILL.md` (both directories verified present and
  populated on a real install). The Pi extension gains `pi.registerTool()` registrations —
  verified available at load and in `session_start`, refreshing in-session without
  `/reload` — shelling to the same CLI, so Pi gets typed tools instead of prose.
  `crates/scribe-client/src/hook_setup.rs:30-70` already probes `~/.claude` / `~/.codex`
  and self-repairs the Pi extension at startup, so regeneration after a version or policy
  change needs no new mechanism.
- **`crates/scribe-client/src/tab_bar.rs`** — `TabData.agent_active` (data only).
- **`crates/scribe-client/src/titlebar.rs:471-614`** and **`main.rs:6979`** — the actual
  glyph rendering, leading-slot layout, `title_columns` allowance, and the AccessKit tab
  label addition.
- **`crates/scribe-client/src/dialog.rs`, `ipc_bridge.rs`, `main.rs`** — the agent consent
  prompt: modal, IPC routing, default-Deny focus, Escape denies.
- **`crates/scribe-client/src/settings/{model,apply,values,window}.rs`** — the Agent API
  settings page.
- **`crates/scribe-server/Cargo.toml:22-32,140`**, **`dist/macos/build-dmg.sh:53,121-123`**
  — package `scribe-cli` for Debian stable/dev and stage/sign it in the macOS bundle. It
  ships in neither today, so without this the promised consumer does not exist on a real
  install.
- **`README.md`** plus a user-facing doc — JSON schemas, exit codes, policy config,
  the same-UID limitation, and the egress disclosure.
- **`lat.md/{protocol,server,client,common,test}.md`** — design intent.

## Data Model

All external types live in `scribe-common/src/agent.rs`.

- `AgentRequest` — tagged enum: `World`, `Siblings`, `ReadScreen { session_id,
  scrollback_lines: Option<u32> }`, `DispatchAction { action, window: Option<WindowId> }`,
  `WriteInput { session_id, text, submit }`, `Capabilities`. Every variant carries
  `agent_label: String` (bounded 64 chars, **self-asserted and displayed as untrusted** —
  it is disclosure, not authentication), `origin_session_id: Option<SessionId>` taken from
  the caller's `SCRIBE_SESSION_ID`, and a client-generated `request_id: u64` used to
  correlate the reply. `Siblings` is `World` filtered to the origin session's window, and
  returns `NotFound` without a valid origin — it exists because "read the pane next to me"
  is the primary use case and should not require an agent to plumb ids across four calls.
  `origin_session_id` is orientation, not authorization; same-UID trust (Q3) already
  concedes it is forgeable, and no capability decision depends on it.
- `AgentResponse` — `{ request_id, result: Result<AgentPayload, AgentError> }`.
  `AgentPayload` is a tagged enum mirroring the request variants.
- `AgentCapability` — `ReadMetadata`, `ReadContent`, `DispatchAction`,
  `DispatchDestructiveAction`, `WriteInput`. Destructive dispatch is split out so
  `ClosePane` / `CloseTab` / `OpenUpdateDialog` cannot ride a benign grant. An **exhaustive
  `match` maps every `AutomationAction` variant** to one of the two dispatch capabilities,
  so a future action fails to compile rather than defaulting to the weaker gate.
- `AgentPolicyMode` — `Deny` (default) | `Allow` | `Prompt`.
- `AgentApiConfig` — one `AgentPolicyMode` per capability plus bounded numerics, each with
  a compile-time clamp applied on load: `max_response_bytes` (default and hard ceiling
  256 KiB), `max_scrollback_lines` (default 1000, ceiling 10_000), `max_input_bytes`
  (default 4096, ceiling 65_536), `prompt_timeout_ms` (default 60_000, ceiling 300_000),
  `burst_window_ms` (default 500, ceiling 5_000), `activity_dwell_ms` (default 1_500).
  Additive, `#[serde(default)]`, unknown keys tolerated, no migration — older builds
  ignore the table. There is no master `enabled` flag; all-`Deny` is off.
- `AgentError` — `Denied`, `PromptTimeout`, `NotFound`, `AmbiguousTarget`, `Unsupported`,
  `TooLarge`, `Busy`, `VersionMismatch`, `ActionFailed`, `Internal`. Tagged, stable code
  plus a human message. **`OutOfScope` is deliberately absent**: Q5 made every session
  in-scope, so the condition cannot arise; this supersedes that Spec Review decision.
- `AgentWorldSnapshot` — `{ windows: Vec<AgentWindow>, workspaces: Vec<AgentWorkspace>,
  sessions: Vec<AgentSession>, snapshot_id: u64, captured_at }`.
- `AgentWindow` — `window_id`, `workspace_names: Vec<String>`, `session_count`,
  `connected`, `sharing_mode`, `participant_count`. Controller and participant *identity*
  are omitted deliberately: they are another user's device and login name, and no agent
  story needs them.
- `AgentWorkspace` — `workspace_id`, `name: Option<String>`, `window_id`, `session_ids`.
- `AgentSession` — `session_id`, `window_id`, `workspace_id`, `title: Option<String>`,
  `cwd: Option<PathBuf>`, `provider: Option<..>`, `ai_state: Option<..>`,
  `task_label: Option<String>`, `context_fill_percent: Option<u8>`, `is_caller: bool`
  (true for the entry matching the request's `origin_session_id`, so an agent can locate
  itself without string-matching an env var against the list). Optional fields carry
  `skip_serializing_if` because the source values are themselves optional. Explicitly
  excludes `launch_id`, `prompt_state`, prompt text, `conversation_id`, model/tool/agent
  fields, and the environment envelope.
- `AgentScreenText` — `session_id`, `title: Option<String>`, `cwd: Option<PathBuf>`,
  `text`, `lines`, `truncated: bool`, `captured_at`, `snapshot_id`. Title and cwd are
  present so an agent cannot confuse two panes (US1 AC2).
- `AgentActionResult` — `action`, `outcome: Completed | Failed`,
  `created_session_id: Option<SessionId>`. **No `Queued` variant** — Q2 requires real
  completion.
- `AgentActivityLease` (server-internal) — per-session refcount plus a dwell deadline, so
  overlapping calls cannot clear each other's indicator.
- Audit is emitted as structured `tracing` fields on target `scribe::agent_api`, event
  `agent_call`: `agent_label`, `capability`, `target_kind` (`server|window|session`),
  `target_id`, `decision`, `response_bytes`. Never content.

No persistent storage and no migration. `Always` decisions mutate the in-memory policy and
round-trip through the existing config-write path, matching spec 010.

## API / Interface Changes

**Client → server (additive):**

- `ClientMessage::AgentRequest(AgentRequest)` — added to `is_transient_first_frame`.
- `ClientMessage::AgentPromptResponse { prompt_id, decision }` — from the GUI client.
- `ClientMessage::ActionCompleted { correlation_id, outcome, created_session_id }` — the
  client's completion report for a correlated action.
- `ClientMessage::Hello { .., agent_api: bool }` — capability bit, mirroring
  `clipboard_gating`.

**Server → client (additive):**

- `ServerMessage::AgentResponse(AgentResponse)`.
- `ServerMessage::AgentPromptRequest { prompt_id, agent_label, capability, target }` —
  sent only to participants advertising `agent_api`.
- `ServerMessage::AgentActivity { session_id, active: bool }` — same gating.
- `ServerMessage::RunActionCorrelated { correlation_id, action }` — the correlated
  counterpart to today's routing-only `RunAction`/`ActionDispatched` pair, which stay
  unchanged for existing callers. The client reserves queue capacity *before*
  acknowledging, so the 16-entry queue cannot silently drop a correlated action
  (`remote_chrome.rs:37-47,185-192`). Timeout or client disconnect yields
  `AgentError::ActionFailed`.
- `ServerMessage::Welcome { .., agent_api: bool }`.

**CLI contract:** `scribe agent world`, `scribe agent siblings`,
`scribe agent read <session-id> [--scrollback N]`, `scribe agent action <action>
[--window W]`, `scribe agent write <session-id> --text <t> [--submit]`,
`scribe agent capabilities`, plus `scribe agent skill`, which prints the generated
affordance markdown rather than returning data. All data subcommands emit a versioned
JSON envelope
`{"v":1,"ok":true,"data":…}` or `{"v":1,"ok":false,"error":{"code":…,"message":…}}` on
stdout; diagnostics go to stderr. Exit codes: `0` ok, `1` typed error, `2` usage, `3`
server unreachable or unsupported.

**No breaking changes.** Every existing message keeps its semantics. Against an older
server the new first frame is undecodable, so the server never replies; the CLI applies a
3 s deadline (well inside the 5 s `LOCAL_PRE_HELLO_TIMEOUT`) and reports `Unsupported`.

## Testing Strategy

Per constitution 3, each story gets a named runnable path; test code is added where
coverage must change.

- **Unit (`scribe-common`)** — `AgentApiConfig` defaults are all `Deny`; every numeric
  clamps to its ceiling; unknown keys tolerated. `AgentError` code stability. DTO
  serialization asserts excluded fields are absent — the regression that matters most, so
  a future `SessionInfo` field cannot leak. Exhaustive `AutomationAction` →
  capability mapping, asserted variant-by-variant so a new action breaks the build.
- **Unit (`scribe-server`)** — `agent_api::text`: wrap-joining, hard breaks, blank-tail
  trimming, wide characters, `[image omitted]`, OSC 8 label-kept/URI-dropped, byte cap
  setting `truncated`. `agent_api::policy`: mode resolution, headless deny (no capable
  client), prompt timeout deny, burst reuse inside/outside the window keyed by
  `(agent_label, capability, target)`, queue cap 64, `Always` persistence, live refresh
  cancelling pending prompts. `agent_api::activity`: overlapping leases do not clear
  early; dwell elapses; disable and disconnect release all leases.
- **Integration (`scribe-test`)** — over a real socket: an agent request registers no
  window, attaches no session, and does not resize the PTY, asserted against server state
  before and after. `Deny` returns `Denied` with zero content bytes. A **nonexistent**
  session returns `NotFound`; because Q5 makes every live session in-scope there is no
  authorized-vs-unauthorized distinction to hide, so `Denied` and `NotFound` are
  unambiguous and the earlier draft's contradiction is resolved. A session-creating action
  returns `created_session_id`. `WriteInput` acknowledges only after the bytes land, and a
  PTY write failure surfaces `ActionFailed` rather than being dropped as `KeyInput` does
  today (`ipc_server.rs:7872-7911`). Over-cap input returns `TooLarge` *before* any prompt
  is raised. An old client (no `agent_api` bit) receives no `AgentActivity` frame.
- **GPUI headless** — `TabData.agent_active` renders the leading glyph; it coexists with
  `ai_indicator` and both remain visible; the AccessKit tab label includes the
  agent-active text; the consent dialog has accessible title/body/actions, defaults focus
  to Deny, and Escape denies.
- **E2E functional** — `tests/e2e/func/agent-read.sh` (CLI run from inside a Scribe pane
  reading its sibling, plus the deny path), `agent-world.sh`, `agent-action.sh`
  (asserting the created session id), `agent-write.sh`.
- **E2E visual** — one screenshot each for the tab agent icon and the consent dialog.
- **Packaging** — assert the `scribe` CLI is present in the Debian stable and dev asset
  lists and in the macOS bundle, and is on `PATH` from inside a Scribe pane.
- **Performance (constitution 4)** — `cargo bench -p scribe-server --bench agent_api`,
  200 warm iterations after 20 warmup, reporting p95: world ≤50 ms, viewport read
  ≤100 ms, 1000-line scrollback read ≤250 ms, serialized response ≤256 KiB. Idle cost:
  with all capabilities `Deny` no prompt, lease, or extraction path executes — asserted by
  a test that a request returns `Denied` without touching `Term`.

## Risks

- **Broad capability + weak identity.** Q2/Q3/Q5 accepted. Mitigation is honest
  documentation plus the indicator and audit; residual risk recorded, not engineered away.
  Server-issued agent tokens remain an additive upgrade path.
- **`agent_label` is self-asserted.** A hostile caller can claim to be anything. It is
  disclosure, not authentication, and the prompt and audit must present it as
  caller-supplied. Same-UID trust (Q3) already concedes this class.
- **Correlated action completion is the largest unknown.** GPUI runs actions later on its
  foreground queue (`main.rs:5137`) and today's `ActionDispatched` means routed only.
  Mitigation: the correlated path is its own foundational-tier item with a spike before
  the handlers depend on it. If the client round-trip proves unreliable the honest
  fallback is to cut `DispatchAction` from v1 — not to weaken the contract to `Queued`,
  which would contradict Q2.
- **Terminal-lock contention.** Hard line and byte caps, copy-under-lock/format-after, and
  a bounded semaphore owned by `agent_api::mod` (permits = 4, excess returns `Busy`).
- **`ipc_server.rs` contention across parallel workers.** Mitigated structurally: the
  single dispatch hook lands once in the foundational item and every handler lives in its
  own `agent_api` submodule.
- **Rollback.** All-`Deny` defaults mean a shipped-but-unwanted surface is inert; disabling
  removes every path from reach without a revert.

## Sequencing

Ordering is expressed as dependency edges. Two items may run in parallel only when their
files are disjoint **and** they share no new primitive.

**Foundational tier (each blocks its consumers; all must land before handler work):**

- *Agent contract types* — `scribe-common/src/agent.rs` with `AgentRequest`,
  `AgentResponse`, `AgentCapability`, `AgentError`, every DTO, the exhaustive
  `AutomationAction` mapping, and serialization tests. Blocks everything.
  Acceptance: the crate compiles with the exhaustive mapping, and the DTO tests assert
  every excluded field is absent from serialized output.
- *Agent config table* — `AgentApiConfig` with all-`Deny` defaults and every numeric
  clamped on load; server-side projection and `ConfigReloaded` refresh. Depends on the
  contract types. Blocks the policy engine, settings page, and CLI.
  Acceptance: defaults deserialize to all-`Deny`; an over-ceiling value loads clamped; a
  live edit changes behavior with no restart.
- *Agent wire variants + capability bit* — the additive `ClientMessage` / `ServerMessage`
  variants, `Hello`/`Welcome` `agent_api: bool`, and per-participant recording. Depends on
  the contract types. Blocks every handler, the client work, and the CLI.
  Acceptance: a client without the bit receives no agent-family frame, proven by test.
- *Agent dispatcher* — `agent_api/mod.rs` plus **the only three `ipc_server.rs` edits**:
  `is_transient_first_frame`, the dispatch arm, and `IpcServerState` wiring; module
  declarations in `lib.rs`/`main.rs`; the concurrency semaphore. Depends on the wire
  variants. Blocks every handler and serialises them out of `ipc_server.rs`.
  Acceptance: an `AgentRequest` reaches a stub handler and returns `Denied` under default
  config, registering no window and attaching no session.

**Core capability tier (all depend on the dispatcher; mutually parallel — disjoint files):**

- *Policy engine* — `agent_api/policy.rs`: mode resolution, prompt issue/correlate/park,
  60 s timeout, 500 ms burst keyed by `(agent_label, capability, target)`, queue cap 64,
  headless deny, `Always` persistence, refresh-cancels-pending. Blocks every handler and
  the consent dialog.
  Acceptance: unit-tested state machine covering all six behaviors above.
- *Bounded text extractor* — `agent_api/text.rs`. Blocks the read handler.
  Acceptance: golden-text tests for wrap, wide chars, images, hyperlinks, and truncation.
- *Activity leases* — `agent_api/activity.rs`, refcounted with dwell. Blocks the indicator
  and every handler that takes a lease.
  Acceptance: overlapping leases test proves no early clear; disconnect releases.
- *Correlated action transport* — `RunActionCorrelated` / `ActionCompleted` end to end,
  including client-side queue-capacity reservation. Depends on the wire variants; touches
  `scribe-client/src/{main,ipc_bridge}.rs`, so it is ordered **before** the tab indicator
  to avoid a second writer in `main.rs`. Blocks the action handler.
  Acceptance: an action reports `Completed` with `created_session_id` for
  session-creating actions; a disconnected client yields `ActionFailed`.

**Handler tier (each depends on policy; own submodule, mutually parallel):**

- *World and siblings handler* — `agent_api/world.rs`, aggregating from
  `IpcServerState.live_sessions`, `workspace_manager`, and `window_shares` under one
  short-lived ordered lock acquisition producing a `snapshot_id`; `Siblings` reuses the
  same snapshot filtered to the origin session's window. Acceptance: windows, workspaces,
  and sessions returned in one internally consistent snapshot matching what `ListWindows`
  reports at capture time; exactly one entry carries `is_caller: true` when a valid
  `origin_session_id` is supplied and none does otherwise; `Siblings` without a valid
  origin returns `NotFound`.
- *Screen read handler* — also depends on the extractor. Acceptance: returns viewport plus
  requested bounded scrollback with `title`/`cwd`, sets `truncated` past the cap.
- *Action handler* — also depends on the correlated transport. Acceptance: benign and
  destructive actions gate on their own capabilities; omitted target with two windows
  returns `AmbiguousTarget`.
- *Write-input handler* — Acceptance: over-cap returns `TooLarge` before any prompt;
  acknowledgement follows the completed write; a PTY failure returns `ActionFailed`.
- *Capabilities handler* — Acceptance: reports surface version and supported capabilities.
- *Audit emission* — depends on the dispatcher and policy; a single emission point in
  `agent_api/mod.rs`. Acceptance: a structured-log capture test asserts the six fields are
  present and no content substring appears.

**Client and consumer tier:**

- *CLI subcommand tree* — depends on the wire variants and config table; disjoint files,
  parallel with handlers. Acceptance: each subcommand emits the versioned envelope, exit
  codes match, and an old server produces `Unsupported` within 3 s.
- *Consent dialog* — depends on the policy engine and wire variants; touches
  `dialog.rs`/`ipc_bridge.rs`/`main.rs`, so ordered **after** the correlated action
  transport. Acceptance: dialog names the caller-supplied agent label and capability,
  defaults focus to Deny, Escape denies, AccessKit roles present.
- *Tab agent indicator* — depends on activity leases and the consent dialog (shared
  `main.rs`). Acceptance: leading glyph while leased, coexists with the AI indicator,
  clears after dwell, AccessKit label includes agent-active text.
- *Settings page* — depends on the config table; `settings/{model,apply,values,window}.rs`
  are disjoint from every other item. Acceptance: each capability mode is settable and
  applies live.

**Closing tier:**

- *Affordance generator* — `scribe agent skill`, rendered from the clap command tree and
  the live `AgentApiConfig`. Depends on the CLI. Blocks the affordance install.
  Acceptance: the output names every subcommand the binary actually exposes, proven by a
  test that walks the clap tree and fails if any subcommand is missing from the rendered
  text — this is the mechanism that makes drift impossible; a capability set to `Deny`
  renders as unavailable with its settings path instead of as a callable operation.
- *Agent affordance install* — the three installer extensions plus Pi tool registration.
  Depends on the affordance generator. Disjoint from every server item.
  Acceptance: a fresh install creates exactly one Scribe-owned skill file per present
  provider; re-running with unchanged output performs no write; a pre-existing file
  without the Scribe ownership marker is reported and left untouched; a version or policy
  change regenerates on next launch; `pi.registerTool()` exposes the operations as typed
  tools; every registered tool and the skill text no-op when `SCRIBE_SESSION_ID` is unset.
- *Packaging* — `scribe` CLI into Debian stable/dev assets and the macOS bundle. Depends
  on the CLI. Acceptance: package-presence test plus `PATH` reachability from a pane. No
  skill asset ships — it is generated on the target machine.
- *User documentation* — README plus the agent-API doc: JSON schemas, exit codes, policy
  config, same-UID limitation, egress disclosure. Depends on the CLI and packaging.
- *Performance verification* — depends on the handlers and extractor. Acceptance: the
  named bench meets every budget.
- *E2E functional and visual scripts* — depend on the CLI, handlers, dialog, and indicator.
  Includes an affordance-install script asserting idempotence and foreign-file refusal.
- *`lat.md/` sync + `lat check`* — depends on everything; single writer, lands last.

## Backlog Refinement

No backlog inputs. No `source_backlog`, `epic`, or `backlog` ids were supplied, and the P4
closure is empty. Nothing to refine, supersede, cover, or retire.

## Alignment fixes applied

Two parallel passes (spec↔plan alignment, plan quality). Every must-fix applied:

- **Wrong owner corrected (A, B, must).** Enumeration was assigned to `session_manager.rs`,
  whose map is a creation-time staging area cleared immediately. Now
  `IpcServerState.live_sessions` + `workspace_manager` + `window_shares`.
- **Forced serialization on `ipc_server.rs` broken (B, must).** Five handlers all editing
  one 13k-line file would have serialised the whole plan. Collapsed to a single
  `AgentRequest` wire variant and one foundational dispatcher owning the only three
  `ipc_server.rs` edits; handlers now live in their own `agent_api` submodules.
- **Old-client compatibility (B, must).** Broadcasting an unknown `AgentActivity` breaks
  the client's exhaustive `ServerMessage` match. Added the `Hello`/`Welcome`
  `agent_api: bool` bit, mirroring spec 010's `clipboard_gating`, gating both activity and
  prompt frames.
- **Prompt lifecycle planned end to end (A, B, must).** Was policy state with no wire, UI,
  routing, or persistence. Added `AgentPromptRequest`/`AgentPromptResponse`, the GPUI
  dialog with default-Deny focus and Escape-denies, target-client routing, headless deny,
  60 s timeout, queue cap 64, burst key, and `Always` persistence.
- **Correlated action completion designed, `Queued` fallback removed (A, B, must).** The
  fallback contradicted Q2. Added `RunActionCorrelated`/`ActionCompleted` with client-side
  queue-capacity reservation, made it its own foundational-tier item, and recorded that the
  honest failure mode is cutting `DispatchAction` from v1, not weakening the contract.
- **Activity race fixed (A, B, must).** A boolean broadcast let the first completion clear
  a second concurrent call. Replaced with reference-counted per-session leases plus a
  1500 ms dwell, resolving Open Question 1.
- **Workspace enumeration added (A, must).** Goal G2a promised workspaces; the DTO set had
  none. Added `AgentWorkspace` and `AgentWorldSnapshot`.
- **`AgentScreenText` gained `title` and `cwd` (A, must)** — US1 AC2 requires the response
  to identify the pane.
- **Input bounds and typed write failure (A, B, must).** Added `max_input_bytes` (4 KiB
  default, checked *before* prompting) and `ActionFailed` propagation, since `KeyInput`
  silently drops both today. Resolves Open Question 3: policy plus the byte cap suffice; no
  separate paste-style risk check.
- **All limits clamped (B, must).** `max_response_bytes` contradicted the stated 256 KiB
  hard cap; every numeric now has a ceiling applied on load, and the extractor semaphore
  has a named owner and permit count.
- **`enabled` master flag dropped (A, must).** It produced `Disabled` where the spec's
  acceptance criteria require `Denied`. All-`Deny` defaults are the off switch; there is no
  `AgentError::Disabled`.
- **Test-plan contradictions resolved (B, must).** The Deny→`Denied` vs `NotFound`
  ambiguity is gone because Q5 makes every session in-scope; "no handler reachable when
  disabled" is restated as the observable "returns `Denied` without touching `Term`".
- **Packaging added (A, B, must).** `scribe-cli` ships in neither the Debian assets nor the
  macOS bundle today, so the promised consumer would not have existed on a real install.
- **User-facing docs added (A, B, must).** Q4's disclosure and the same-UID limitation need
  a user-reachable home, not only `lat.md`.
- **Settings and accessibility made concrete (B, must).** Named the real files, and added
  AccessKit coverage for both the tab label and the consent dialog.
- **CLI contract specified (A, must).** Versioned JSON envelope, stdout/stderr split, exit
  codes, and the 3 s old-server timeout mapping to `Unsupported`.
- **`agent_label` defined (A, B, must).** Self-asserted, bounded, presented as
  caller-supplied in both prompt and audit; explicitly not authentication.
- **Exhaustive action→capability mapping (B, must).** A compile-time exhaustive `match` so
  a future `AutomationAction` cannot default to the weaker gate.
- **`OutOfScope` dropped with a reason (A, should).** Q5 eliminated the condition; recorded
  as superseding that Spec Review decision.
- **Named benchmark command (A, B, must)** — `cargo bench -p scribe-server --bench
  agent_api`, with sample count, warmup, and per-operation p95 targets.
- **Sequencing acceptance criteria tightened (B, should).** Every item now carries an
  observable pass/fail condition so none becomes a vague bead.
- **DTO optionality (B, should).** Optional source fields are `Option` with
  `skip_serializing_if`, and audit gained a tracing target and event name.
- **Controller/participant identity omitted deliberately (A, should)** — another user's
  device and login name serve no agent story; recorded as a privacy decision rather than an
  oversight.

Added after the analysis gate, on the user's question "how are we exposing this API to our
LLMs?": the plan built the API but no discovery layer, so an agent with shell access would
never learn the command exists.

The first attempt — three hand-authored skill documents — was rejected on review as
half-done, correctly. It reintroduced the exact drift class this plan flags as must-fix
elsewhere (three copies of a contract that silently lie the moment a capability is added),
it left the primary use case at four chained calls with the ergonomics pushed into prose,
and it papered over two real API holes with documentation. Replaced with:

- *Affordance generator* — `scribe agent skill` renders the text from the clap command
  tree and live policy, so the binary is the single source of truth and a tree-walking
  test makes drift impossible rather than merely unlikely. Generated text also reports
  ungranted capabilities as unavailable instead of inviting a wasted turn.
- *`Siblings` request and `scribe agent siblings`* — collapses "read the pane next to me"
  from four calls to one.
- *`origin_session_id` on every request and `is_caller` on every session* — the agent
  locates itself through the API instead of string-matching an env var against a list.
  Orientation only; no capability decision depends on it.
- Installers write generated output; no skill asset is packaged. Pi gets typed tools via
  `pi.registerTool()`, verified available at load and in `session_start`.

Verified before committing to the paths: `~/.claude/skills/` and `~/.codex/skills/` both
exist and are populated on a real install; `dist/pi-extension.ts` is currently
listener-only, so tool registration is new code in a file Scribe already owns.

Not applied: requirement-ID renumbering of every acceptance criterion (should-fix). The
bead DAG provides the traceability it was meant to give, and renumbering mid-flight would
invalidate the line references throughout this document.
