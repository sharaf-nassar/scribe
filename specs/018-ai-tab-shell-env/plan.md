# Plan: ai-tab-shell-env

## Architecture Approach

Implements the seven binding clarification decisions from
`specs/018-ai-tab-shell-env/spec.md` (Q1-Q7). The design in one pass:

**Structured AI launch, server-owned argv (Q1).** `ClientMessage::CreateSession`
(crates/scribe-common/src/protocol.rs:278-304) gains an additive
`#[serde(default)] ai_launch: Option<AiLaunchSpec>` field carrying
`{provider, resume_mode, conversation_id}`. The client DUAL-WRITES: every AI
launch sends both `ai_launch` AND the legacy `command` argv
(`[shell, "-lic", "exec …"]`) in the same frame. A new server prefers
`ai_launch` and ignores the legacy argv; an old server — including the
un-restartable live one (Principle 7) — deserializes the unknown field away
(serde default-tolerant, precedent: `Hello.clipboard_gating` / 
`Welcome.participant_id`, protocol.rs:381-393, :745-765) and runs the legacy
argv, i.e. exactly today's behavior. No silent-failure window.
`REMOTE_PROTOCOL_VERSION` bumps 3→4 (protocol.rs:15-27); the exact-match
remote gates (ipc_server.rs:3460 for LAN, RemoteHandshakeReply for tailnet)
make cross-version remote pairs refuse loudly with `IncompatibleVersion`.

**Server-side shell resolution, passwd-first (Q1, decided here).** For
structured AI launches the server resolves the shell on the HOST as
passwd account shell → daemon `$SHELL` → `sh`. Rationale: passwd is the
live source of truth (`chsh` takes effect without re-login), while the
daemon's `$SHELL` is PAM-session-inherited at daemon start and can be stale
(spec Constraints, "Daemon env"; validated: daemon does have
`SHELL=/bin/bash`). This inverts `default_shell_program()`'s env-first order
(crates/scribe-common/src/shell.rs:19-39) for AI launches only; a new
`ai_login_shell_program()` (or parameterized resolver) in scribe-common
keeps the existing resolver untouched for plain tabs and the sibling
launchers (`shell_command_argv`, `spawn_background_command`, main.rs:5855-5868
— explicitly out of behavior-change scope).

**Real login+interactive invocation with pre-exec preamble (Q3).** The server
builds, per shell kind:

- bash: `bash -lic '<preamble>; exec <binary> [resume-args]'` where the
  preamble is
  `[ -n "${SCRIBE_INTEGRATION_SCRIPT:-}" ] && source "$SCRIBE_INTEGRATION_SCRIPT"`.
  The script path crosses server→shell ONLY as the `SCRIBE_INTEGRATION_SCRIPT`
  env var — never string-interpolated (validated: naive interpolation breaks
  silently). `SCRIBE_AI_TAB=1` is injected into the PTY env; scribe.bash's
  AI mode skips its own startup-file sourcing block
  (dist/shell-integration/bash/scribe.bash:36-60 — otherwise /etc/profile +
  profile run twice, verified) and all prompt/PS1/PS0/DEBUG/delta-hook wiring,
  while KEEPING restore-delta sourcing (:202-208). `-i` must stay: the script
  no-ops unless its interactive guard passes. The dead `--rcfile` insertion for
  command-bearing bash launches (session_manager.rs:884-889) is removed — no
  argv combination ships where one flag disables another.
- zsh / fish: `-lic` is login+interactive on both (validated). Integration
  still arrives via `ZDOTDIR` / `XDG_DATA_DIRS` injection
  (shell_integration.rs:179-199). `SCRIBE_AI_TAB=1` makes scribe.zsh /
  scribe.fish skip their pre-rc restore-delta consumption and prompt/baseline
  wiring; the server-built preamble (shell-kind-specific syntax) applies the
  delta instead, AFTER login files. Exact preamble syntax per shell (the
  bash `[ -n … ] && source` guard is NOT valid fish):
  - zsh (POSIX-form, valid zsh):
    `[ -n "${SCRIBE_RESTORE_ENV_DELTA_FILE:-}" ] && [ -f "$SCRIBE_RESTORE_ENV_DELTA_FILE" ] && . "$SCRIBE_RESTORE_ENV_DELTA_FILE" && command rm -f "$SCRIBE_RESTORE_ENV_DELTA_FILE"; exec <binary> …`
  - fish:
    `if test -n "$SCRIBE_RESTORE_ENV_DELTA_FILE"; and test -f "$SCRIBE_RESTORE_ENV_DELTA_FILE"; source "$SCRIBE_RESTORE_ENV_DELTA_FILE"; command rm -f "$SCRIBE_RESTORE_ENV_DELTA_FILE"; end; exec <binary> …`
  - fish additionally restores the user's original `XDG_DATA_DIRS` pre-exec:
    the server passes `SCRIBE_ORIG_XDG_DATA_DIRS` and the preamble re-exports
    it (erasing when originally unset), so Scribe's vendor-conf.d prepend does
    not leak into the exec'd AI process's descendants.
- nushell: `nu -l -i -c "exec <binary> …"` with NO integration and no
  preamble — documented limitation (validated on nu 0.114.1: vendor autoload
  loads only in the REPL path). For AI launches the server also skips the
  `XDG_DATA_DIRS` vendor injection entirely (nothing would consume it) and
  skips restore-delta staging (no consumer → per-launch temp-file leak
  otherwise). Plain-tab nushell autoload (shell_integration.rs:201-210) is
  not regressed.
- PowerShell: FOLDED into the Unknown arm for AI launches (decided here):
  `pwsh -lic` is invalid PowerShell syntax and Windows AI-tab work is out of
  scope (spec Non-Goals). Documented consequence: a PowerShell-default user's
  AI tab launches the binary through the unknown-shell path with no
  integration; the plain-tab PowerShell arm is untouched.
- Unknown: `[shell, "-lic", "exec …"]`, no integration, no restore-delta
  staging, debug log (preserves session_manager.rs:824-826 behavior).

**Restore delta applied post-login, delta wins (Q4, FR-008).** For AI tabs on
bash the delta is sourced by scribe.bash (AI mode keeps :202-208, which runs
after the login files because the preamble runs after `-l` startup); for
zsh/fish the preamble itself sources-and-deletes
`$SCRIBE_RESTORE_ENV_DELTA_FILE`. This is a deliberate mechanism split —
bash consumes the delta inside scribe.bash's AI mode, zsh/fish consume it in
the server-built preamble — but both paths source the staged file exactly
once, AFTER login files, and DELETE it; no path leaves the temp file behind.
Shell kinds with no delta-apply path (nushell, PowerShell, Unknown) get no
staging at all (see above). Either way the delta lands AFTER login
files and BEFORE `exec` — delta wins, satisfying spec-006 FR-008. The
plain-tab zsh/fish pre-rc non-conformance is NOT fixed here; it is filed as a
separate spec-006 defect bead. Baseline emission (Q3d, planning decides):
DROP it for AI tabs — `SCRIBE_AI_TAB=1` skips `__scribe_emit_env_baseline`
(scribe.bash:374-395) and the zsh/fish equivalents. AI tabs exec away
pre-prompt and never emit deltas, so a baseline buys nothing and costs a
helper fork on the tab-open hot path (Principle 4). The preamble unsets
`SCRIBE_AI_TAB` and `SCRIBE_INTEGRATION_SCRIPT` before `exec` so they do not
leak into the AI binary's descendants.

**`ai_tab_cwd` goes live, client-resolved, server-guarded (Q5).** The CLIENT
resolves the concrete cwd from state it already holds — `pane` (default) =
focused session's ChromeMetadata.cwd (tracked via CwdChanged,
main.rs:8708-8710, read-back :4893-4901); `project_root` =
WorkspaceSlot.project_root → pane cwd → home; new `home` variant = `None`
(server falls to `$HOME`) — and sends it in the EXISTING `cwd` field of
CreateSession (no protocol change needed for this story; it even works
against old servers). Fallback chain enumerated end-to-end: no focused pane
(first tab in a fresh window), AutomationAction/CLI-triggered AI tabs (no
visible focus), and panes whose shell never emitted OSC 7 all resolve the
focused-session cwd to `None` → the request carries `cwd: None` → server
`$HOME`; resume tabs use the SAME fresh-create resolution (the resumed
conversation, not the prior session's directory, carries the context).
The SERVER keeps its final `is_dir → $HOME` guard
(session_manager.rs:793) — remote-safe because the client's cached values
originated from the server. Fresh creates only; cold-restart relaunch keeps
the persisted LaunchRecord.cwd (restore_replay.rs:595-633). Ships as its own
bead within the epic.

**Alternatives rejected:**

- *Client-side fix-only (keep client argv, patch flags)*: leaves shell
  resolution on the controlling client — wrong shell and wrong-filesystem cwd
  for remote sessions (spec Critical Q1), and keeps two argv construction
  sites plus server token-sniffing as permanent invariants.
- *`--rcfile` without `-l` (spec OQ3a)*: empirically disproven — the script
  is sourced but `shopt login_shell` stays false, so scribe.bash's own
  profile emulation (:52-60) skips login files on Linux; does not deliver
  the login env without deeper script surgery, and never gives real login
  semantics (`$0`, profile guards).
- *Capability negotiation instead of dual-write*: the local socket has no
  version handshake to extend (client+server ship together; drift is handled
  by `stale_server_reason`, server_lifecycle.rs:66), and adding one for a
  single additive field is strictly more moving parts than dual-write, which
  degrades to exactly today's behavior on the live server with zero
  negotiation.

**Plain-tab login-shell follow-up (scribe-ad2): REJECT.** Feature 018's
login-and-interactive launch architecture must not be generalized to plain
tabs. An AI shell runs login files, applies a server-owned post-login preamble,
and immediately `exec`s the provider, so it deliberately skips resident-shell
prompt hooks, baseline capture, and recurring env-delta integration. A plain
shell remains alive and needs all of that integration. Sharing a `-l` flag
would therefore leave two distinct startup modes while changing the behavior
of every ordinary tab.

| Shell kind | Current plain-tab startup and integration | Login-unification impact |
|---|---|---|
| Bash | `build_shell` starts a non-login interactive shell with `--rcfile scribe.bash`; the script sources `~/.bashrc` on Linux and emulates login-profile order on macOS, then applies a restore delta and captures the baseline. | Real `-li` startup would require a new plain-tab source mode to avoid sourcing profiles twice. On Linux it would replace the long-standing `.bashrc` contract with first-profile-wins login behavior; feature 018's AI mode cannot be reused because it exits before prompt and capture wiring. |
| Zsh | A no-argument interactive launch uses the `ZDOTDIR` `.zshenv` bootstrap, restores the user's `ZDOTDIR`, loads the integration, then lets normal interactive rc finish before first-prompt restore and baseline capture. | `-l` adds login-only startup files and their side effects around that sequence. The transport still works, so the flag adds compatibility risk without removing a separate integration path. |
| Fish | A no-argument interactive launch discovers Scribe through vendor `conf.d`; the script removes the injected `XDG_DATA_DIRS` entry and defers restore/baseline work until the first prompt after user config. | `-l` activates login-conditional user setup for every tab. Vendor injection and first-prompt restore remain necessary, so no architecture or matrix is eliminated. |
| Nushell | A no-argument REPL discovers the vendor autoload script and applies its JSON restore path in the resident shell. | `-l -i` can remain a REPL, but nushell startup/autoload behavior is version-sensitive and still needs its unique integration and restore dialect. The extra mode changes user config behavior without a shared preamble benefit. |
| PowerShell | The plain path is its own `-NoLogo -NoExit -File scribe.ps1` contract, with profile-aware restore and prompt wrapping inside the script. | POSIX `-lic` is not a PowerShell launch contract. A login change would need a separate PowerShell design while retaining `-File` ordering, so it cannot participate in a uniform conversion. |
| Unknown | Scribe passes no invented startup flags and lets the PTY-backed shell use its native interactive behavior; integration remains unsupported. | No portable login or command flag exists. Adding `-l`/`-i` could prevent the shell from starting, violating the graceful unsupported-shell contract. |

The spec-006 contract also depends on reconstructing the same startup baseline,
then layering the persisted delta after startup so the delta wins (FR-008).
Changing plain tabs to login startup would change that baseline beneath
already-persisted `LaunchRecord`/envelope associations. Handoff-held PTYs would
keep the old process, but the next cold restore would silently respawn under a
different rc contract; no persisted startup-mode marker or opt-out exists to
migrate that boundary safely.

There is no remote-correctness gain to offset the change. A default plain tab
is already resolved by the host server through `default_shell_program()`
(`SHELL` first, then the host passwd entry), and an explicit custom command
remains explicit. Reusing feature 018's passwd-first resolver would only change
which host source wins. It would also make every ordinary tab pay login-profile
costs such as nvm/conda/mise setup, without feature 018's AI-only performance
budget or an escape hatch.

Revisit only for a demonstrated plain-tab user need under a new opt-in spec
that defines per-shell startup ordering, remote/custom-command scope,
spec-006 baseline migration, compatibility fallback, and an every-tab latency
budget. It is not a continuation of feature 018.

Constitution check: P1 typed `AiLaunchSpec` + explicit fallback chains; P2
plain tabs untouched (Q2), AI tabs gain parity; P3 manual verification named
per story, only existing round-trip/test files extended; P4 ~1s budget with
named `--ai-tab-only` command; P5 full-env + restore-delta flow into AI CLIs
recorded as deliberate user intent (Q7), `SHELL` joins EXCLUSION_SET;
P6 untouched (no network); P7 dual-write compat documented, live server never
restarted, lat.md synced. Tension noted: P2 vs Q3e — bash users whose
interactive setup lives only in `~/.bashrc` without profile chaining will see
AI tabs stop reading it (real login semantics); this is the user's explicit
redesign directive and is documented, not mitigated.

Story priorities: US-1 and US-2 are the MVP slice; US-3 (`ai_tab_cwd`) ships
in parallel as its own beads; US-4 is the verification matrix over the same
mechanism. macOS is out of scope for verification this feature (Linux dev
machine); the code paths stay OS-neutral and the Darwin branch of
scribe.bash's emulation block is untouched for plain tabs.

## Affected Components

- **crates/scribe-common/src/protocol.rs** — `AiLaunchSpec` struct (+
  wire-stable serde derives), `CreateSession.ai_launch: Option<AiLaunchSpec>`
  with `#[serde(default)]` (:278-304); `REMOTE_PROTOCOL_VERSION` 3→4 with
  doc-comment history entry (:15-27); `AiResumeMode` relocates here from
  crates/scribe-client/src/restore_state.rs:137 (see Data Model — persisted
  TOML representation stays byte-identical); extend the existing msgpack
  round-trip test module (mod tests, :1441+) for the new field (changed
  existing coverage — allowed under Principle 3).
- **crates/scribe-common/src/shell.rs** — add passwd-first resolver for AI
  launches (`ai_login_shell_program()` or a parameterized
  `resolve_default_shell_path` caller); `default_shell_program()` unchanged.
- **crates/scribe-common/src/ai_state.rs** — no semantic change;
  `AiProvider`/`resume_args` reused by the server-side argv builder. Confirm
  serde derives are wire-suitable for protocol reuse (or mirror a small wire
  enum in protocol.rs if drift risk is unacceptable).
- **crates/scribe-client/src/ipc_bridge.rs** — `SessionLaunch` gains
  `ai_launch: Option<AiLaunchSpec>` (:1005-1030); `create_session` forwards
  it on the frame (:1116-1130). Dual-write happens here or in the callers —
  callers pass both structured spec and legacy argv.
- **crates/scribe-client/src/main.rs** — the two AI argv call sites collapse
  into a structured-launch builder: AI action handlers (:1531-1541) call a
  new `create_ai_tab(provider, resume)` instead of
  `create_tab(Some(ai_tab_command(…)))`; `ai_tab_command` (:5870-5879)
  becomes the LEGACY-argv builder used only for the dual-write compat field;
  `create_tab` (:3430-3455) grows the structured path and the resolved
  `cwd` (US-3); `launch_binding_for` (:568-579) is rewired so AI launches
  construct `LaunchKind::Ai` bindings DIRECTLY from the structured spec —
  argv-sniffing (`detect_ai_command`) stays only for custom-command
  classification. Without this rewire AI tabs cold-restart as plain shells
  (validated regression, spec Q1). Launch identity is UNCHANGED: the
  structured path mints the same `LaunchBinding` and its `launch_id` still
  rides as `env_envelope_id` on the frame, so server-side restore-delta
  staging (session_manager.rs:786-788) and env-envelope keying work
  identically for structured launches.
- **crates/scribe-client/src/restore_replay.rs** — `command_argv` (:188-210)
  changes in lockstep: AI replay variants return the structured spec (plus
  legacy dual-write argv) instead of building `[shell, -lic, exec …]`
  client-side; `shell_single_quote` of conversation_id moves server-side;
  `ReplayLaunch`/`queue_from_launch_record` (:595-633) carry the structured
  spec through to `create_session`. `restore_state.rs` `LaunchKind`
  (:129-133) already persists `{provider, resume_mode, conversation_id}` —
  no migration (spec Non-Blocking, OQ10 dissolved); `AiResumeMode` (:137)
  becomes a re-export of the relocated scribe-common type.
- **crates/scribe-server/src/ipc_server.rs** — `handle_create_session`
  (:6203) forwards `ai_launch` into `SessionLaunchRequest`. Remote gate
  (:3460) needs no code change; refusal behavior follows from the version
  bump.
- **crates/scribe-server/src/session_manager.rs** —
  `SessionLaunchRequest.ai_launch` (:240-250); `ResolvedShell::for_request`
  (:821-828) gains the AI arm: structured launch → passwd-first host
  resolution (ignores `command[0]`); server-owned argv builder producing the
  per-shell-kind `-lic '<preamble>; exec …'` command (new function beside
  `build_shell`, :872-935); REMOVE the dead `--rcfile` insertion for
  command-bearing bash launches (:884-889); `ai_provider_hint` set directly
  from the structured field, `command_ai_provider_hint` token-sniffing
  (:840-846) retained only for legacy/custom argv — tab-title/AI-state
  hinting reads the structured hint, and the built command string still
  contains `exec <binary>` so token-based discovery keeps working for
  legacy-argv launches; PowerShell AI launches fold into the Unknown arm;
  restore-delta staging is SKIPPED for shell kinds with no apply path
  (nushell/PowerShell/Unknown, debug-logged); shell resolution is a typed
  fallback chain (passwd → daemon `$SHELL` → `sh`) with the chosen tier
  logged; `build_pty_options` (:740-796) injects `SCRIBE_AI_TAB=1` and
  `SCRIBE_INTEGRATION_SCRIPT` for AI launches; cwd guard (:793) unchanged;
  the FR-008 contract comment block (:1352-1360) is updated for the AI-tab
  preamble consumer.
- **crates/scribe-server/src/shell_integration.rs** — expose the bash script
  path for env-var injection on AI launches (reuse
  `integration_script_path`, :159-170); AI launches SKIP `inject_bash`'s
  `ENV=<script>` injection (:172-177 — dead for non-POSIX bash and
  `SCRIBE_INTEGRATION_SCRIPT` supersedes it; plain tabs keep it); fish AI
  launches additionally get `SCRIBE_ORIG_XDG_DATA_DIRS` for the preamble's
  pre-exec restore; nushell AI launches get no `XDG_DATA_DIRS` injection;
  `build_env` (:141-210) unchanged for plain tabs; note (not fix) the
  daemon-env `ZDOTDIR`/`XDG_DATA_DIRS` reads (:181, :188-210) —
  pre-existing bug class, out of scope.
- **dist/shell-integration/bash/scribe.bash** — AI mode: when
  `SCRIBE_AI_TAB=1`, skip the startup-file block (:36-60), skip
  PS1/PS0/DEBUG/prompt-hook wiring and baseline emit (:374-395), keep
  restore-delta sourcing (:202-208).
- **dist/shell-integration/zsh/scribe.zsh, zsh/.zshenv,
  fish/vendor_conf.d/scribe.fish** — `SCRIBE_AI_TAB=1` skips their pre-rc
  restore-delta consumption (scribe.zsh:139-142, scribe.fish:150-154) and
  prompt/baseline wiring, so the preamble's post-login apply is the only
  consumer and the file is not deleted early.
- **crates/scribe-server/src/env_store/delta.rs** — add `SHELL` to
  `EXCLUSION_SET` (:31-77); add `ENV`, `ZDOTDIR`, `XDG_DATA_DIRS`,
  `SCRIBE_AI_TAB`, `SCRIBE_INTEGRATION_SCRIPT` alongside (Q7 amendment:
  control-flow vars, not staleness). These additions affect PLAIN-tab env
  capture too (any shell stops persisting these names) — Q7-sanctioned, one
  documenting sentence in lat.md. Update any existing exclusion-list tests
  (:276-280 area) — changed existing coverage, allowed.
- **crates/scribe-common/src/config.rs** — `AiTabCwd` gains `Home` variant
  (:1118-1147); doc comments updated.
- **crates/scribe-client/src/settings/{model,apply,values,window}.rs** —
  model.rs choice list adds `("home", "Home")` (:342-345); apply.rs
  string-match arm adds `"home"` (:424-430 — the one non-compile-checked
  spot); values.rs `enum_str` (:121) handles the new variant automatically;
  window.rs section mapping (:2905-2918, `ai_section` matching
  `terminal.ai_tab_cwd`) verified and the per-variant description text kept
  in lockstep with shipped behavior (US-3: no dead options).
- **tools/perf-ab-rig/run-perf-ab.sh** — new `--ai-tab-only` timed mode
  reusing `open_owned_tab` session-appearance polling (:600-619).
- **lat.md/server.md** — correct :572 (falsely asserts uniform post-rc delta
  apply across shells); update Sessions/Session Creation + Env Persistence
  for the structured-launch pipeline, including an explicit list of the
  integration features inherently MOOT in AI tabs (prompt-command OSC marks,
  per-prompt env-delta emission, baseline-emit timing — the shell execs away
  pre-prompt) and the nushell/PowerShell no-integration limitations.
- **specs/006-persist-terminal-env/** — amendment note recording the AI-tab
  delta-apply mechanism (preamble consumer, post-login, delta wins) and the
  known plain-tab zsh/fish pre-rc non-conformance filed as a defect bead.
- **lat.md/client.md** — correct :1424 (describes `ai_tab_cwd` as already
  live and client-side `-lic` shell resolution as current); rewrite for
  structured launch.
- **lat.md/common.md** — Configuration/Terminal: `AiTabCwd` `home` variant;
  protocol: `ai_launch` field + v4.
- **lat.md/settings.md** — AI tab working dir choice list.

## Data Model

- **`AiLaunchSpec`** (protocol.rs, wire type):
  `{ provider: AiProvider, resume_mode: AiResumeMode, conversation_id: Option<String> }`
  — deliberately field-aligned with the persisted `LaunchKind::Ai`
  (restore_state.rs:129-133) so client binding construction and replay are
  1:1 conversions. Recorded deviation from the spec Q1 wording: cwd is NOT
  part of `AiLaunchSpec` — CreateSession already has a `cwd` field and US-3
  reuses it, which is precisely what lets the cwd story work against old
  servers too. `AiResumeMode` today lives in the CLIENT
  (restore_state.rs:137, TOML-persisted, no `rename_all`): it MOVES to
  scribe-common and restore_state.rs re-exports it, keeping the persisted
  TOML representation byte-identical (variant names `New`/`Resume`
  unchanged; NO new `rename_all` on the persisted type). A round-trip case
  asserts the TOML variant names are unchanged.
- **`AiTabCwd`** (config.rs:1118-1131): third variant `Home` — an escape
  hatch preserving today's de facto behavior, not a compat shim. Serde
  default stays `Pane` (validated: already the default) — NO persisted-state
  or config migration needed.
- **No LaunchRecord/restore-state migration** (validated): `LaunchKind::Ai`
  already persists structured fields and regenerates argv at replay; only
  `CustomCommand` stores raw argv, and custom commands keep the legacy
  `command` path unchanged.
- New PTY env vars for AI launches: `SCRIBE_AI_TAB=1`,
  `SCRIBE_INTEGRATION_SCRIPT=<abs script path>` (bash only). Both are unset
  by the preamble before `exec` and join EXCLUSION_SET.

## API / Interface Changes

- **CreateSession** (protocol.rs:278-304): additive
  `#[serde(default)] ai_launch: Option<AiLaunchSpec>`. No field removed;
  `command` stays and keeps its meaning for custom commands and legacy
  compat.
- **Dual-write semantics:**

  | Client | Server | Effective behavior |
  |---|---|---|
  | old | old | Legacy argv, today's behavior (baseline) |
  | old | new | No `ai_launch` → server runs legacy `command` path unchanged (minus the dead bash `--rcfile` insertion, which never took effect for `-l` launches) |
  | new | old | Old server drops the unknown field, runs the dual-written legacy argv → exactly today's behavior; covers the un-restartable live server |
  | new | new | Server prefers `ai_launch`, owns argv, legacy `command` ignored for spawn |

- **REMOTE_PROTOCOL_VERSION 3→4** (protocol.rs:27): CreateSession is
  remote-visible; exact-match gates mean a v3 peer and a v4 peer refuse
  loudly with `IncompatibleVersion` (LAN gate ipc_server.rs:3460; tailnet
  RemoteHandshakeReply). Documented consequence: mixed-version remote pairs
  stop interoperating until both sides update — consistent with the v2/v3
  precedent.
- **No local-socket version constant exists** — client and server ship
  together; local drift is already handled by `stale_server_reason`
  (crates/scribe-client/src/server_lifecycle.rs:66) prompting a server
  refresh. Dual-write covers the window where the live server predates the
  field.
- **`SessionLaunch`** (ipc_bridge.rs) and **`SessionLaunchRequest`**
  (session_manager.rs:240-250) grow the same optional field — internal,
  non-wire.

## Testing Strategy

Per Principle 3, NO new test files or suites. Two existing-coverage
extensions are explicitly justified:

1. **protocol.rs round-trip tests** (mod tests, :1441+): the msgpack-named
   round-trip suite already covers message compatibility; adding
   `CreateSession` cases with `ai_launch` present/absent (and an
   old-frame-without-field decode) is changed existing coverage protecting
   the dual-write contract — the single highest-risk compat surface.
2. **env_store/delta.rs exclusion tests** (:276-280 area): the suite
   enumerates exclusion behavior; extending it for `SHELL` (and the other
   added names) keeps the asserted-intended semantics honest.

Everything else is manual, per user story, on the `scribe-dev` flavor
(fully isolated by executable stem: socket/config/state/keystore). Named
commands — NEVER `just server` / `just restart-server` (stable flavor →
live server; Principle 7):

- Build/install: `just ready`, then `just install-dev`; run
  `/usr/bin/scribe-dev`.
- **US-1 (login env)**: add `export SCRIBE_SENTINEL=profile` to
  `~/.bash_profile`; open an AI tab (ctrl+alt+c) in scribe-dev;
  `tr '\0' '\n' < /proc/$(pgrep -n claude)/environ | grep SCRIBE_SENTINEL`.
  Also verify PATH resolution itself, not just env inheritance: install a
  shim `claude` in a profile-PATH-only directory and confirm
  `readlink /proc/<pid>/exe` resolves to the shim.
  Repeat with the sentinel in `~/.zprofile` / fish `config.fish` after
  pointing the passwd shell at zsh/fish (`chsh -s`), restoring afterwards.
- **US-2 (integration parity / delta restore)**: with env persistence on in
  the dev flavor, export a marker var in a plain tab, cold-restart the DEV
  server only, confirm the relaunched AI tab's `/proc/<pid>/environ`
  contains the marker (delta applied post-login). Assert the staged delta
  temp file EXISTS in the per-flavor `$XDG_RUNTIME_DIR` staging dir before
  the AI tab consumes it, and is deleted after startup. Confirm no
  double-sourcing: sentinel `echo` in a `/etc/profile.d`-visible file
  appears once in the AI tab's startup. Leak check (zsh/fish): the exec'd
  AI process's `/proc/<pid>/environ` shows the user's ORIGINAL `ZDOTDIR`
  and `XDG_DATA_DIRS` (not Scribe's injected values) and contains no
  `SCRIBE_AI_TAB` / `SCRIBE_INTEGRATION_SCRIPT`.
- **US-3 (ai_tab_cwd)**: `cd /tmp` in the focused pane, open an AI tab,
  check `readlink /proc/<pid>/cwd` for each of `pane` / `project_root` /
  `home` set live via the settings UI (no restart).
- **US-4 (shell matrix)**: repeat US-1/US-2 spot checks under zsh and fish;
  nushell: confirm the AI binary launches and document the no-integration
  limitation; unknown shell: confirm spawn + debug log.
- **Perf (Q6)**: `tools/perf-ab-rig/run-perf-ab.sh --ai-tab-only --live` —
  measures tab-open keystroke → first PTY byte emitted by the exec'd
  program, using a stub AI binary that prints a marker immediately (so the
  real CLI's own startup is excluded by construction); the timed path
  contains no fixed sleeps. Budget: within ~1s on this machine's profile.
  Verification on 2026-08-01 used project-built release binaries inside a
  disposable `--network none` container with Xvfb + openbox, an isolated
  `/run`, `HOME`, config, state, data, and cache, and no host Scribe socket.
  Result: **587.627 ms — PASS** against the 1000 ms soft budget.
- **Regression sweep**: `just ready`, then `just e2e-func
  func/shell-integration.sh`, `just e2e-func func/env-persistence.sh`,
  `just e2e-func func/cold-restart.sh` — plain-tab integration, env
  persistence, and cold-restart replay must stay green.
- **Remote refusal**: optional spot check that a v3 peer is refused with
  `IncompatibleVersion` (log inspection), no new harness.
- **PowerShell non-regression**: one smoke check that the plain-tab
  PowerShell arm still injects `-File` (its arm gates on `args.is_empty()`,
  so AI launches never used it).

## Risks

- **Old-live-server window**: the live server cannot be restarted; until the
  user approves an upgrade, new clients hit the old server. Dual-write
  guarantees exactly-today's behavior in that window; the new pipeline
  activates only after an approved server upgrade. Rollback is equally
  cheap: revert the client to legacy-argv-only — old and new servers both
  accept it.
- **Double-sourcing regression**: if the `SCRIBE_AI_TAB=1` gate in
  scribe.bash regresses, /etc/profile + profile run twice (verified
  failure mode). Mitigation: the gate short-circuits the entire
  startup-file block, and US-2's manual check includes a
  sourced-once sentinel.
- **Preamble quoting**: the script path and delta path cross as env vars,
  never interpolated; the only interpolated user data is `conversation_id`,
  which moves server-side and must reuse the `shell_single_quote` discipline
  (restore_replay.rs) — with fish/nushell syntax variants where applicable.
- **zsh ZDOTDIR daemon-env read** (shell_integration.rs:181 reads the
  DAEMON's `ZDOTDIR` for `SCRIBE_ORIG_ZDOTDIR`): pre-existing bug class,
  explicitly OUT of scope here (noted in lat.md correction, candidate
  follow-up bead) — the daemon env is ZDOTDIR-empty in practice, and fixing
  it belongs with the plain-tab pipeline.
- **`--rcfile` removal fallout**: a hand-typed custom command `["bash"]`
  (interactive non-login) previously got integration via the inserted
  `--rcfile`; after removal it does not. Spec mandates the removal
  ("no argv combination ships where one flag disables another"); documented
  behavior change, tiny blast radius.
- **nushell no-integration**: documented limitation; risk is user surprise,
  mitigated by lat.md/common.md documentation.
- **Flavor split PATH crossing**: a full login PATH can put the stable
  `scribe` flavor's tools ahead of `scribe-dev`'s (or vice versa) inside an
  AI tab; the server-injected absolute `SCRIBE_HOOK_HELPER`
  (session_manager.rs:765-767) is authoritative and shields the hook
  scripts from PATH order — one documented sentence in lat.md.
- **Login-profile latency variance**: nvm/conda/mise chains can push past
  the ~1s budget on other machines; Q6 fixes the budget to this machine's
  profile and forbids an escape-hatch setting — measured, not mitigated. If
  the rig shows a miss, that is a finding to surface, not silently absorb.
- **Remote pairs refuse after bump**: v3↔v4 remote sessions stop working
  until both ends update; loud `IncompatibleVersion`, consistent with
  precedent, called out in docs.
- **Cold-restart regression**: if `launch_binding_for` is not rewired, AI
  tabs restore as plain shells (validated). The rewire ships in the same
  bead as the client structured launch, and US-2's cold-restart check
  covers it.
- **Baseline-drop interaction**: skipping baseline emission for AI tabs is
  safe (they never emit deltas; hook_ingress.rs:186,284 only gates delta
  ingestion) but must not leak into plain tabs — the skip is gated on
  `SCRIBE_AI_TAB=1` only.

## Sequencing

Ordered work items (1:1 bead candidates). Dependency edges explicit;
items marked [P] are parallel-safe with their siblings once their blockers
are done.

1. **Protocol and structured-launch plumbing** — `AiLaunchSpec`,
   `CreateSession.ai_launch` with serde(default), `REMOTE_PROTOCOL_VERSION`
   3→4 + doc history; `AiResumeMode` moves to scribe-common with a
   re-export from restore_state.rs, persisted TOML representation
   byte-identical (`New`/`Resume` names unchanged, no new `rename_all` on
   the persisted type) plus a round-trip case asserting the TOML variant
   names; `SessionLaunch`/`SessionLaunchRequest` field plumb-through
   (ipc_bridge.rs, ipc_server.rs `handle_create_session`,
   session_manager.rs request struct); msgpack round-trip extension.
   Acceptance: the server accepts and stores `ai_launch`, and spawn
   behavior is byte-identical until the server argv item lands.
   Foundation. Blocks: 2b, 3.
2a. **Shell-script AI-mode gates** — scribe.bash / scribe.zsh / scribe.fish
   `SCRIBE_AI_TAB=1` gates: skip startup-block re-source, prompt/PS1/PS0/
   DEBUG/delta-hook wiring, and baseline emit; bash keeps restore-delta
   sourcing; zsh/fish skip their pre-rc delta consumption. The gates are
   no-ops until the server sets the env var, so this is deployable
   standalone. Blocks: 6 (measurement), 8. [P with 1]
2b. **Server-owned AI argv and env** — passwd-first host shell resolution
   as a typed fallback chain (passwd → daemon `$SHELL` → `sh`) with the
   chosen tier logged; per-shell `-lic '<preamble>; exec …'` builders with
   the exact zsh/fish preamble syntax from Architecture Approach; fold
   PowerShell AI launches into the Unknown arm; remove dead bash
   `--rcfile` insertion; `ai_provider_hint` from the structured field;
   `SCRIBE_AI_TAB` / `SCRIBE_INTEGRATION_SCRIPT` PTY env injection; skip
   `inject_bash` `ENV=` for AI launches; fish `SCRIBE_ORIG_XDG_DATA_DIRS`
   restore; no `XDG_DATA_DIRS` injection for nushell AI launches;
   acceptance criterion: restore-delta staging is SKIPPED (debug-logged)
   for shell kinds with no delta-apply path (nushell/PowerShell/Unknown) —
   no per-launch temp-file leak; documented consequence sentence for
   AI-binary-not-found (shell prints command-not-found and the tab exits).
   Blocked by: 1. Blocks: 6 (measurement), 7b, 8. [P with 3]
3. **Client structured launch, binding rewire, dual-write** — AI action
   handlers → structured `create_ai_tab`; `ai_tab_command` demoted to
   legacy dual-write builder; `launch_binding_for` constructs
   `LaunchKind::Ai` directly; `restore_replay::command_argv` +
   `queue_from_launch_record` replay the structured spec with dual-write;
   conversation-id quoting removed client-side. Acceptance: the structured
   path mints the same `LaunchBinding` and sends the same `launch_id` as
   `env_envelope_id`, so restore-delta staging keys identically. Build new
   launch values rather than mutating shared config/binding state
   (immutability discipline). Blocked by: 1. Blocks: 4b, 6 (measurement),
   8. [P with 2b]
4a. **`AiTabCwd::Home` variant and settings UI** — `Home` variant in
   config.rs; model.rs `("home","Home")` choice; apply.rs `"home"` arm;
   values.rs/window.rs verified; per-variant description text matches
   shipped behavior. Blocks: 4b. [P with everything]
4b. **Client cwd resolution wired into the create path** — pane =
   ChromeMetadata.cwd of the focused session, project_root =
   slot.project_root → pane cwd → home, home = `None`; feeds the existing
   CreateSession `cwd` field (works against old servers); live setting
   reads flow through `TerminalView::reload_config` (main.rs:1342), so the
   next AI tab honors a settings change with no restart; enumerate the
   no-focused-session fallback (fresh window / automation-triggered → `cwd:
   None` → server `$HOME`). Build new request values, never mutate shared
   state (immutability discipline). Blocked by: 3, 4a. Blocks: 7b, 8.
5. **EXCLUSION_SET additions** — `SHELL` plus `ENV`, `ZDOTDIR`,
   `XDG_DATA_DIRS`, `SCRIBE_AI_TAB`, `SCRIBE_INTEGRATION_SCRIPT`; update
   existing exclusion tests; one documenting sentence that plain-tab
   capture is affected (Q7-sanctioned). Independent. Blocks: 8.
   [P with everything]
6. **Perf rig `--ai-tab-only` mode and measurement** — rig mode times
   tab-open keystroke → first PTY byte from the exec'd program using a
   stub AI binary that prints a marker immediately; no fixed sleeps on the
   timed path; reuses `open_owned_tab` session-appearance polling. This
   item OWNS the measurement and records the number; the verification gate
   only checks it was run and met budget. Script authoring is independent
   [P]; running the measurement is blocked by: 2a, 2b, 3. Blocks: 8.
7a. **Follow-up beads** — file the spec-006 zsh/fish plain-tab FR-008
   defect bead; the plain-tab login-shell unification bead; the DUAL-WRITE
   RETIREMENT bead (trigger: once the live server runs protocol v4, drop
   the legacy argv twin and the argv-sniffing fallback; `launch_binding_for`
   must not regress to sniffing); sibling-launcher (`shell_command_argv`,
   `spawn_background_command`) and split-pane-cwd follow-ups. Independent.
   Blocks: nothing. [P with everything]
7b. **Docs and lat.md sync** — server.md:572 correction + AI-tab-moot
   feature list, client.md:1424, common.md, settings.md; update the FR-008
   contract comment (session_manager.rs:1352-1360); amendment note in
   specs/006-persist-terminal-env/; flavor-split PATH sentence. Blocked
   by: 2b, 3, 4b (documents final behavior). Blocks: nothing — runs in
   parallel with the verification gate.
8. **Manual verification gate** — execute the full Testing Strategy command
   list on the `scribe-dev` flavor (US-1..US-4 matrices incl. staged-file
   and ZDOTDIR/XDG_DATA_DIRS leak checks, PowerShell smoke, cold-restart
   replay); confirm the perf measurement from the rig item met budget; run
   `just ready` and the regression sweep (`just e2e-func
   func/shell-integration.sh`, `func/env-persistence.sh`,
   `func/cold-restart.sh`); record results in the epic. Blocked by: 2a,
   2b, 3, 4b, 5, 6. Final gate.

Dependency check: 1→{2b,3}; 3→4b; 4a→4b; {2a,2b,3}→6; {2b,3,4b}→7b;
{2a,2b,3,4b,5,6}→8; 7a and 7b block nothing — acyclic.

## Backlog Refinement

None — no P4 sources.

## Target Epic

New epic to be created: ai-tab-shell-env.

## Alignment fixes applied

Two audit passes (A: spec↔plan alignment, B: plan quality) were applied.

- **A1 (must)** — launch_id/env-envelope identity stated in Affected
  Components (main.rs bullet) and sequencing item 3; US-2 gains the
  staged-temp-file-exists-then-deleted assertion.
- **A2 (must)** — ZDOTDIR/XDG_DATA_DIRS leak verification added to the US-2
  manual matrix (`/proc/<pid>/environ` original-values check) and covered by
  item 2b's fish restore + gate item 8.
- **A3 (must)** — cwd fallback chain enumerated end-to-end in Architecture
  Approach (no focused pane, automation/CLI-triggered, no-OSC-7 panes,
  resume tabs → `cwd: None` → server `$HOME`; resume uses fresh-create
  resolution).
- **A4 (should)** — settings/window.rs:2905-2918 added to Affected
  Components with per-variant description-text requirement.
- **A5 (should)** — lat.md/server.md deliverable now lists the AI-tab-moot
  integration features explicitly.
- **A6 (should)** — US-1 gains the shim-binary PATH-resolution check via
  `readlink /proc/<pid>/exe`.
- **A7 (should)** — Data Model records the deviation: cwd stays in
  `CreateSession.cwd`, not `AiLaunchSpec` (works against old servers).
- **A8 (should)** — bash-vs-zsh/fish delta-sourcing mechanism split made
  explicit; both paths delete the staged file exactly once.
- **A9 (should)** — exact zsh and fish preamble syntax spelled out (bash
  guard noted as invalid fish).
- **A10 (should)** — Risks gains the scribe/scribe-dev flavor-split PATH
  crossing line; server-injected absolute `SCRIBE_HOOK_HELPER` is
  authoritative.
- **A12 (should)** — story priority ranking (US-1/US-2 MVP, US-3 parallel,
  US-4 verification matrix) and macOS out-of-scope sentence added.
- **A13 (should)** — clause added: tab-title/AI-state hinting reads the
  structured hint; `exec <binary>` stays discoverable for legacy argv.
- **A14 (should)** — immutability-discipline notes added to sequencing
  items 3 and 4b.
- **A15 (should)** — delta.rs bullet documents that EXCLUSION_SET additions
  affect plain-tab capture and are Q7-sanctioned.
- **B1 (must)** — `AiResumeMode` relocation plan corrected: moves to
  scribe-common, re-exported from restore_state.rs, persisted TOML
  byte-identical (no new `rename_all`), TOML-variant round-trip assertion;
  "wire-stable serde attributes" phrasing removed.
- **B2 (must)** — item 2b acceptance criterion: skip restore-delta staging
  for shell kinds with no delta-apply path (nushell/PowerShell/Unknown,
  debug-logged), preventing a per-launch temp-file leak.
- **B3 (must)** — PowerShell AI launches decided: folded into the Unknown
  arm (`pwsh -lic` invalid; Windows out of scope) with documented
  consequence; plain-tab arm untouched.
- **B4 (must)** — item 4 split: 4a Home variant + settings UI ([P]); 4b cwd
  resolution in the create path (blocked by 3, 4a).
- **B5 (must)** — dual-write retirement follow-up bead named in item 7a
  with explicit trigger and no-sniffing-regression guard.
- **B6 (should)** — decided in item 2b: AI launches skip `inject_bash`'s
  `ENV=` injection; `SCRIBE_INTEGRATION_SCRIPT` supersedes it.
- **B7 (should)** — decided: fish preamble restores
  `SCRIBE_ORIG_XDG_DATA_DIRS` pre-exec; nushell AI launches get no
  `XDG_DATA_DIRS` injection at all.
- **B8 (should)** — perf criterion rewritten: keystroke → first PTY byte
  from a marker-printing stub binary, no fixed sleeps on the timed path;
  item 6 owns the measurement, item 8 only verifies it ran and met budget.
- **B9 (should)** — item 7 split: 7a follow-up beads ([P], blocks nothing);
  7b lat.md/docs sync (after 2b/3/4b, no edge into 8).
- **B10 (should)** — item 8 (and Testing Strategy) adds `just ready` plus
  `just e2e-func` regressions: func/shell-integration.sh,
  func/env-persistence.sh, func/cold-restart.sh.
- **B11 (should)** — item 2b gains the typed shell-resolution fallback
  chain (passwd → `$SHELL` → `sh`, logged tier) and the documented
  command-not-found consequence.
- **B12 (should)** — item 1 acceptance line: server accepts/stores
  `ai_launch`; spawn behavior byte-identical until item 2b lands.
- **B13 (should)** — item 2 split: 2a shell-script AI-mode gates (no-op
  until the env var is set, [P] with 1); 2b server Rust (blocked by 1).
- **B14 (should)** — item 4b names `TerminalView::reload_config`
  (main.rs:1342) as the live-read mechanism and enumerates the
  no-focused-session fallback.
- **B15 (should)** — docs item updates the FR-008 in-code contract comment
  (session_manager.rs:1352-1360) and adds an amendment note under
  specs/006-persist-terminal-env/.

Sequencing edges re-verified after the 2a/2b, 4a/4b, 7a/7b splits:
acyclic (see Dependency check line in Sequencing).
