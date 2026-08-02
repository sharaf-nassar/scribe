# Spec: ai-tab-shell-env

## Problem Statement

AI tabs (Claude Code via ctrl+alt+c, Codex via ctrl+alt+x, and their resume
variants) launch through a client-built argv of
`[<shell>, "-lic", "exec <binary> [resume-args]"]`
(`ai_tab_command`, crates/scribe-client/src/main.rs:5870-5879). This design
has three defects:

1. **Bash shell integration silently never attaches to AI tabs.** The server
   prepends `--rcfile <scripts>/bash/scribe.bash` for bash
   (`build_shell`, crates/scribe-server/src/session_manager.rs:884-889), but
   bash's `-l` flag makes it a login shell, and a login shell ignores
   `--rcfile` entirely (verified on this machine). So for bash AI tabs the
   integration script — including its restore-delta sourcing
   (dist/shell-integration/bash/scribe.bash:202-207) and env-baseline /
   prompt-hook emission — never runs. Plain tabs (`bash --rcfile <script>`,
   no `-l`) do source it, so AI tabs and plain tabs diverge silently.
   In particular, the env-persistence restore path
   (`SCRIBE_RESTORE_ENV_DELTA_FILE`, session_manager.rs:786-788) is a no-op
   for bash AI tabs: the staged temp file is never sourced.

2. **`terminal.ai_tab_cwd` is dead config.** The setting is parsed
   (`AiTabCwd`, crates/scribe-common/src/config.rs:1123-1131, field at
   :1147) and editable in the settings UI
   (crates/scribe-client/src/settings/{model,apply,values,window}.rs), but
   no runtime code consumes it. The client always sends `cwd: None` for AI
   tabs (`create_tab`, crates/scribe-client/src/main.rs:3448), and the
   server falls back to `$HOME`
   (session_manager.rs:793). The documented default, `pane` ("inherit the
   focused pane's CWD"), is therefore never honored — and the behavior
   users actually get (always `$HOME`) is not even a representable value
   of the enum.

3. **Redesign directive (user):** AI tab launching must launch whatever
   `$SHELL` is set to, such that the ENTIRE user environment loads — for
   bash, the standard login startup files (`/etc/profile`, then the first
   of `~/.bash_profile` / `~/.bash_login` / `~/.profile`) load normally —
   instead of the current hardcoded client-side shell resolution
   (`default_shell_program`, crates/scribe-common/src/shell.rs:19-39,
   resolved in the CLIENT process: `$SHELL` → passwd entry → `sh`) combined
   with the server-side rcfile override that conflicts with login-shell
   startup. The redesign must preserve shell-integration attachment for AI
   tabs and reconcile with each shell's integration mechanism (bash
   rcfile/`ENV`, zsh `ZDOTDIR`, fish/nushell `XDG_DATA_DIRS`).

The net user-visible symptoms today: AI CLIs can start with an incomplete
environment relative to the user's real login shell, persisted terminal env
is not restored into bash AI tabs, and AI tabs always start in `$HOME`
regardless of the focused pane's directory or the `ai_tab_cwd` setting.

## Goals

- AI tabs launch the user's shell as a real login+interactive shell
  (`bash -lic '<preamble>; exec <binary> …'`; zsh/fish via `-lic`) so the
  full user environment (PATH, API keys, profile exports) is loaded
  before the AI binary runs (Principle 2: consistent UX; Principle 7:
  documented, compatible change). Decided: Clarifications Q3.
- The client sends a structured `ai_launch` field (provider + resume
  flag/conversation + cwd) on CreateSession; the SERVER owns argv
  construction and resolves the shell from the HOST's env/passwd.
  Dual-write compat (legacy `command` argv in the same frame) keeps old
  servers — including the un-restartable live one — on exactly today's
  behavior; `REMOTE_PROTOCOL_VERSION` bumps 3→4. Decided:
  Clarifications Q1.
- Shell integration attaches to AI tabs on bash/zsh/fish via a pre-exec
  source-preamble (script path crossing server→shell as the
  `SCRIBE_INTEGRATION_SCRIPT` env var, never string-interpolated) with
  an AI-tab mode (`SCRIBE_AI_TAB=1`) that skips double-sourcing and
  prompt wiring; nushell AI tabs get no integration (documented
  limitation). Decided: Clarifications Q3.
- The env-persistence restore delta (`SCRIBE_RESTORE_ENV_DELTA_FILE`) is
  applied via the pre-exec preamble AFTER login files (delta wins,
  spec-006 FR-008) on bash, zsh, and fish. Decided: Clarifications Q4.
- `terminal.ai_tab_cwd` becomes a live setting with a new `home`
  variant: the CLIENT resolves the concrete cwd from state it already
  holds (`pane` default = focused session's ChromeMetadata.cwd;
  `project_root` = slot.project_root → pane cwd → home) and the server
  keeps its final `is_dir → $HOME` guard (Principle 1: typed, explicit
  fallback; Principle 2). Decided: Clarifications Q5.
- Performance budget (Principle 4): an AI tab reaches the AI CLI prompt
  within ~1s of tab-open on this machine's profile (excluding the AI
  CLI's own startup), measured via a new
  `tools/perf-ab-rig/run-perf-ab.sh --ai-tab-only` mode. Decided:
  Clarifications Q6.
- lat.md sections for sessions, env persistence, and terminal config are
  updated to match the new launch pipeline, including correcting
  lat.md/server.md:572 (falsely asserts uniform post-rc delta apply) and
  lat.md/client.md:1424 (Principle 7).

## Non-Goals

- No change to plain-tab launch semantics (decided, Clarifications Q2:
  AI tabs only). Plain-tab login-shell unification is a follow-up bead.
- No fix here for the zsh/fish plain-tab FR-008 non-conformance (delta
  applied pre-rc, baseline captured pre-rc — Clarifications Q4); it is
  filed as a separate spec-006 defect bead.
- No escape-hatch/fast-path setting for slow login profiles
  (Clarifications Q6), and no AI-tab opt-out of the restore delta
  (Clarifications Q7).
- No change to the AI hook channel contract (`SCRIBE_HOOK_SOCK` /
  `SCRIBE_SESSION_ID` / `SCRIBE_HOOK_HELPER`, session_manager.rs:750-767)
  — these are injected directly into the PTY env and are inherited by the
  exec'd AI binary independent of shell startup files.
- No new AI providers, no changes to resume semantics
  (`resume_args`, crates/scribe-common/src/ai_state.rs:91-100:
  `claude --resume`, `codex resume`).
- No Windows/PowerShell AI-tab work beyond not regressing the existing
  PowerShell integration path (build_shell PowerShell arm,
  session_manager.rs:890-902).
- No automated test-suite mandate: per Principle 3, test code is written
  only when explicitly requested; this spec documents manual verification
  paths in acceptance criteria instead.
- No daemon environment redesign (the scribe-server systemd user unit
  stays env-minimal; dist/scribe-server.service has no `Environment=`
  lines, and the client imports only six GUI vars —
  crates/scribe-client/src/server_lifecycle.rs:280-331).

## Backlog Inputs

None.

## Target Epic

No existing epic covers this work; this run will create one.

## User Stories

### US-1: Full login environment in AI tabs

As a Scribe user with environment setup in my shell's login startup files
(PATH additions, `ANTHROPIC_API_KEY`, language toolchain shims), I want an
AI tab to launch my `$SHELL` so the entire user environment loads, so that
`claude` / `codex` resolve from my real PATH and see my real environment —
identical to running them by hand in a terminal.

Acceptance Criteria:
- Given `SHELL=/usr/bin/bash` and a PATH export that exists only in
  `~/.bash_profile` (or `~/.profile`), opening an AI tab launches the AI
  binary found via that PATH, and `env` inside the AI CLI's shell-out
  shows the profile-exported variables.
- The shell actually launched is resolved SERVER-side from the HOST's
  env/passwd (Clarifications Q1; validated: the daemon env has
  SHELL=/bin/bash via PAM session inheritance, and passwd is the
  more-live source), not a hardcoded `bash`/`sh` and not the controlling
  client's shell in remote sessions.
- Bash startup follows normal login-shell order (`/etc/profile`, then the
  first existing of `~/.bash_profile`, `~/.bash_login`, `~/.profile`);
  zsh follows `.zshenv` → `.zprofile` → `.zshrc` (login+interactive);
  fish runs its normal `config.fish` + vendor conf.d startup. Real login
  semantics means `~/.bashrc` runs only if the user's profile chains it
  (on this machine `~/.bash_profile` does NOT — Clarifications Q3e).
- Manual verification path (Principle 3): documented one-liner per shell,
  e.g. put a sentinel export in the profile file, open an AI tab, inspect
  the child env via `/proc/<pid>/environ` or the CLI's `!env`.

### US-2: Shell integration parity between AI tabs and plain tabs

As a Scribe user relying on Scribe's shell integration (env-delta
persistence, restore-delta application, hook-helper discovery), I want the
integration to attach to AI tabs the same way it attaches to plain tabs,
so that AI tabs are not a silently degraded session class.

Acceptance Criteria:
- On bash, the integration script (dist/shell-integration/bash/scribe.bash)
  is sourced during AI-tab startup via the pre-exec preamble
  (`[ -n "${SCRIBE_INTEGRATION_SCRIPT:-}" ] && source
  "$SCRIBE_INTEGRATION_SCRIPT"` — path crossing server→shell as an env
  var, never string-interpolated), with `SCRIBE_AI_TAB=1` skipping the
  script's own startup-file sourcing (:36-60) and prompt/PS1/PS0/DEBUG/
  delta-hook wiring while keeping restore-delta sourcing
  (Clarifications Q3). The `--rcfile X -l` conflict is eliminated and
  the now-dead `--rcfile` insertion for command-bearing bash launches
  (session_manager.rs:884-889) is removed; no argv combination ships
  where one flag disables another.
- A staged `SCRIBE_RESTORE_ENV_DELTA_FILE` is sourced (and deleted) via
  the pre-exec preamble AFTER login files (delta wins, spec-006 FR-008)
  on bash, zsh, and fish, so persisted env from a previous session
  reaches the AI CLI after a cold restart (Clarifications Q4). AI tabs
  consume the delta but never write envelopes of their own — they exec
  away pre-prompt and never emit deltas.
- `SCRIBE_SHELL_INTEGRATION=1` and the per-shell injection vars
  (bash `ENV`, zsh `ZDOTDIR`/`SCRIBE_ORIG_ZDOTDIR`, fish/nushell
  `XDG_DATA_DIRS` — crates/scribe-server/src/shell_integration.rs:141-210)
  remain correct for the new invocation, including not leaking Scribe's
  `ZDOTDIR` into the exec'd AI process's descendants.
- Explicitly documented: which integration features are inherently moot in
  AI tabs because `exec <binary>` replaces the shell before any prompt is
  drawn (prompt-command OSC marks, per-prompt env-delta emission,
  baseline emission timing). Baseline emission does NOT enable env
  restore for AI tabs; keep it as cheap bookkeeping or drop it for AI
  tabs — either is functionally fine, planning decides
  (Clarifications Q3d).
- Plain-tab behavior is unchanged (decided, Clarifications Q2;
  Principle 2: no surprise changes to existing sessions; Principle 7).

### US-3: `ai_tab_cwd` controls the AI tab starting directory

As a Scribe user working in a project directory, I want a new AI tab to
start in the directory `terminal.ai_tab_cwd` dictates — by default the
focused pane's current directory — so that the AI CLI opens on the project
I am actually working in instead of `$HOME`.

Acceptance Criteria:
- With `ai_tab_cwd = "pane"` (the default; the serde default is ALREADY
  Pane, so no migration is needed — Clarifications Q5), opening an AI
  tab while the focused pane's CWD is `/home/user/proj` starts the AI
  CLI with CWD `/home/user/proj`. The CLIENT resolves the concrete cwd
  from state it already holds — `ChromeMetadata.cwd` of the focused
  session, tracked via `CwdChanged` metadata
  (crates/scribe-client/src/main.rs:8708-8710, read-back at
  :4893-4901) — so the request carries a real `cwd` instead of `None`.
- With `ai_tab_cwd = "project_root"`, the tab starts at the
  server-computed workspace project root (workspace root + first path
  component, workspace_manager.rs:376-387), which already flows to the
  client (WorkspaceNamed / WorkspaceSlot.project_root); it falls back
  to the pane CWD (then `$HOME`) when the focused pane is not inside a
  configured root.
- With the new `ai_tab_cwd = "home"` variant (an escape hatch
  preserving today's de facto behavior, not a compat shim), the tab
  starts in `$HOME`.
- Fallback chain is explicit and typed (Principle 1): unknown/stale/
  non-directory CWD values degrade to the next tier; the server retains
  its final `is_dir` + `$HOME` guard (session_manager.rs:793) —
  remote-safe, because the client's cached values originated from the
  server.
- `ai_tab_cwd` governs fresh creates only; cold-restart relaunch keeps
  using the persisted LaunchRecord.cwd (restore_replay.rs:597-633).
- Changing the setting in the settings UI affects the next AI tab opened,
  with no restart required. The UI gains a ("home","Home") choice
  (settings/model.rs:342-345) plus the apply.rs string-match arm
  (:424-430 — the one non-compile-checked spot).
- The settings UI description matches the shipped behavior for every
  enum variant (no dead options). This story ships as its own
  implementation bead within the same epic (Clarifications Q5).

### US-4: Correct behavior on zsh, fish, and nushell

As a Scribe user whose `$SHELL` is zsh, fish, or nushell, I want AI tabs
to load my full environment and integration exactly as bash users do, so
that shell choice does not change AI-tab quality.

Acceptance Criteria:
- zsh: AI-tab startup reads Scribe's `$ZDOTDIR/.zshenv`
  (dist/shell-integration/zsh/.zshenv), which restores the user's
  `ZDOTDIR`, sources the user's real `.zshenv`, and loads `scribe.zsh`;
  login startup then proceeds through the user's `.zprofile`/`.zshrc`.
  For AI tabs the restore-delta file is applied via the pre-exec
  preamble AFTER login files (delta wins, FR-008 — Clarifications Q4),
  not at `.zshenv` time.
- fish: vendor conf.d injection via `XDG_DATA_DIRS`
  (shell_integration.rs:188-199) survives the new invocation; the user's
  own `config.fish` runs; the AI binary resolves from the user's fish
  PATH manipulations.
- nushell: documented limitation — AI tabs get NO shell integration
  under any `nu -c` variant (validated on nu 0.114.1: vendor autoload
  loads only in the REPL path; no `env.nu`/`config.nu` under `-c`).
  The `XDG_DATA_DIRS` vendor autoload path
  (shell_integration.rs:201-210) is not regressed for plain tabs
  (Clarifications Q3).
- The command string passed to each shell is valid for that shell's
  command-flag syntax (validated: grouped `-lic` is login+interactive
  on bash, zsh, and fish; no `nu -c` variant loads integration —
  "Verify Before Implementing" discipline satisfied).
- Unknown shells (`ShellKind::Unknown`) still launch the AI binary with a
  sane environment and no integration, with a debug log
  (session_manager.rs:824-826 behavior preserved).

## Constraints

Code-trace evidence, verified against the worktree at
/home/mamba/work/scribe/.worktrees/ai-tab-shell-env (all line numbers from
that checkout):

- **Client AI-tab argv**: `ai_tab_command`
  (crates/scribe-client/src/main.rs:5870-5879) builds
  `[shell, "-lic", "exec <binary> [resume-args]"]` where `shell` comes
  from `scribe_common::shell::default_shell_program()`
  (crates/scribe-common/src/shell.rs:19-39): `$SHELL` of the CLIENT
  process → passwd account shell → `"sh"`. Related launchers
  `shell_command_argv` (:5855-5857) and `spawn_background_command`
  (:5862-5868) use `-lc` with the same resolver; keep them in mind when
  changing the resolver, but they are out of scope for behavior change.
- **Session create request**: `create_tab`
  (crates/scribe-client/src/main.rs:3430-3455) always sends `cwd: None`
  (:3448) via `SessionLaunch`
  (crates/scribe-client/src/ipc_bridge.rs:1015). The launch binding /
  `launch_id` rides along for env-envelope identity (:3442-3450) — the
  redesign must not break cold-restart relaunch of AI commands.
- **Server shell build**: `SessionManager::create_session`
  (crates/scribe-server/src/session_manager.rs:385-428) →
  `prepare_session_launch` (:446-491) → `build_shell` (:872-935). For a
  `Some(command)` launch with `ShellKind::Bash`, `--rcfile <script>` is
  inserted at args[0..2] (:884-889) — producing the conflicting
  `bash --rcfile <script> -lic "exec claude"`. Verified: bash `-l` makes
  it a login shell and login shells ignore `--rcfile` (rcfile applies to
  interactive non-login only); bash honors `$ENV` only in POSIX mode /
  when invoked as `sh`. Zsh/fish/nushell get no argv mutation (:903-906).
- **PTY env composition**: `build_pty_options`
  (session_manager.rs:740-796) sets TERM, COLORTERM, TERM_PROGRAM,
  TERM_PROGRAM_VERSION, SCRIBE_HOOK_SOCK, SCRIBE_SESSION_ID (:750-760),
  SCRIBE_HOOK_HELPER (:765-767), SCRIBE_ENV_PERSIST (:776), integration
  env when enabled (:778-780), SCRIBE_RESTORE_ENV_DELTA_FILE (:786-788).
  CWD fallback `cwd.filter(is_dir).or_else(home_dir)` (:793). Because
  these are PTY-env injections (not rc-file side effects), the exec'd AI
  binary inherits SCRIBE_HOOK_SOCK/SESSION_ID/HOOK_HELPER even today —
  AI hook eventing is NOT broken by bug 1; env restore and profile
  loading are the material losses.
- **Integration env injection**: `shell_integration::build_env`
  (crates/scribe-server/src/shell_integration.rs:141-155) sets
  `SCRIBE_SHELL_INTEGRATION=1`; bash `ENV=<script>` (:172-177), zsh
  `ZDOTDIR` redirect + `SCRIBE_ORIG_ZDOTDIR` (:179-186), fish
  `XDG_DATA_DIRS` prepend (:188-199), nushell `XDG_DATA_DIRS` vendor
  autoload (:201-210). `integration_script_path` (:159-170) exists only
  for bash and PowerShell.
- **Bash integration script**: dist/shell-integration/bash/scribe.bash —
  login-profile emulation (:40-60, including a `shopt -q login_shell`
  branch that sources `/etc/profile` + first-of profile files),
  restore-delta sourcing + delete (:202-207), env-baseline emit and
  prompt-hook registration (:374-395). Note the script ALREADY handles
  being sourced in a login shell correctly — the problem is purely that
  bash never sources it when `-l` and `--rcfile` are combined.
- **Zsh integration**: dist/shell-integration/zsh/.zshenv restores the
  user's `ZDOTDIR`, sources the user's real `.zshenv`, then sources
  `scribe.zsh`. This mechanism is login-shell-compatible (zsh always
  reads `$ZDOTDIR/.zshenv` first), so zsh AI tabs today likely get BOTH
  integration and login startup — bash is the anomalous shell.
- **Daemon env**: the PTY child inherits the scribe-server daemon env
  (systemd user unit; dist/scribe-server.service contains no
  `Environment=` lines). The client imports exactly six GUI vars into
  the user manager (`sync_linux_service_environment`,
  crates/scribe-client/src/server_lifecycle.rs:280-331): DISPLAY,
  WAYLAND_DISPLAY, XDG_SESSION_TYPE, XDG_RUNTIME_DIR,
  DBUS_SESSION_BUS_ADDRESS, XAUTHORITY. Correction (validated): the
  daemon env DOES have SHELL=/bin/bash (PAM session inheritance) — the
  env-poverty concern is real for PATH, not SHELL — and passwd is the
  more-live source anyway, so server-side shell resolution from host
  passwd/env is sound (Clarifications Q1). Env crossing the
  client→server IPC boundary must still stay deliberate and minimal
  (Principle 5: default-safe trust boundaries).
- **`ai_tab_cwd` config**: `AiTabCwd`
  (crates/scribe-common/src/config.rs:1123-1131) has exactly two
  variants: `pane` (default, "inherit the focused pane's CWD") and
  `project_root`. Consumers are config.rs itself and the settings UI
  only (settings/values.rs:121, model.rs:342, window.rs:2912,
  apply.rs:299,424-429) — no runtime consumer exists. There is no
  `home` variant today; the always-`$HOME` behavior is unrepresentable.
  (A `home` variant is added by this feature — Clarifications Q5.)
- **Server-side re-detection**: `ResolvedShell::for_request`
  (session_manager.rs:821-828) + `shell_binary_str` (:833-838) re-derive
  the shell kind from `command[0]`, falling back to the SERVER's
  `default_shell_program()` when `command` is `None`. AI-provider
  hinting scans the command tokens for the binary name
  (`command_ai_provider_hint`, :840-846) — the redesigned argv must keep
  the AI binary name discoverable in the command for tab-title/state
  hinting and cold-restart relaunch.
- **Q1 decision fallout (validated)**: `launch_binding_for`
  (crates/scribe-client/src/main.rs:568-579) derives launch intent by
  sniffing argv and must be rewired to construct `LaunchKind::Ai`
  bindings directly, or AI tabs cold-restart as plain shells. The
  server may set `ai_provider_hint` directly from the structured
  `ai_launch` field instead of token-sniffing
  (session_manager.rs:840-846). Both argv sites (`ai_tab_command`
  main.rs:5870, `restore_replay::command_argv` :188-208) change in
  lockstep. The remote protocol gate (ipc_server.rs:3460) makes
  cross-version remote pairs refuse loudly with IncompatibleVersion
  after the `REMOTE_PROTOCOL_VERSION` 3→4 bump.
- **Process invariants**: Principle 7 — never restart the live Scribe
  server without explicit user approval; lat.md must be synced
  (lat.md/server.md Sessions/Session Creation + Env Persistence,
  lat.md/common.md Configuration/Terminal, lat.md/settings.md) when the
  launch pipeline changes. Principle 3 — no test code unless requested;
  manual verification paths are specified in the user stories.
- **Immutability discipline** (user rule): launch-request construction
  should build new values rather than mutating shared config state.

## Open Questions

Resolved — see Clarifications. Retained below as historical record.

1. **Scope of the login-shell redesign**: does "launch via `$SHELL` as a
   login shell with full user environment" apply only to AI tabs, or
   should PLAIN tabs adopt it too? Plain bash tabs today are
   interactive non-login (`bash --rcfile <script>`) and rely on the
   script's own login-profile emulation (scribe.bash:40-60). Unifying
   would simplify the matrix but changes long-standing plain-tab
   startup behavior (Principle 2 risk).
2. **Where is `$SHELL` resolved** — client-side (GUI session env, the
   current source of truth, richer and user-visible) or server-side
   (daemon env, minimal by design, currently used only when `command`
   is `None`)? If server-side, does `SHELL` need to join the imported
   env set, and is widening that set acceptable under Principle 5?
   Should the client stop sending a shell argv entirely and instead
   send a structured "AI launch" request (provider + resume flag +
   cwd), letting the server own argv construction?
3. **Bash attachment mechanism without `--rcfile` on a login shell**:
   options include (a) keep `--rcfile` and DROP `-l`, letting
   scribe.bash's existing `__scribe_source_login_profile` emulation
   provide the login env (smallest change; but emulation fidelity vs a
   real login shell is unverified for edge cases like `$0` starting
   with `-`, `shopt login_shell`, profile guards that test it);
   (b) real login shell (`-l`) plus injecting the integration by
   appending to the command string (e.g.
   `source <script>; exec claude`), keeping user startup files fully
   native; (c) `--init-file` tricks or `PROMPT_COMMAND`/`BASH_ENV`
   channels. Which fidelity do we pick, and does the answer differ for
   AI tabs (which exec away) vs plain tabs?
4. **Does `exec <binary>` stay?** With exec, aliases/functions never
   matter and prompt hooks never fire — only env (PATH, exports) and
   the restore-delta application before exec are load-bearing. Keeping
   exec means the "shell integration parity" goal reduces to "env
   effects of integration parity"; dropping exec (leave a shell under
   the AI CLI) changes process-tree semantics, exit behavior, and AI
   state detection. Recommendation to confirm: keep exec.
5. **`ai_tab_cwd` semantics**: is the two-variant enum (`pane`,
   `project_root`) sufficient, or should a `home` variant (current de
   facto behavior) and/or a fixed-path variant be added so existing
   users who prefer `$HOME` can keep it? What exactly defines
   "workspace project root" for `project_root`, and what happens with
   no focused pane (first tab in a fresh window)?
6. **zsh under the new invocation**: confirm on a real zsh that
   `zsh -lic` (or the redesigned argv) reads `$ZDOTDIR/.zshenv` →
   user `.zshenv` → `.zprofile` → `.zshrc` in that order with Scribe's
   ZDOTDIR redirect, that `SCRIBE_ORIG_ZDOTDIR` unwinding leaves the
   exec'd AI process with the user's original `ZDOTDIR`, and that the
   restore-delta file is applied pre-exec (where does zsh source it —
   scribe.zsh needs checking for the SCRIBE_RESTORE_ENV_DELTA_FILE
   consumer).
7. **fish/nushell command-flag syntax**: verify `fish -lic "exec …"`
   parses as intended (fish supports grouped short flags and `-c`, but
   `exec` semantics inside `-c` need checking) and define the nushell
   AI-tab argv (nushell has `-l`, `-i`, `-c` but different startup-file
   semantics; vendor autoload only landed in recent versions).
8. **Restore-delta dependency**: the env-restore contract
   (specs/006-persist-terminal-env) assumes the integration script
   sources `SCRIBE_RESTORE_ENV_DELTA_FILE` post-rc. If bash AI tabs
   move to a mechanism where scribe.bash is not sourced at all (option
   3b), who sources and deletes the restore file — a command-string
   preamble, or a dedicated one-shot script? And should baseline
   emission (`__scribe_emit_env_baseline`, scribe.bash:374-387) be
   suppressed for AI tabs since the shell execs away immediately?
9. **Failure surfacing**: when the AI binary is missing from the user's
   PATH after full login startup, what does the user see? Today the
   shell prints "command not found" and the tab dies; should the client
   surface a typed error (Principle 1) or a hint toward install docs?
10. **Interaction with cold-restart relaunch**: the launch binding
    stores the original command argv for relaunch (main.rs:3436-3444).
    If argv construction moves server-side or changes shape, old
    persisted bindings must still relaunch correctly (Principle 7
    compatibility) — is a migration or dual-read needed?

## Spec Review

Six parallel review passes (requirements, gaps, ambiguity, feasibility,
scope, stakeholders) were run against this draft; several passes also
empirically verified shell behavior on this machine. Convergent findings
are merged below.

### Critical Questions (answer before planning)

1. **Where is `$SHELL` resolved and who owns argv — and the answer must
   account for REMOTE sessions.** A Scribe client can drive a window
   whose sessions live on another machine's server (crates/scribe-client/
   src/remote.rs, lan_dial.rs; tailnet). Client-side resolution sends the
   CONTROLLING machine's shell path (and cwd validated against the wrong
   filesystem) into the HOST machine's PTY. Server-side resolution
   inherits the daemon's minimal env — and the server-side integration
   injectors already read `ZDOTDIR`/`XDG_DATA_DIRS` from the daemon env
   (shell_integration.rs:179-210), a pre-existing bug class this decision
   either fixes or doubles down on. A structured "AI launch" request
   (provider + resume + cwd; server owns argv) changes the IPC contract
   (protocol.rs CreateSession) and raises mixed-version client/server
   compatibility on a server that cannot be hot-restarted (Principle 7).
   — why it matters: decides patch vs IPC redesign, and remote
   correctness falls out of it; flagged by: ambiguity, gaps, feasibility,
   scope, stakeholders.
2. **Scope fork: do plain tabs also become login shells, or AI tabs
   only (OQ1)?** Answering "yes" invalidates the Non-Goals section,
   roughly doubles the verification matrix, and changes every session
   Scribe launches; the current text is contradictory (Non-Goals says
   plain tabs unchanged; US-2 hedges "unless resolved otherwise").
   — why it matters: largest single effort/blast-radius multiplier;
   flagged by: scope, feasibility, ambiguity.
3. **Bash attachment mechanism (OQ3) + restore-delta/baseline ownership
   (OQ8) must be decided together, and the verified options all have
   teeth.** Empirically verified: `--rcfile X -ic` (drop `-l`) DOES
   source the script but leaves `shopt login_shell` false, so
   scribe.bash's own profile emulation (:52-60) skips login files on
   Linux — option (a) does NOT deliver the login env without a script
   change; option (b) (`-l` + `source <script>; exec …` preamble) works
   but runs the script AFTER login files and re-sources `~/.bashrc`,
   needing an AI-tab mode flag in the shared script. Separately,
   suppressing the env baseline for AI tabs would void US-2's restore
   promise: hook_ingress.rs:186,284 drops every delta for a session
   with no recorded baseline, so no envelope would ever be written.
   Also pick the MVP shell set: verified that nushell loads vendor
   autoload only in the REPL path — under ANY `nu -c` variant there is
   no integration and no `env.nu`/`config.nu`, so US-4's nushell
   criterion is unachievable as written; fish and zsh are verified fine
   as-is. — why it matters: the bash design is genuinely undetermined
   and nushell needs an explicit mechanism-or-limitation decision;
   flagged by: feasibility, ambiguity, requirements, stakeholders.
4. **Restore-delta vs login-profile precedence: pick a winner.** The
   shells disagree TODAY: zsh applies the delta from `.zshenv` (before
   `.zprofile`/`.zshrc`, so user rc silently wins) while bash applies it
   post-rc (delta wins). Under full login startup, "applied before exec"
   is satisfiable by both orders while semantics diverge — this is an
   observable amendment to the spec-006 env-persistence contract and
   must be documented as such (Principle 7).
   — flagged by: requirements, gaps, ambiguity, feasibility.
5. **`ai_tab_cwd` decision package**: (a) add a `home` variant (today's
   de facto behavior is unrepresentable — silent behavior flip for every
   existing user otherwise, Principle 2; config.rs already has a
   migration precedent); (b) define `project_root`'s source of truth —
   client `WorkspaceSlot.project_root` vs server `workspace_roots` are
   competing definitions and both are frequently `None`; (c) specify the
   full fallback chain for the dominant real cases: no focused pane,
   remotely/CLI-triggered AI tabs (AutomationAction has no visible
   focus), AI panes that exec away and never emit OSC 7, resume tabs
   (prior session's dir vs current pane?), and cold-restart relaunch
   (restore_replay.rs:595-633 uses persisted record cwd — does
   `ai_tab_cwd` apply there?); (d) decide whether the cwd fix ships as
   its own unit — it shares almost no surface with the launch redesign.
   — flagged by: all six dimensions.
6. **Performance budget or explicit waiver (Principle 4), plus an
   escape hatch.** Full login startup (nvm/conda/mise chains) adds
   user-visible latency to a hot interactive action (ctrl+alt+c); the
   constitution requires a stated budget or an explicit
   "inapplicable" marking, and there is currently no documented way
   back to today's fast path if a user's profile is slow or
   interactive-hostile. — flagged by: requirements, gaps, ambiguity,
   feasibility, scope, stakeholders.
7. **Record the Principle 5 decision and the verification path.**
   (a) Full-env fidelity is the user's explicit ask, but the
   restore-delta path additionally hands PERSISTED env (decrypted from
   the keystore, EXCLUSION_SET filters staleness not secrecy) to AI
   processes — record the decision (and whether AI tabs get a restore
   opt-out) rather than inheriting it. (b) Every story requires new
   server behavior, and the live server must not be restarted
   (CLAUDE.md); name the manual verification vehicle (dev-flavor
   `scribe-dev` identity split, shell_integration.rs:107-130, or an
   explicitly approved restart) so acceptance criteria are actually
   reachable. — flagged by: stakeholders, requirements, gaps.

### Non-Blocking Observations

- Good news, verified: OQ10 largely dissolves — `LaunchKind::Ai`
  persists structured `{provider, resume_mode, conversation_id}` and
  regenerates argv at replay (restore_replay.rs:188-208); only
  `CustomCommand` stores raw argv. BUT both argv-construction sites
  (`ai_tab_command` and `restore_replay::command_argv`) must change in
  lockstep or fresh launches and relaunches silently diverge.
- Good news, verified: zsh and fish need no new mechanism (`-lic` is
  login+interactive in both; vendor conf.d loads under `fish -c`; zsh
  `.zshenv` redirect is login-compatible). OQ6's premise is stale —
  scribe.zsh:139-142 and scribe.fish:150-154 already source-and-delete
  the restore file. (Correction from validation: they source it at the
  WRONG TIME — pre-rc — violating FR-008; see Clarifications Q4.)
- OQ4 (`exec`) is not a real fork: keep `exec`; move it from Open
  Questions to a recorded decision. Prompt-time features are inherently
  moot; only pre-exec env effects are load-bearing.
- lat.md ALREADY documents the unshipped behavior as real
  (lat.md/client.md:1424 "follows the ai_tab_cwd setting";
  lat.md/server.md:190) — the doc sync must correct existing text, not
  just add new sections, and client.md belongs on the update list.
- `ENV=<script>` injection (shell_integration.rs:172-177) is dead for
  non-POSIX bash yet leaks to every descendant `sh`; clean up alongside.
- Once bash AI tabs emit baselines for the first time, `SHELL`, `ENV`,
  `ZDOTDIR`, `XDG_DATA_DIRS` should join EXCLUSION_SET to avoid
  persisting Scribe-private values.
- PowerShell is trivially unaffected (its build_shell arm gates on
  `args.is_empty()`, so AI launches never got `-File <script>`); one
  smoke check suffices for the "no regression" non-goal.
- Fuller env can now carry `ANTHROPIC_BASE_URL`/`CLAUDE_CONFIG_DIR`/
  proxy vars into AI CLIs, and a login PATH can cross the
  `scribe`/`scribe-dev` hook-helper flavor split — one documented
  sentence each.
- Sibling launchers (`shell_command_argv`, `spawn_background_command`)
  and split-pane cwd (`request_pane_session` also sends `cwd: None`)
  are the predictable day-after asks; name them as explicit follow-up
  beads. macOS (launchd env, path_helper, zsh default) deserves a
  one-line in/out-of-scope statement.
- User stories are unranked; assign priorities during planning so the
  MVP slice is explicit. US-2/3/4 also need named manual verification
  commands like US-1 has (Principle 3).

## Clarifications

The seven Spec Review critical questions were answered by the user,
accepting the recommended options AS AMENDED by a validation research
pass. Each entry below is binding on planning; the Goals, Non-Goals,
User Stories, and Constraints sections have been updated to match.

### Q1 → 1A amended: server-owned argv via structured AI launch

The client sends a structured `ai_launch` field (provider + resume
flag/conversation + cwd) on CreateSession; the server owns argv
construction and resolves the shell from the HOST's env/passwd (passwd
is live; the daemon env does have `SHELL` — validated). Compat:
DUAL-WRITE — the client sends both `ai_launch` AND the legacy `command`
argv in the same frame; new servers prefer `ai_launch`, old servers
(including the un-restartable live one) degrade to exactly today's
behavior. No silent-failure window. `REMOTE_PROTOCOL_VERSION` bumps
3→4 (CreateSession is remote-visible; validated gate at
ipc_server.rs:3460 — cross-version remote pairs refuse loudly with
IncompatibleVersion). Client fallout (validated): `launch_binding_for`
(main.rs:568-579) currently derives launch intent by sniffing argv —
it must be rewired to construct `LaunchKind::Ai` bindings directly, or
AI tabs cold-restart as plain shells. The server may set
`ai_provider_hint` directly from the structured field instead of
token-sniffing (session_manager.rs:840-846). Both argv sites
(`ai_tab_command` main.rs:5870, `restore_replay::command_argv`
:188-208) change in lockstep.

### Q2 → 2A: AI tabs only

Plain-tab launch semantics are unchanged in this feature; a follow-up
bead considers unifying later. Non-Goals is now firm and the US-2
hedge is removed.

### Q3 → 3A amended: real login+interactive bash + source-preamble

AI-tab bash argv: `bash -lic '<preamble>; exec <binary> …'`.
Empirically validated end-to-end. Amendments:
(a) the integration script gets an AI-tab mode (e.g. `SCRIBE_AI_TAB=1`)
that SKIPS scribe.bash's own startup-file sourcing block (:36-60) —
otherwise /etc/profile + profile run twice (verified) — and skips
prompt/PS1/PS0/DEBUG/delta-hook wiring; restore-delta sourcing is
kept.
(b) `-i` must stay: the script no-ops unless its interactive guard
passes.
(c) the script path crosses server→shell via the env var
`SCRIBE_INTEGRATION_SCRIPT`, sourced as
`[ -n "${SCRIBE_INTEGRATION_SCRIPT:-}" ] && source
"$SCRIBE_INTEGRATION_SCRIPT"` — never string-interpolated (validated:
naive interpolation breaks silently).
(d) corrected rationale: baseline emission does NOT enable env restore
for AI tabs (they exec away pre-prompt, never emit deltas, never write
an envelope — the restore file path is future-proofing); keep the
baseline emit as cheap bookkeeping or drop it for AI tabs — either is
functionally fine, planning decides.
(e) documented consequence: real login semantics means `~/.bashrc`
runs only if the user's profile chains it (true login behavior; on
this machine `~/.bash_profile` does NOT chain it).
(f) remove the now-dead `--rcfile` insertion for command-bearing bash
launches (session_manager.rs:884-889).
zsh/fish already work via `-lic` (validated); nushell AI tabs get NO
integration under any `nu -c` variant (validated on nu 0.114.1) —
documented limitation.

### Q4 → 4A reframed: enforce existing FR-008 for AI tabs; file the plain-tab defect separately

Delta-wins is ALREADY the spec-006 contract (FR-008 MUST; research.md
R1.3 apply-after-rc). Validation found zsh and fish plain tabs violate
it TODAY: the delta is applied pre-rc (.zshenv-time / conf.d-time) AND
the env baseline is captured pre-rc, so user rc exports get persisted
into the envelope as user-set deltas. This feature: AI tabs apply the
delta via the pre-exec preamble AFTER login files (delta wins) on
bash/zsh/fish. The plain-tab zsh/fish non-conformance is filed as a
SEPARATE spec-006 defect bead (not fixed here). lat.md/server.md:572
(falsely asserting uniform post-rc apply) is corrected as part of this
feature's doc sync.

### Q5 → 5A amended: `ai_tab_cwd` live with `home` escape hatch

Add a `home` variant; the default stays `pane` (the serde default is
ALREADY Pane — validated, no migration needed; `home` is an escape
hatch, not a compat shim). `project_root` = the server-computed
workspace project root (workspace root + first path component,
workspace_manager.rs:376-387), which already flows to the client
(WorkspaceNamed / WorkspaceSlot.project_root). Ownership split: the
CLIENT resolves the concrete cwd from state it already holds (pane =
ChromeMetadata.cwd of the focused session; project_root =
slot.project_root → pane cwd → home) and sends it in the request; the
SERVER keeps its final `is_dir → $HOME` guard (remote-safe: the
client's cached values originated from the server). `ai_tab_cwd`
governs fresh creates only; cold-restart relaunch keeps using the
persisted LaunchRecord.cwd (restore_replay.rs:597-633). Settings UI:
add a ("home","Home") choice (model.rs:342-345) plus the apply.rs
string-match arm (:424-430 — the one non-compile-checked spot). Ships
as its own implementation bead within the same epic.

### Q6 → 6A upgraded: soft budget + named command

Budget: an AI tab reaches the AI CLI prompt within ~1s of tab-open on
this machine's profile (excluding the AI CLI's own startup).
Measurement: extend tools/perf-ab-rig/run-perf-ab.sh with a timed
`--ai-tab-only` mode reusing the `open_owned_tab` (:600-619)
session-appearance polling — giving Principle 4 a named command. No
escape-hatch/fast-path setting.

### Q7 → 7A amended: full env accepted; verify via dev flavor; add SHELL to EXCLUSION_SET

Decision recorded (Principle 5): the full login env AND the persisted
restore-delta flow into AI CLIs deliberately — explicit user intent,
no AI-tab opt-out. EXCLUSION_SET is staleness-only with zero secret
patterns (persisting API_TOKEN is asserted-intended in its own tests,
delta.rs:276-280) — accepted. Amendment: add `SHELL` to EXCLUSION_SET
in this feature (a restored stale SHELL could redirect which
interpreter launches — control-flow, not staleness); consider
ENV/ZDOTDIR/XDG_DATA_DIRS alongside. Verification vehicle: the
`scribe-dev` flavor (fully isolated by executable stem:
socket/config/state/keystore; a dev server already runs on this
machine). Named commands: `just install-dev`, `/usr/bin/scribe-dev`,
`tools/perf-ab-rig/run-perf-ab.sh --live`. NEVER `just server` /
`just restart-server` (stable flavor → live server). The live
production server is never restarted.
