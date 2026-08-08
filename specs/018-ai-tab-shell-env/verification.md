# Verification: ai-tab-shell-env

**Status:** SUPERSEDED — see `spec.md` for what replaced this design and why. Retained as history; it does not describe current behavior.


Feature 018's final gate and `scribe-0ve.12` repair rerun completed on 2026-08-02 against fresh release artifacts in disposable, networkless containers. Overall status is **PASS** for every in-scope runnable assertion; explicit out-of-scope limitations remain below.

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
  inspection confirmed the boundary before Scribe execution. The repair rerun
  additionally mounted one fresh temporary results directory read-write; it
  contained only the matrix driver, copied artifacts, and captured logs.
- No task containers remained after verification.

Fresh `just build-release` artifacts and their matching in-container hashes:

| Artifact | SHA-256 | Build time (PDT) |
|---|---|---|
| `scribe-server` | `cfa9cc589b7798ca9c5efbdbb9961ba1d4478fd0d815ed68da4ef8acf2633ed3` | 2026-08-02 02:02:21 |
| `scribe-client` | `2362364190ebf9b5a980064b7bfe4af97073e7ef94c9553569c4b7fe8608a4f4` | 2026-08-02 02:09:31 |
| `scribe-test` | `19cf9a3ca728fdb9885335cca425b0a6a3e0f69746d59bbbc2f9613889ea8595` | 2026-08-02 02:02:39 |
| `scribe-hook-helper` | `dc40dd1c0ab4052e327125147a325319d34be35ffbc04b569f98f4eb9c4c26e1` | 2026-08-02 02:01:41 |

## User-story matrix

The matrix distinguishes runtime evidence from static or unit evidence and marks excluded rows explicitly as out of scope.

| Story / assertion | Status | Evidence |
|---|---|---|
| US-1 bash profile sentinel | PASS | AI shim captured `SCRIBE_SENTINEL=profile`. |
| US-1 profile-only PATH resolution | PASS | Server started without shim directory on PATH; login profile added it; AI process captured `/proc/self/exe=/tmp/profile-bin/claude`. |
| US-2 bash profile sourced once | PASS | AI exec PID appeared exactly once in profile-source log. |
| US-2 staged file exists, delta wins, then file is deleted | PASS (consumer only) | Runtime-dir fixture existed before consumer launch; profile set `DELTA_WINS=profile`; AI process captured `DELTA_WINS=delta`; consumer removed file and unset `SCRIBE_RESTORE_ENV_DELTA_FILE`. |
| US-2 encrypted-envelope staging | PASS | `ai-shell-env-bash.sh`, `ai-shell-env-zsh.sh`, and `ai-shell-env-fish.sh` seed encrypted envelopes through the production env-delta debounce, launch AI sessions with those envelope ids, observe the restored values in the Claude stub, and require each session-specific staging file to be consumed. |
| US-2 launch-variable and original-env cleanup | PASS for bash/zsh/fish | The func shell matrix captured `SCRIBE_SHELL_INTEGRATION=1`; launch-only and restore-file variables were absent at provider exec. Zsh removed `ZDOTDIR`/`SCRIBE_ORIG_ZDOTDIR`; fish removed `SCRIBE_ORIG_XDG_DATA_DIRS` and restored the original absent `XDG_DATA_DIRS`. |
| US-2 zsh/fish leak cleanup | PASS | `ai-shell-env-zsh.sh` and `ai-shell-env-fish.sh` exercised the production startup injection and pre-exec cleanup in the func container. |
| US-2 cold-restart AI relaunch | PASS | Pre-crash restore index/window snapshot existed; after SIGKILL and fresh sandbox server, client claimed and replayed one AI launch and the shim ran once at persisted CWD. |
| US-3 `pane` CWD | PASS | Repair rerun focused the pane at `/tmp/project-root/subdir`; AI process captured the same CWD. |
| US-3 `home` CWD | PASS | Repair rerun kept the pane at `/tmp/project-root/subdir` while the AI process captured isolated `/home/sandbox`. |
| US-3 `project_root` CWD | PASS | With `[workspaces].roots = ["/"]`, the pane at `/tmp/project-root/subdir` produced project root `/tmp`; the AI process captured `/tmp` after metadata settled. |
| US-4 bash / zsh / fish | PASS | `ai-shell-env-bash.sh`, `ai-shell-env-zsh.sh`, and `ai-shell-env-fish.sh` select each shell through live passwd mutation, prove the server's `tier = "passwd"` trace, and cover login startup, provider argv, integration cleanup, cwd, and encrypted restore staging. |
| US-4 nushell | OUT OF SCOPE | Nushell command-mode integration remains unsupported by design. Its sanctioned verification path will be classified by the upcoming spec-019 US-7 taxonomy. |
| US-4 PowerShell | OUT OF SCOPE | The functional image has no PowerShell interpreter. Its sanctioned verification path will be classified by the upcoming spec-019 US-7 taxonomy. |
| US-4 unknown shell | OUT OF SCOPE | Unknown-shell runtime coverage is outside the bash/zsh/fish matrix. Its sanctioned verification path will be classified by the upcoming spec-019 US-7 taxonomy. |

### Func shell-matrix scope

The bash, zsh, and fish func scripts cover the launch contract through the integrated `scribe-test` AI flags and deterministic Claude stub, with encrypted restore rows gated by `SCRIBE_KEYRING=1`.

Each script mutates the running container user's passwd shell with `usermod -s` after the disposable server starts. The server trace must name the selected binary and `tier = "passwd"`, proving production passwd-first resolution is live rather than inherited from daemon `SHELL`.

The scripts always assert resumed-provider argv, requested-directory use, missing-directory fallback to `$HOME`, per-shell login/startup order, `SCRIBE_SHELL_INTEGRATION=1`, and shell-specific cleanup. With `SCRIBE_KEYRING=1`, each also seeds a real encrypted envelope through the plain-shell env-delta debounce, launches an AI tab with `--env-envelope-id`, observes the restored delta in the Claude stub, and proves the production staging file was consumed.

For US-3, this matrix narrows `ai_tab_cwd` evidence to the server-side contract: a concrete directory sent by the client is honored, and a nonexistent directory falls back to home. Client-side selection among pane, project-root, and home remains covered by the GPUI unit/runtime evidence recorded above.

### Project-root repair trace

The initial failure traced to two parts of the same metadata path. A reconcile frame drained parked `WorkspaceInfo` before the initial client-local region adopted the active server workspace ID, so `apply_workspace_info` returned `Unclaimed`. Reattach also seeded `SessionList.workspaces` into chrome only, leaving `WorkspaceSlot.project_root` empty.

The repair parks authoritative `SessionList` workspace entries on the existing ordered metadata queue and adopts the initial server ID before draining it. Metadata still precedes pane/session adoption, preserving split-workspace reconciliation. In all three reruns the client logged one session and one workspace, then `refreshed a workspace region's metadata`; no run logged an unclaimed workspace. The exact CWD matrix was `pane=/tmp/project-root/subdir`, `project_root=/tmp`, and `home=/home/sandbox`.

## Protocol and compatibility

Initial protocol-v4 dual-write behavior passed compile, unit, static, and new-server runtime checks. The later structured-only retirement replaces that transitional client behavior without claiming old-server runtime compatibility.

| Assertion | Status | Evidence |
|---|---|---|
| Remote protocol version | PASS | `REMOTE_PROTOCOL_VERSION` is exactly `4`; remote gates use exact-match semantics. |
| Structured-only AI frame | PASS | `create_session_ai_launch_round_trips_through_msgpack_named` passed with `command: None` and preserved `ai_launch`. |
| Structured-only cold replay | PASS | `ai_replay_uses_structured_launch_only` passed with provider, resume mode, and conversation id intact. |
| Initial structured + legacy dual-write frame | HISTORICAL PASS | The original verification retained both fields before the compatibility path was retired. |
| New-server structured preference | PASS | Server source branches on `Some(ai_launch)` into `build_ai_shell`; fresh sandbox launches exercised server-owned bash login argv. |
| Missing field / legacy decode | PASS | `create_session_missing_ai_launch_defaults_to_none` passed. |
| Initial cold-replay dual values | HISTORICAL PASS | The original sandbox cold replay relaunched the AI shim before structured-only retirement. |
| Known-old-server runtime | OUT OF SCOPE | The updated client deliberately carries no AI legacy argv. Mixed local-version verification will be classified by the upcoming spec-019 US-7 taxonomy. |

## Regression and performance evidence

All existing scripts ran against fresh artifacts inside the audited networkless functional sandbox.

| Check | Status | Evidence |
|---|---|---|
| `tests/e2e/func/shell-integration.sh` | PASS | All five phases passed. |
| `tests/e2e/func/env-persistence.sh` | PASS | Unique launch-envelope IDs and live second session passed. |
| `tests/e2e/func/cold-restart.sh` | PASS | All eight disconnect/reattach/geometry/job phases passed. |
| Restore-shell unit matrix | PASS | Bash, zsh, fish, nushell, and PowerShell restore-file tests passed. |
| `cargo test -p scribe-client` | PASS | All 524 existing client tests passed: 462 library and 62 binary tests. |
| `just ready` | PASS | Suppression, reachability, parity, fmt, clippy, and workspace tests passed. |
| AI tab open to first PTY byte | PASS | First isolated run measured **583.095 ms** against the 1000 ms budget. A warmed exact-executable shim run measured 56.026 ms; Home and project-root attempts measured 569.776 ms and 569.574 ms. |

## Limitations and disposition

Runtime coverage now includes bash, zsh, and fish, including encrypted-envelope creation, staging, precedence, and cleanup. Nushell, PowerShell, unknown-shell, and known-old-server paths are explicitly out of scope and await classification by the upcoming spec-019 US-7 taxonomy.

- Overall: **PASS** for every runnable assertion; the project-root repair is
  verified in the full pane/project-root/home runtime matrix.
- Runtime shell matrix: bash, zsh, and fish complete; nushell, PowerShell, and
  unknown shells are out of scope pending the upcoming US-7 taxonomy.
- Restore staging: encrypted-envelope creation, post-login precedence, and
  session-specific staging-file consumption pass for bash, zsh, and fish.
- Legacy compatibility: serialization/defaulting passed; known-old-server
  execution is out of scope pending the upcoming US-7 taxonomy.
