# Verification: ai-tab-shell-env

Feature 018's final gate ran on 2026-08-02 against fresh release artifacts in disposable, networkless containers. Overall status is **FAIL** because `project_root` CWD fell back to the pane directory; follow-up `scribe-0ve.12` owns that defect.

## Safety boundary and artifact identity

All Scribe process execution stayed inside Docker containers. No host-installed Scribe binary, service, process, socket, display, runtime directory, or user state was accessed.

- Docker server: 29.6.2.
- Functional image: `scribe-test-func:latest` at
  `sha256:279aa5f93a381a7c0503e313e3f9a2cc92027a5833c151287e626681da749a98`.
- Visual image: `scribe-test-visual:latest` at
  `sha256:1aa980ccc2170dc707ec478f752af000620d4d2bf7955ff46c736a0be7d3c822`.
- Every runtime container used `--network none`, isolated `HOME` and all XDG
  directories, a private tmpfs `/run`, blank host display/Wayland/D-Bus
  variables, and no host runtime, display, or user-state mounts.
- Visual runs additionally used a read-only root filesystem, all capabilities
  dropped, `no-new-privileges`, a 512-process limit, and internal Xvfb/openbox.
- Only task-worktree release artifacts, shell-integration files, existing E2E
  scripts, and the existing perf rig were mounted read-only. Container
  inspection confirmed the boundary before Scribe execution.
- No task containers remained after verification.

Fresh `just build-release` artifacts and their matching in-container hashes:

| Artifact | SHA-256 | Build time (PDT) |
|---|---|---|
| `scribe-server` | `cfa9cc589b7798ca9c5efbdbb9961ba1d4478fd0d815ed68da4ef8acf2633ed3` | 2026-08-02 01:19:37 |
| `scribe-client` | `853a6f4e1357f0db591f494b4f20568e07ef41ac38fe04dec069c0a017869d55` | 2026-08-02 01:19:49 |
| `scribe-test` | `19cf9a3ca728fdb9885335cca425b0a6a3e0f69746d59bbbc2f9613889ea8595` | 2026-08-02 01:19:54 |
| `scribe-hook-helper` | `dc40dd1c0ab4052e327125147a325319d34be35ffbc04b569f98f4eb9c4c26e1` | 2026-08-02 01:18:56 |

## User-story matrix

The matrix distinguishes runtime evidence from static or unit evidence and records every unavailable row as `NOT-RUN`.

| Story / assertion | Status | Evidence |
|---|---|---|
| US-1 bash profile sentinel | PASS | AI shim captured `SCRIBE_SENTINEL=profile`. |
| US-1 profile-only PATH resolution | PASS | Server started without shim directory on PATH; login profile added it; AI process captured `/proc/self/exe=/tmp/profile-bin/claude`. |
| US-2 bash profile sourced once | PASS | AI exec PID appeared exactly once in profile-source log. |
| US-2 staged file exists, delta wins, then file is deleted | PASS (consumer only) | Runtime-dir fixture existed before consumer launch; profile set `DELTA_WINS=profile`; AI process captured `DELTA_WINS=delta`; consumer removed file and unset `SCRIBE_RESTORE_ENV_DELTA_FILE`. |
| US-2 encrypted-envelope staging | NOT-RUN | No safe isolated keyring/envelope fixture was available. Fresh AI tabs intentionally emit no envelope. `prepare_restore_env_file` and restore-shell unit coverage provide static/unit evidence only. |
| US-2 launch-variable and original-env cleanup | PASS for bash | AI process captured `SCRIBE_AI_TAB`, `SCRIBE_INTEGRATION_SCRIPT`, and restore-file var unset; original `ZDOTDIR` and `XDG_DATA_DIRS` values survived. |
| US-2 zsh/fish leak cleanup | NOT-RUN | Container has neither shell and tools were not installed. Builder/script inspection plus restore-shell unit tests passed. |
| US-2 cold-restart AI relaunch | PASS | Pre-crash restore index/window snapshot existed; after SIGKILL and fresh sandbox server, client claimed and replayed one AI launch and the shim ran once at persisted CWD. |
| US-3 `pane` CWD | PASS | Focused pane at `/tmp/pane-cwd`; AI process captured the same CWD. |
| US-3 `home` CWD | PASS | Focused pane remained elsewhere; AI process captured isolated `/home/sandbox`. |
| US-3 `project_root` CWD | FAIL | Server logged workspace auto-name `project-root` and client received that `WorkspaceInfo`, but AI process captured `/tmp/project-root/subdir`, the pane fallback, after both rig and settled manual runs. |
| US-4 bash | PASS | Real login/interactive AI launch, integration preamble, profile/PATH inheritance, cleanup, delta consumer, and cold replay all ran. |
| US-4 zsh / fish / nushell | NOT-RUN | `zsh`, `fish`, and `nu` were absent from the audited image. No tools were installed. Existing restore-shell unit tests passed for all three. |
| US-4 PowerShell / unknown shell | NOT-RUN | No safe existing runtime harness or interpreter was available; source builders were inspected. |

### Project-root failure trace

The failure reproduces after configuring `[workspaces].roots = ["/tmp"]`, restarting the disposable server, waiting for workspace metadata, and opening the AI tab after a 3.8-second settle.

Server log recorded `workspace auto-named from CWD` with name `project-root`; client log recorded `workspace info received` with that name. Source trace points to initial reconciliation ordering: `reconcile_panes` drains `WorkspaceInfo` before `adopt_server_workspace` rekeys the initial client-local region. The unclaimed metadata is then unavailable when the region is rekeyed. Follow-up `scribe-0ve.12` owns diagnosis and repair.

## Protocol and compatibility

Protocol v4 and dual-write behavior passed compile, unit, static, and new-server runtime checks, while old-server runtime compatibility remains explicitly unexecuted.

| Assertion | Status | Evidence |
|---|---|---|
| Remote protocol version | PASS | `REMOTE_PROTOCOL_VERSION` is exactly `4`; remote gates use exact-match semantics. |
| Structured + legacy dual-write frame | PASS | `create_session_ai_launch_round_trips_through_msgpack_named` passed and retained both fields. |
| New-server structured preference | PASS | Server source branches on `Some(ai_launch)` into `build_ai_shell`; fresh sandbox launches exercised server-owned bash login argv. |
| Missing field / legacy decode | PASS | `create_session_missing_ai_launch_defaults_to_none` passed. |
| Cold-replay dual values | PASS | `ai_command_detection_and_replay_argv` passed; sandbox cold replay relaunched the AI shim. |
| Known-old-server runtime | NOT-RUN | No provenance-verifiable pre-feature server artifact was available, and verification did not build or install one. Compatibility evidence is named-MessagePack defaulting plus retained legacy argv. |

## Regression and performance evidence

All existing scripts ran against fresh artifacts inside the audited networkless functional sandbox.

| Check | Status | Evidence |
|---|---|---|
| `tests/e2e/func/shell-integration.sh` | PASS | All five phases passed. |
| `tests/e2e/func/env-persistence.sh` | PASS | Unique launch-envelope IDs and live second session passed. |
| `tests/e2e/func/cold-restart.sh` | PASS | All eight disconnect/reattach/geometry/job phases passed. |
| Restore-shell unit matrix | PASS | Bash, zsh, fish, nushell, and PowerShell restore-file tests passed. |
| `just ready` | PASS | Suppression, reachability, parity, fmt, clippy, and workspace tests passed. |
| AI tab open to first PTY byte | PASS | First isolated run measured **583.095 ms** against the 1000 ms budget. A warmed exact-executable shim run measured 56.026 ms; Home and project-root attempts measured 569.776 ms and 569.574 ms. |

## Limitations and disposition

Three limitations prevent an all-green gate: project-root behavior is a confirmed failure, unavailable shells could not be installed, and no safe keyring-backed envelope or known-old-server fixture existed.

- Overall: **FAIL** pending `scribe-0ve.12` for project-root metadata adoption.
- Runtime shell matrix: bash complete; zsh/fish/nushell/PowerShell/unknown are
  `NOT-RUN`, with static/unit evidence where available.
- Restore staging: consumer ordering/deletion/precedence passed with an explicit
  runtime-dir fixture; encrypted-envelope creation/staging is `NOT-RUN`.
- Legacy compatibility: serialization/defaulting passed; known-old-server
  execution is `NOT-RUN`.
