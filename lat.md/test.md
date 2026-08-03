# Test Harness

Scribe tests combine server-focused functional checks with GPUI headless and
visual end-to-end coverage for the rebuilt client.

## Architecture

CLI binary (`scribe-test`) dispatches subcommands to a long-lived daemon that holds an open IPC connection to scribe-server and buffers per-session state.

The two-process model keeps the server connection alive across many short-lived CLI invocations. The CLI process sends a single  over a Unix socket, the daemon executes it against live session state, and returns a . The CLI exits immediately after receiving the response.

### Error Model

Two exit codes distinguish failure kinds.  has two variants: `TestFailure` (exit 1) for assertion mismatches, and `InfraError` (exit 2) for socket, spawn, or timeout problems.

## Daemon

Long-lived process that maintains an open IPC connection to scribe-server, buffers per-session output and screen state, and serves CLI requests over a Unix socket.

The daemon is started with `scribe-test daemon start` (spawns itself as `daemon run`) and stopped with `scribe-test daemon stop` (sends a `Shutdown` request). The  function owns the main event loop, running a server-reader task and a command-listener task concurrently.

After connecting to scribe-server, the daemon sends `ClientMessage::Hello { window_id: None }` as its first message. The server then runs  which adopts any unconnected window-with-sessions instead of allocating a fresh `WindowId`. Without this, a `daemon stop` / `daemon start` cycle would leave the new daemon owning a brand-new window while the prior daemon's sessions remain bound to the prior `WindowId`, and the server would deny any subsequent `AttachSessions` request as cross-window. The reconnect e2e test exercises exactly this flow.

### Session State

Per-session data buffered in : 65 KB output ring buffer, `latest_snapshot` with 100 ms TTL, `last_output_at` for idle detection, `cwd`, `title`, `SessionStatus`, a never-trimmed `live_bytes` counter, and the replay log.

`live_bytes` is separate from the ring buffer because the buffer is capped: a trimmed buffer's length cannot place a replay frame in the session's byte order, and that ordering is what the replay bookkeeping stamps each frame with.

An `empty_output_frames` counter rides alongside it, incremented (and logged) whenever a zero-byte `PtyOutput` arrives. The byte counters cannot express that event — an empty frame moves neither of them — yet it is exactly the waste the server's [[server#Server#Sessions#PTY Reader Task|empty-frame send guard]] exists to prevent, so the harness records it separately and lets a test assert on it.

All sessions are keyed by `SessionId` inside , which also tracks `last_workspace_id` and `last_session_created` for workspace and session-create responses, the `window_id` assigned in `Welcome`, and the most recent automation action received in `RunAction`.

### Request Handling

Each incoming connection receives one  and returns one . Wait-type requests (WaitOutput, WaitCwd, WaitIdle, AssertExit) block on `Arc<Notify>` channels until the condition is met or the timeout fires.

### Notification System

 holds six `Arc<Notify>` channels: `output`, `cwd`, `exit`, `workspace_info`, `session_created`, and `replay`.

The server-reader task fires the matching channel on each incoming `ServerMessage`, waking whichever wait handler is blocked on it.

## Command Protocol

Request/response protocol between the CLI and daemon over a Unix socket at `/run/user/{uid}/scribe/test-daemon.sock` using msgpack framing from `scribe_common::framing`.

The socket path is returned by . The helper  creates a short-lived tokio runtime, connects, sends one , and receives one .

Key request variants: `CreateSession`, `AttachSession`, `CloseSession`, `Send`, `Resize`, `RequestScreenshot`, `RequestSnapshot`, `WaitOutput`, `WaitCwd`, `WaitIdle`, `AssertCell`, `AssertCursor`, `AssertExit`, `AssertSnapshotMatch`, `AssertNoEmptyOutput`, `ReplayStatus`, `ReplayScreen`, `AssertReplayMatchesScreen`, `WindowId`, `LastAction`, `ClearAction`, and `Shutdown`.

Key response variants: `Ok`, `SessionCreated { session_id }`, `ScreenshotData { snapshot }`, `ReplayStatus { applied, failed, live_bytes, last }`, `WindowId { window_id }`, `LastAction { action }`, `AssertFailed { message }`, and `Error { message }`.

`WindowId` (surfaced as `scribe-test daemon window-id`, printed by ) exists so a second process can be pointed at the daemon's window instead of claiming one of its own — the join target the shared-pane visual rig passes to the GPUI client as `SCRIBE_JOIN_WINDOW` (). It errors rather than returning a placeholder when no `Welcome` has arrived, because joining "no window" would silently reproduce the empty-window bug it exists to prevent.

### Automation Action Oracle

The daemon records the last `AutomationAction` delivered in `RunAction`, making headless CLI dispatch observable without GUI effects or discarded daemon logs.

`scribe-test daemon last-action` prints the Rust variant name, such as `NewTab`, or `none` before an action arrives. `scribe-test daemon clear-action` resets that state, so a smoke phase can clear, dispatch, then compare exact output without inheriting an earlier action.

## Session Management

Create, attach, and close terminal sessions through the daemon; each operation prints the confirmed session UUID to stdout for use in subsequent commands.

 sends `CreateSession` and prints the UUID.  sends `AttachSession` and prints the confirmed UUID.  sends `CloseSession` and expects `Ok`. All three are routed through .

The create path mints a launch id and sends it as `env_envelope_id`, and  reports it back per session. Without that id the server has no envelope to key a session's environment by, so every harness-created session was inert and no E2E could observe env persistence at all.

Both create and attach name a grid. `session create --cols N --rows N` spawns the PTY at that geometry the way a real client names the pane the session is about to fill, and `session attach <id> --cols N --rows N` drives the attach flow's pre-snapshot resize the way a tab switch does; omitting either flag keeps the older behaviour — the server's 80x24 default on create, and no dimensions at all on attach. The cell box is 1x1 on both paths, matching `session resize`, so a create and a later attach or resize at the same grid produce a byte-identical `TIOCSWINSZ`, which the kernel answers with no `SIGWINCH` at all. That is what lets a script count the signals a geometry change really costs.

The create path also sends the same `Subscribe` a real client sends, and deliberately no `AttachSessions`: the server attached this connection while it started the session, so re-attaching would only replay a terminal that has emitted nothing.

## Input Simulation

Send keystrokes to a session with escape sequence expansion (`\n`, `\t`, `\\`, `\xNN`).

 converts the string argument to raw bytes before forwarding via a `Send` request.  validates the session ID, calls `parse_escapes`, and sends the byte payload.  sends a `Resize` request to change terminal dimensions.

## Wait Primitives

Blocking synchronization helpers: wait for regex output, CWD change, or terminal silence — each with a configurable timeout in milliseconds.

 sends `WaitOutput { pattern, timeout_ms }` and blocks until the daemon's regex matches the session's *visible* content: the output ring buffer is normalised before matching by stripping ANSI/OSC/CSI escape sequences and lone CRs, and the regex is built with multi-line mode enabled, so `^X$` anchors match line boundaries of what the user would see on the terminal grid rather than positions within the raw `\r\n`/escape-laden PTY stream.  sends `WaitCwd { path, timeout_ms }` and blocks until the session's CWD matches.  sends `WaitIdle { quiet_ms, timeout_ms }` and blocks until no output has arrived for `quiet_ms` milliseconds.

## Assertions

Verify screen cell content, cursor position, snapshot equality, stream shape, and how a session's child died — returning `TestFailure` (exit 1) on mismatch.

 checks that a specific cell contains the expected character; on failure the daemon includes a 3×3 neighborhood context in the error message.  verifies the cursor is at the expected row/col.  loads a reference JSON snapshot and compares cell content, cursor position, and cursor visibility.  waits up to `timeout_ms` for the session to exit with the expected code.

[[crates/scribe-test/src/assert.rs#assert_no_empty_output|assert-no-empty-output]] is the odd one out: it asserts about the *stream* rather than the screen, failing if any zero-byte `PtyOutput` ever arrived for the session. No screen assertion can catch that frame — it changes no cell — and the smoke suite runs it last, after phases that have driven the PTY filters, so a filter that starts shipping emptied chunks is caught by an existing test rather than a dedicated one.

Exit assertions come in two shapes because the wire keeps a terminating signal in its own `SessionExited` field rather than folding it into `exit_code`: `assert-exit` matches the code, [[crates/scribe-test/src/assert.rs#assert_signal|assert-signal]] matches the signal number. Both also fail when more than one `SessionExited` arrived for the session — the server elects a single exit path per session through one compare-and-swap, so a second frame is a real defect rather than noise.

## Screen Capture

Capture the current terminal state as a PNG screenshot or a JSON text snapshot for later comparison.

 requests a `ScreenshotData` response from the daemon and writes the snapshot to a PNG file via .  requests the same data but serializes the `ScreenSnapshot` to pretty-printed JSON.

### PNG Rendering

 uses `cosmic-text` for shaping, xterm-256 ANSI palette for colours, and alpha blending for compositing. Cells are 10×20 px at 14 pt.  covers I/O and PNG encoding failures.

## Replay Observation

The daemon inflates every `SessionReplay`, applies it to a local terminal, and keeps that terminal fed with the session's later output, so attach-path content and ordering are assertable rather than invisible.

Before this the daemon logged and dropped the frame, and assertions read `RequestSnapshot` — which the server answers from its own `Term` regardless of what it put on the wire. A replay that was late, lossy, duplicated, or never sent looked exactly like a correct one.

The receiving half mirrors a real client and reuses the server's own machinery — `build_term_config`, `ScribeEventListener`, `vte::ansi::Processor`, and `snapshot_term` — so the replayed view cannot disagree with the server for reasons that live in the harness. Applying a frame inflates the zstd payload, feeds the ANSI into a fresh `Term` at the frame's geometry, then trims the blank history the replay's own `ESC [ 2J` scrolls into a fresh grid (a client reaches the same state through `TrimScrollback`). Later `PtyOutput` bytes and `Resize` requests are applied to the same terminal, so the view stays comparable to the server's screen. A frame that fails to inflate is counted and logged rather than fatal, matching the client's graceful skip.

Replayed bytes deliberately stay out of the output ring buffer. Keeping that buffer the raw live PTY stream is what lets an assertion separate "this text came back in the replay" from "this text arrived after the replay"; each applied frame instead records the session's `live_bytes` count at its arrival, which is the ordering fact the buffered-flush and paced-replay work has to check.

Three CLI surfaces read it back:

- `scribe-test replay status <session>` prints `frames`, `failed`, `live-bytes`, and a `last-frame` line carrying geometry, cursor, alt-screen flag, compressed and inflated sizes, and the live bytes before and after the frame. `--min-frames` blocks on the `replay` notifier, because the attach reply lands on the reader task and would otherwise race the next CLI invocation; `--expect-frames 0` is how a test states that a session was never sent a replay at all.
- `scribe-test replay screen <session>` prints the replayed screen as text, or writes it as snapshot JSON with `--json`, so scripts grep replayed content directly.
- `scribe-test replay assert-matches <session>` requests a fresh server snapshot and compares it against the replayed view read back under the same lock — the losslessness oracle for an attach. Cursor *visibility* is excluded, because the encoder deliberately leaves the cursor hidden on alt-screen replays so the app's own output owns it. Callers settle the session with `wait-idle` first, since output arriving while the request is in flight legitimately moves the view ahead.

## Server Lifecycle

Start, stop, and hot-reload the scribe-server process from tests using PID-file tracking and socket polling.

 spawns `scribe-server`, writes its PID to `/run/user/{uid}/scribe/scribe-server.pid`, then polls until the server socket appears (5 s timeout).  reads the PID file, sends SIGTERM, waits up to 3 s, escalates to SIGKILL if needed, and removes the PID file.  launches `scribe-server --upgrade`, waits for the old process to exit (10 s timeout), polls for the new socket, and updates the PID file.

## IPC Client

Thin async wrapper around the `scribe_common::framing` layer for sending `ClientMessage` and receiving `ServerMessage` over the server's Unix socket.

 opens a `UnixStream` to the server socket path.  encodes and writes a `ClientMessage` over the write half.  reads and decodes a `ServerMessage` from the read half. The daemon's `run` function uses these to maintain its persistent server connection. See  for message types.

## Test Lifecycle

Typical end-to-end test pattern using the `scribe-test` binary as a shell-scriptable harness.

```
# Start infrastructure
scribe-test server start
scribe-test daemon start

# Create session and capture ID
SID=$(scribe-test session create)

# Drive the session
scribe-test send "$SID" "echo hello\n"
scribe-test wait-output "$SID" "hello" --timeout 3000
scribe-test wait-idle "$SID" --ms 200

# Assert and capture
scribe-test assert-cell "$SID" 0 0 'h'
scribe-test assert-cursor "$SID" 1 0
scribe-test screenshot "$SID" out.png

# Cleanup
scribe-test session close "$SID"
scribe-test daemon stop
scribe-test server stop
```

### Smart Selection Manual Verification

Smart Selection currently relies on manual quickstart scenarios rather than new test code, matching the project instruction for this feature.

Manual coverage lives in `specs/002-smart-selection/quickstart.md`: configure quad-click and double-click activation, verify default matches for words, namespace identifiers, paths, quoted strings, include paths, URIs, Objective-C selectors, and emails, edit and restore rules in Settings, and confirm context-menu actions execute only after explicit menu selection.

## Installer Script Regression Harness

Offline shell harness for Debian `postinst` behavior so packaging regressions can be caught without touching the live user session.

`tests/install/postinst-regressions.sh` sources only function definitions from
`dist/debian/postinst`, then tests fixtures without a live user session. It
checks zombie client exits and the Vulkan-less upgrade guard: a failed probe
restores the preinst stash, leaves a running session alive, emits a warning,
and disables relaunch. `just test-install-vulkan-guard` runs this guard in a
disposable Debian container.

## E2E Recipe Contract

Docker E2E recipes default to portable software rendering and keep test sources immutable, while explicit image and environment parameters support focused diagnostics.

`just e2e-func <script> image=<tag>` and `just e2e-visual <script> image=<tag>` select a prebuilt release or debug image. Both recipes pass through `TEST_TIMEOUT`, `RUST_LOG`, and `SCRIBE_KEYRING` from the host environment.

Visual recipes omit GPU passthrough by default because lavapipe supplies deterministic software Vulkan. Set `SCRIBE_E2E_GPUS=all` (or another Docker GPU request) to opt in; hosts without NVIDIA container tooling need no special flag.

Every recipe mounts `./tests/e2e` at `/tests:ro`, leaving `/output` as the only writable bind mount. Both entrypoints export `SCRIBE_E2E_SANDBOX=1`, and every func or visual shell script checks that sentinel before its first command and exits 99 when invoked directly on the host.

`just e2e` builds the release functional image and runs all executable `tests/e2e/func/*.sh` scripts. Its explicit inventory includes AI launch, bash/zsh/fish launch, and CLI coverage plus every older geometry, lifecycle, input, and persistence script. A sorted inventory comparison aborts before the first container if a script is omitted. Only `env-persistence.sh` receives `SCRIBE_KEYRING=1`; the more expensive encrypted rows in the three AI shell scripts remain the focused opt-in commands documented below.

`just e2e-all-visual` builds the release visual image and runs every executable `tests/e2e/visual/*.sh` test serially. `update-common.sh` is a non-executable sourced helper, not a test. Each test delegates to its specialized recipe when one exists, preserving that recipe's timeout, fixture, and config contract; remaining tests use `e2e-visual`. A sorted inventory-to-mapping comparison makes omissions fatal.

The visual aggregate truncates `test-output/e2e-visual-summary.jsonl` at startup, then immediately appends one JSON object after each script. Rows contain `script`, `recipe`, `status` (`pass` or `fail`), integer `exit_code`, and integer `duration_s`. Failures do not stop later scripts, but any failure makes the aggregate exit nonzero after collection.

E2E aggregate runs are serial-only on one host. Concurrent runs would race on shared `test-output/`, overwrite the single release/debug Docker image tags, and collide in the shared `/run/user/<uid>` Scribe socket namespace. Use separate hosts rather than parallel local invocations.

No aggregate silently retries a failure. A repeatedly flaky script needs a quarantine bead recording evidence and an owner before any temporary exclusion; the inventory check must represent that quarantine explicitly so omission cannot masquerade as coverage. Current functional and visual inventories have no quarantined scripts.

## AI-Launch Harness Plumbing

The functional harness can launch structured AI sessions and inspect the provider process without requiring a GPUI client.

`scribe-test session create --ai-provider <claude|codex>` populates the production `AiLaunchSpec`; `--ai-resume-mode <new|resume>` defaults to `new`, and `--ai-conversation-id <id>` supplies the resume target. `--cwd <path>` names the PTY working directory, while `--env-envelope-id <id>` overrides the harness-minted launch id for restore-envelope scenarios. Omitting every new flag preserves plain-session behavior.

`tests/e2e/bin/claude` is the deterministic Claude stand-in already reachable through the functional container's `/tests/bin` PATH. It atomically writes `${SCRIBE_AI_STUB_OUT:-/tmp}/claude-invocation.txt`: argv one argument per line, an `--ENV--` delimiter, then locale-sorted environment entries. The stub writes only to the requested directory, never the read-only `/tests` mount.

`tests/e2e/func/ai-launch-smoke.sh` requests a resumed Claude launch with a quoted conversation id, explicit cwd, and envelope override. It asserts the stub's exact argv, `PWD`, and daemon-reported envelope id.

## E2E Functional Tests

Functional end-to-end tests that drive real sessions through the `scribe-test` harness and assert rendered output.

### E2E Image Build Profiles

Docker harness images build from an explicit release or debug binary profile so diagnostics never overwrite the normal release image tags.

`just docker-func` and `just docker-visual` stage release binaries by default
and retain the `scribe-test-func` and `scribe-test-visual` tags. Run
`just docker-func profile=debug` or `just docker-visual profile=debug` to
stage `target/debug` binaries under the separate `scribe-test-func-debug` or
`scribe-test-visual-debug` tags. No other profile is accepted.

The staging helper rejects a required binary that is missing or older than
the newest commit touching `crates/`. Rebuild with `just build-release` for
release images or `just build` for debug images before retrying the Docker
recipe.

The `docker/Dockerfile.func` image bundles the workspace's `dist/shell-integration` tree at `/usr/local/share/scribe/shell-integration` so the in-container `scribe-server`'s  resolves them and injects `SCRIBE_SHELL_INTEGRATION=1` plus the per-shell rcfile/ZDOTDIR/XDG plumbing into every spawned PTY. Without this copy, the `shell-integration.sh` e2e test never sees the env var or the OSC marks the integration scripts emit.

### CLI Smoke E2E

`tests/e2e/func/cli-smoke.sh` validates the headless `scribe` CLI against the disposable functional server.

Covered surface is `windows`, explicit-window `action new-tab`, `profile active`, `profile list`, and a server-absent `windows` failure. Window enumeration must return only the connected daemon window, proving the transient CLI request creates no extra window. Action dispatch must arrive at that daemon as `RunAction(NewTab)`. Profile reads must identify the active profile without changing the profile store, and server absence must return nonzero with useful socket error text.

Bare interactive passthrough is intentionally not exercised. Profile-writing commands (`save`, `switch`, `import`, and `export`) and the GUI effect of routed actions are also outside this smoke test; the action oracle covers routing only.

### AI Shell Environment Matrix

Three functional scripts verify structured AI launch behavior across the supported bash, zsh, and fish login shells without relying on a GPUI client.

Every script changes root's passwd shell with `usermod -s` after the disposable container server starts, then requires the server debug log to report the selected binary with `tier = "passwd"`. The deterministic Claude stub records resumed-provider argv and the environment after shell startup.

The matrix covers the server side of `ai_tab_cwd`: an existing `--cwd` is preserved, while a nonexistent path falls back to `$HOME`. Client selection among pane, project-root, and home modes remains covered by the spec-018 GPUI suites documented in `specs/018-ai-tab-shell-env/verification.md`.

Each script keeps its startup, integration, cleanup, argv, and cwd assertions active without a keyring. With `SCRIBE_KEYRING=1`, the functional entrypoint's integrated Secret Service fixture enables a real plain-shell env delta to pass through the production 100 ms debounce into an encrypted `.envz`; an AI launch using `--env-envelope-id` must expose that delta in the deterministic Claude stub and consume its session-specific file from `$XDG_RUNTIME_DIR/scribe/env-apply/`.

Run the covered keyring rows exactly as:

```bash
SCRIBE_KEYRING=1 just e2e-func func/ai-shell-env-bash.sh
SCRIBE_KEYRING=1 just e2e-func func/ai-shell-env-zsh.sh
SCRIBE_KEYRING=1 just e2e-func func/ai-shell-env-fish.sh
```

The source session stays alive while polling for the `.envz` and through AI consumption because persistence has no shutdown flush. With the flag unset, every script skips only the encrypted row and retains all non-keyring assertions.

#### Bash AI Shell Environment

`tests/e2e/func/ai-shell-env-bash.sh` verifies first-profile-wins bash login startup and the AI integration preamble before provider exec.

The stub must see only `.bash_profile`'s marker and PATH, not `.bash_login`, `.profile`, or `.bashrc`. It also requires `SCRIBE_SHELL_INTEGRATION=1` while launch-only `SCRIBE_AI_TAB`, `SCRIBE_INTEGRATION_SCRIPT`, restore-file, and `ENV` variables are absent.

#### Zsh AI Shell Environment

`tests/e2e/func/ai-shell-env-zsh.sh` verifies `.zshenv` → `.zprofile` → `.zshrc` ordering through the redirected integration bootstrap.

The stub must see the complete order and integration marker, with `ZDOTDIR`, `SCRIBE_ORIG_ZDOTDIR`, launch-only variables, and the restore-file variable removed before exec.

#### Fish AI Shell Environment

`tests/e2e/func/ai-shell-env-fish.sh` verifies Scribe's vendor `conf.d` script runs before the user's `config.fish` and supplies the provider PATH.

The stub must see the vendor/config order and integration marker, with `SCRIBE_ORIG_XDG_DATA_DIRS`, launch-only variables, and the restore-file variable removed and the originally absent `XDG_DATA_DIRS` restored before exec.

### AI Indicator E2E

Two scripts covering the AI state indicator and its context-window percentage, both driving the  rather than OSC 1337 and reading the result back through .

Transport and readback are the two things these scripts get right that a naive version cannot. AI state, prompt text, and context % travel over the hook channel — OSC 1337 parsing for them was removed by spec 003 FR-022 — so the scripts run `scribe-hook-helper` inside the session shell, where the server exports `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID`. Readback cannot use a screen snapshot:  returns the server's PTY grid, and the prompt bar and tab label are client chrome that never appears in it. `scribe-test ai-chrome` renders the session's live AI state through  instead, emitting one `prompt-bar:` line whenever a percentage exists and one `tab:` line only from the warn band up.

Both scripts park the shell in `read` after firing hook events. A returning shell prompt (OSC 133;A) tells the server the AI tool exited, and  then synthesizes an `AiStateCleared` that would wipe the state the helper just set. Parking the shell reproduces production, where hooks fire while the AI tool owns the foreground; releasing the parked shell is how each phase resets to a clean slate.

#### AI Context Thresholds E2E

Seven-phase test in `tests/e2e/func/ai-context-thresholds.sh` validating prompt-bar and tab inline % across all threshold bands for Claude and Codex.

Claude phases set `processing` plus a prompt and a context refresh of 50/72/91. Phase 1 asserts `50%` renders on exactly one chrome surface and Phase 4 reads that as the tab inline being suppressed below `warn=70`; phases 2 and 3 assert the Warn/Danger values render on two surfaces (prompt bar + tab inline). Codex phases repeat the same provider-symmetric checks at 51/73/92.

#### AI State Indicator E2E

Seven-phase test in `tests/e2e/func/ai-state-indicator.sh` covering the state machine end of the same channel.

It cycles all five `AiState` variants without corrupting the grid, asserts a context refresh of 42 reaches the AI chrome, confirms a legacy OSC 1337 payload is still consumed silently with the text on either side of it preserved, drives rapid transitions without deadlock, asserts `state_cleared` empties the chrome, and closes a session while an AI state is active.

### Env Persistence Create-Path E2E

`tests/e2e/func/env-persistence.sh` verifies launch ids on every run and, with the opt-in keyring fixture, encrypted env-delta persistence and restore.

It reads the entrypoint session's id back with `scribe-test session envelope-id`, creates a second session and asserts its id is a distinct UUID (a shared constant would satisfy a bare non-empty check), and drives the second session to a prompt so the id provably belongs to a live launch rather than a bookkeeping entry.

With `SCRIBE_KEYRING=1`, the functional entrypoint starts a session D-Bus and unlocked Secret Service before the server. The script enables persistence, exports a unique value from a fresh integrated shell, and polls the launch's real `.envz` past the 100 ms production debounce. It requires private file permissions, rejects plaintext leakage, then creates another session with the same envelope id and observes the restored value, proving the encrypted write/read path end to end.

Without `SCRIBE_KEYRING=1`, only the launch-id and live-session checks run. The remaining persistence caveat is that there is no shutdown flush: tests must keep the source session alive and poll for debounce completion before closing it.

### Plain-Shell Env Restore Ordering

Disposable zsh/fish startup probes verify post-rc restore precedence without attaching to a Scribe process or host socket.

Each probe uses isolated `HOME`/XDG/runtime paths, a staged restore file, an rc/config export that conflicts with it, and a stub hook helper. At the first prompt the restored value must win, the file and transport variable must be gone, and exactly one `--baseline-ready` frame must contain both the restored value and an rc-only export. A second prompt after one session-only export must emit only that export as the delta, proving rc-only values remain baseline state. Companion AI-mode probes require the marker to be consumed while the restore file remains and no plain-tab initializer is installed, preserving post-login preamble ownership.

### Fresh Create Geometry E2E

`tests/e2e/func/fresh-create-geometry.sh` asserts the create-is-an-attach contract: the PTY starts on the grid the create named, the create draws no `SessionReplay`, and no `SIGWINCH` is spent on a grid that did not change.

The oracle for the signal count is a size reporter running inside the pane — a `trap 'winch=$((winch+1))' WINCH` installed once the shell is at its prompt — read back with `echo` alongside `stty size`. It has to be in-pane because a shrink-and-regrow pair is invisible in every screen snapshot: the grid ends exactly where it started. Phase 3 re-attaches at the pane's own geometry and requires the counter to still read zero; phase 4 asserts that attach *did* send a replay, which is what keeps phase 2's `replay status --expect-frames 0` from passing vacuously, then resizes to a different grid and requires exactly one signal.

### Shared-Grid Debounce E2E

`tests/e2e/func/viewport-debounce.sh` asserts the trailing half of the shared-window grid debounce: a drag that outlives one debounce window costs the pane exactly one `SIGWINCH`, and it lands on the last reported size.

It reuses the in-pane `trap 'winch=$((winch+1))' WINCH` oracle for the same reason the geometry script does — a mid-drag apply and its correction leave no trace in any screen snapshot. Phase 1 is what makes the test reach the code at all: `Resize` is only a viewport report in a shared mode, and the mode is snapshotted onto the share at claim time, so the script rewrites the config to `free_for_all` and restarts the disposable server *before* the harness daemon says Hello. Phase 3 then reports 16 viewports 50 ms apart — closer together than the 250 ms debounce, but a full second end to end — and fails loudly if the drag happened to fit inside one window, since nothing would have been coalesced. A per-report timer fails phase 4 with a double-digit signal count; the pre-fix binary times out there.

### Resize Coalescing E2E

`tests/e2e/func/resize-coalescing.sh` asserts the paced half of the direct grid-set path: a `SingleController` drag costs at most four applies per second, and it still lands on the size it stopped at.

It is the `SingleController` counterpart to the shared-grid script — the default mode, so it needs no config rewrite — and reuses the same in-pane `trap 'winch=$((winch+1))' WINCH` oracle, since a drag's intermediate grids leave no trace in a snapshot that ends where the last report put it. Phase 2 reports 40 one-column steps 30 ms apart, times the drag, and derives the signal ceiling from the elapsed span rather than a constant, so a container slow enough to space the reports an interval apart fails as "cannot distinguish pacing" instead of passing vacuously. An unpaced server spends one signal per report and fails phase 3 on the verdict the pane computes for itself.

### Session Lifecycle E2E

Scripted lifecycle coverage proving the GPUI client survives detach, hot-reload, and full cold restart against a disposable test server — never the user's live server (the CLAUDE.md invariant), as the harness runs its own `scribe-test server`.

In every script the `scribe-test` daemon is the client stand-in: `daemon stop` is the client going away and `daemon start` is a fresh client that must re-attach.  sends `Hello { window_id: None }` so  adopts the unconnected window-with-sessions, which is what makes every re-attach flow below possible.

`tests/e2e/func/session-exit-status.sh` covers the child-exit watcher's reporting. It runs `exit 42` in one session and asserts the code arrives verbatim, then `exec`s a `sleep` over a second session's interactive shell — which ignores SIGTERM — and `kill -TERM`s the pid it printed, asserting a signal 15 termination rather than an exit code. A third session backgrounds a HUP-ignoring subshell before exiting, so a descendant still holds the slave fd and the master never ends: the exit is observable only through the watcher, and the script asserts the real code still arrives. A settle window and a re-assertion of all three sessions prove no second `SessionExited` follows.

`tests/e2e/func/reconnect.sh` covers plain detach/reattach: run a command, start a background job, `daemon stop`, `daemon start`, `session attach`, then assert fresh input works and the background job survived the disconnect. It also asserts the attach *replay* itself — one applied frame carrying the pre-detach marker, and a closing `replay assert-matches` proving the replayed view plus the output that followed it still equals the server's screen.

`tests/e2e/func/attach-lossless.sh` covers the same reattach with output *in flight*, which is the only condition the snapshot/install window is visible under. It backgrounds a 400-line trickle in the session, cycles `daemon stop` / `daemon start` / `session attach` twice while the trickle runs, then lets it drain and closes on `replay assert-matches`. Because the replayed view is the last replay plus every `PtyOutput` byte after it, a chunk lost in the attach window shifts it against the server's screen permanently and a duplicated flush shifts it the other way — neither survives the comparison, while `RequestSnapshot` alone stays green for both.

`tests/e2e/func/hot-reload.sh` covers server `--upgrade` under a live client: it snapshots a session, stops the daemon, runs  (fd handoff to the new server), then reconnects and asserts the session, its background job, and its on-screen scrollback all survived the graceful handoff.

`tests/e2e/func/handoff-truecolor.sh` covers the same `--upgrade` with a screen the replay decoder used to choke on. It paints 60 rows of per-cell 24-bit fg/bg — every cell forcing its own SGR run — and asserts the truecolor cell count is unchanged across the handoff, so a decode that failed and left [[server#Server#Handoff#Session Replay Encoding|restore_from_handoff]] with no legacy snapshot to fall back on shows up as a blank grid rather than passing quietly. The regression proof lives in the Rust round-trip tests, which size the fixture against the retired bound directly; this script is the end-to-end confirmation on a disposable server.

`tests/e2e/func/cold-restart.sh` covers the **server** half of cold-restart recovery: every session, its scrollback, its terminal geometry, and its background jobs survive the client going away entirely. It opens three sessions with distinct markers, resizes one to 132x50, starts a background job, then disconnects the client (`daemon stop`) while the server keeps the sessions. A fresh `daemon start` must re-attach all three; the script asserts each replayed and accepts input, the resized session still reports 132 cols, and the background job survived.

It is deliberately **not** the oracle for the client's own restore. The daemon has no window, no layout and no restore store, so it can neither write a `RestoreStore` snapshot nor replay one — a green run here says nothing about `--restore-child` fan-out or window geometry persistence. Those are asserted against the real client process by .

### Failure-Path E2E

Scripted degraded-path coverage proving the client fails loudly (never hangs) when the server is unavailable or vanishes mid-session, and recovers cleanly once it returns. Both scripts drive the disposable test server only.

`tests/e2e/func/failure-server-down.sh` covers server-down-at-launch and adoption failure. With the server stopped, `daemon start` must return non-zero within its bounded socket wait rather than block, because  fails its initial `ipc::connect()` and the client socket never appears. It then recovers, and — on a fresh daemon with no cached `SessionCreated` — asserts that adopting a nonexistent session id errors (server denies,  times out) without crashing the still-usable client.

`tests/e2e/func/failure-socket-loss.sh` covers a mid-session server crash. A SIGTERM `server stop` drops the client's IPC with no upgrade handoff; the daemon's server-reader loop ends, so it tears down and removes its command socket. The script polls until commands fail (proving loss detection, not a hang), reconnects to a freshly started server, asserts the crashed session is gone (PTYs died with the server, so re-adopt fails — the deliberate contrast with hot-reload), and confirms a fresh session works end to end.

## Visual E2E Tests

Visual end-to-end tests run the real `scribe-client` window headlessly (`docker/Dockerfile.visual`) and assert against screenshots written to `/output`.

`docker/entrypoint-visual.sh` starts Xvfb, an `openbox` window manager, `scribe-server`, the daemon, and the GPUI client, then runs the test script. The image also ships `scribe-hook-helper` and the entrypoint exports `SCRIBE_RUNTIME_DIR`, so a test that needs a provider event (task label, AI state, context %) drives the real hook channel instead of forging a frame. The image pins `VK_ICD_FILENAMES` to lavapipe's software Vulkan ICD (shipped in `mesa-vulkan-drivers`) so the client renders deterministically with no GPU, and sets `SCRIBE_DISABLE_ANIMATIONS=1` so consecutive frames are byte-identical. Tests drive the client through `xdotool`/`xclip` and capture frames with `scrot`. An optional `SCRIBE_EXTRA_CONFIG` env var seeds `config.toml` before the *server* starts so a test can exercise opt-in settings (e.g. `terminal.paste_confirmation`); the shared-pane rig appends to the same file, which is why both are written up front rather than one clobbering the other. `SCRIBE_VISUAL_APP=settings` swaps the client launch for `scribe-client --settings` (logged to `/output/settings.log`) so the settings window can be driven as its own app, and `SCRIBE_SEED_TRUST=1` plants a trusted network and an approved device into the server's LAN trust stores before it starts.

The client's stderr is redirected to `/output/client.log` and its pid and log path are exported as `SCRIBE_CLIENT_PID` / `SCRIBE_CLIENT_LOG`, so a script can assert on runtime behaviour that leaves no pixels behind and can prove the process never restarted. `RUST_LOG` defaults to `scribe_server=info,scribe_client=info` so those client lines are actually emitted.

The server's output is captured the same way: the entrypoint exports `SCRIBE_TEST_SERVER_LOG=/output/server.log`, which  honours when it spawns `scribe-server`, and re-exports it as `SCRIBE_SERVER_LOG` for scripts. That is how a test proves a *client-to-server* message crossed the wire — the server logs the window id it received it on — rather than trusting the client's own "I sent it" line.

The test script's own output goes to `/output/result.log` by plain redirection, and the entrypoint streams that file to the container's stdout with a `tail -f` it owns and kills. It must never be a pipe: every process a test backgrounds inherits the test's stdout, so while the run was `timeout … | tee /output/result.log`, a single orphaned `scribe-client` held the write end open, `tee` never saw EOF, and the container hung forever *after* the test printed PASS — `TEST_TIMEOUT` bounds the test process, which had already exited. A file descriptor on the log blocks nothing, so an orphan can no longer wedge an unattended run, and the entrypoint reaps stray clients once the test returns. A script that relaunches the client still owns that process: `tests/e2e/visual/reconnect.sh` logs its phase-3 relaunch to `SCRIBE_CLIENT_LOG` and kills that pid before it exits, the same discipline `tab-window-chords.sh` and `pane-workspace-layout.sh` follow.

The GPUI client sets its X11 `WM_NAME`/`_NET_WM_NAME` to `Scribe` via  so `xdotool search --name "Scribe"` can locate the window for focus and capture.

`openbox` is required, not cosmetic:  runs on the client's live key path and suppresses synthetic key input whenever `_NET_ACTIVE_WINDOW` does not name the client window, and only a window manager sets that root property under Xvfb. Without a WM, `xdotool`-driven visual tests cannot type.

Screenshots are taken full-screen because a Vulkan surface may not be readable per-window, so any test that measures pixels must crop to the client window first. The WM title bar is a saturated light blue that on its own clears a "hundreds of colored pixels" threshold — an uncropped measurement passed for months over a completely black grid.

### Shared-pane rig

`SCRIBE_SHARED_PANE=1` (`just e2e-visual-shared <script>`) is how a visual test gets a live pane that BOTH the GPUI client and `scribe-test` can see. Without it the client is blind to the harness's session and the harness is blind to the client's.

By default `docker/entrypoint-visual.sh` launches the client and only then runs `scribe-test session create`. The server sends `SessionCreated` solely to the connection that asked for it, and  answers a window with its OWN sessions (falling back to unowned ones), so the running client never learns the daemon's session exists — it renders an empty grid while the pane the test drives runs untouched. That is what made the emoji check a false pass and left the reconnect capture unasserted.

Under `SCRIBE_SHARED_PANE=1` the entrypoint instead seeds `[remote] sharing_mode = "free_for_all"` into `config.toml` *before* the server starts, creates the session first, reads the daemon's window id with `scribe-test daemon window-id`, and passes it to the client as `SCRIBE_JOIN_WINDOW`. The client's `Hello` names that window, the server resolves it as a local additive share join (), and the client's `AttachSessions` ADDS its sink alongside the daemon's. Both stay attached: the window under the camera shows the pane, and `scribe-test send` / `wait-output` / `snapshot` still work against it. `free_for_all` (rather than `shared_single_typist`) is the mode because both participants must be able to type — the harness through `KeyInput`, the user's simulated input through the client.

The entrypoint gates on the client's own `attaching to session` line before running the test body, so a script never drives a window that is still an empty grid — a state no screenshot can distinguish from an idle pane.

### Server-upgrade reattach oracle

`tests/e2e/visual/server-upgrade-reattach.sh` proves the running GPUI process redials after a real `scribe-server --upgrade` handoff and reattaches its rendered session without a client relaunch.

The shared-pane rig keeps the test daemon and visible GPUI window on one session. The script records initial topology and attach log counts, upgrades the live server through , then requires both counts to advance while the original `SCRIBE_CLIENT_PID` and `Scribe` window remain alive. Because the harness daemon's own stream ends with the old server, the final oracle types a sentinel through the surviving GPUI window and requires a substantial terminal-body repaint after the replacement connection completes `Hello` / `ListSessions` / `AttachSessions`.

### Color emoji renders in color

`tests/e2e/visual/color-emoji.sh` proves color emoji render in color rather than as monochrome/tinted glyphs — the US3 headline parity item promoted to an automated visual check.

It runs on the , so the emoji it sends through `scribe-test` reach the window it photographs. It prints a grid of solid color-block and pictographic emoji, waits for the sentinel to echo back on the PTY, crops the capture to the client window, and asserts via ImageMagick's HSL saturation channel that a strongly-saturated pixel count clears a floor. A monochrome/tinted fallback tints every glyph the pale foreground color, so its saturated-pixel count collapses to near zero.

A lit-pixel phase runs first and fails separately when the window holds no pane content at all. Both guards exist because this script previously reported ~980 saturated pixels — comfortably over its 300 floor — against a completely black grid: the client was attached to nothing, and every one of those pixels came from the openbox title bar outside the window crop.

`tests/e2e/visual/paste-confirmation.sh` verifies the spec-011 paste gate (): a single-line paste carrying control/escape bytes pops the confirmation with a caret-escaped preview (`^[`), while a plain single line and a tab-separated line paste straight through without a dialog.

### Overlay actions run for real

`tests/e2e/visual/overlay-actions.sh` is the scripted oracle for : it asserts the *effect* of a chosen palette or context-menu row, which no headless test over the overlay models can reach.

The overlay models passed their unit suites the whole time the shell was dropping their events, so a green `#[gpui::test]` proves nothing about reachability here. The script instead drives the live window once per action class. It right-clicks the grid and clicks the smart-selection row, then asserts the row's exact payload lands on the session's PTY (`scribe-test wait-output`) AND that the echoed command and the shell's answer appear as new lit pixels — the first names the bytes, the second proves they also reached the window on screen. It opens the palette, filters to "New Tab", confirms, and waits for a new `opened a new tab` line, which the client only ever writes after `CreateSession` comes back as `SessionCreated`. Finally it confirms "Open Settings", the palette's one row whose destination is a different top-level window, and requires both the client's `opened the settings window` line and a mapped X11 window named "Scribe Settings"; the full entry-point matrix lives in .

The context-menu row is clicked at a pixel offset calibrated against the captured frame, so a layout change to the menu box shows up as a failing phase rather than a silent miss.

It runs on the . The script used to open with a phase 0 that killed the client, ran `scribe-test daemon stop` to release the window ownership hiding the session, and relaunched — the only way to get a pane in front of the camera before that rig existed, and one that cost every server-side assertion because `wait-output` needs the daemon it had just stopped. That preamble is gone; phase 0 now only confirms the shared pane is painted before any action is driven at it.

### In-app settings entry points

`tests/e2e/visual/settings-entry.sh` (`just e2e-visual-settings-entry`) is the app-level oracle for the `settings` parity row: it drives the running terminal window and asserts a second top-level window really maps (see ).

The settings window was complete and unreachable for the whole rebuild — `KeyAction::OpenSettings` hit a swallow arm — so the only evidence that matters is a window on screen, which no `#[gpui::test]` can produce. Four phases drive the real client through XTEST and count windows titled "Scribe Settings", the exact title  sets. `ctrl+comma` (the `settings` binding's Linux default) must map that window and paint it; pressing it again from the terminal window must leave the count at one and log the focus line only the retained handle path writes; the palette's "Open Settings" row and the titlebar gear must reach the same handler with the same no-duplicate result.

Geometry comes from `xwininfo`, not `xdotool getwindowgeometry`: openbox reparents the window into a decorated frame and xdotool reports that frame's origin, so a frame-relative gear click would land in the window manager's own title bar. The gear offset is derived from the titlebar's fixed 34px band and its 34/40px buttons, so a titlebar layout change fails the phase rather than missing silently.

### Tab and window chords reach their actions

`tests/e2e/visual/tab-window-chords.sh` is the scripted oracle for the `close_tab` and `new_window` parity rows, which were unreachable in the running client while their headless coverage stayed green.

Both chords were claimed by  before the binding dispatcher ever saw them, so only the live window can prove the fix. The script presses `ctrl+shift+q` and waits for the `closing the active tab` line that  alone writes, then presses `ctrl+shift+n` and requires both the `opened a new terminal window` line and a second mapped X11 window — a log line alone would not distinguish "the action ran" from "a window actually appeared". A third phase opens the close dialog on its relocated `ctrl+shift+d` and asserts the frame really repainted, so moving the overlay off `close_tab`'s default did not strand the surface. It reuses the session-adoption preamble documented under .

### Pane and workspace layout is live

`tests/e2e/visual/pane-workspace-layout.sh` is the scripted oracle for the fourteen pane and workspace parity rows, whose models () passed their headless suites for the whole rebuild while the running binary never instantiated them.

A `#[gpui::test]` over the trees cannot tell "the layout is live" from "the action is an intercepted no-op", so each phase drives the real window through XTEST and asserts an effect only a live  can produce. `ctrl+shift+backslash` must log the split, pull a second session off the server, adopt it into the new pane, and republish BOTH panes at about half the window's 120 columns. Typing then has to add ink to the RIGHT half of the grid and not the left — the new pane is focused and is a separate terminal, not a second view of the first — and after `shift+ctrl+alt+Left` the same test has to come out the other way round, which is what proves directional focus moved where keystrokes go. `ctrl+Tab` covers `cycle_pane`; `ctrl+shift+w` must both drop the pane locally and make the server log `session closed by client`; `ctrl+alt+backslash` must split the window into a second region and report `regions=2`.

The ink assertions crop to the grid area only, excluding the titlebar above and the status bands below, and the log assertions run against an ANSI-stripped view of the client log because `tracing_subscriber::fmt` colours field names so `field=value` is never contiguous in the file. One chord is rebound through `SCRIBE_EXTRA_CONFIG`: `workspace_focus_left` defaults to `ctrl+alt+Left`, which openbox — the window manager the container must run so the client's X11 guard sees a real `_NET_ACTIVE_WINDOW` owner — grabs for desktop switching before any application sees it. The workspace *split* on the same layer still fires from its untouched default, so the chord path itself stays proven. The script reuses the session-adoption preamble documented under .

### Workspace IPC on the wire

`tests/e2e/visual/workspace-ipc.sh` (`just e2e-visual-workspace-ipc`) is the app-level oracle for the `CreateWorkspace`, `CloseWorkspace`, `MoveSession` and `ReportWorkspaceTree` client rows and the inbound `WorkspaceInfo` row (see ).

The gap those rows had was reachability, not correctness: the window's workspace tree was live after the pane/workspace shell landed, but every region it opened was pure client-side layout and the server heard nothing about any of it. A `#[gpui::test]` over the tree passes in both worlds, so the  wire tap (`SCRIBE_SHARE_TAP=1`) records the real socket and the record is truncated at the phase boundary, making every asserted frame attributable to the action that produced it. Phase 0 is the same daemon-stop-and-relaunch preamble  documents.

`ctrl+alt+backslash` must put a `CreateWorkspace` on the wire, draw a `WorkspaceInfo` out of the real server, and make the client log that a region adopted that exact id; the same split must then report a `ReportWorkspaceTree` whose tree is a two-leaf split naming it, and the session the split seeded must be reconciled with a `MoveSession` targeting it — the session is created through the *first* region's workspace, so a client that never sends this leaves every pane filed under the window's first workspace. `ctrl+shift+w` collapses the second region and must put `CloseWorkspace` for that id on the wire and re-report a one-leaf tree.

The inbound half is injected rather than provoked, because that is what makes it an assertion about the reader rather than about the split: a `WorkspaceInfo` carrying a display name must repaint the status bar's left group, and the same frame with `name: null` must repaint it back to the original pixels. The injected accent is the server's own, echoed out of the recorded frame, so the name is the only thing that moves between captures. The band is derived from each capture's own content bounding box rather than from `xdotool getwindowgeometry`, whose rectangle does not line up with the client area under openbox, and is cropped to the left quarter so the status bar's own 2 s sparkline resample cannot swamp the comparison.

### Config live reload

`tests/e2e/visual/config-reload.sh` is the scripted oracle for the `ConfigReloaded` parity row: it edits `config.toml` under an already-running client, the user-visible scenario the headless suites cannot reach.

The script screenshots the baseline window, rewrites the config with a new theme, font size, `line_padding`, opacity, and command-palette combo in one save, then asserts four things in order: the client logged a `config hot-reloaded` line it had not logged before (the watcher fired and  ran), the client pid is unchanged (a reload, not a restart), the captured frame is no longer pixel-identical (the new theme and font actually reached the paint path), and the newly bound `ctrl+shift+o` opens the command palette even though that combo did not exist when the window started.

Asserting on the log rather than on pixels alone is deliberate: the status bar's sparklines resample on a timer, so a screenshot diff on its own could pass without any reload having happened.

### X11 focus guard gates the live key path

`tests/e2e/visual/x11-focus-guard.sh` is the scripted oracle for the X11 focus guard parity row: it proves the guard is started by  on the real window and actually gates keystrokes, which no unit test over  can show.

The probe keystroke is Ctrl+Shift+U, the client-local tooltip-demo toggle, so the guard's verdict is a pure pixel change inside the window and nothing can reach a PTY. The script asserts the startup line naming the guarded window id, then walks three phases: with the client active the toggle changes the tooltip crop; with a second client window holding `_NET_ACTIVE_WINDOW` the same `xdotool key --window` keystroke leaves the crop pixel-identical and adds a `suppressed keystroke` line (proving the key was delivered and dropped, not merely lost); and after re-activation the toggle lands again and the crop returns to its pre-toggle state.

The crop excludes the status bar deliberately — its sparklines resample every two seconds, so a whole-window comparison could never assert pixel identity.

### IME composition over XIM

`tests/e2e/visual/ime-preedit.sh` (`just e2e-visual-ime`) is the scripted oracle for the IME / preedit parity row: it proves the composition handler is actually registered on the live window.

No unit test over  can show that — the module shipped complete and unreferenced, and the failure mode is silent: every key falls through to the byte encoder and the raw latin letters land in the shell.

`SCRIBE_IME=1` starts a real input method before the client launches (`ibus-daemon --panel disable --xim`, plus `XMODIFIERS=@im=ibus`), because GPUI's X11 backend finds an XIM server through that variable and reads it once, when it builds its connection. The engine is `table:cangjie3`, whose composition is a fixed table lookup rather than a phonetic guess, so `h`-`q`-`i` (竹手戈) yields 我 every run. `--daemonize` is load-bearing: without it ibus watches the shell that launched it and exits before any engine registers. The rig also runs on the  with a UTF-8 locale exported before the server starts, so `scribe-test` reads the very pane the client types into and bash's readline will accept multibyte input.

Four phases, each asserting a different half of the wiring. Composing raises marked text in the client log — a line only the platform can produce, and only through the registered handler — and repaints the window. The server-owned PTY shows no `hqi`, which is the regression itself. Committing puts a non-ASCII character on that same PTY while the latin keys still never appear. Finally, switching to a passthrough engine and typing one key must add exactly one character: registering a handler makes GPUI follow an un-stopped `KeyDown` with `replace_text_in_range(key_char)`, so without the root listener's `stop_propagation` every printable character is typed twice.

Both PTY comparisons squeeze out spaces, because 我 is double-width and the snapshot reads its trailing spacer cell back as one. Phases poll rather than sleep: a commit still has to cross the IPC socket, reach the PTY, be echoed, and return as a screen update, and selecting an ibus engine is asynchronous — keys pressed while the switch is in flight are swallowed outright.

### Window sharing and control handoff

`tests/e2e/visual/share-control.sh` is the app-level oracle for the feature-015 share rows: it drives the real client against the real server and asserts both the pixels and the wire.

Multi-machine sharing needs a second machine, so the run interposes  (`scribe-test share-tap`, enabled with `SCRIBE_SHARE_TAP=1`) between the client and `scribe-server`: the entrypoint renames the real socket to `server-upstream.sock` and the tap binds the original path, so the client's `Hello` handshake, its `SessionList`, and every byte of pane output still come from the real server over the real framed protocol. The tap records every frame in both directions to `/output/share-wire.jsonl` and injects the four notices a remote participant would have caused via  (`scribe-test share-inject`). The daemon connects before the tap is interposed, so the client under test is the tap's newest connection and therefore the injection target.

The script walks the surface in five phases, screenshotting each: a `ShareRoster` raises the roster panel and the presence badge; a `ControlRequested` opens the modal prompt, swallows an ordinary keystroke, and Esc puts `ControlGrant { accept: false }` on the wire; a roster handing control to the remote peer makes the client a viewer, whose keystroke is swallowed and raises the take-control hint from which Enter puts `ControlClaim` on the wire; `ControlDenied` posts its notice; and `ShareEnded` tears the surfaces down. The wire assertions read the recorded JSONL, so they prove the client emitted the frames rather than that a test constructed them.

Keystroke *suppression* is asserted from the client log rather than from an absent `KeyInput`: the GPUI client cannot create its own first session yet (`CreateWorkspace` is missing, FU-6), so its window holds no PTY in this rig and would emit no `KeyInput` either way. `run_share_key` logs every swallowed key for exactly that reason, and the absent-`KeyInput` check is kept alongside it as a regression guard.

### AI task labels rename the tab

`tests/e2e/visual/ai-task-label.sh` is the app-level oracle for the four provider task-label rows (`TaskLabelChanged`, `TaskLabelCleared`, `CodexTaskLabelChanged`, `CodexTaskLabelCleared`), which the client dropped until  routed them.

### AI indicator paints provider state

`tests/e2e/visual/ai-indicator.sh` posts a real provider state hook and asserts pixels in both the titlebar tab and pane-border strip. It proves the live hook-to-paint path documented in .

Nothing is stubbed. The image ships `scribe-hook-helper`, so the script posts a real hook event to the real `scribe-server` (the hook channel's endpoint *is* the server socket, exported as `SCRIBE_RUNTIME_DIR` by the entrypoint), the server translates and broadcasts the notice through , and the running client's tab strip repaints. `--provider=claude_code` drives the provider-tagged pair and `--provider=codex_code` the legacy Codex pair, because the server splits Codex back out for backward compatibility — so all four wire variants are exercised against one window.

Phase 0 borrows `overlay-actions.sh`'s trick for handing the client a pane: the entrypoint's `$SESSION` belongs to the test daemon's window and is therefore hidden from the client's `ListSessions`, so the daemon is stopped and the client relaunched, after which it adopts the session through the normal attach path. `scribe-hook-helper` needs no daemon — it addresses the socket directly with the session id in its environment — so the hook channel outlives that teardown.

Each provider runs a set/clear cycle asserted twice over: the client's own `tab task label updated` line must appear with the label text (proving the notice reached the reader, not just the socket), the left half of the window's top band must differ from the pre-label capture by at least 40 pixels (proving the strip repainted), and after the clear that same band must be pixel-identical to the baseline again (proving the shell title came back rather than the label merely being overwritten).

### Clipboard and OSC 52 bridge

`tests/e2e/visual/clipboard-osc52.sh` is the app-level oracle for : the two-hop OSC 52 bridge, the confirmation modal, and the copy / paste chords, none of which a headless test can show.

It runs through the recording wire tap ( with `SCRIBE_SHARE_TAP=1`, described under ) with `terminal.clipboard.{read,write}_mode = "prompt"` and a 1 ms burst window seeded through `SCRIBE_EXTRA_CONFIG`, so each phase raises its own modal instead of inheriting the previous decision. Phase 0 relaunches the client after stopping the test daemon, which both hands it the harness session and makes it the window's only participant — the server routes an OSC 52 prompt to the window's *controller*, so a client sharing the window with the daemon might never see one.

The OSC 52 phases drive the escape from inside the real pane (`printf '\033]52;c;<base64>\a'` typed through XTEST), because only a PTY-side emission reaches the server's policy engine. Each asserts three things: the client raised a modal, the answer left as a `ClipboardPromptResponse` frame on the wire, and the effect landed — the allowed write shows up in `xclip -o -selection clipboard`, and the allowed read comes back as a `ClipboardBridgeReadReply` whose `Ok` payload carries what was on the host clipboard.

The copy phase drags a real mouse selection across the pane and presses the `copy` chord, then requires the X11 clipboard to hold the selected needle — a chord that reached only the drop counter leaves the clipboard at its seeded value. The paste phase seeds the clipboard, presses the `paste` chord, and asserts the pasted bytes appear inside a `KeyInput` frame on the wire and that the shell's echo repaints the grid.

### Terminal viewport navigation

`tests/e2e/visual/terminal-viewport.sh` is the app-level oracle for : scrollback paging, font zoom, vi / copy mode, split-scroll, and the smart-selection context menu, each of which shipped as a unit-tested module with no caller.

It runs on the  with `terminal.scroll_pin = true` seeded through `SCRIBE_EXTRA_CONFIG`, because split-scroll is opt-in and the pin can only appear in a client whose config asked for it. Every phase asserts a log line the wired path alone writes *and* a pixel effect, because either alone is weak: a log line does not prove the frame changed, and a screenshot diff does not prove which code produced it.

`shift+PageUp` must produce a `terminal scrollback moved` line with a non-zero offset and repaint the whole viewport — the second half is what would have caught the original defect, where the snapshot read the live screen and ignored the display offset entirely, so scrolling logged fine and changed nothing on screen. `shift+End` must report offset 0 again. `ctrl+-` must step the zoom level to `-1` and rescale the grid; `ctrl+0` must return it to `0`.

The vi-mode phase asserts three things, because the mode is only correct if all three hold: `ctrl+shift+space` logs `active=true`, three `k` presses add the hollow cursor box to the frame, and the daemon's own screen snapshot contains no `kkk` — a copy mode that leaks its motions into the shell is worse than no copy mode. `Escape` must log `active=false`.

Split-scroll needs both halves of its gate, so the phase posts a real `state_changed` event through `scribe-hook-helper` to make the client believe the pane is a Claude Code session, then pages up and requires the reported `pin_rows` to be non-zero. Finally a right-click over a viewport filled with URLs must log `smart selection matched` naming the `URI` rule, which only the live context-menu path can write.

### The wheel scrolls and mouse reports reach the PTY

`tests/e2e/visual/mouse-reporting.sh` is the app-level oracle for : the wheel over the terminal grid, and the X10 / SGR-1006 reports a mouse-tracking application receives.

Both halves were invisible to every headless test. The crate contained no scroll-wheel handling at all, and `mouse_reporting.rs` was an unwired module with a green golden-byte suite — a suite that cannot tell a wired encoder from an unwired one, which is exactly how the pair survived to the launch gate. The run therefore uses the  plus the  wire tap (`SCRIBE_SHARE_TAP=1`), and asserts against three independent oracles: the client's own log lines, the recorded `KeyInput` bytes, and the pane's own screen.

The third oracle is what makes the run end-to-end. Each tracking phase starts a real `cat -v` in the pane behind the DEC modes under test and a non-canonical, non-echoing line discipline, so every byte the client forwards is printed straight back onto the pane as visible text (`^ plus the  wire tap (`SCRIBE_SHARE_TAP=1`), and every phase needs a screenshot diff *and* a recorded `Resize` before it passes.

`ctrl+-` must log `level=-1`, repaint the grid, and publish a smaller cell box with *more* columns; two `ctrl+=` presses must reach `level=1` with a bigger cell box and fewer columns than both the zoomed-out grid and the baseline. The column assertions are the point: a client that rescales glyphs inside a frozen `cols`x`rows` box also repaints and also emits a `Resize`, so pixels and frame counts alone cannot tell a real zoom from a cosmetic one. `ctrl+0` must return `level=0`, republish the pre-zoom geometry field for field, and leave a frame within a few hundred pixels of the pre-zoom capture — the seeded rows are short enough that no zoom level wraps them, so a restored grid is a restored image.

### A live window resize republishes every pane

`tests/e2e/visual/window-resize.sh` is the app-level oracle for a window-manager resize: the geometry has to reach the server and the PTY, and the pane has to keep showing the server's own screen throughout a stepped drag.

Nothing headless could have caught the defect it covers. The cell arithmetic was always right; what was missing was the *frame* that observes the new grid band, because the band's rect is written during prepaint and the one repaint a bounds change buys still reads the previous rect. So the client re-laid its panes on screen while every PTY kept its pre-resize size — visible only as an application wrapping at the old column count. The run therefore uses the  plus the  wire tap (`SCRIBE_SHARE_TAP=1`).

Each resize phase needs three independent things before it passes: a screenshot that differs from the previous shape by thousands of pixels, a recorded `Resize` whose `cols`x`rows` moved in the right direction (up when the window grows to 1700x1000, back down when it shrinks to 900x600), and `stty size` inside the pane reporting exactly those cell counts. The last one is the end-to-end oracle — the kernel's window size is set by the server, so a client that only wrote a frame to a socket cannot satisfy it.

A fourth phase watches an idle window for several seconds and requires *zero* further `Resize` frames. The republish is scheduled from the measuring write, so a gate that fired on every frame rather than on a moved rect would turn a redraw into a `Resize` storm; that regression looks identical to a correct fix in every other assertion here.

Every assertion above is about geometry, and geometry is not integrity: a pane can publish the right cell counts, drive the PTY to them and repaint tens of thousands of pixels while showing nothing at all. A whole-pane rebuild is state rather than a delta — it paints every row as exactly `cols` characters and ends on an absolute CUP — so replaying one into a grid of a different shape autowraps the entire screen into scrollback and leaves the viewport blank. Three further phases close that hole, and they are the only assertions in the file that would have failed on the pre-fix client.

They seed five marker commands whose "command not found" line is 125 columns wide, then compare the window against `scribe-test snapshot` row for row. The server's snapshot is the oracle for what should be on screen; the window is read back as per-row ink from an `import -window` crop of the grid band, so every row the server calls non-empty has to carry ink at the same row index and at most a slack of two rows may carry ink the server does not have. Phase 5 runs the comparison with **no** resize at all, which is what separates a rest-state corruption from a resize defect. Phase 6 drags the window narrow in eight `xdotool windowsize` steps — a drag's cadence, so each configure event lands while the previous round trip is still in flight — and requires both that the reflow onto two rows per marker reached the screen and that no row was lost. Phase 7 drags back out, replaying rebuilds *wider* than the grid the client is leaving, which wraps in the opposite direction.

The seed length is what makes the comparison sharp. At 178 columns each marker occupies one row and at 107 it occupies two, so a client still painting the shape a rebuild was rendered at keeps the old row profile and fails even though its published `cols`x`rows`, its `stty size` and its pixel diff are all perfect. No keyboard input is sent during any content phase, so nothing but the resize pipeline can be repairing the screen. The row arithmetic mirrors the client's own layout constants (`TITLEBAR_HEIGHT`, `STATUS_STRIP_HEIGHT`, `STATUS_BAR_HEIGHT`, and the 1.35 line-height ratio at font size 14), with each band inset by 3 px so the focused pane's 2 px accent border is never mistaken for text.

### Prompt marks and mark-relative jumps

`tests/e2e/visual/prompt-marks.sh` is the app-level oracle for : OSC 133 ingestion, the three mark-relative jumps, and the server's `ScrollBottom` snap, none of which the client could reach before.

It runs on the  so a real shell writes real OSC 133 bytes into the very pane the window renders — the server's OSC interceptor and its `PromptMark` emission are therefore on trial alongside the client. Three commands are recorded with the middle one exiting non-zero, and each block's filler rows carry a bar that grows at a per-block rate, so two viewports parked on different marks are visibly different frames rather than near-identical walls of text.

The expected landing rows are read back out of the client's own `prompt mark recorded` lines rather than hard-coded, which keeps the assertions independent of how many rows the window happens to have. Pressing `ctrl+shift+b` before any mark exists must log `prompt jump found no mark` and leave the frame alone (FR-011) — a different observation from the chord being swallowed, which produces no line at all. `ctrl+shift+z` twice must walk to two successively older marks, `ctrl+shift+x` must land back on the first of them, and `ctrl+shift+b` must land on the *middle* command's row, which separates the wired behaviour from both the newest mark (what a plain jump-up reaches) and the oldest.

The final phase scrolls away from the bottom, then arms the pane as an AI session and emits a real ED 3 so the server suppresses it and sends `ScrollBottom`. The snap is asserted twice: the client logs it with `moved=true`, and a following `scroll_bottom` chord must report `moved=false offset=0`, which is the only way to show the viewport genuinely ended at the live tail.

### The command-mark scrollbar paints its thumb and ticks

`tests/e2e/visual/scrollbar.sh` is the app-level oracle for : the overlay thumb, its success/failure command ticks, the shift a scrollback trim causes, and the idle fade.

None of that reached a pixel before, because `scrollbar.rs` shipped with a green unit suite and no caller at all.

Every assertion is made in a 24 px strip cropped from the pane's right edge, which is the only region the overlay writes into. It runs on the  so a real shell writes the OSC 133 bytes into the very pane being measured, and the filler rows are kept short so no glyph ever reaches the strip.

Hover is what makes the run deterministic instead of a race: parking the pointer in the hit zone pins the overlay fully opaque and clears the idle timer, so the strip can be captured and compared without the 1.5 s delay expiring between screenshots. The control is a rested capture taken twice with the pointer parked away from the edge; it must be byte-identical to itself, or a later "the thumb painted" diff would prove nothing.

The ticks are asserted by hue rather than by position: a pixel counts as a success tick when its green channel leads the other two, and as a failure tick when its red does. A strip holding both is what separates a wired paint path from one that renders neutral marks, because it also requires the OSC 133 `A` → `D` exit codes to have been resolved.

The trim phase drives the server's real AI path — arm the pane with `ScribeAiLaunch`, emit an ED 3 to set the preserved-scrollback baseline, grow the history with plain output, then emit a second ED 3. The filler between the two carries no OSC 133 deliberately: a `PromptStart` tells the server the AI tool exited, which clears the provider and the baseline with it, and the second ED 3 would never be filtered. The trim is then asserted twice over — the client logs the rows its own grid dropped and how many marks survived, and the topmost success tick has to sit on a different row than before.

### Subscribe and snapshot session tooling

`tests/e2e/visual/session-tooling.sh` is the app-level oracle for the `Subscribe` and `RequestSnapshot` parity rows: it drives the real client against the real server and asserts both frames on the wire at the lifecycle points that produce them.

Neither message could be proven by a headless test, because the gap was reachability — the frames existed in the frozen protocol and nowhere in the client. The run therefore reuses the  wire tap (`SCRIBE_SHARE_TAP=1`) purely as a recorder, truncating `/output/share-wire.jsonl` at each phase boundary so a frame found afterwards can only have come from the action that phase performed. Phase 0 is the same daemon-stop-and-relaunch preamble as `overlay-actions.sh`, which is what puts the client on the `ListSessions` →  path.

The `Subscribe` half asserts a client frame naming the attached session, that its record line follows the `AttachSessions` that authorises it, and that `scribe-server` logged no `Subscribe denied for unattached session`. The `RequestSnapshot` half types a marker into the pane, edits `appearance.font_size` under the running window so  fires, then asserts the request follows its `Resize`, that the server answered with a `ScreenSnapshot` whose per-cell grid contains the marker, and that the client logged `repainted pane from server screen snapshot` with the same `cols`/`rows` the recorded frame carried — tying the repaint to that exact reply rather than to some other snapshot.
### Terminal bell attention routing

`tests/e2e/visual/bell.sh` is the app-level oracle for the `Bell` parity row: it drives a real BEL byte out of a real shell and asserts what the running client does with it (see ).

The module's own `#[gpui::test]`s were green the whole time the client did nothing with a bell, because the gap was reachability — `bell.rs` was outside `main.rs`'s import closure and every `ServerMessage::Bell` fell into the reader's catch-all. Nothing here is forged: `SCRIBE_SHARED_PANE=1` puts the client in the daemon's window, so `scribe-test send` runs `printf "\a"` in the very pane on screen, the server's `Term` turns the BEL into a `MetadataEvent::Bell`, and the server broadcasts it.

The routed behaviour is asserted as a window property rather than as pixels. `Window::request_attention` sets the `WM_HINTS` urgency flag on X11, and `xprop` prints its urgency line only when the flag is set, so the presence of that line is the assertion and its absence is the suppressed case — which is why the image installs `x11-utils`. Three phases separate ingestion from routing: a bell to the focused foreground pane must log `terminal bell received` and yet leave no urgency flag and no `terminal bell requested window attention`; the same bell with the window iconified must produce both; and refocusing must make the pane silent again, which is what proves the middle phase was the gate opening rather than the routing merely having warmed up.

### LAN approval and mutual-TLS dial

`tests/e2e/visual/lan-approval.sh` is the app-level oracle for the eleven feature-014 LAN rows: it drives the real client against the real server and asserts both the pixels and two separate wires.

 passed its headless suite for months while `lan_approval.rs` was outside `main.rs`'s import closure, which is exactly the failure mode this script exists to catch — so every assertion is either a frame recorded leaving the real client or a pixel change in the real window.

Two rigs stand in for what one machine cannot supply, and neither fakes the client or the protocol. The  wire tap (`SCRIBE_SHARE_TAP=1`) relays the Unix socket, recording every frame in both directions; `LanApprovalRequest` is injected through it because the owning server only pushes one for a real unknown device.  (`scribe-test lan-peer`) stands in for the second machine's LAN listener: it borrows this machine's own device identity over `GetLanDialIdentity` and terminates a REAL mutual-TLS handshake with the same `LanTls` builder the shipped listener uses, so the `LanHello` it records is the one the client actually put on the encrypted wire. `SCRIBE_KEYRING=1` starts a session D-Bus and an unlocked gnome-keyring in the entrypoint, because the LAN device key is keyring-sealed and `scribe-server` fails closed without one — every other visual test keeps the lighter, keyring-free container.

Four phases run after the same daemon-stop-and-relaunch preamble  documents. The startup probe's `GetLanEnv` and `ListLanPeers` are asserted on the wire together with the real server's `LanEnv` and `LanPeerList` answers and the client's own log lines proving it acted on them — no injection is involved, and the peer list is legitimately empty in a container with no mDNS. An injected `LanApprovalRequest` must then change the window's body pixels, and a bare Enter on the default focus must put `LanApprovalDecision { approve: false }` on the wire; a second request with `name_collision` set, Tab, and Enter must put `approve: true` on it. Finally a client launched with `SCRIBE_LAN_DIAL` must fetch its dial identity from the real server, land a `LanHello` on the stand-in peer's encrypted wire, show "Waiting for approval on the peer…" while the peer holds the gate, and send `Hello` over the LAN link once the gate approves.

Phase 1's baselines are sampled before the relaunch, because the LAN probe runs as part of connecting; sampling afterwards would race the frames the phase waits for. The dial phase skips loudly rather than passing silently when the stand-in cannot borrow an identity, so a container without a keyring reports a gap instead of a green run.

### Tailnet remote control

`tests/e2e/visual/remote-control.sh` is the app-level oracle for the eleven feature-013 tailnet rows: it drives the real client against the real server and asserts both the pixels and two separate wires.

 and  passed their headless suites for months while both modules sat outside `main.rs`'s import closure, which is exactly the failure mode this script exists to catch — so every assertion is either a frame recorded leaving the real client or a pixel change in the real window.

Two rigs stand in for what one machine cannot supply. The  wire tap (`SCRIBE_SHARE_TAP=1`) relays the Unix socket, recording every frame in both directions; `WindowTakenOver`, `RemoteDisconnect` and the viewer `ShareRoster` are injected through it because the owning server only produces them for a real second machine.  (`scribe-test remote-peer`) stands in for the second machine's tailnet listener — a much smaller rig than its LAN twin, because the tailnet transport is plain TCP and identity is `tailscaled`'s `WhoIs` on the owning side, so there is nothing to borrow and nothing to pin. It refuses any first frame but `RemoteHandshake`, answers the mandatory reply, and splices an accepted connection directly to the tap's upstream server socket so the GPUI client remains the tap's injection target.

Six phases run after the same daemon-stop-and-relaunch preamble  documents. The startup probe's `GetRemoteEnv` and `ListRemotePeers` are asserted on the wire together with the real server's `RemoteEnv` and `RemotePeerList` answers — no injection is involved, and a container with no tailnet legitimately produces the fail-closed `tailscale_detected = false` and an empty peer list, which is precisely why the phase asserts the client's own log lines rather than the payloads. The next phase opens the command-palette picker in the real window, injects one discovered peer, screenshots its visible row, proves Enter sends `RemoteHandshake`, `ListWindows`, and receives `WindowList` on the stand-in's TCP wire, then dismisses both picker stages before later palette input. An injected `WindowTakenOver` must then change the window's body pixels, a plain letter must add no `KeyInput` to the wire while the window is frozen, and Enter must put `ControlClaim` on it and repaint. An injected `RemoteDisconnect` must surface its typed reason. A viewer roster then makes a palette "New Tab" row leave as `DispatchAction` and come back as `ActionDispatched`, and an injected `RunAction` must open a real tab. Finally a client launched with `SCRIBE_REMOTE_DIAL` must land a real `RemoteHandshake` on the stand-in peer's TCP wire, accept its reply, and send `Hello` over the tailnet link.

Phase 1's baselines are sampled before the relaunch, for the same reason the LAN script's are: the remote probe runs as part of connecting. The suppressed-keystroke assertion is a *count* on the recorded wire rather than a screenshot, because "nothing happened" is exactly what a screenshot cannot distinguish from an idle pane.

### Settings trust and preflight controls

`tests/e2e/visual/settings-trust.sh` is the app-level oracle for the redesigned settings window's grouped navigation, search, trust rows, and env preflight: it drives the real window against the real server and asserts each frame on the wire.

A green unit test over  cannot show anything here — every one of those helpers passed its parser tests while having no caller at all. The run therefore starts the container with `SCRIBE_VISUAL_APP=settings`, which launches `scribe-client --settings` instead of the terminal window, plus `SCRIBE_SHARE_TAP=1`. The tap is mandatory rather than optional: the settings window never registers a client connection, so its one-shot transient sockets are observable only in the tap's `/output/share-wire.jsonl` record.

`SCRIBE_SEED_TRUST=1` plants one trusted network and one approved device into the server's `lan_trusted_networks.toml` / `lan_trusted_devices.toml` before it starts, because a single container has no second machine to approve and no fingerprintable Wi-Fi to trust — without the seed the Remove and Revoke rows would never exist to reach. The documents use the real on-disk shape, whose `version` and `owner` fields the server validates on load.

The script walks eight phases, screenshotting each: the sidebar focus seed is proved inert (nothing on the wire); keyboard-traversing the whole grouped nav to Remote puts `GetLanEnv`, `ListTrustedNetworks`, and `ListTrustedDevices` on the wire and renders both list replies; a Ctrl+K search for `remote` narrows the nav to that one page and its Refresh control re-issues the queries; the seeded device's Revoke sends `RevokeTrustedDevice` carrying that device id; the seeded network's Remove sends `RemoveTrustedNetwork` carrying that record id and both lists re-render as empty; "Trust it" sends `AddCurrentNetworkTrusted` (last of the remote-page phases, because a fingerprintable network would add a row and no later phase may depend on the list length); a search for `keystore` reaches the environment page's action, which sends `EnvPreflight` and gets an `EnvPreflightResult` back; and a search for `persist environment` flips that toggle ON, which runs the same gate before committing.

Every control is reached through a semantic target rather than a coordinate: the search field via its own Ctrl+K shortcut, and each control via the traversal order of [[crates/scribe-client/src/settings/window.rs#SettingsWindow#focus_targets]] — visible nav pages, then the Remote page's live trust actions, then the selected page's actionable controls. A phase therefore says "the second focus target while the search reads `remote`", so grouped nav, the 1500x1050 geometry, the custom client chrome, and the search bar can all be re-laid-out without touching the script, and no production layout constant exists to keep a click landing on a row. The device is revoked before the network is removed because device rows follow network rows in that order, and each phase re-seeds its origin first.

The single pointer gesture is that seed: a click on the empty sidebar background below the last nav item. It exists because nothing holds the GPUI focus handle when the window opens, so the first keystroke would otherwise dispatch to the window root and be dropped; the click hands the root its handle and resets `focus_index` to 0 through `clear_keyboard_navigation`, which is why every phase can count Down presses from a known origin. Phase 0 asserts it sends no frame, so a future sidebar that puts a control there fails loudly instead of silently mutating config.
### Window lifecycle over the wire

`tests/e2e/visual/window-lifecycle.sh` is the app-level oracle for the seven window-lifecycle parity rows: it drives the real client against the real server and reads every frame off the recorded wire (see ).

Nothing is stubbed and nothing is injected. The wire tap (, `SCRIBE_SHARE_TAP=1`) is interposed purely as a recorder, so every server frame the script asserts on is one the real `scribe-server` chose to send in answer to something the real client sent. `tests/e2e/visual/window-lifecycle-config.toml` is seeded through `SCRIBE_EXTRA_CONFIG` to turn `remote.enabled` on, because the window-list poll is gated on it exactly as the winit client gates it; the entrypoint writes that file after the server has already started, so only the client's poll is affected and no remote listener is bound.

The five phases each assert a different half of the conversation. A phase-0 preamble hands the client a live pane through the same daemon-stop-and-relaunch trick  documents, asserted on the client's own `AttachSessions` frame. Phase 1 waits for a `ListWindows` and its `WindowList` answer to both appear and for the client to log the reply's shape, so a dropped reply cannot pass. Phase 2 iconifies and re-activates the window and asserts the exact `FocusChanged { gained: null, lost: <session> }` and its mirror image, then creates a second tab and asserts a report that names a gain *and* a loss. Phase 3 sends WM_DELETE_WINDOW through openbox's Alt+F4 (`xdotool windowclose` is deliberately not used — it calls `XDestroyWindow` and bypasses the protocol), asserts the client vetoed the close and painted its dialog instead of dying, and then that "Quit Scribe" put `QuitAll` on the wire, that the server broadcast `QuitRequested`, and that the process exited on it. Phase 4 relaunches, reads the window id out of the fresh `Welcome`, and asserts "Kill Window" sent `CloseWindow` naming that id, that the server answered `WindowClosed`, and that the client exited.

Exiting is asserted as process death rather than as a screenshot, because the whole point of the acknowledgement is that the app goes away; a pixel check could not tell a torn-down window from a hung one.

### Cold-restart restore drives the real client

`tests/e2e/visual/cold-restart.sh` (`just e2e-visual-cold-restart`) is the app-level oracle for spec 016's cold-restart restore and window geometry persistence rows. Every assertion is produced by the real `scribe-client` process (see ).

It exists because neither requirement can be shown headlessly and neither can be shown by the daemon stand-in  uses. The test therefore reproduces a crash literally: the client is `SIGKILL`ed, because an orderly quit deliberately *deletes* the snapshot, and the disposable test server is then genuinely restarted, so both PTYs die with it and the relaunched client meets the empty `SessionList` that is the only condition under which a snapshot may be replayed. Server readiness is gated on the `scribe-server` process rather than on the socket file, because a stopped server leaves its socket behind and the replacement would otherwise be declared up while it was still losing the lock race.

The wire tap is deliberately not interposed — it renames the socket out from under `scribe-test server stop/start`, which this test has to perform. Assertions instead read both process logs: the client's for what it claimed, replayed and requested, and the server's for the PTYs it actually spawned in answer. Because the client logs with ANSI styling on, every numeric field is read through an escape-stripping filter; a raw `grep` would silently compare against a colour-coded value and pass on garbage.

Phase 0 hands the client a live pane through the same daemon-stop-and-relaunch trick  documents, then splits it so the snapshot has a real pane tree, waiting for the split pane to actually adopt a session (an un-adopted pane is pruned from the snapshot). Phase 1 asserts exactly one window snapshot on disk carrying two `` plus a geometry record. Phase 2 resizes the window with `xdotool` and asserts the geometry record followed it, within a few pixels of WM frame slop. Phase 3 crashes the client and cold-restarts the server. Phase 4 asserts the relaunched client claimed the snapshot, replayed two panes, sent two restored `CreateSession` frames, and that the *server* answered with two brand-new PTYs. Phase 5 asserts the window reopened at the persisted geometry, that every restored pane adopted a session, and that both panes asked for the same, less-than-full width — which is the pane tree surviving rather than one full-width pane coming back.

### Update surfaces

`tests/e2e/visual/update-trigger.sh` and `tests/e2e/visual/update-dismiss.sh` are the scripted oracle for the `UpdateAvailable` / `UpdateProgress` / `TriggerUpdate` / `DismissUpdate` parity rows, driving a real server end to end (see ).

Nothing is stubbed on the client side. `tests/e2e/visual/fake-update-api.py` stands in for GitHub's releases API and the container is started with `SCRIBE_UPDATE_API_URL` pointing at it, so `scribe-server` decides on its own that a newer version exists and broadcasts `UpdateAvailable` on its normal 30 s startup check. `tests/e2e/visual/update-config.toml` is seeded through `SCRIBE_EXTRA_CONFIG` to turn the status bar's sparklines off, because they resample every 2 s and would swamp the one band the tests diff.

Both scripts share `tests/e2e/visual/update-common.sh`, which grows the window first — both bottom bands are on screen at the default size now that it is derived (see ), but a wider window spreads the status bar's left and right groups apart and leaves the centred CTA clear space of its own — then diffs the centred status-bar band before and after the broadcast. A non-zero delta proves the CTA rendered; the bounding box of the changed pixels is where the script actually moves the pointer and clicks, so the click cannot silently miss.

`update-trigger.sh` then presses Enter on the default "Update Now" and waits for the server's `client triggered update window_id=…` line — the server only logs that on receiving `TriggerUpdate` from that window — before capturing the CTA relabelled "Downloading..." and then "Update failed" as the server's real download and (deliberately invalid) signature check drive `UpdateProgress`. `update-dismiss.sh` Tabs onto "Later" instead, waits for `client dismissed update notification window_id=…`, and asserts the CTA band is once again pixel-identical to the no-update baseline.

### Desktop notifications fire, coalesce, and focus on click

`tests/e2e/visual/notifications.sh` (`just e2e-visual-notifications`) is the app-level oracle for . The Bell parity row does not cover it: a bell reaches `Window::request_attention`, an entirely different mechanism.

Nothing is stubbed on either side. `scribe-hook-helper` posts real provider hook events to the real server, which broadcasts `AiStateChanged` to the window; delivery lands on `tests/e2e/visual/notify-daemon.py`, an actual `org.freedesktop.Notifications` service claiming the well-known name on a session bus started by the entrypoint under `SCRIBE_NOTIFY=1`. A recorded `Notify` call is therefore proof the client's zbus dispatcher ran, and the service records every call to a JSONL file the script asserts against.

Phase 1 minimizes the window (the lever on the default `when_unfocused` condition) and asserts a `Notify` arrives with `replaces_id = 0` and a summary naming the state. Phase 2 runs a second attention cycle on the same session and asserts the call carried `replaces_id` equal to the first id *and* that the service answered with that same id — the `replaces_id` contract, which a client that stacked toasts would fail. Phase 3 writes to the service's control FIFO to emit a real `ActionInvoked`, and asserts the client both reports the click and raises its window, observable from outside the process through `_NET_ACTIVE_WINDOW`. Phase 4 refocuses and asserts a further transition on the focused foreground pane fires nothing, which is what proves phase 1 was the gate opening rather than the path merely warming up.

### Dropped file paths insert into the pane

`tests/e2e/visual/drag-drop.sh` (`just e2e-visual-drag-drop`) is the app-level oracle for , driving a real XDND drag source against the running window.

`xdotool` cannot do this: a file drop is not pointer input but the XDND protocol — a ClientMessage handshake plus an X selection transfer. `tests/e2e/visual/xdnd-drop.py` is a genuine drag source on the same X server: it owns `XdndSelection`, walks the client's X11 backend through `XdndEnter` / `XdndPosition` / `XdndDrop`, and answers the client's own `text/uri-list` selection conversion.

The dropped path deliberately holds a space and a single quote, because surviving as ONE argument is the whole point of the ported quoting. The pane is parked at `cat` under the shared-pane rig, so the PTY echo is a byte-level oracle: phase 1 asserts `scribe-test wait-output` sees exactly the POSIX-quoted form, and phase 2 types a marker straight after the drop and asserts it arrives separated, which is the trailing space.

### Server autostart and stale-socket diagnosis

`tests/e2e/visual/server-lifecycle.sh` (`just e2e-visual-server-lifecycle`) is the app-level oracle for . The two halves are asserted separately because they fail separately.

Phase 1 plants a bound-but-unlistened socket — exactly what a crashed server leaves, so `connect` gets `ECONNREFUSED` rather than `ENOENT` — with no `systemctl` on `PATH`, so the autostart cannot succeed and the diagnosis is what the client is left holding. It asserts the client names the stale socket, tries the autostart anyway, and carries the diagnosis into the status-line failure rather than reporting a bare exit code.

Phase 2 replants the same stale socket and puts a `systemctl` shim on `PATH` that starts the real `scribe-server`. The shim stands in for the service manager only — systemd does not exist in a container — while the refused connect, the decision to start, the retry loop, the handshake and the mapped window are all the shipped code path. It asserts the client reaches `connected to scribe-server`, completes a `Welcome` handshake with the server it just started, and maps a painting window.

### Window chrome bands stay on screen

`tests/e2e/visual/window-chrome-bands.sh` (`just e2e-visual-chrome-bands`) is the app-level oracle for : it measures, on the running client, that the derived window size really does fit the whole terminal grid *and* every chrome band.

The measurement is geometric rather than golden-image. Phase 1 reads the client window's own size with `xdotool getwindowgeometry` and asserts it is the derived 1008x765 — the same arithmetic the crate does, restated in the script so a drift fails here instead of silently clipping pixels — then confirms the whole window is on the Xvfb screen by trimming a full-screen capture. Phases 2-4 crop window-relative bands out of `import -window` captures, so no WM decoration can shift an offset.

Phase 2 fills the pane with `seq 1 40` through the shared-pane rig (`SCRIBE_SHARED_PANE=1`, so `scribe-test send` writes to the very pane on screen) and asserts the *last* grid row carries ink: at the old 960x680 the bottom five rows fell outside the 596 px viewport, and this is the assertion that catches it. Phase 3 asserts the pane status strip and the window status bar each carry ink in their own band at the window bottom. Phase 4 posts a real `prompt_received` event down the AI hook channel and asserts the band above the strip *repaints* — ink alone would prove nothing there, since it held grid rows a moment earlier — while both status bands below it keep their ink, which is what the bands' `flex_none` layout guarantees.

## GPUI IPC Bridge

Unit tests for the GPUI client's  — the inbound coalescing drain and the outbound  — proving keystroke-before-output ordering and Zed-style 4 ms / 100-event coalescing over the frozen IPC protocol.

### Coalesce collapses per pane

 folds an interleaved two-pane run into one buffer per pane, preserving first-seen pane order and byte order within each pane; an empty run yields an empty batch.

### Prompt marks split a pane's output run

A prompt mark or a suppressed-ED-3 snap closes the pane's open output entry, so  emits the bytes before it, then the event, then the bytes after it.

That ordering is what lets the drain anchor a mark against a grid holding exactly the output that preceded it.

### Rebuilds bound the output runs around them

A whole-pane rebuild between two output runs stays its own op: [[crates/scribe-client/src/ipc_bridge.rs#coalesce]] closes the run ahead of it, emits the rebuild, and starts a fresh run behind it that still coalesces normally.

The op survives a round trip back through the queue's re-coalescing rehydration, so the boundary holds on the path an overflowing inbound queue takes as well as on the ordinary one.

### Drain coalesces firehose

 batches a 300-event two-pane firehose into at most one `write_output` per pane per 100-event batch, so the total write count stays bounded while every pane's byte stream is reconstructed in exact order.

### Batch byte cap splits a drain

A 2.5 MiB backlog of 64 KiB frames — well under both the 100-event bound and the queue's own ceilings, so bytes are the only bound left — drains as at least three batches, none over [[crates/scribe-client/src/ipc_bridge.rs#MAX_BATCH_BYTES]], with every byte still delivered.

Before the cap the same backlog was one 2.5 MiB batch, so the split is the assertion that proves the bound is doing the work rather than the event bound.

### Oversize event drains alone

An event twice the size of [[crates/scribe-client/src/ipc_bridge.rs#MAX_BATCH_BYTES]], queued between two small ones, is drained as its own batch: the small event before it is flushed first and the one after it starts the next batch.

This is the half of the policy that says the cap bounds rather than rejects — an event too big for any batch still reaches the pane whole, because splitting a pane's bytes would tear its VTE stream.

### Keystroke before output

A keystroke enqueued on the  reaches the outbound channel promptly even with a 10 000-event backlog churning through the inbound drain, because the outbound path never traverses the drain.

### Typing under firehose

Typing a full command while flooding the inbound drain between keystrokes preserves keystroke order on the wire with no per-key latency spike, the scripted no-reorder / no-stall check the launch gate requires.

### Create answers are claimed once each

Two `CreateSession` frames leave two claims outstanding, a clone of the sink claims both, and a third claim reports none left.

The clone matters because the reader holds one handle and the GPUI view another; the count is how a `SessionCreated` acknowledging an `AttachSessions` is told from the answer to a create.

The count is what decides whether the reader adopts a session the server already attached or attaches it itself, so a claim that leaked or double-counted would either replay a fresh session or leave a reattached one unattached.

### Refused create leaves nothing to claim

A `create_session` refused by a closed writer leaves no claim behind, so the next `SessionCreated` — which cannot be that request's answer, because the request never went out — still takes the attach path.

### Resize before key input

A `Resize` enqueued on the sink before a `KeyInput` is delivered first, since the IPC-writer channel is a single ordered FIFO.

### Sink reports closed writer

`IpcSink::key_input` returns [[crates/scribe-client/src/ipc_bridge.rs#SinkError]]`::Closed` rather than panicking when the writer task has dropped its receiver.

### Inbound queue bounds a firehose

A pane firehosed thirty-two times past the byte ceiling with no drain running leaves at most [[crates/scribe-client/src/ipc_bridge.rs#INBOUND_QUEUE_EVENTS]] events and [[crates/scribe-client/src/ipc_bridge.rs#INBOUND_QUEUE_BYTES]] of payload buffered.

The same assertion covers the other half of the policy: the pane whose events were evicted is recorded as owing a resync, because a drop the drain never hears about is a pane left silently wrong.

### Overflow resyncs the dropped pane

After an overflow, the drain sends exactly one `RequestSnapshot` for the dropped pane once it has caught up — the client-detected resync ([[crates/scribe-client/src/ipc_bridge.rs#PendingResync#settle]]) that keeps a bounded queue from losing screen state.

### Outbound queue refuses at its cap

Filling the sink to [[crates/scribe-client/src/ipc_bridge.rs#OUTBOUND_QUEUE_FRAMES]] and then pushing further keystrokes refuses every one of them with `SinkError::Refused` and raises a tear request, instead of evicting anything already queued.

Draining afterwards must yield the original run byte for byte, in order, with nothing extra: that is the assertion that distinguishes this policy from the inbound one, since a single evicted `KeyInput` would hand the server a truncated command line that then runs.

### Torn connection requeues its in-flight frame

[[crates/scribe-client/src/ipc_bridge.rs#OutboundTear#wait]] stays parked while the queue is healthy and resolves as soon as a send is refused, which is what lets a refusal interrupt the wedged write rather than wait it out.

The frame the writer already took is put back through [[crates/scribe-client/src/ipc_bridge.rs#OutboundReceiver#requeue]] and comes back out first, so tearing the connection at the cap costs no input at all.

### Wedged socket tears at the cap

[[crates/scribe-client/src/ipc_bridge.rs#write_or_tear]] driven against a writer whose `poll_write` is permanently `Pending` returns `Torn` once the queue fills behind it, proving the refusal cancels the wedged write instead of waiting it out.

The backlog is intact afterwards and the cancelled frame requeues ahead of it, which is the end-to-end statement of the policy: a client behind a server that stopped reading redials on a bounded queue and loses no keystroke.

## GPUI Sync Frame Queue

Unit tests for the ported  —  sitting in front of  — proving `CSI ? 2026` commit boundaries survive IPC chunking and that expiry and catch-up match the winit client.

### Splits committed burst across IPC boundaries

A synchronized-update frame chunked across four IPC messages (BSU split mid-escape, body in two parts, ESU last) is reassembled by  into exactly one committed burst, so a single  hands the terminal the whole frame with its original markers intact.

### Preserves per-commit boundaries

A tail frame followed by two distinct sync commits drains as three separate frames, so each `CSI ? 2026` commit reaches `feed_output` as its own burst rather than being concatenated.

### Restarts a broken marker match at the next escape

A near-miss marker, two adjacent bare escapes and a short CSI all pass through byte for byte, and the real marker behind them still commits as its own frame.

The scan restarts at the next escape rather than backing up a byte at a time, so these are the inputs that would expose a mis-placed restart.

### Releases a marker prefix that never completes

A message ending on a partial marker withholds those bytes; the next message proves they were ordinary output, so they are released ahead of the real marker behind them rather than being dropped or reordered.

### Passes a run of bare escapes through intact

Sixty-four consecutive escapes — the input that restarts the match on every byte — reach the terminal unchanged with only the trailing escape withheld, so the worst case for the scan neither swallows bytes nor grows the held prefix past a marker.

### Presents one burst per redraw when caught up

With a backlog below ,  applies one committed burst then stops with  `HasMore`, so light traffic animates incrementally one frame per redraw.

### Drains through backlog past threshold

Once the queue depth exceeds the catch-up threshold, a single `drain_until_frame` replays every backlogged burst to the latest frame and reports `Drained`, so stale frames never pile up under a firehose.

### Rebuilds the snapshot once per presented burst

A backlog past the catch-up threshold reaches the target as six advanced frames and exactly one snapshot rebuild, so the frames the pacer drains through cost a parse and nothing else, while a caught-up pane still rebuilds once per presented burst.

An empty queue does not reach the target at all, so a pacer tick on an idle pane publishes nothing.

### Applies a rebuild as its own burst

A rebuild handed to [[crates/scribe-client/src/sync_frames.rs#present_rebuild]] behind a queued commit and a still-open `CSI ? 2026 h` reaches the target as a frame of its own, concatenated onto neither.

The queued commit lands first and the half-open update is sealed and committed rather than dropped, which is the ordering a `SessionReplay` depends on: folded into a neighbouring commit it would be swallowed by a synchronized update the pane it replaces had opened. The whole boundary publishes one snapshot: the frames it clears out of the way are replaced by the rebuild before anything paints.

### Flushes raw sync update on expiry

An unterminated `CSI ? 2026 h` arms a 150 ms raw deadline via ;  commits nothing before the deadline and, at it, appends the BSU-stripped bytes as a frame so the buffered output still reaches the terminal.

### Flushes parser sync update on expiry

A committed frame that opens but never closes a synchronized update arms the VTE parser's own timeout;  commits the held bytes at the deadline and clears the parser timeout.

### Split sync frame reaches terminal whole

Driving a four-way-split synchronized frame through the queue into a real  renders the committed content, proving the queue never advances the VTE processor with a torn frame.

### Advancing a frame defers the snapshot

[[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#advance_output]] moves the grid while every reader keeps the snapshot it already holds, so a frame the pacer skips builds no viewport of its own.

[[crates/scribe-client/src/terminal.rs#DisplayOnlyTerminal#publish_content]] then catches the snapshot up to everything advanced since the last one, and a second publish rebuilds nothing — the deferral is a coalesced rebuild, never a dropped one.

## GPUI URL Detection

Unit tests for the GPUI client's ported  scanner —  over Zed's Alacritty fork — proving byte-for-byte parity with the winit detector across hard-break joins and OSC 8 handling.

### Explicit hyperlink segment geometry

 collapses a multi-row OSC 8 run into exact per-row s, and `Osc8CellRange::contains` hit-tests a partial middle row by its own segment bounds rather than a bounding rectangle, so hover coverage stays exact.

## GPUI Terminal Selection

Unit tests for the ported  state —  and its vi-mode wrapper — proving cell/word/line granularity, `WRAPLINE`-aware extraction, and copy-on-select over Zed's Alacritty fork.

### Cell selection extracts a substring

 over a single-row cell range returns exactly the covered characters.

### Reversed cell selection normalizes

A range whose start is after its end extracts the same text as the forward range, because  orders the endpoints first.

### Word bounds snap to word characters

 expands a cursor inside a token to the full word, treating `_` and other identifier punctuation as word characters.

### Word bounds on a delimiter select one cell

A cursor resting on a whitespace delimiter yields a single-cell word range rather than swallowing an adjacent word.

### Line bounds span the full row

 returns the first through last column of the logical line for a non-wrapped row.

### WRAPLINE joins a wrapped row without a newline

 joins a row that ends with the `WRAPLINE` flag to its continuation row without inserting a newline.

### Hard line break inserts a newline

A selection spanning two rows separated by a hard line break (no `WRAPLINE`) is extracted with a `\n` between them.

### Word bounds follow a wrapped line

 crosses a `WRAPLINE` boundary so a word split across two screen rows selects as one token.

### Line bounds span a wrapped logical line

 follows `WRAPLINE` flags to cover every screen row of a wrapped logical line.

### Contains-cell honors selection shape

 includes only the partial first/last rows and every full middle row of a multi-row selection.

### Selection state copies on select

 returns the selected text after a cell/word/line gesture and `None` for an empty selection.

### Word drag extends by whole words

 in word mode extends the range by whole words from the double-click anchor to the drag point.

### Pixel mapping resolves grid cells

 maps a pointer pixel inside the content area to the correct grid cell and rejects pixels above the content area.

### Vi mode toggles and moves the cursor

 enters copy mode,  moves the vi cursor, and motions are no-ops while vi mode is inactive.

### Selection projects onto visible rows

 turns a multi-row selection into one inclusive span per painted row: the first row starts at the anchor column, middle rows span the full width, and the last row stops at the drag column.

### Scrollback selection follows the offset

A selection anchored in the scrollback paints nothing at the live bottom and lands on the matching screen row once the viewport scrolls onto it.

That is what keeps the highlight attached to content rather than to a screen position.

### Empty selection paints nothing

An empty range (a plain click), a zero-row viewport, and a zero-column viewport all yield no spans, so the paint path never has to guard against a degenerate highlight.

## GPUI Animation Policy

Unit tests for  —  — proving the config/override motion policy resolves correctly, transitions clamp to the 150 ms budget, and the disabled path yields a zero duration for byte-identical screenshots.

### Config default enables motion

With `appearance.animations` true and no environment override,  leaves motion enabled.

### Config false disables motion

Setting `appearance.animations` to false disables motion even without the environment override, so the config key alone acts as the reduce-motion user setting.

### Truthy env override forces motion off

A truthy `SCRIBE_DISABLE_ANIMATIONS` value (`1`, `true`, `yes`, `on`, case- and whitespace-insensitive) force-disables motion even when the config bool is true, the E2E determinism hook.

### Falsy env value leaves config in charge

A falsy, empty, or unparseable override value leaves the config bool in charge, so a stray `SCRIBE_DISABLE_ANIMATIONS=` never silently kills motion.

### Enabled duration clamps to 150 ms

 clamps an over-budget request to the 150 ms `MAX_TRANSITION` cap and passes a within-budget request through unchanged.

### Disabled duration is zero

When motion is disabled, `duration` returns `Duration::ZERO` and  builds a zero-length animation, so GPUI paints the end state on the first frame.

## GPUI Terminal Search

Unit tests for , the ported regex find-in-terminal state, proving whole-grid match collection and forward/backward cycling with wraparound.

### Cycles matches with wraparound

 and `select_prev` advance the highlighted match in reading order and wrap at both ends of the match list.

### Match endpoints cover the whole hit

A collected  reports inclusive start and end cells that span the entire matched run.

### Empty and unmatched queries stay valid

An empty query, a valid regex with no matches, and an invalid regex are handled without panicking — the first two yield an empty search and the last returns `None`.

## GPUI Smart Selection

Unit tests for , the ported iTerm2-style regex matcher, proving precision ranking, capture-parameter expansion, and rule-compilation errors.

### Highest-precision rule wins

 returns the highest-precision rule's match when several rules overlap the cursor.

### Legacy capture parameters expand

 expands a legacy `\0` parameter to the full matched text and labels the action by rule and kind.

### Invalid regex reports an error

A rule whose regex fails to compile is recorded in 's `errors` rather than aborting compilation.

## Drag-drop path insertion

Unit tests for , the ported shell-aware quoting for dropped file paths, proving each shell's escaping and the trailing-space insertion payload match the legacy client byte-for-byte.

### POSIX quoting escapes single quotes

 wraps the path in single quotes and rewrites embedded quotes as `'"'"'`, leaving quote-free paths simply single-quoted.

### Fish quoting escapes backslash and quote

 escapes backslash and single-quote with a backslash inside the single-quoted string, matching fish's quoting rules.

### PowerShell quoting doubles single quotes

 doubles each single quote inside the single-quoted string, the only escape PowerShell needs.

### Nushell raw-string fencing

 uses a plain single-quoted string when no quote is present and otherwise emits a raw string, widening the `#` fence until it no longer collides with the path.

### Shell dispatch selects quoter

 routes to the fish, PowerShell, or nushell quoter by shell name and falls back to POSIX quoting for anything else.

### Insertion appends trailing space

 appends a single trailing space to the quoted path so the shell treats it as a complete, separated argument.

## Window geometry compat

Unit tests for  and for the live-window capture/restore pair that persists geometry across restarts. Together they prove old-client geometry insets correctly under the new custom titlebar and that a GPUI window round-trips through a record.

### Legacy geometry gains titlebar inset

An unnormalized legacy geometry grows in height by  so the terminal area below the in-window titlebar keeps its old size, while position and monitor survive unchanged.

### Normalization is idempotent

Running  a second time on already-normalized geometry returns it unchanged, so a save-and-reload never insets twice.

### Maximized geometry keeps its size

A maximized legacy geometry keeps its stored size (the compositor overrides it on restore) but is still marked normalized.

### Out-of-range legacy size is clamped

A hostile or corrupt oversized geometry is clamped into the accepted range so the restored window stays usable, satisfying .

### Default geometry is already normalized

A freshly-created  is already in the new coordinate system, so normalization is a no-op on it.

### Legacy TOML lacks the normalized flag

A `state.toml` written by the old client has no `titlebar_normalized` key; it deserializes to `false` (via `serde(default)`) and therefore triggers the one-time normalization.

### Sanity range rejects extremes

 rejects zero, too-small, and too-large edges and accepts the range boundaries.

### Live bounds round-trip through a record

A live window's `Bounds` captured by  and reopened through  returns the identical origin and size.

The record is also already marked normalized: a capture off a GPUI window is in the new coordinate system, so a save-and-restore cycle must not inset it under the custom titlebar a second time.

### Maximized record reopens maximized

A record with `maximized = true` yields `WindowBounds::Maximized`, so the window is maximized from its first frame instead of being resized a frame later the way the winit client's async `set_maximized` was.

### Position-less record keeps the fallback origin

A record whose `x`/`y` are `None` reopens at the caller's centred fallback origin rather than at `(0, 0)`, while still taking its saved size.

Wayland never exposes a window's origin, so the capture stores `None` instead of a bogus `(0, 0)`; honouring the fallback is what stops a later X11 launch from restoring into the screen corner.

## X11 focus guard

Unit tests for , the pure reactivation state machine backing the ported X11 focus guard, proving the suppression semantics that the visual E2E exercises against the live `_NET_ACTIVE_WINDOW`.

### Inactive window suppresses input

 suppresses keyboard input whenever our window is not the active window (a compositor overlay is up).

### Reactivation debounce suppresses stray keys

After an inactive→active transition, `observe` keeps suppressing for  so a stray keystroke that arrives as the overlay closes is caught, then resumes passing input once the window elapses.

### Steady active window allows input

A window that has been continuously active is never suppressed by `observe`.

### Genuine focus event clears debounce

 drops the debounce on a real focus event (which overlays never send), so input flows immediately after a genuine refocus.

### Poll transition arms debounce

 arms the debounce when the periodic poll observes the inactive→active transition, so a key seen just afterward is still suppressed.

## Server lifecycle

Unit tests for , the pure staleness decision behind the ported local-server refresh path, proving path drift and rebuild detection without a live socket.

### Path drift marks server stale

A running server whose executable path differs from the installed binary is reported stale so the caller refreshes it.

### Newer installed binary marks server stale

A running server that started before the installed binary's modification time is reported stale (an in-place rebuild landed).

### Matching fresh server is not stale

A running server at the same path that started after the installed binary's modification time is not stale.

### Unknown timestamps are not stale

When neither the process start time nor the installed modification time is known, the server is treated as fresh rather than force-refreshed.

### Missing socket is named as no server

 reports an absent socket file as "no scribe-server is running for this user" rather than as a stale one, because nothing was left behind to clean up.

### Refused socket is named as stale

A socket file that exists but refuses connections is reported as a stale socket left by a server that exited without unlinking it — the case that otherwise surfaces as a bare "connection refused".

### Other connect failures keep the OS error

Permission denied gets its own sentence, and any other `io::Error` is passed through verbatim so an unanticipated failure is not mislabelled as one of the two known shapes.

## GPUI OSC 52 Clipboard Bridge

Unit coverage for the ported host clipboard bridge (): OSC 52 routing, the FR-019 focus gate, primary-selection read/write with AI cleanup, and reply-message construction.

An in-memory `FakeClipboard` stands in for the live arboard handle so the read+write roundtrip runs without a display server; the arboard-backed E2E stays a manual / launch-gate parity item.

### Write-read roundtrip on the system clipboard

A payload written through  to the system clipboard reads back verbatim through  — the scripted OSC 52 bridge roundtrip at the unit level.

### Primary and system selections stay independent

Writes to `ClipboardSelection::Primary` and `ClipboardSelection::Clipboard` land in separate buffers and each reads back its own value, proving the per-selection routing.

### Unavailable backend reports a bridge error

Both  and  collapse a dead backend onto `BridgeError::Unavailable` so the server maps it to an empty OSC 52 reply.

### Focus gate drops only enabled unfocused writes

 returns true only when `focus_gate_writes` is enabled and the window is unfocused, and false for the other three combinations.

### Gated write is a silent no-op

A gated write on an unfocused window returns `Ok(())` without mutating the clipboard, while the same write on a focused window goes through — the FR-019 anti-hijack behavior.

### Read reply wraps the payload

 performs the host read and wraps the value in `ClientMessage::ClipboardBridgeReadReply` under the originating `request_id`.

### Read reply forwards a bridge error

When the backend is unavailable,  still emits a `ClipboardBridgeReadReply` carrying the `Err(BridgeError)` payload rather than dropping the request.

### Prompt response echoes id and decision

 builds `ClientMessage::ClipboardPromptResponse` echoing the prompt's `request_id` and the user's decision.

### Primary read skips empty content

 returns `None` for an absent or empty primary selection so a middle-click paste is skipped, and `Some(text)` when content is present.

### Primary write applies cleanup

 runs the AI copy-cleanup transforms (dedent, unwrap) before writing to the primary selection when cleanup is enabled.

### Primary write is verbatim when cleanup off

 skips empty input entirely and writes the raw text unchanged when cleanup is disabled.

### Bridge starts ungated

 reports no gating until  adopts the `Welcome` capability bit, so a frame arriving before negotiation is refused rather than acted on.

### Parked prompt is taken once

 hands the parked request to the foreground exactly once and reports `None` afterwards, so one server prompt can never raise two modals.

### Bridge jobs drain in arrival order

 returns queued writes and reads in the order the reader saw them and leaves the queue empty, because OSC 52 ordering is what a PTY-side program observes.

### Bridge queue is bounded

Pushing past  evicts the oldest job and reports the eviction, so an OSC 52 firehose cannot grow the queue without bound between foreground ticks.

## GPUI Paste Chunking

Unit coverage for , the split that keeps a paste inside the server's `KeyInput` size limit while the shell still sees one bracketed-paste region.

### Small paste is one frame

A paste that fits the limit becomes a single frame, wrapped in the DEC 2004 markers only when the pane enabled bracketed paste, and empty input yields no frames at all.

### Large paste splits under the limit

A paste larger than  splits into several frames, each within the limit, whose concatenation is the original bytes.

### Markers ride the first and last frame

A large bracketed paste carries the start marker only on the first frame and the end marker only on the last, so exactly one marker pair spans the whole paste.

## GPUI Notification Dispatcher

Unit coverage for the platform-independent notification dispatcher logic (): the `replaces_id` coalescing state machine and the freedesktop `expire_timeout` mapping.

The zbus transport and click-to-focus wiring are verified by the manual parity checklist.

### Timeout mode maps to expire_timeout

 maps `SystemDefault` to `-1`, `Never` to `0`, and `Custom` to `timeout_secs * 1000` (saturating on overflow).

### Same session reuses replaces_id

Repeated shows for one session reuse the live notification id via  and , keeping exactly one live toast.

### Expired toast reallocation drops stale mapping

When the daemon allocates a fresh id despite a non-zero `replaces` (the prior toast expired), `record_shown` drops the stale reverse mapping so a later click cannot mis-route.

### Session close removes both mappings

 returns and clears the session's id once, then `None` thereafter, leaving no dangling id.

### Daemon closed signal clears mappings

 drops both mappings for a closed notification id and no-ops on an unknown id.

### Shutdown closes every live toast

 enumerates every live id for the shutdown close-all and  empties the state afterward.

## GPUI Notification Gate

Unit coverage for the decision half of desktop notifications (): which AI transitions earn a toast, which focus states suppress one, and the focus-on-activate fallback. The zbus delivery half is proven by the scripted E2E.

### Processing to attention fires once

 fires only on `Processing → attention`: a first observed state does not notify (nothing was processing before it) and sitting in the same attention state is not a new transition.

### Non-attention states never fire

`Processing` and `Error` are not attention states, so reaching them never produces a payload even under `NotifyCondition::Always`, while the following `Processing → PermissionPrompt` cycle still does.

### Disabled notifications never fire

With `enabled = false` the tracker still folds every transition in but produces no payload, so re-enabling it mid-session picks the state machine up where it left off instead of needing a fresh `Processing` cycle to re-seed.

### Focus conditions gate delivery

 reproduces the winit gate over : `when_unfocused` suppresses any focused window, `when_unfocused_or_background_tab` suppresses only the focused foreground pane, and `always` suppresses nothing.

### Pending focus is consumed once

 yields the recently notified session exactly once, and  clears it so an exited session cannot be focused by a late activation.

### State labels name the attention state

 maps the three attention states onto "Ready", "Waiting for input" and "Permission required", and degrades any other state to a generic "Attention" rather than failing to build a summary.

## GPUI Perf A/B Gate

The launch-blocking performance comparison for the GPUI client rebuild. The `tools/perf-ab-rig/run-perf-ab.sh` rig drives all five Clarification-Q3 metrics on both clients and writes a per-metric pass/fail report.

The five metrics and thresholds are: startup-to-first-frame (no worse than the old client end-to-end, median of 3 samples per client, also gating splash deletion — re-scoped 2026-07-24 from the original `<= 500 ms` absolute budget, see the spec's "Q3 re-scope": the 190 ms figure the absolute budget was anchored to was the old client's phase-scoped GPU-init timer, not process-start-to-first-frame, and the GPU bring-up floor alone exceeds 500 ms on the reference host; amended 2026-07-25 with an absolute `<= 150 ms` cap on Scribe-attributable startup, the part of the span outside gpui's `cx.open_window`), input latency (no worse than old client), cat-firehose throughput (no worse than old client), memory at 10 tabs (`<= old + 20%`), and scroll (sustained 60 fps with `< 1%` dropped frames). Old-client baselines live in `specs/016-gpui-client-rebuild/perf-baseline.md` as a machine-readable `perf_baseline_<key>=<value>` block that `--record-baseline` rewrites in place; the generated report is `specs/016-gpui-client-rebuild/perf-ab-report.md`. A `--live --startup-only` run scores metric 1 alone without opening tabs or typing keys — the fast loop for the startup perf bead — and `--scroll-only` does the same for metric 5.

The rig has two modes. `assess` (default) generates the current-state report from the committed baseline plus a static capability check without launching any GUI or touching the live server, marking every live-only metric `NOT-MEASURED`. `--live` is the launch-gate mode: it launches each target client on the same machine/session, drives every workload through `xdotool`, and enforces the thresholds; it attaches to the already-running server and never restarts it. The server it attaches to is always the isolated `scribe-dev` one — see .

`--startup-only`, `--latency-only` and `--scroll-only` narrow a live run to one metric so a perf bead can re-measure a fix in a minute rather than paying for every workload; everything they exclude reports `NOT-MEASURED`, so a narrowed run is an iteration loop and never a gate verdict.

### AI tab open latency

The separate `--ai-tab-only --live` mode gates AI-tab launch overhead against a soft 1000 ms budget without adding a sixth metric to the five-result A/B report.

It waits for the seed pane's PTY byte count to become idle before timing, then reuses `open_owned_tab` ownership polling with the `ctrl+alt+c` chord and ends on the first PTY-counter increase from a PATH-first `claude` marker stub. The timed span has no settle delay; the ordinary 1.5 s post-open sleep is skipped. The stub must contain and print `SCRIBE_AI_TAB_PERF_MARKER` immediately, then remain alive until the rig closes the owned session, so a real AI CLI cannot be timed accidentally.

The 2026-08-01 verification ran project-built release binaries under Xvfb and openbox in a disposable, network-disabled container with isolated runtime, home, config, state, data, and cache directories and no host Scribe socket. It measured **587.627 ms**, passing the 1000 ms budget.

The latency workload takes 60 samples rather than the 25 it started with. Both clients land in the 0.2–0.4 ms band once they are measured at the same pipeline stage, and there the median of 25 moved by more than the gate's own 10% allowance between back-to-back runs of the *same* binary (0.260 then 0.366 ms) — enough noise to decide the verdict by itself.

The three comparative metrics are enforced with a 10% run-to-run noise allowance, which is the repeatability of these measurements on a loaded desktop rather than extra headroom. A comparative metric with no committed baseline reads `NO-BASELINE`, and the overall verdict is `INCOMPLETE` unless all five metrics are measured and inside their thresholds; a `FAIL` on any metric fails the gate and reopens the perf bead.

### Startup instrumentation

Startup is gated in two parts because the platform, not the client, owns most of the span: Scribe-attributable startup must be `<= 150 ms` absolute, and total startup-to-first-frame must be no worse than the old client measured the same way.

Both clients report the total through the shared probe's `startup_first_frame_ms`, latched by  on the first painted frame and timed from the probe arm — the first statement of each client's `main` — so the two halves of the A/B are the same measurement rather than two different sub-spans. The rig launches each client cold, waits for that key, and kills it. A binary built before the probe carried that key (the installed old client) falls back to the startup-log method: the wall-clock delta between its first `client startup timing` line and the `init_gpu_and_terminal_done` line. That fallback stops at GPU-ready rather than first paint, so it slightly understates the old client, and the phase-scoped `total_ms` printed on that line — GPU-init only, after config load, the host-stats walk and app construction — is never compared against the GPUI marker; doing exactly that is what produced the unreproducible 190 ms "baseline" the original absolute budget was anchored to.

The GPUI client additionally writes the marker named by `SCRIBE_GPUI_STARTUP_TIMING`.  latches on the first painted frame and writes `first_frame_ms`, `gpu_bringup_ms` and `scribe_startup_ms`, all timed from the `PROCESS_START` origin captured at the top of `main`. The bring-up figure comes from , stamped by the root-view builder that GPUI invokes at the end of `cx.open_window`; that span is wgpu adapter enumeration, device creation and surface configure, and no Scribe code runs inside it. `scribe_startup_ms` is the remainder and is what the absolute half of the gate caps.

On the reference host `cx.open_window` alone costs 610–751 ms and the old client's own `configure_wgpu` costs 464–561 ms, so no client can paint inside 500 ms there. Measured like-for-like through the probe, the old client reaches its first frame in 3401–4682 ms against the GPUI client's 634–780 ms, of which only 24–29 ms is Scribe's own.

### Live mode never drives the stable server

A live run seeds sessions, opens ten tabs, types shell commands into them and types `exit` to close them again, so against the stable install every one of those lands in whatever terminal the developer is actually using.

Scribe derives its whole runtime identity from the running executable's file stem in : a binary named `scribe-dev` resolves to `/run/user/<uid>/scribe-dev/server.sock`, `~/.config/scribe-dev` and `~/.local/state/scribe-dev`, and anything else resolves to the stable slug. There is no environment override, so the rig copies every binary it launches — both clients and the `scribe-test` helper — into its work directory under that name and runs the copies. The copies are byte-identical to the binaries passed in, so the numbers still describe the binaries under test. `--live` reports the dev socket's absence as a blocker rather than falling back.

### Live-run preflight

A `--live` run checks its inputs before it launches anything and aborts with a diagnosis when they cannot produce valid numbers, because both of the ways it used to degrade surfaced as `NO-BASELINE` rather than as a missing prerequisite.

Two conditions are fatal. A client binary that carries no `SCRIBE_PERF_PROBE` string was built without  and can never write a probe report, so every rig wait keyed off that file burns its full timeout; the installed `/usr/bin/scribe-client` is exactly such a binary, which is why the gate command names `target/release/scribe-client`. An unusable `scribe-test` leaves the client with no detached session to attach to, and a client that claims an empty window has no workspace, so both clients refuse to open a tab and every typing workload is unmeasurable.

Neither used to stop the run: the missing helper logged "continuing without a seeded session" and the probe-less binary was only noticed 30 s later as "client … never reached a first frame". Both then reported the input-latency, firehose and memory metrics as `NO-BASELINE`, which reads as "no baselines have been captured yet" rather than "this run was handed inputs it cannot measure" — the misdiagnosis that cost two full gate runs during `scribe-38e.42`.

`--startup-only` is the deliberate exception to the probe check. Metric 1 has a documented fallback to the startup-log method for a binary predating the probe key, and such a run opens no tabs, so it needs neither the probe nor `scribe-test`; a probe-less binary is logged there instead of rejected. An environment that cannot host a client at all — no `DISPLAY`, no `xdotool`, no running server — stays non-fatal and reports `NOT-MEASURED`, because it describes the machine rather than the run's arguments.

### Driving the workloads

Live mode drives both clients with `xdotool`, and three delivery details are load-bearing because getting any of them wrong makes a workload measure nothing at all rather than fail loudly.

Enter is always sent as its own key event: `xdotool type` does not deliver a trailing newline as `Return` to either client, so a command typed with `\n` appended echoes onto the command line and never runs — which silently zeroed the firehose and scroll metrics until the rig started submitting with an explicit `Return`. The scroll workload advances `less` with `space` rather than PageDown for the same class of reason: a synthetic `Next` reaches the client's own `ctrl+Next` next-tab binding but is dropped before the PTY, so it drove no repaint at all. Key delivery also falls back: window-targeted synthetic events are preferred because a stray keystroke cannot then escape into another application, but a toolkit reading keys through XInput2 ignores them, so a new-tab attempt that produces no tab is retried with XTEST and the mode that worked sticks for the run.

Live mode consequently needs a window-managed display. Under a bare `Xvfb` the winit-based old client receives neither delivery mode, and its half of the A/B reports `NOT-MEASURED` rather than a fabricated number.

### The scroll metric measures the renderer, not the rig

Metric 5 is the one absolute threshold, so both clients are measured purely to attribute a failure: a target the old client also misses is a property of the workload, not of the client under test.

The rig had no old-client arm until bead `scribe-38e.91`, and without it a reproducible `29 fps, 14% dropped` reading could not be diagnosed at all.

Adding the arm showed the old client pinned at the same ceiling, and both halves of the shortfall were in the rig. The measurement window is derived from the probe's `uptime_ms` stamps, and the probe rewrites its report only when the client paints or drains PTY bytes — so a snapshot taken after the settle wait carried the stamp from whenever the client last had work, charging that whole idle stretch to the drive. It penalised the client that idles best the most: the GPUI client stops painting entirely and its report went 4.1 s stale, against 0.5 s for the old client, which kept draining bytes. The rig now waits for a report rewrite before opening the window, so both edges name a moment the client was busy.

The second defect was the workload itself. It paged `less` with synthetic `space` events, and each page-forward produces exactly one repaint, so the measured frame rate was the rate at which `xdotool` could deliver keys — 21 ms per key on the reference host regardless of `--repeat-delay`, a ceiling near 47 fps against a 60 fps target. Both clients sat on it at 5-10% of one CPU core. The scroll is now driven by an unpaced writer inside the pane with no keys sent while the window is open, and the GPUI client sustains 60 fps with no dropped frames while the old client reaches 41-51 fps with 6-8% dropped. The threshold needed no re-scoping; `perf-baseline.md` carries the numbers.

### Runtime probe instrumentation

Four of the five metrics are only observable from inside a running client, so both clients link the same probe and the A/B compares identical measurement points rather than two different instrumentations.

 activates only when  (`SCRIBE_PERF_PROBE`) names a report path, so a normal run costs one atomic load per call site and writes nothing.  counts painted frames and scores gap-derived dropped frames,  opens the echo round-trip clock that  closes while also counting drained PTY bytes, and  publishes the client's tab session list and focused session. Memory is the exception: the rig samples `VmRSS` from `/proc` externally, using the probe's session count only to know when 10 tabs are actually open.

The report is a flat `key=value` file rewritten at most every 200 ms with cumulative counters plus an `uptime_ms` stamp, so the rig derives a per-workload number from before/after deltas and the client needs no window bookkeeping. The session list doubles as the rig's safety interlock: a typing workload only runs in a tab the rig opened and watched appear as focused, so it can never type into a pane that was already open.

Both clients call the probe from the same three places, and "the same" is load-bearing down to the pipeline stage. The GPUI client reports frames from  on render, stamps keystrokes in , and counts PTY bytes in `on_pane_output_message` — its IPC read task. The old client reports frames from  on redraw, stamps keystrokes alongside its `ClientCommand::KeyInput` send, and counts PTY bytes in  — its IPC read task.

PTY output is stamped where it enters the client, in the task that reads the frame off the socket, and never at a later stage. Stamping the two clients at different stages does not merely add a constant; it compares two different quantities. The old client used to count bytes on its UI thread, in `handle_stream_user_event`, three hops downstream of the read task: read task, `EventLoopProxy::send_event`, winit's user-event queue, then the redraw loop. Behind that unbounded queue the probe measures the UI thread's backlog rather than the server round trip, and both probe-derived comparative metrics were corrupted by it in opposite directions. The firehose scored the old client at 0.232 MiB/s against the GPUI client's 17.623 MiB/s, because 32 MiB of `cat` output reached the UI thread far slower than it reached the socket. Input latency scored the old client at 0.032 ms against the GPUI client's 0.209 ms — *faster than a bare local socketpair round trip on the same host*, which is the tell — because with a backlog standing in the queue the next `handle_stream_user_event` after a keystroke was an already-queued stale payload, so the sample timed one event-loop turn instead of a key-to-echo trip. That 6.5x "regression" (bead `scribe-38e.92`) was the measurement, not the client.

The pairing is also session-scoped:  records which session the keystroke was routed to and  closes the clock only for output on that session, so a chatty background pane cannot fabricate a round trip. Bytes are still counted for every session, because the firehose metric is about total drain rate. An unmatched keystroke holds the slot for at most ; past that the next keystroke re-arms the clock, so an echo that never arrives cannot silently zero the sample count for the rest of the run.

#### Frame gaps score missed 60 fps slots

A gap between painted frames is converted into the number of 60 fps slots it skipped, so a stall during a driven scroll is charged against the `< 1%` dropped-frame budget rather than being invisible.

#### Idle gaps are not dropped frames

A client sitting idle between workloads paints nothing, and without a ceiling every pause would be scored as thousands of drops, so gaps past the idle threshold count as zero missed frames.

#### Latency statistics summarise samples

The report carries the median and mean of the echo round-trip samples, including the empty-sample and even-length cases, because the gate compares medians across clients.

#### Report renders every rig key

The rig parses the report by key, so serialisation must emit every key it reads — counters, uptime, latency statistics, session list and focused session — in the exact `key=value` shape.

#### Probe stays inert without the env var

With `SCRIBE_PERF_PROBE` unset every entry point must be a no-op that neither writes a file nor panics, since both clients call them on every frame and every keystroke in normal use.

#### Counters pair input with its echo

A keystroke sent while another is still unmatched must not restart the clock, so each recorded latency is an unambiguous key-to-echo pair and the byte counter accumulates independently.

#### Another pane's output is not an echo

Output arriving for a session other than the one the keystroke was routed to counts toward drained bytes but must leave the echo clock running, so a background pane draining output cannot fabricate a round trip far shorter than the real one.

#### An echo that never comes releases the clock

A keystroke whose echo never arrives must not hold the pairing slot for the rest of the run, so once it ages past the pending-input TTL the next keystroke re-arms the clock instead of going unmeasured.

#### First frame latches the startup span

The startup metric is reported by a probe the rig reads long after launch, so the first painted frame must latch the span once and every later frame must leave it alone; before any frame the report omits the key entirely rather than claiming a zero.

## GPUI Client Headless Suites

The `#[gpui::test]` and golden suites in `scribe-client` are the primary correctness oracle for client-internal *logic*. They need no display server, and each landed suite maps to the `parity-inventory.md` row whose logic it exercises.

**A headless suite does not satisfy a parity row.** It proves the module behaves correctly when constructed; it says nothing about whether the running client ever constructs it. The 016 reachability audit found 113 of 164 user-facing parity rows unreachable while their suites were green, so `parity-inventory.md` now carries a mandatory "Reachable from" column and retains `gpui-test` as the row-level method only for the nine removed-configuration-key rows. Read the map below as *logic coverage*; the row's own verification method and reachable-from symbol live in the inventory and are authoritative.

**Nor does a reachable row set prove parity unless the rows are the requirements.** A requirement with no row is scored by no oracle at all, which is how nine spec requirements stayed invisible through both gates until 2026-07-27. The inventory's row set is now derived from `spec.md`'s requirement register and the derivation is enforced by `tools/check-parity-inventory.sh`; see .

These suites run under `just test` (and the `Dockerfile.func` image's Rust toolchain). This section consolidates the headless coverage the `gpui-client-rebuild` epic (`scribe-38e`) accumulates, and the coverage frontier lists the parity rows whose suites unblock as their feature beads land.

| Headless suite | Spec section | Parity-inventory row(s) whose logic it covers |
| --- | --- | --- |
| Pane tree entity ops |  | Input/keybinding "Pane layout" |
| Pane split-tree logic |  | Input/keybinding "Pane layout" |
| Workspace tree entity ops |  | `CreateWorkspace`, `MoveSession`, `ReportWorkspaceTree` |
| Input byte encoder golden |  | `KeyInput`, Terminal shortcuts |
| Keybindings dispatch |  | Pane/Workspace/Tab/Navigation/View keybinding actions |
| Config load with removed keys |  | "Removed configuration keys" rows |
| Config live reload |  | `ConfigReloaded` live reload |
| Window chrome geometry |  | Rendering/window status-bar and prompt-bar chrome |
| Window opacity |  | Rendering/window `appearance.opacity` |
| Update surfaces |  | `UpdateAvailable`, `UpdateProgress`, `TriggerUpdate`, `DismissUpdate` |
| Cell-accurate paint path |  | Box drawing, Font fallback, Ligatures |
| Find overlay |  | `SearchRequest`, `SearchResults`, `find` keybinding |
| URL/OSC8 detection |  | hover/dwell/open surface |
| IPC bridge ordering |  | Executor-model ordering risk |
| Remote connect picker |  | `ListRemotePeers`, `ListLanPeers`, `RemotePeerList` remote connect picker |
| Remote handshake |  | `RemoteHandshake` preamble + dial-env spawn |
| Lost control banner |  | `WindowTakenOver` displaced-client reclaim |
| LAN device approval |  | `LanApprovalRequest`/`LanApprovalDecision` prompt |
| LAN chrome |  | `LanApprovalRequest`, `LanPeerList`, `LanEnv` shared state |
| LAN dial preamble |  | `LanHello`, `LanApprovalPending`, `LanApprovalResult` |
| Remote chrome |  | `RemoteEnv`, `RemotePeerList`, `WindowTakenOver`, `RunAction` shared state |
| Window sharing |  | `ShareRoster`, `ControlClaim`/`ControlRequest`/`ControlGrant` |
| Local share join |  | `Hello` window claim (harness plumbing, no parity row) |
| Pane dividers |  | "Pane divider drag-resize" chrome |
| Focus borders |  | "Focused pane/workspace border" chrome |
| Split-scroll |  | "Split-scroll live-bottom pin" AI-pane chrome |
| Terminal viewport |  | `scroll_up`/`scroll_down`/`scroll_top`/`scroll_bottom`, vi mode, smart selection reachability |
| Font zoom |  | "Zoom in/out/reset" View keybinding actions |
| Mouse reporting |  | "Mouse reporting (X10/SGR-1006, modes 1000/1002/1003)", mouse-wheel scrolling |
| OSC 52 clipboard bridge |  | `ClipboardPromptResponse`, `ClipboardBridgeReadReply`, `ClipboardBridgeWrite`, `ClipboardBridgeReadRequest` OSC 52 bridge |
| Notification dispatcher |  | Notification `replaces_id` coalescing + click-to-focus |
| Terminal chrome metadata |  | `CwdChanged`, `GitBranch`, `EnvStatus`, `SessionContextChanged`, `WorkspaceNamed` status-bar segments |

### Coverage frontier

Testing-Strategy suites not yet consolidated are blocked on their feature beads landing in `scribe-38e`. Each is tracked here against the parity row whose logic it will cover.

Completing this frontier closes the headless oracle, which is a prerequisite for the launch-gate bead (`scribe-38e.42`) but not its metric — that is the reachable-row count in `parity-inventory.md`.

Pending headless suites and the parity rows they will satisfy:

- Selection model (cell/word/line, WRAPLINE) — terminal selection surface.
- Sync-frame queueing + 150 ms expiry — CSI-2026 burst preservation.
- Replay application — `SessionReplay` reconnect restore.
- Reconnect topology rebuild — `WorkspaceInfo` layout restore beyond the existing  `from_tree` path.
- Degraded/failure paths — server-down at launch, socket vanish mid-session, adoption failure, replay decode failure (pane error state, no crash), reconnect retry/timeout.

### Pane split-tree logic

The pure  split-tree drives the "Pane layout" keybinding actions (`close_pane`, `cycle_pane`, `focus_left`/`right`/`up`/`down`) beneath the  entity wrapper, so its navigation and mutation logic is asserted directly without a GPUI context.

Over a 2x2 pane grid the suite exercises the surface the entity tests do not reach directly:  resolves a direct neighbor on all four axes and wraps to the opposite edge along the same column when none exists;  cycles panes in depth-first order and wraps past the last leaf;  exchanges two leaf positions; and  promotes a closed pane's sibling while refusing to remove the sole remaining leaf.

### GPUI keybindings dispatch

Verifies the ported  parser and  dispatch so no configured shortcut regresses across the GPUI cutover.

Driving each action from its default binding, the suite asserts every one of the 50+  variants resolves to its named value, that command-palette/settings/find produce the right , and that the seven terminal shortcuts emit their fixed escape sequences. It also checks combo parsing (`cmd`/`super` → platform modifier, named keys, rejected garbage), exact-modifier matching that ignores the GPUI function flag and is case-insensitive on the base character, key-down-only gating (press and repeat match, release does not), and that invalid combos are skipped without aborting the parse.

Four cases lock , the rule that kept `close_tab` and `new_window` unreachable until it existed. Every entry in  must resolve to its overlay *and* match no default binding, so a future overlay chord cannot quietly land on a user action; the `close_tab` and `new_window` defaults must resolve to their `LayoutAction` and be declined by ; a config that rebinds `close_tab` onto an overlay's own chord must still reach the action; and a key release matches no overlay chord.

### GPUI tab session strip

Locks the selection rules of , the ordered strip the shell's tab shortcuts and the IPC reader both mutate, so a tab's label and the attached session can never disagree.

The suite drives the shortcut side —  appends and focuses a new tab, `focus_next`/`focus_prev` wrap in both directions, `select` jumps by index and reports no change for an out-of-range or already-active position — and the server side, where  preserves the focused session across a `SessionList` rebuild (falling back to the first tab when it is gone) and `remove` clamps the cursor as tabs exit. One case guards the attach feedback loop: because the server re-announces `SessionCreated` to acknowledge every `AttachSessions`, `insert_active` must report a known session as "not added" and leave the selection untouched.

### GPUI tab task labels

Locks the precedence rule behind , the mutator the four provider task-label notices land in, so an AI tab shows its task and falls back to its shell title once the task ends.

A set label outranks the title through  and leaves sibling tabs untouched; a title arriving mid-task is stored but stays behind the label until it clears; a blank label is treated as a clear, so a provider cannot blank a tab down to nothing; and an identical set, a repeated clear, and an unknown session all report "no change" so the reader does not repaint for nothing.

### GPUI terminal chrome metadata

Locks the per-session merge rules of , the store the IPC reader fills from the terminal-chrome messages and the status bar reads once per frame.

The suite writes a CWD, branch and env status for one session and a CWD for another, then asserts the fields land independently, that a sibling pane's update never leaks across, that `set_git_branch(None)` really clears the segment (the server sends it when the CWD leaves a repository, so treating `None` as a no-op would strand a stale branch), and that  drops only the exited session.

### GPUI terminal chrome labels

Verifies that  and  lower a `SessionContext` onto the status bar the way the legacy client's `frame_status_snapshot` did.

A context with `remote: false` yields no host label — a local pane must keep this machine's own name — while still exposing its tmux session, a remote context yields the host, and a remote context with an empty host falls back to the local label rather than rendering a blank segment.

### GPUI workspace naming and reseed

Covers  and , the two paths that repopulate the chrome after a reattach instead of waiting for the next shell prompt.

Seeding from an authoritative `SessionList` adopts the listed CWD and context, leaves a live branch the list omits untouched (the list is a snapshot, not a transition), prunes any session missing from it, and takes each workspace name from the batched `workspaces` entries. A rename to whitespace clears the workspace segment rather than rendering an empty one.

### Config load with removed keys

Confirms a config carrying every removed appearance key deserializes without error and leaves the GPUI-consumed surface intact, satisfying the parity inventory's "Removed configuration keys" rows.

The test parses the removed-keys TOML into , asserts the live appearance fields (font, font size, theme) parsed correctly, then resolves the full  snapshot and checks the theme, derived chrome colors, and parsed bindings all populate — proving the removed keys are inert and never reach the paint path.

### Config live reload

A scripted reload confirms that edits to theme, font, and keybindings reapply live without a restart, backing the `ConfigReloaded` parity row.

Building a  from an initial config and calling  with an edited config, the test asserts the returned  flags the theme and font as changed, the resolved theme/chrome and font metrics actually updated, and the re-parsed  reflect the new combo. Companion cases assert an opacity-only edit is scoped to `opacity_changed` and an identical config reports no change.

Those cases prove the plan is computed correctly, but not that a running window ever asks for one. The child cases below cover the runtime path that closes that gap — watcher signal, foreground poll, painted font, and the outbound `ConfigReloaded` — and  drives the whole chain against a real window.

#### Watcher signal collapses a burst

Confirms  turns the several `notify` events one editor save emits into exactly one reload, and reports nothing when the file is untouched.

The test polls a fresh signal (no reload due), fires three `signal()` calls to stand in for the delete/create/modify sequence a save-by-rename produces, and asserts a single  consumes all three and the next poll is clean. This is the property that lets the foreground poll on a timer instead of waking per filesystem event without either missing a save or reloading three times per save.

#### Runtime applies a watcher-signalled edit

Drives the full foreground path — signal, poll, reload, read back — over , proving a window only reloads when the watcher fires and that every live surface swaps in one step.

Using  so no real config directory is touched, the test asserts an unsignalled poll does not reload, then signals and applies an edit changing theme, font, opacity, and the command-palette combo at once. It checks the plan flags all three surfaces, the resolved theme and font actually updated,  carries the new value to the opacity hook, the re-parsed  expose the new palette key, and the consumed signal leaves no second reload queued.

#### Grid font tracks the live appearance config

Verifies  derives the grid's painted metrics from `[appearance]` so a font edit changes pixels rather than only the stored config.

The test builds metrics from an edited appearance block and asserts the family, size, `line_padding`-inclusive row height, and the cell advance reported to the server all follow the config. A companion assertion drives `font_size = 0` and checks the value is clamped to the floor, so a bad edit degrades to a small grid instead of collapsing the window to nothing.

#### Reload announces ConfigReloaded

Backs the `ConfigReloaded` parity row at the protocol boundary: the reload path must put the message on the wire, ordered ahead of whatever the user types next.

Driving  followed by a `KeyInput` on the same ordered writer channel, the test asserts a `ClientMessage::ConfigReloaded` is dequeued first. Ordering is the point: the server must have re-read the config before it interprets the next keystroke, otherwise a policy edit applies a keypress late.

### Find overlay

Covers the pure halves of : the query/reply state machine, the viewport projection of the server's absolute grid rows, and the recolouring that turns a match into painted cells.

None of these prove the running client finds anything — the round trip they stand in for is a wire property, verified end to end by `tests/e2e/visual/find-overlay.sh` (`just e2e-visual-find`), which asserts `SearchRequest` leaving the real client and `SearchResults` coming back from the real server while screenshotting the overlay and its highlights.

#### A typed query asks the server once

Every request costs the server a full-scrollback scan, so the overlay debounces edits; this pins that a burst coalesces into one request for the settled text, that every kind of edit still re-asks, and that a no-op edit does not.

Two edits inside one debounce window emit nothing until it elapses and then emit a single `::QueryChanged` carrying the final query. Typing, pasting, Backspace and Delete each re-ask once settled, while popping an already-empty query, clearing an already-empty query, and a control character emit nothing. Dismissing retires a scheduled request, so a closed overlay never searches on.

#### A stale reply never replaces live matches

A pause mid-word settles the query and sends it, so several replies can be in flight at once and the answer to an abandoned prefix must never be shown.

 ignores a  answering an earlier query, adopts the one answering the typed query, and adopts any given reply exactly once — so a redraw cannot reset the match the user has cycled to.

#### Cycling wraps and drives the counter

The `n/m` header is the only feedback the overlay gives about where in the match list the user is, so the cycling and the counter have to stay in step.

`next_match` and `prev_match` wrap in both directions, the header reads `Find  1/3` at the top of a three-match list, a query with no matches reads `Find  no matches` instead of a zeroed counter, and cycling an empty match set is a no-op.

#### Only on-screen matches are highlighted

The server reports absolute grid rows including negative scrollback rows, while this client paints the active viewport only; clamping an off-screen match onto a visible row would highlight text that does not match.

 drops matches above and below the viewport, clamps a span to the last painted column, marks exactly the current index, and yields nothing for a degenerate grid.

#### Matches recolour the cells they cover

Highlighting has to go through the per-cell resolve step rather than being drawn over the finished grid, because the current match inverts its foreground for contrast.

Applying spans to one resolved row leaves every cell outside them untouched, gives the current match the opaque accent plus its contrast foreground, and gives a passive match its own background blended towards the accent with its text colour preserved.

#### The overlay's query reaches the wire

The overlay can only ask; the sink is what turns the ask into a frame, so the lowering is asserted on the outbound channel.

 enqueues a `ClientMessage::SearchRequest` naming the session, the query verbatim, and the 256-match limit.

### Cell-accurate paint path

Locks the pieces of  that can be asserted without a display server: the snapshot's per-cell state, the box-drawing quad reduction, and the font configuration each shaped run carries.

None of these prove the running client paints anything — a headless case passes identically whether or not the app constructs `TerminalElement`. They exist to pin the pure inputs the paint call consumes, so a regression shows up as a failing assertion rather than as a wrong screenshot. The painted result is a visual-E2E property: bead `scribe-38e.63` confirmed per-cell SGR colours, seamless box joins, a live `appearance.ligatures` flip, and `U+F09B`/`U+F121` resolving through the embedded fallback face against a real X11 window, per .

#### Snapshot carries per-cell colour and attributes

Verifies the parser-to-paint boundary: `Content` must carry each cell's raw colour fields and SGR flags, because a snapshot of plain strings can only ever be painted in one colour.

Feeding a bold-red-on-blue run, a true-colour underlined run, and a reset through , the test asserts each cell's `fg`, `bg`, and `flags` survive into the snapshot — named colours as named, a 24-bit colour as `Color::Spec`, and BOLD/UNDERLINE set only on the cells that carry them. It also checks blank cells still pad the row to terminal width, since the paint path indexes cells by grid column.

#### Box-drawing quads reproduce the mask

Confirms the quad reduction the GPUI overlay depends on is lossless, because a paint path that draws rectangles instead of a texture is only correct if the rectangles are the texture.

For a spread of strokes, corners, crosses, shades, and blocks, the test rasterizes  back into a flat alpha buffer and asserts it equals 's mask pixel for pixel, with no empty, transparent, or overlapping rectangle. It also pins the full block to exactly one edge-to-edge quad — the property that keeps a screen of box drawing affordable and its tiling seamless — and checks an unhandled character still falls through to the font.

#### Ligature shaping follows appearance.ligatures

Backs the Ligatures parity row at the configuration boundary: the setting must reach the `Font` the paint path actually shapes with, not merely a metrics struct.

The test builds  from an appearance block with ligatures on and off, asserting `calt` is left at the font default in the first case and explicitly disabled in the second, then checks the same feature travels on the run font for both plain and bold cells.

#### Every run carries the Nerd Font fallback chain

Backs the Font fallback parity row: omitting the chain silently falls back to GPUI's own platform font selection, which does not preserve Scribe's ordering.

Across plain, bold, italic, and bold-italic cells the test asserts every run carries the full ordered fallback list with `Symbols Nerd Font Mono` first and `Unifont Sample` absent, and that the configured `font_weight` / `font_weight_bold` and the italic style select the right variant.

#### Embedded Nerd Font survives GPUI face eviction

Backs the other half of the Font fallback row: naming the chain is useless if GPUI's `load_family` evicts the face, so the embedded asset must keep the exact shape the eviction check and the chain resolution depend on.

Parsing , the test asserts the family name is exactly `Symbols Nerd Font Mono` (the chain's first entry), that `U+006D` maps to a glyph — the property gpui `f96212f` requires to keep a face at all, added by `tools/patch-nerd-symbols-font.py` — and that the powerline and Font Awesome codepoints the visual capture relies on (`U+E0A0`, `U+E0B0`, `U+E0B2`, `U+F09B`, `U+F121`) are all covered.

#### Box-drawing cells leave the shaped text

Pins the substitution that makes the overlay authoritative: a box-drawing codepoint must not also be shaped from the font, or the glyph's bearing gaps reappear on top of the quads.

The test asserts box-drawing and block codepoints become spaces before shaping while ordinary and Nerd Font codepoints pass through, that a control character is blanked so `shape_line` can never see a newline, and that a row's blank tail is trimmed except where an underline or strikeout must still be drawn.

### Window chrome geometry

Locks the arithmetic behind the derived startup window size, so the terminal grid and the chrome bands can never again be sized to overlap. See .

These cases cover the derivation only. That the running window really shows its last grid row and all three bands is a display-server property, verified by .

#### Default window size clears every chrome band

Confirms  leaves the whole grid *and*  room, which the old hardcoded 960x680 did not.

At the shipped 120x36 grid and font size 14 the derived height minus the chrome must still cover 36 rows of 18.9 px and the width must cover 120 cells of 8.4 px. The test also pins the exact shipped answer, because `120 * (14.0 * 0.6)` lands a hair above 1008.0 in `f32` and a naive `ceil()` would spend a whole extra pixel on that float noise. A degenerate font metric (zero cell width, negative line height) must collapse the grid to the minimum edge rather than producing a zero-size or negative window.

#### Startup size never exceeds the display

Verifies  shrinks an oversized request to the screen, because a window taller than the display moves the status bar off the desktop instead of off the window.

A `font_size = 72` grid asks for a window far past 1920x1080 and must come back clamped to exactly that; a window that already fits must pass through untouched; and a nonsense display report must not clamp the window below the minimum edge.

### Window opacity

Locks the `appearance.opacity` paint model — clamping, which surfaces scale, and which deliberately do not — so the launch gate's live-translucency row cannot regress into the hardcoded-opaque state that bead `scribe-38e.56` fixed. See .

These cases cover the derivation only. The composited result — an opacity edit shifting real pixels toward the desktop behind the window, live and without a restart — is a display-server property and is verified against a running window on a composited host.

#### Clamps configured opacity

Confirms  saturates out-of-range values instead of producing an invalid colour, because the config file is never validated on load.

The test drives an in-range value through unchanged, checks `1.5` and `-0.2` saturate to `1.0` and `0.0`, and checks NaN falls back to fully opaque so a malformed edit degrades to a normal window rather than an invisible one.

#### Backgrounds carry the opacity alpha

Verifies  folds the configured opacity into a theme slot's alpha and touches nothing else.

Painting a theme slot at `1.0` leaves it opaque; at `0.85` only the alpha moves while the RGB channels stay at the theme's colour, so a composited desktop blends toward the backdrop instead of shifting hue. Out-of-range values saturate through the same clamp.

#### Already-translucent chrome multiplies

Checks that a colour which is already partly transparent keeps its relative translucency when opacity scales it, and that foreground slots are exempt.

 and  multiply a half-alpha colour at half opacity down to a quarter, matching the legacy renderer's per-cell background scaling, while  returns a foreground colour's own alpha untouched.

#### Chrome backgrounds scale, chrome content does not

Proves  makes the titlebar alpha-aware without dimming its text, satisfying the parity row's requirement that chrome backgrounds repaint alpha-aware.

Building the palette at `1.0` and `0.85` from a real theme, the test asserts the bar background, active-tab background and gradient top each scale by the opacity while their RGB stays put, and that text, separator and accent alphas are identical at both values. Out-of-range opacities clamp rather than overshooting or inverting the bar.

#### Status bar band scales with opacity

Verifies  scales only the filled band, keeping every readable element at full strength.

The test compares a theme-derived palette against its scaled copy and asserts the background alpha follows the opacity while the text, top hairline and dimmed stat-label alphas do not, then drives `1.5` and `-0.2` through the same clamp.

### GPUI Update Surfaces

Locks the transition table of  — what the server's broadcasts arm, which confirmation the CTA opens, and what the user's decision clears. See .

These cases cover the state holder only. That the running window actually receives a real `UpdateAvailable`, paints the CTA, and puts `TriggerUpdate` / `DismissUpdate` on the wire is a whole-app property, verified by .

#### Server broadcasts arm the status bar CTA

Confirms an untouched state offers nothing and that  is what arms the CTA, so a client that never heard from the server cannot invent an update.

The test asserts the default state has no version, no progress and no confirmation, then feeds one announcement and checks the version and release URL read back and the confirmation is the install variant. It then feeds `Downloading` progress and a *second* announcement, asserting the in-flight progress survives — the winit client keeps it because the server only re-announces a version it still considers installable.

#### Restart-required outranks a pending version

Verifies  raises the cold-restart modal ahead of an install offer when both apply, matching the winit `open_update_dialog` precedence.

With both an announced version and a `CompletedRestartRequired` progress state present, the resolved dialog is the restart-required kind, so the user is asked about the restart they already paid for rather than about downloading again. The test also asserts the dialog's cancel action is the secondary one, which is what makes Esc and a backdrop click safe: neither can cold-restart the machine.

#### Trigger and dismiss clear the CTA

Checks that both terminal decisions retire the CTA, so a confirmed or declined update cannot keep re-offering itself on every repaint.

 clears the version and release URL but is deliberately narrower than , which also drops progress: after a trigger the server's own `UpdateProgress` takes over the label, whereas after a dismissal the server suppresses the version entirely and even a stale `Failed` state must not linger.

### GPUI Window Lifecycle

Locks the decision table of  — when a close or quit may be sent, which acknowledgement ends the window, what a focus transition reports, and how a window list projects. See .

These cases cover the shared state machine only. That the running window actually emits `CloseWindow` / `QuitAll` / `ListWindows` / `FocusChanged` and acts on the server's answers is a whole-app property, verified by .

#### Close and quit wait for their acknowledgement

Confirms both shutdown requests are one-shot and that neither of them, on its own, ends the window — only the server's answer does.

"Kill Window" is inert until a `Welcome` names this connection's window, because the server refuses a `CloseWindow` that names any other one. Once claimed,  and  both refuse a second request, so a repeated Enter on the dialog cannot put a second frame on the wire.  stays empty until the matching acknowledgement arrives and yields the exit exactly once, and a `QuitRequested` caused by a *different* window still exits this one.

#### An unrelated close ack is ignored

Verifies  only obeys an acknowledgement naming the window this client asked to close.

A `WindowClosed` arriving with no pending close, or naming somebody else's window while ours is still pending, must leave the window running and the pending close intact — closing a live window on another window's ack is the failure this guard exists for, and it mirrors the winit client's "ignoring unexpected `WindowClosed` ack" branch.

#### Focus reports collapse window and session state

Checks that OS activation and pane selection collapse into a single reported value, so every kind of focus movement produces the one gained/lost pair the server expects.

A window that is active but showing no pane reports nothing; focusing a pane reports a gain; re-asking with the same pane reports nothing, which is what keeps the lifecycle tick from re-sending on every poll; a tab switch reports the gain and the loss together; and a blur reports the loss even though the tab did not change. A tab switch made *while* blurred stays silent, and the following re-focus reports the pane that is actually on screen rather than the one that was focused before the blur.

#### Window list projects remote controllers

Verifies  keeps only the windows a remote peer controls and reports whether the summary changed.

Locally-controlled windows carry no controller and contribute nothing, so an all-local reply leaves the status-bar summary empty and returns `false` — no repaint. A reply that adds a controller changes the summary; an identical follow-up reply does not, which is what stops a 2 s poll from repainting the bar forever.

### GPUI remote connect picker

Verifies the ported  picker state machine — the transport-free core of the winit  — so the multi-machine connect flow behaves identically over the frozen IPC protocol.

The suite drives  and  to assert the tailnet/LAN merge: a dual-reachable machine collapses to one LAN-preferred row with an "also Tailscale" hint, an incompatible-version LAN peer remains as one dimmed row, and online peers sort before offline. It then walks the step transitions through  — a manual `host:port` entry winning over the highlighted peer, a probe dialing over the row's transport, and the window step producing `Attach`/`NewWindow`  intents with feature-015 share occupancy. Finally it checks the typed failure copy for tailnet/LAN refusals, the awaiting-approval overlay swap, and the  key/click actions, all read back through the flattened .

### GPUI remote handshake

Exercises the ported dial preamble  over an in-memory `tokio::io::duplex` pair against a scripted fake server, proving the frozen `RemoteHandshake` / `RemoteHandshakeReply` exchange maps to the right .

The scripted server reads the client's first frame, asserts it is a well-formed  `RemoteHandshake` at the negotiated version, then replies: an accepted reply yields `Accepted`, a typed refusal propagates, a reason-less refusal and any non-reply frame and an EOF all merge into `ConnectionFailure`. Companion parser cases lock the  grammar (`host`, `host:port`, bad-port fallback, bare IPv6 literal) and the `SCRIBE_REMOTE_WINDOW` / takeover-flag parsing without mutating process env.

### GPUI lost control banner

Confirms the ported  — the transport-agnostic displaced-client state from the winit  — names the new controller and gates reclaim to Enter only.

The suite asserts  renders `Controlled by <device> (<account>)` and that reclaim fires on `Enter` while every other key stays suppressed, matching the FR-009b banner copy and one-action reclaim obligation.

### GPUI LAN device approval

Confirms the ported  state — the model half of the winit  — keeps the safe Decline-default focus and word-wraps the approval body.

The suite asserts Decline is the initial focus (so an unexpected prompt never silently grants trust), that focus cycles between the two buttons, and that  lists the requesting device, its trusted network, and its fingerprint words wrapped within the dialog width, adding the name-collision hint only when flagged.

### GPUI LAN chrome

Confirms  — the state the IPC reader folds every feature-014 answer into and the window renders from — hands an approval prompt to the foreground exactly once and derives the right status line.

The suite is the headless half of a hand-off that spans two threads, so the take-once rule is what it asserts hardest: a parked prompt is returned by the first  and never again, so a later tick cannot raise a duplicate modal for a `request_id` already being answered, and a second request arriving before the first is raised replaces it rather than stacking. It also asserts the derived line: nothing at all before the environment is probed (rather than a misleading "0 peers"), the dormancy note with the server's own reason when the current network cannot be fingerprinted, the online-only peer count otherwise, and that a non-idle  outranks all of it.

#### Approval hand-off is take-once

A parked prompt is returned once and then gone, so the foreground tick cannot raise the same `request_id` twice.

#### A second request replaces an unraised one

A second `LanApprovalRequest` arriving before the first was raised replaces it, keeping at most one modal; the displaced peer is still held by the server until its own approval timeout.

#### Status line reports peers and dormancy

An unprobed environment yields no line; an unfingerprintable network yields the server's dormancy reason; otherwise the line counts only currently-advertised peers.

#### Dial status outranks the environment

A client waiting on — or refused by — a peer reports that instead of the local peer count, and each typed  maps to its own copy.

### GPUI remote chrome

Confirms  — the state the IPC reader folds every feature-013 answer into and the window renders from — derives the right status line, freezes and reclaims exactly once, and bounds its automation queue.

The suite is the headless half of two cross-thread hand-offs. The displacement one is asserted hardest: a window is frozen until  returns `true` exactly once, so the key path can never put a second `ControlClaim` on the wire for a banner that is already gone, and it returns `false` when nothing was displaced so an Enter on a normal window is not mistaken for a reclaim. The automation one asserts FIFO order and the overflow rule: past the bound the OLDEST request is dropped, because the newest is the one the user just typed and a wedged window must not replay a minute of stale actions. It also asserts the derived line's precedence — nothing at all before the environment is probed, the passive "not detected" note when `tailscaled` was unreachable, the online-only peer count otherwise, and displacement outranking even a severed link.

#### Status line reports the tailnet account

An unprobed environment yields no line; a fail-closed reply yields the passive "not detected" note; otherwise the line names the signed-in account and counts only online peers.

#### Dial and severance outrank the environment

A client refused by — or connected to — a peer reports that instead of the local peer count, each typed  maps to its own copy, and a severed link outranks the dial that established it. Only an accepted dial lights the transport indicator.

#### Displacement freezes and reclaims once

A `WindowTakenOver` stores the banner headline and outranks every other status; the reclaim clears it exactly once and is a no-op on a window that was never displaced.

#### Automation queue is bounded and FIFO

Queued `RunAction`s drain in arrival order, and an overflow drops the oldest rather than the newest.

### GPUI LAN dial

Confirms  — the connecting side's `LanHello` preamble and approval gate — settles on the right  and fails closed on every malformed answer.

The TCP and mutual-TLS half needs a real peer and is covered by ; what is testable without one is the framed exchange, so these drive the preamble over an in-memory duplex. The waiting-state callback is asserted by count, because reporting it for an already-trusted device would show a "waiting for approval" state that never existed.

#### Trusted device is admitted without a pending frame

A peer that answers `LanApprovalResult { approved: true }` straight away is accepted and the waiting callback never fires.

#### Unknown device waits then settles

A peer that answers `LanApprovalPending` first reports the waiting state exactly once and then settles on the typed refusal that follows.

#### Malformed gate answers fail closed

A refusal with no reason, a frame that is not the gate at all, and a peer that hangs up before answering all collapse to `ConnectionFailure`, so no window data is ever trusted from a connection that skipped the gate.

### GPUI window sharing

Confirms the ported feature-015 sharing surfaces —  and the control overlays from the winit  — derive roster roles correctly and lower control passing onto the frozen v3 protocol.

The suite checks roster-derived multi/holder/label state and  formatting, the  expiry window, and that a viewer's take-control and a  answer lower through  to `ControlClaim` / `ControlRequest` / `ControlGrant`  messages.

The live-path aggregate  is covered below. Its user-visible half — that the running client actually renders and sends any of this — is proven separately by , not here.

#### Roster drives the presence surfaces

A multi-participant `ShareRoster` raises the status-bar presence badge and the roster rows (holder marked, local machine named); a roster that drains back to one participant tears the badge and the viewer affordances down again.

#### Viewer keystrokes claim control

A viewer's first keystroke is swallowed and raises the take-control hint; pressing Enter while that hint is up emits `ControlClaim`. Once the roster returns control to this machine, keys pass straight through to the terminal.

#### Prompt is modal until answered

While a `ControlRequested` prompt is pending every other key is swallowed, so no keystroke leaks to the PTY mid-decision; Enter emits `ControlGrant { accept: true }` and Esc emits `ControlGrant { accept: false }`, each clearing the prompt.

#### Denied and ended notices

`ControlDenied` leaves the share intact and only posts a transient notice, while `ShareEnded` clears the roster, any pending prompt, and the viewer state, leaving the reason notice behind.

#### Self id resolves the local seat

The `participant_id` carried by `Welcome` wins over the roster's `is_local` flag when resolving which seat is this connection, so a client seated as a non-local participant still reads its own holder state correctly.

### GPUI local share join

Locks the  hook that decides whether this client claims a window of its own or joins one another local process already holds — the client half of the shared-pane rig ().

A full UUID (with or without surrounding whitespace) parses to that `WindowId`; empty, blank, non-UUID, and the short `Display` label (`win-1234abcd`) all yield `None`, which leaves the stock `Hello { window_id: None }` handshake in place rather than failing the launch. The env read itself is not exercised — the workspace lints ban `set_var` — so the parser is called directly.

## GPUI Pane Dividers

Covers pure divider geometry in , live overlay wiring, and drag-resize math. `tests/e2e/visual/pane-workspace-layout.sh` drags the real divider and asserts both grids re-lay.

### Horizontal split divider is a centered vertical line

A side-by-side (`SplitDirection::Horizontal`) split produces one 1px-wide vertical divider centered on the boundary between the two child rects, carrying the first subtree's leaf as its `first_pane`.

### Vertical split divider is a centered horizontal line

A stacked (`SplitDirection::Vertical`) split produces one 1px-tall horizontal divider centered on the boundary, spanning the full width and honoring the split ratio.

### Nested splits yield one divider per split node

A tree with an outer split whose second child is itself a split emits exactly one divider per internal split node, so every resize boundary is hittable.

### Hit test honors 4px tolerance

 matches a mouse within the  4px band around a 1px line and misses beyond it, so thin dividers stay easy to grab.

### Drag maps position to clamped ratio

 captures the parent extent and origin, and  maps a drag position to a `[0.1, 0.9]`-clamped ratio so a resize can never collapse a pane.

### Drag on degenerate parent extent falls back to half

A drag whose captured parent extent is zero returns a neutral 0.5 ratio instead of dividing by zero, keeping the layout stable during a zero-area transient.

### Viewport insets clip vertical dividers below the tab bar

 clips a vertical divider below the tab bar and insets its top/bottom edges by the content padding when they touch the viewport boundary.

## GPUI Focus Borders

Covers the focus-border edge geometry in  — the four accent strips the GPUI paint path fills for a focused pane or workspace, kept pure so the corner-overlap math is verifiable without a window.

### Border edges frame the rect without corner overlap

`border_edges` returns full-width top/bottom strips and vertically inset left/right strips at the  2px width, so the four quads frame the rect without double-painting the corners.

### Border side strips clamp on tiny rects

On a rect shorter than twice the border width, the left/right strip heights clamp to zero instead of going negative, so a tiny pane never produces an inverted quad.

## GPUI Split-Scroll

Covers the split-scroll live-bottom logic in  — eligibility, pin sizing, cursor-anchored translation, logical-line alignment, and viewport geometry — the AI-pane pinned-prompt behavior ported renderer-independent from the winit client.

### Eligible only for scrolled AI panes on the normal screen

 activates only when the pin is enabled, the pane runs a supported AI provider, the view is scrolled up, and the pane is on the normal screen — never on the alternate screen, encoding the alt-screen exclusion.

### Pin rows fit the AI prompt block or clamp on tiny screens

 reserves the AI prompt block height when the screen has room and clamps to a `MIN_PIN_ROWS` floor and `screen - MIN_PIN_ROWS` ceiling on small screens so the top portion never vanishes.

### Cursor-anchored translation keeps the prompt visible

 shifts live cells so the cursor row lands on the last screen row, keeping an AI tool's prompt visible in the pin even when it draws in the upper half, and saturating to zero when the cursor is already at or past the bottom.

### Geometry stacks top divider and pinned bottom

 stacks a scrollback top portion, a 1px divider, and a pinned bottom of the requested height, docking the jump-to-bottom chip inside the top portion where  resolves it.

### Pin height clamps to the content rect

A pin height larger than the content rect collapses the top portion to zero rather than overflowing, so an oversized pin request stays inside the pane.

### Pin alignment absorbs soft-wrapped logical lines

 expands the pin upward across `WRAPLINE`-flagged rows so the split never starts mid-way through a soft-wrapped logical line, and leaves the requested rows unchanged when there is no wrap.

## GPUI Terminal Viewport

Unit tests for the live client's terminal viewport —  and the pointer mapping in  — proving the snapshot honours the display offset, the split-scroll pin, the vi cursor, and click-to-cell resolution.

These are the reachability tests for : the pure modules already had unit tests, so what is asserted here is that the *running* client's snapshot and pointer path actually consume them.

### Scrolling paints scrollback and returns to the live bottom

 moves the display offset and rebuilds the snapshot from it, so a paged-up viewport paints scrollback rows; scrolling past the oldest row reports no movement, and `Scroll::Bottom` restores the live tail.

### Prompt marks anchor and scroll in absolute rows

 reports the history size, screen height, and cursor cell a mark is anchored by,  names the viewport's top row in the same absolute space, and  lands on a given row, reporting no movement when it is already there.

### Scrollback trim drops rows and shifts marks

 really removes the display grid's oldest rows and reports the drop, and  applies it to the marks.

The surviving top row is the one after the cut rather than a renumbered original, anchors below the cut shift down, anchors inside it are retired, and a trim that keeps everything the grid already holds is a no-op.

### Split-scroll pins the live rows under the scrollback

With the eligibility gate open,  makes the snapshot's trailing rows read the live screen anchored on the shell cursor while the rows above stay at the scrolled offset; closing the gate restores one contiguous region.

### Vi mode publishes a cursor the paint path can draw

Toggling vi mode publishes a viewport-space cursor on the snapshot, a motion moves it a row, and leaving vi mode clears it — which is what makes the keyboard cursor visible to .

### Smart selection resolves through the scrolled viewport

 resolves a viewport cell against the display offset before matching, so a rule still matches text that has scrolled into history; blank space yields no actionable candidate.

### A parse in flight blocks neither the registry nor a paint

Holding one pane's stream lock — standing in for a batch mid-parse — leaves the registry free for another pane's batch and leaves both panes' published projections readable, including the busy pane's.

This is the regression guard for the parse being off the registry lock: if a paint-path read ever reached back through the stream instead of the projection, this test would deadlock rather than fail quietly.

### A moved grid area asks for a republish

 reports `true` for the first measurement and for every rect that moved or resized, and `false` for an idle repaint of the same rect.

That is the gate which turns a measured band into exactly one deferred republish — never none (the stale-render defect) and never one per frame (a `Resize` storm).

### Pointer positions lower onto grid cells

`cell_at` divides the pointer offset by the live cell metrics to name a row and column, and returns nothing outside the grid rect so a click on the titlebar or status bar can never resolve to row 0.

### The jump chip is only hit while the pin is up

 re-derives the paint pass's split geometry and matches only points inside the docked chip, and matches nothing at all when there is no pin, so an unsplit grid passes every click through.

## GPUI Mouse Reporting

Golden byte-capture and decision tests for  — the X10 / SGR-1006 encoders against the captured legacy fixture, and the pure gates the live pointer path consults around them.

The encoder half is the US1 correctness oracle described in . The decision half exists because the shell's event handlers hold no policy of their own: everything they branch on is one of the functions below, so a wrong branch is a failing unit test rather than a silent behaviour change nobody can see without a mouse.

### Wheel routing orders its three consumers

 gives the wheel to a mouse-tracking application first, on either screen buffer, and otherwise to the client's own scrollback.

Mode-1007 cursor keys are the fallback only on the alternate screen with alternate scroll on — the winit client's priority order exactly.

### Wheel deltas convert to signed rows

 passes GPUI's already-scaled `Lines` notch through as three rows and divides a trackpad's `Pixels` delta by the row height.

Sub-row travel rounds to zero rather than to a phantom row, a degenerate row height drops the event entirely, and the sign inverts only when `natural_scroll` is on.

### Alternate scroll sends one cursor key per row

 emits one CUU per row backwards and one CUD per row forwards, and nothing at all for a zero delta, so a pager under mode 1007 scrolls by the same amount the viewport would have.

### Shift takes the pointer back from a tracking application

 forwards a button event only while the application tracks the mouse and Shift is not held, so the universal text-selection override works inside vim and tmux and an untracking pane never forwards anything.

### Scroll direction follows the signed row delta

 maps a positive row delta to button 64 and a negative one to button 65, and the resulting SGR sequences match the golden fixture's, so a wired wheel is byte-identical to the winit client's.

## GPUI Command Scrollbar

Covers the bespoke command-mark scrollbar in  — thumb geometry, fade/hover-widen animation, click/drag scroll math, and command-status tick placement with trim-shift — the renderer-independent core the GPUI paint path lowers onto quads.

The running client's use of that core is asserted separately, in .

### No scrollback yields no thumb

 returns nothing and  never matches when the pane has zero scrollback rows, so an unscrolled pane shows no overlay.

### Thumb sizes and positions from the viewport

`compute_thumb` sizes the thumb from the visible-to-total row ratio (floored at ) and positions it down the track from the display offset, right-aligned inside the pane with the fixed inset.

### Track click maps to a scroll offset

 maps a click at the track top to the oldest scrollback and a click at the bottom to the live view, with mid-track clicks landing part-way up the history.

### Drag maps vertical delta to offset

 converts the vertical drag delta from the captured start into a new display offset — dragging down scrolls toward the live bottom and dragging up toward the top of history.

### Hit zone widens the right edge threefold

`hit_test_scrollbar` accepts points inside a `3x`-width band anchored to the pane's right edge (the  padding) and rejects points left of the band or above the track top.

### Thumb hit test tracks the thumb rect

 matches only points inside the computed thumb rectangle, so a point elsewhere on the track (for click-to-jump) is distinguished from a point on the thumb (for drag).

### Command ticks colour by status and shift with trim

`build_scrollbar_render` colours each tick by its  — theme green for success, red for failure, neutral for unknown — orders them by `abs_pos`, and re-places them after a trim shifts positions.

### Stale mark position clamps inside the track

A mark whose `abs_pos` is stale (larger than the post-resize history) clamps to the track bounds so it never renders outside the scrollbar, absorbing the transient between a resize and the next trim shift.

### Invisible scrollbar renders nothing

`build_scrollbar_render` returns `None` while the fade opacity is zero, so a rested scrollbar emits no thumb or ticks even with scrollback and marks present.

### Fade idles then fades over the configured windows

 holds full opacity through the 1.5s idle delay after a scroll, ramps opacity down across the 0.3s fade window, and settles to invisible past it.

### Hover holds opacity and widens the thumb

`on_hover_enter` pins full opacity and clears the fade timer; `build_scrollbar_render` retargets the width wider and `tick_fade_at` lerps the display width toward it, while `on_hover_leave` re-arms the fade and relaxes the target.

### Mark colours fall back without an ANSI palette

 reads the theme's ANSI green (index 2) and red (index 1) for the success/failure tick hues, so themed palettes drive the tick colours directly.

## GPUI Font Zoom

Covers the runtime font-zoom math in  — the in/out/reset point delta the GPUI shell applies over the configured font size, isolated so clamping and the size floor are verifiable without a window.

### Zoom steps clamp to the point range

Repeated  and  calls saturate at the `+7` / `-7` point bounds rather than overflowing the level.

### Reset returns to the configured size

 returns the level to zero so  yields the unmodified configured size.

### Effective size applies the delta and honors the floor

`effective_font_size` adds the zoom delta to the base size and floors the result at the 6pt minimum so extreme zoom-out still renders legible cells.

## GPUI Status Bar

Unit tests for , the ported window-status-bar segment model, proving every parity segment (connection, command/env glyphs, sparklines, labels, remote/share surfaces, update CTA) builds with the right text and colour without a live window.

### Connection dot reflects connection state

 paints the connection dot with the connected (ANSI green) colour when attached and the disconnected (ANSI red) colour otherwise.

### Command status glyphs distinguish outcomes

 maps Success to a check, Failure to a cross, and Unknown to a dimmed `?` that is never failure-styled, while an absent status renders no glyph.

### Env warning fires only when degraded

The feature-006 env-capture warning glyph is emitted only for `EnvStatusState::Degraded`; `Active` and absent states render nothing.

### Sparkline maps percentage to block height

 maps 0–100% onto the eight block glyphs, clamps non-finite input to the lowest bar, and the network variant saturates at 100 MB/s.

### Usage color escalates with load

 returns green below 60%, yellow from 60–85%, and red at or above 85%.

### Network rate formats to four columns

 renders byte rates right-aligned in exactly four columns across the B/K/M and `>1G` ranges.

### CWD shortens home to tilde

 replaces a `$HOME` prefix with `~` and leaves paths outside home untouched.

### Right side stitches enabled segments in order

 emits git branch, session count (singular/plural), tmux, host, and clock segments in order, each gated on its input being present.

### Remote control summary tallies windows per device

 deduplicates controllers by device in first-seen order and pluralises the per-device window count (FR-009b).

### Share presence badge names the control holder

The feature-015 presence badge reports the attached-participant count and names the current control holder, or states no one holds control when unheld.

### Centered update CTA reflects progress state

 resolves the centred CTA label and clickability from the update-available version and each `UpdateProgressState`, returning nothing when no update is pending.

### Sparklines pad short history to fixed width

 left-pads a short CPU/GPU history to the fixed eight-bar width and renders the CPU, MEM, GPU, and network groups when their config flags are on.

## GPUI Settings Window

Unit tests for the GPUI settings window that replaces the deleted `scribe-settings` GTK/wry app, proving the rebuilt surface stays 1:1 with the old page inventory and that a second launch hands off focus rather than opening a duplicate. See .

### Per-page parity checklist

Every page in  exposes controls, and every config-backed control routes cleanly through the ported  with the value the window reads for it, so no editable setting regresses versus `settings.html/js`.

### Keybinding coverage

The keybindings page lists every action the apply path routes under `keybindings.*` (the full 50+ set), and each action's current combos read back through  without panicking, so no shortcut silently disappears.

### Singleton focus handoff

 makes the first launch the primary; a second launch against the same paths sends a `focus` command with the anchor and returns `AlreadyRunning`.

The primary then accepts the handoff connection, verifies the peer UID, and reads back that exact focus command — proving the second launch focuses the running window instead of opening a duplicate.
