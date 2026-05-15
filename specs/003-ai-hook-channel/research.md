# Research: AI Hook Channel

**Phase 0 output for [plan.md](./plan.md). Each decision below resolves an unknown surfaced by the Technical Context.**

---

## Decision 1 — Helper packaging: a single Rust binary, `scribe-hook-helper`

**Decision**: Ship one statically-linked Rust binary (`scribe-hook-helper`), built as a new workspace crate alongside `scribe-server`. Provider adapter scripts (`dist/ai-hook-*.sh`) are 10–20 line POSIX `/bin/sh` files that read the AI tool's hook stdin payload, extract the relevant fields with `python3 -c '…'` (already a Scribe install dependency per `dist/setup-codex-hooks.sh`), and `exec scribe-hook-helper --provider=… --event=… …` exactly once. The helper does the connect + msgpack frame + write + close.

**Rationale**:

- The wire is length-prefixed msgpack (`crates/scribe-common/src/framing.rs:1-73`). Implementing length-prefixed framed-msgpack correctly from POSIX shell is a non-starter; Python is feasible but introduces a 50–150 ms cold-start per hook event (large enough to threaten the 200 ms p95 budget under interactive load when multiple hooks fire per turn).
- The helper must satisfy FR-008 (no stdout) and FR-009 (no stderr) with **zero** exceptions, in every error path. A Rust binary makes that easy (one `process::exit(0)` after `let _ = …` everywhere). Shell scripts that exec other commands can leak stderr on `command not found`, syscall failure, or partial-write conditions; suppressing all of those reliably requires defensive sh hygiene that's both fragile and easy to regress.
- Rust gives the helper a sub-10 ms cold start on warm caches (cargo profile `release-cli` precedent across the workspace). Combined with a 100 ms write timeout (FR-012), the helper's contribution to end-to-end latency is dominated by socket round-trip, not process startup.
- A workspace crate ships through the same `cargo build --release` and Debian/macOS packaging pipeline as `scribe-server` (`crates/scribe-server/Cargo.toml:68-112`). One entry added to the deb assets list; nothing new in the build infrastructure.
- The helper is small (~150 lines) and reuses `scribe-common::framing`, so it's effectively a thin client over the existing IPC primitives.

**Alternatives considered**:

- *Inline emit in each provider adapter script*: rejected. Writing msgpack from shell requires either `python3` (slow) or a bespoke binary anyway; once a binary exists, every adapter should use it.
- *Python single-file helper*: rejected on latency. 100 ms cold-start under hook-heavy turns is too close to the SC-002 budget, and the helper's bytes-on-the-wire logic deserves to be in the same language as the IPC framing it depends on.
- *POSIX sh + `nc -U`*: rejected because `nc -U` is not POSIX-portable (Debian's `netcat-openbsd` vs `netcat-traditional` differ, macOS uses a different netcat entirely) and framing a length-prefixed msgpack payload via shell heredoc-ery is brittle.

---

## Decision 2 — Endpoint topology: reuse the existing `server.sock` with new transient `ClientMessage` variants

**Decision**: The hook ingress is **not** a new socket. The helper connects to the existing `server.sock`, sends one `ClientMessage::HookEvent { … }` framed via the existing msgpack framing, and disconnects. The server handles `HookEvent` in a transient-connection branch (no `Welcome`, no window registration, no per-message reply expected by default).

**Rationale**:

- The transient-one-shot connection pattern is already in production: `ClientMessage::CheckForUpdates` and `ClientMessage::ListReleases` are dispatched on incoming connections via dedicated `handle_transient_*` branches at `ipc_server.rs:519-533`. `HookEvent` slots into exactly that pattern with no new infrastructure.
- A dedicated hook socket would mean two listeners, two paths to manage, two lifetime stories (especially across the existing zero-downtime handoff at `scribe-server::handoff`). One socket halves the operational surface.
- Path discovery is solved already: `scribe-server` knows its socket path and can pass it via env var without adding a new path-resolution scheme.
- The existing connection accept loop (`ipc_server.rs:350-380`) is hardened (timeouts, error logging, concurrent accept) and the helper benefits from that hardening for free.

**Alternatives considered**:

- *Dedicated `hooks.sock`*: rejected per above; doubles operational surface for no win. The only theoretical advantage — isolating hook ingress from main IPC failure modes — is not material because both sockets would be served by the same `scribe-server` process.
- *Abstract Unix socket* (`\0scribe-hooks-…`): rejected for portability — macOS doesn't support abstract Unix sockets.
- *Datagram socket (`SOCK_DGRAM`)*: rejected. Stream framing already exists and works; datagrams would mean a second framing scheme. No reliability gain because both are local IPC.

---

## Decision 3 — Wire protocol: msgpack via existing `scribe_common::framing`

**Decision**: `ClientMessage::HookEvent` is a new variant on the existing serde-tagged `ClientMessage` enum (`crates/scribe-common/src/protocol.rs:78-211`). It carries a sub-enum `HookEventKind` with one variant per event type from spec FR-005. Serialization uses `rmp_serde::to_vec_named` exactly like every other `ClientMessage`. Frame layout is unchanged: 4-byte big-endian u32 length prefix + payload, 64 MiB cap (`framing.rs:11`).

**Rationale**:

- Reusing the framing module means zero new bytes-on-the-wire format. Forward/backward compatibility, length-prefix validation, the 64 MiB cap, and the `read_message`/`write_message` async helpers (`framing.rs:25-71`) all apply automatically. The protocol is already battle-tested by `PtyOutput` traffic.
- `serde` + named-msgpack keeps the protocol self-describing and amenable to schema evolution (renaming a field; adding optional fields). The existing IPC protocol uses the same pattern across 60+ `ServerMessage` variants.
- The helper binary depends on `scribe-common` directly, so the wire types compile in both ends with no manual JSON schema drift risk.

**Alternatives considered**:

- *Newline-delimited JSON*: rejected. Adding a second wire format on the same socket means the server's accept loop must sniff the format. Pointless complication.
- *Sled/cap'n proto/etc.*: rejected. Solving a problem we don't have. msgpack already works.

---

## Decision 4 — Discovery via environment variables: `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID`

**Decision**: Scribe injects two environment variables into every PTY it spawns, at the existing build site (`crates/scribe-server/src/session_manager.rs:538`, the `env` HashMap inside `build_pty_options`):

```
SCRIBE_HOOK_SOCK=<absolute path to the server's existing server.sock>
SCRIBE_SESSION_ID=<the SessionId UUID for this PTY>
```

Adapter scripts and the helper read both. Both must be set for the helper to attempt emission. Absence of either → silent exit 0 (FR-003).

Shell integration scripts (`dist/shell-integration/*`) propagate these unchanged from the inherited parent env; no per-shell modification needed beyond confirming inheritance behaves correctly (bash/zsh/fish/nu/powershell all inherit env by default).

**Rationale**:

- The PTY env is the single point where Scribe can guarantee both values reach hook subprocesses, because hook subprocesses inherit env from the AI tool, which inherits from the user's shell, which inherits from the PTY's initial environment.
- Two variables, not one combined string, because the helper's no-op decision (`SCRIBE_HOOK_SOCK` unset) is conceptually distinct from the routing key (`SCRIBE_SESSION_ID`). Bundling would tempt clever-parsing in shell.
- Naming follows the existing convention: `SCRIBE_SHELL_INTEGRATION=1` is already set by Scribe (`shell_integration.rs:72`). `SCRIBE_HOOK_SOCK` / `SCRIBE_SESSION_ID` follow the same prefix.
- The chosen session-id type is `SessionId` from `crates/scribe-common/src/ids.rs:57-59`, a UUID-v4 minted per PTY at `session_manager.rs:298`. It is **stable across AI tool restarts inside the same pane** (user does `claude → exit → claude` in the same Scribe pane: same `SessionId`) — which is the correct semantic for routing UI updates. The AI tool's own `conversation_id` is carried in the event payload for downstream record-keeping but is not the routing key.

**Alternatives considered**:

- *Single combined env var* like `SCRIBE=sock=/path,session=uuid`: rejected. Parsing cost in shell; harder to no-op cleanly when only one half is set.
- *Use `XDG_RUNTIME_DIR` to derive socket path implicitly*: rejected. Forces convention; explicit path is clearer and survives non-standard runtime dirs.
- *Use AI tool's own `conversation_id` as routing key*: rejected. It changes every time the user re-launches the AI tool inside the same pane, which would orphan tab indicator state. `SessionId` is the right key.

---

## Decision 5 — Stop-hook classifier port

**Decision**: Port the regex heuristics from `dist/detect-claude-question.sh:40-49` and `dist/detect-codex-question.sh:40-48` into a single Rust module `crates/scribe-server/src/stop_classifier.rs`. The classifier is provider-independent and exposes one entry point: given the assistant's last-message text (truncated payload from the `SessionStopped` event variant), return `AiProcessState`'s `state` field as either `idle_prompt` or `waiting_for_input`.

**Rules to preserve** (lifted verbatim from current shell sources):

1. Trailing `?` on any of the last ~20 non-empty lines, after stripping fenced code blocks → `waiting_for_input`.
2. Match against question phrases: `would you like`, `should i`, `do you want`, `which option`, `please (choose|select|pick)`, `how (should|would|do|to)`, `what (should|would|do)`, `let me know`, `your (choice|preference|call)` → `waiting_for_input`.
3. Match against approval/review phrases: `please (review|approve)`, `review and approve`, `once approved`, `approve (the|this|it|above)`, `waiting for.*(approval|review)`, `i'll execute.*once approved`, `confirm (the|this|before)`, `ready to (proceed|execute|start)`, `proceed\?` → `waiting_for_input`.
4. Default → `idle_prompt`.

**Rationale**:

- Behavioral parity with today's user experience requires the same heuristics. Differential behavior would be a user-visible regression unrelated to the channel migration.
- One module, tested in isolation, replaces ~120 lines of duplicated shell + jq + grep. The tests can run as plain `cargo test` instead of bash regression harnesses.
- Centralizing the heuristic in Rust means Codex and Claude both get the same classification logic for free, satisfying FR-013a's "single, provider-independent classifier" requirement.

**Alternatives considered**:

- *Keep regex heuristic in adapter scripts*: rejected per Spec Clarification Q2 (user chose Option B).
- *Drop the heuristic entirely and rely on structured signals*: rejected per Spec Clarification Q2 (would regress prose-question detection).
- *Use a more sophisticated NLP approach*: rejected. The shell heuristic works well enough today; replacing it with anything non-trivial is out of scope.

---

## Decision 6 — Adapter script shape and per-provider work

**Decision**: One thin POSIX `/bin/sh` adapter per supported event source. Each adapter is registered at install time as the hook command in the AI tool's own settings (e.g. `~/.claude/settings.json` for Claude Code, `~/.codex/hooks.json` for Codex). Each adapter does exactly:

1. Read the AI tool's hook event JSON from stdin into a shell variable. (Or rely on Python one-liner to extract just the needed fields — same pattern used by the existing `dist/setup-claude-hooks.sh` UserPromptSubmit hook.)
2. Decide the `--event` kind and extract any payload fields.
3. `exec scribe-hook-helper --provider=X --event=Y [--field=value …]`.

Adapter files: `dist/ai-hook-claude.sh`, `dist/ai-hook-codex.sh`, `dist/ai-hook-statusline.sh`. The statusline adapter is for the Claude Code statusline subprocess (FR-021), which is not strictly a hook in CC's vocabulary but follows the same shape.

**Rationale**:

- The adapter's only job is provider-specific JSON shape translation. That's irreducibly per-provider (CC sends `tool_name` + `tool_input`; Codex sends different fields). Centralizing translation in the helper would mean every provider's JSON schema lives in the Rust binary — fine in principle but harder to maintain than a thin shell layer that hides the schema noise.
- Keeping adapters as `/bin/sh` (not bash) maximizes portability across the existing CI environments.
- Each adapter must pass through stdin closure correctly. The simplest contract: extract needed fields, exec helper, never write to stderr (FR-009), never write to stdout (FR-008). The exec semantics handle exit code 0 inheritance.

**Alternatives considered**:

- *One adapter binary per provider compiled into Rust*: rejected. Means separate provider binaries plus a statusline binary instead of small shell adapters plus one shared binary; loses the lightweight install footprint advantage of shell adapters.
- *Single mega-adapter with `--provider` selection*: rejected. Hook commands are registered in each AI tool's own settings file; mapping one adapter to many providers means more conditional logic in install scripts. Two small provider adapters keep the install logic simple.

---

## Decision 7 — Helper binary distribution path

**Decision**: `scribe-hook-helper` ships at `/usr/share/scribe/scribe-hook-helper` (and the dev-flavor `/usr/share/scribe-dev/…`), with mode 755, in the same Debian asset table as the existing `scribe-claude-statusline.sh`. macOS DMG includes it in `${RESOURCES_DIR}/`. Adapter scripts locate it via a hard-coded absolute path derived at install time by the setup scripts.

**Rationale**:

- Asset table at `crates/scribe-server/Cargo.toml:68-112` already exists with one entry per dist file. Adding `scribe-hook-helper` is one line per flavor.
- Hard-coding the absolute path in the installed adapter (substituted at install time by `dist/setup-*-hooks.sh`) matches how `setup-claude-hooks.sh` already substitutes the path to `detect-claude-question.sh` into Claude's settings JSON.
- Putting the binary under `share/` (not `bin/`) keeps it out of the user's PATH. It's only meant to be invoked by Scribe-installed hooks, not directly by the user.

**Alternatives considered**:

- *`/usr/lib/scribe/`*: pure preference; `/usr/share/scribe/` matches the directory the existing scripts already live in.
- *Embed as a binary blob inside `scribe-server`*: rejected. Embedding doubles `scribe-server`'s on-disk size and complicates the in-place upgrade story (handoff would need to re-extract).
- *Install at `~/.scribe/bin/`*: rejected. Per-user paths complicate the multi-user Debian story.

---

## Decision 8 — Helper write timeout and failure semantics

**Decision**: The helper sets a 100 ms total deadline covering connect + write + close. On deadline expiry, any in-progress connection is dropped, no bytes are written or partially written-and-flushed, and the process exits 0 silently. No retries.

**Rationale**:

- FR-012 requires a "bounded short time budget" with abandon-on-timeout. 100 ms is comfortably above warm-cache loopback Unix-socket round-trip (sub-millisecond) and gives the server scheduler latency under heavy load. It is well below the 200 ms p95 end-to-end UI budget (SC-002).
- No retries because (a) the event is best-effort by design (FR-003, FR-004 silently no-op), (b) a retry that succeeds outside the 200 ms budget is not user-visible, (c) the hook subprocess must not block the AI tool.
- Silent exit 0 on every failure path is enforced by Rust's type system: the helper's main function returns `()` and every fallible call site uses `let _ = …`, never `?`.

**Alternatives considered**:

- *Retry once on `EAGAIN`*: rejected. Adds latency variance for marginal recovery benefit; the hook will fire again on the next state transition.
- *Longer (1 s) deadline*: rejected. Burns budget for no win; one server-side hiccup costs the user a visible delay.
- *Per-stage deadlines (connect 30 ms, write 70 ms)*: rejected as premature optimization. Single deadline is easier to reason about.

---

## Decision 9 — Test layout and harness reuse

**Decision**:

- **Unit tests** in `crates/scribe-server/src/stop_classifier.rs` cover every heuristic rule from Decision 5. Tests live inline (`#[cfg(test)] mod tests`).
- **Integration tests** in `crates/scribe-server/tests/hook_channel_roundtrip.rs` use the existing in-process server spawn pattern from `crates/scribe-server/tests/replay_roundtrip.rs` plus the `scribe-test::ipc` helpers (`crates/scribe-test/src/ipc.rs:18-42`) to: spawn server, connect a client, send `ClientMessage::HookEvent`, assert the corresponding `ServerMessage::AiStateChanged` arrives on the client subscription.
- **Offline shell regressions** in `tests/install/ipc-hook-regressions.sh` model after `tests/install/codex-context-regressions.sh`: feed mock AI tool stdin JSON into each adapter script, mock `scribe-hook-helper` as a `/bin/sh` echo-stdout binary, assert the adapter calls it with the expected `--provider` and `--event` arguments and exits 0.
- **End-to-end** verification is manual per [quickstart.md](./quickstart.md). No new PTY-capture E2E test is required for v1 because the integration test exercises the full server-side path and the adapter scripts are validated by the offline regression harness.

**Rationale**:

- This three-tier layering mirrors what already exists in the workspace: cargo unit/integration tests for Rust code, `tests/install/` for shell scripts, `crates/scribe-test/` IPC helpers as the bridge.
- No new harness infrastructure is required. The biggest gain over the OSC pipeline is testability: the classifier becomes unit-testable in pure Rust rather than tested only by feeding fake JSON through shell + jq + grep.

**Alternatives considered**:

- *Add full PTY E2E*: rejected for v1. The new path doesn't traverse the PTY for hook events; PTY tests would only validate env var injection, which the integration test can do more cheaply.
- *Skip offline shell regressions*: rejected. Adapter scripts are still where most subtle bugs hide (jq pipeline mistakes, quoting issues); they deserve their own targeted tests.

---

## Open items deferred from this phase to implementation

None blocking. Two minor implementation refinements are noted for the implementer:

- **Provider-set in `scribe-common::ai_state::AiProvider`** already covers `ClaudeCode` and `CodexCode`. No new variants needed for the spec's supported providers.
- The deb asset table gap noted by the landing-zone map (`codex-hook-common.sh` not shipped via stable .deb) becomes moot because that file is deleted by this feature.
