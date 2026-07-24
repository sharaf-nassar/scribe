# Test Harness

Integration test framework for Scribe with PTY capture, IPC helpers, assertion utilities, and screenshot rendering.

## Architecture

CLI binary (`scribe-test`) dispatches subcommands to a long-lived daemon that holds an open IPC connection to scribe-server and buffers per-session state.

The two-process model keeps the server connection alive across many short-lived CLI invocations. The CLI process sends a single [[crates/scribe-test/src/cmd_socket.rs#DaemonRequest]] over a Unix socket, the daemon executes it against live session state, and returns a [[crates/scribe-test/src/cmd_socket.rs#DaemonResponse]]. The CLI exits immediately after receiving the response.

### Error Model

Two exit codes distinguish failure kinds. [[crates/scribe-test/src/main.rs#TestError]] has two variants: `TestFailure` (exit 1) for assertion mismatches, and `InfraError` (exit 2) for socket, spawn, or timeout problems.

## Daemon

Long-lived process that maintains an open IPC connection to scribe-server, buffers per-session output and screen state, and serves CLI requests over a Unix socket.

The daemon is started with `scribe-test daemon start` (spawns itself as `daemon run`) and stopped with `scribe-test daemon stop` (sends a `Shutdown` request). The [[crates/scribe-test/src/daemon.rs#run]] function owns the main event loop, running a server-reader task and a command-listener task concurrently.

After connecting to scribe-server, the daemon sends `ClientMessage::Hello { window_id: None }` as its first message. The server then runs [[crates/scribe-server/src/ipc_server.rs#resolve_window_assignment]] which adopts any unconnected window-with-sessions instead of allocating a fresh `WindowId`. Without this, a `daemon stop` / `daemon start` cycle would leave the new daemon owning a brand-new window while the prior daemon's sessions remain bound to the prior `WindowId`, and the server would deny any subsequent `AttachSessions` request as cross-window. The reconnect e2e test exercises exactly this flow.

### Session State

Per-session data buffered in [[crates/scribe-test/src/daemon.rs#SessionState]]: 65 KB output ring buffer, `latest_snapshot` with 100 ms TTL, `last_output_at` for idle detection, `cwd`, `title`, and `SessionStatus` (`Running` or `Exited`).

All sessions are keyed by `SessionId` inside [[crates/scribe-test/src/daemon.rs#DaemonState]], which also tracks `last_workspace_id` and `last_session_created` for workspace and session-create responses.

### Request Handling

Each incoming connection receives one [[crates/scribe-test/src/cmd_socket.rs#DaemonRequest]] and returns one [[crates/scribe-test/src/cmd_socket.rs#DaemonResponse]]. Wait-type requests (WaitOutput, WaitCwd, WaitIdle, AssertExit) block on `Arc<Notify>` channels until the condition is met or the timeout fires.

### Notification System

[[crates/scribe-test/src/daemon.rs#WaitNotifiers]] holds five `Arc<Notify>` channels: `output`, `cwd`, `exit`, `workspace_info`, and `session_created`.

The server-reader task fires the matching channel on each incoming `ServerMessage`, waking whichever wait handler is blocked on it.

## Command Protocol

Request/response protocol between the CLI and daemon over a Unix socket at `/run/user/{uid}/scribe/test-daemon.sock` using msgpack framing from `scribe_common::framing`.

The socket path is returned by [[crates/scribe-test/src/cmd_socket.rs#daemon_socket_path]]. The helper [[crates/scribe-test/src/cmd_socket.rs#send_request]] creates a short-lived tokio runtime, connects, sends one [[crates/scribe-test/src/cmd_socket.rs#DaemonRequest]], and receives one [[crates/scribe-test/src/cmd_socket.rs#DaemonResponse]].

Key request variants: `CreateSession`, `AttachSession`, `CloseSession`, `Send`, `Resize`, `RequestScreenshot`, `RequestSnapshot`, `WaitOutput`, `WaitCwd`, `WaitIdle`, `AssertCell`, `AssertCursor`, `AssertExit`, `AssertSnapshotMatch`, and `Shutdown`.

Key response variants: `Ok`, `SessionCreated { session_id }`, `ScreenshotData { snapshot }`, `AssertFailed { message }`, and `Error { message }`.

## Session Management

Create, attach, and close terminal sessions through the daemon; each operation prints the confirmed session UUID to stdout for use in subsequent commands.

[[crates/scribe-test/src/session.rs#create]] sends `CreateSession` and prints the UUID. [[crates/scribe-test/src/session.rs#attach]] sends `AttachSession` and prints the confirmed UUID. [[crates/scribe-test/src/session.rs#close]] sends `CloseSession` and expects `Ok`. All three are routed through [[crates/scribe-test/src/cmd_socket.rs#send_request]].

## Input Simulation

Send keystrokes to a session with escape sequence expansion (`\n`, `\t`, `\\`, `\xNN`).

[[crates/scribe-test/src/input.rs#parse_escapes]] converts the string argument to raw bytes before forwarding via a `Send` request. [[crates/scribe-test/src/input.rs#send]] validates the session ID, calls `parse_escapes`, and sends the byte payload. [[crates/scribe-test/src/input.rs#resize]] sends a `Resize` request to change terminal dimensions.

## Wait Primitives

Blocking synchronization helpers: wait for regex output, CWD change, or terminal silence — each with a configurable timeout in milliseconds.

[[crates/scribe-test/src/wait.rs#wait_output]] sends `WaitOutput { pattern, timeout_ms }` and blocks until the daemon's regex matches the session's *visible* content: the output ring buffer is normalised before matching by stripping ANSI/OSC/CSI escape sequences and lone CRs, and the regex is built with multi-line mode enabled, so `^X$` anchors match line boundaries of what the user would see on the terminal grid rather than positions within the raw `\r\n`/escape-laden PTY stream. [[crates/scribe-test/src/wait.rs#wait_cwd]] sends `WaitCwd { path, timeout_ms }` and blocks until the session's CWD matches. [[crates/scribe-test/src/wait.rs#wait_idle]] sends `WaitIdle { quiet_ms, timeout_ms }` and blocks until no output has arrived for `quiet_ms` milliseconds.

## Assertions

Verify screen cell content, cursor position, snapshot equality, and process exit code — returning `TestFailure` (exit 1) on mismatch.

[[crates/scribe-test/src/assert.rs#assert_cell]] checks that a specific cell contains the expected character; on failure the daemon includes a 3×3 neighborhood context in the error message. [[crates/scribe-test/src/assert.rs#assert_cursor]] verifies the cursor is at the expected row/col. [[crates/scribe-test/src/assert.rs#assert_snapshot_match]] loads a reference JSON snapshot and compares cell content, cursor position, and cursor visibility. [[crates/scribe-test/src/assert.rs#assert_exit]] waits up to `timeout_ms` for the session to exit with the expected code.

## Screen Capture

Capture the current terminal state as a PNG screenshot or a JSON text snapshot for later comparison.

[[crates/scribe-test/src/capture.rs#screenshot]] requests a `ScreenshotData` response from the daemon and writes the snapshot to a PNG file via [[crates/scribe-test/src/render.rs#render_to_png]]. [[crates/scribe-test/src/capture.rs#snapshot]] requests the same data but serializes the `ScreenSnapshot` to pretty-printed JSON.

### PNG Rendering

[[crates/scribe-test/src/render.rs#render_to_png]] uses `cosmic-text` for shaping, xterm-256 ANSI palette for colours, and alpha blending for compositing. Cells are 10×20 px at 14 pt. [[crates/scribe-test/src/render.rs#RenderError]] covers I/O and PNG encoding failures.

## Server Lifecycle

Start, stop, and hot-reload the scribe-server process from tests using PID-file tracking and socket polling.

[[crates/scribe-test/src/server.rs#start]] spawns `scribe-server`, writes its PID to `/run/user/{uid}/scribe/scribe-server.pid`, then polls until the server socket appears (5 s timeout). [[crates/scribe-test/src/server.rs#stop]] reads the PID file, sends SIGTERM, waits up to 3 s, escalates to SIGKILL if needed, and removes the PID file. [[crates/scribe-test/src/server.rs#upgrade]] launches `scribe-server --upgrade`, waits for the old process to exit (10 s timeout), polls for the new socket, and updates the PID file.

## IPC Client

Thin async wrapper around the `scribe_common::framing` layer for sending `ClientMessage` and receiving `ServerMessage` over the server's Unix socket.

[[crates/scribe-test/src/ipc.rs#connect]] opens a `UnixStream` to the server socket path. [[crates/scribe-test/src/ipc.rs#send]] encodes and writes a `ClientMessage` over the write half. [[crates/scribe-test/src/ipc.rs#recv]] reads and decodes a `ServerMessage` from the read half. The daemon's `run` function uses these to maintain its persistent server connection. See [[protocol]] for message types.

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

`tests/install/postinst-regressions.sh` sources only the variable and function definitions from `dist/debian/postinst` (truncating before the `SERVER_RUNTIME_GENERATION` invocation so the case-statement and trailing `exit 0` cannot terminate the harness) and exercises individual functions against fixtures. A python launcher `os.fork()`s a child that exits immediately and then sleeps to keep the orphan in zombie (`Z`) state, since bash auto-reaps its own backgrounded children. The harness currently asserts that `wait_for_pid_exit`, `stop_client_processes`, and `restart_singleton_binary` all treat a zombie PID as exited rather than blocking the post-upgrade relaunch.

## E2E Functional Tests

Functional end-to-end tests that drive real sessions through the `scribe-test` harness and assert rendered output.

The `docker/Dockerfile.func` image bundles the workspace's `dist/shell-integration` tree at `/usr/local/share/scribe/shell-integration` so the in-container `scribe-server`'s [[crates/scribe-server/src/shell_integration.rs#find_scripts_dir]] resolves them and injects `SCRIBE_SHELL_INTEGRATION=1` plus the per-shell rcfile/ZDOTDIR/XDG plumbing into every spawned PTY. Without this copy, the `shell-integration.sh` e2e test never sees the env var or the OSC marks the integration scripts emit.

### AI Context Thresholds E2E

Seven-phase test in `tests/e2e/func/ai-context-thresholds.sh` validating prompt-bar and tab inline % across all threshold bands for Claude and Codex.

Claude phases emit `ClaudeState=processing;context=50/72/91` plus matching `ClaudePrompt=...` OSC payloads so the prompt bar is visible. Phase 1 asserts `50%` appears once in the prompt-bar cluster and Phase 4 confirms the tab inline is suppressed below `warn=70`; phases 2 and 3 assert Warn/Danger values appear at least twice (prompt bar + tab inline). Codex phases repeat the same provider-symmetric checks with `CodexState`/`CodexPrompt` at 51/73/92.

## Visual E2E Tests

Visual end-to-end tests run the real `scribe-client-gpui` window headlessly (`docker/Dockerfile.visual`) and assert against screenshots written to `/output`.

`docker/entrypoint-visual.sh` starts Xvfb, an `openbox` window manager, `scribe-server`, the daemon, and the GPUI client, then runs the test script. The image pins `VK_ICD_FILENAMES` to lavapipe's software Vulkan ICD (shipped in `mesa-vulkan-drivers`) so the client renders deterministically with no GPU, and sets `SCRIBE_DISABLE_ANIMATIONS=1` so consecutive frames are byte-identical. Tests drive the client through `xdotool`/`xclip` and capture frames with `scrot`. An optional `SCRIBE_EXTRA_CONFIG` env var seeds `config.toml` before the client starts so a test can exercise opt-in settings (e.g. `terminal.paste_confirmation`).

The GPUI client sets its X11 `WM_NAME`/`_NET_WM_NAME` to `Scribe` via [[crates/scribe-client-gpui/src/main.rs#open_window]] so `xdotool search --name "Scribe"` can locate the window for focus and capture.

`openbox` is required, not cosmetic: the active-window guard (mirroring [[crates/scribe-client/src/x11_focus.rs#X11FocusGuard]]) suppresses synthetic key input whenever `_NET_ACTIVE_WINDOW` does not name the client window, and only a window manager sets that root property under Xvfb. Without a WM, `xdotool`-driven visual tests cannot type.

### Color emoji renders in color

`tests/e2e/visual/color-emoji.sh` proves color emoji render in color rather than as monochrome/tinted glyphs — the US3 headline parity item promoted to an automated visual check.

It prints a grid of solid color-block and pictographic emoji, screenshots the frame, and asserts via ImageMagick's HSL saturation channel that a strongly-saturated pixel count clears a floor. A monochrome/tinted fallback tints every glyph the pale foreground color, so its saturated-pixel count collapses to near zero.

`tests/e2e/visual/paste-confirmation.sh` verifies the spec-011 paste gate ([[client#Dialogs#Paste Confirmation Dialog]]): a single-line paste carrying control/escape bytes pops the confirmation with a caret-escaped preview (`^[`), while a plain single line and a tab-separated line paste straight through without a dialog.

## GPUI IPC Bridge

Unit tests for the GPUI client's [[client#GPUI Client Spike#IPC Bridge]] — the inbound coalescing drain and the outbound [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink]] — proving keystroke-before-output ordering and Zed-style 4 ms / 100-event coalescing over the frozen IPC protocol.

### Coalesce collapses per pane

[[crates/scribe-client-gpui/src/ipc_bridge.rs#coalesce]] folds an interleaved two-pane run into one buffer per pane, preserving first-seen pane order and byte order within each pane; an empty run yields an empty batch.

### Drain coalesces firehose

[[crates/scribe-client-gpui/src/ipc_bridge.rs#run_drain]] batches a 300-event two-pane firehose into at most one `write_output` per pane per 100-event batch, so the total write count stays bounded while every pane's byte stream is reconstructed in exact order.

### Keystroke before output

A keystroke enqueued on the [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink]] reaches the outbound channel promptly even with a 10 000-event backlog churning through the inbound drain, because the outbound path never traverses the drain.

### Typing under firehose

Typing a full command while flooding the inbound drain between keystrokes preserves keystroke order on the wire with no per-key latency spike, the scripted no-reorder / no-stall check the launch gate requires.

### Resize before key input

A `Resize` enqueued on the sink before a `KeyInput` is delivered first, since the IPC-writer channel is a single ordered FIFO.

### Sink reports closed writer

`IpcSink::key_input` returns [[crates/scribe-client-gpui/src/ipc_bridge.rs#SinkClosed]] rather than panicking when the writer task has dropped its receiver.

## GPUI URL Detection

Unit tests for the GPUI client's ported [[client#GPUI Client Spike#GPUI URL Detection Port]] scanner — [[crates/scribe-client-gpui/src/url_detect.rs#PaneUrlCache]] over Zed's Alacritty fork — proving byte-for-byte parity with the winit detector across hard-break joins and OSC 8 handling.

### Explicit hyperlink segment geometry

[[crates/scribe-client-gpui/src/url_detect.rs#segments_from_cells]] collapses a multi-row OSC 8 run into exact per-row [[crates/scribe-client-gpui/src/url_detect.rs#RowSegment]]s, and `Osc8CellRange::contains` hit-tests a partial middle row by its own segment bounds rather than a bounding rectangle, so hover coverage stays exact.

## GPUI Perf A/B Gate

The launch-blocking performance comparison for the GPUI client rebuild. The `tools/perf-ab-rig/run-perf-ab.sh` rig compares the new client against the recorded old-client baselines and writes a per-metric pass/fail report.

The five metrics and thresholds are: startup-to-first-frame (`<= 500 ms` absolute, also gating splash deletion), input latency (no worse than old client), cat-firehose throughput (no worse than old client), memory at 10 tabs (`<= old + 20%`), and scroll (sustained 60 fps with `< 1%` dropped frames). Old-client baselines live in `specs/016-gpui-client-rebuild/perf-baseline.md`; the generated report is `specs/016-gpui-client-rebuild/perf-ab-report.md`.

The rig has two modes. `assess` (default) generates the current-state report from the committed baseline plus a static capability check without launching any GUI or touching the live server. `--live` is the launch-gate mode: it launches the target client on the same machine/session, drives each workload, and enforces the thresholds; it attaches to the already-running server and never restarts it.

### Startup instrumentation

The GPUI client times startup-to-first-frame only when the `SCRIBE_GPUI_STARTUP_TIMING` env var is set, printing a machine-readable marker the rig parses.

[[crates/scribe-client-gpui/src/main.rs#log_first_frame_timing]] latches on the first painted frame and writes a `first_frame_ms=<n>` marker to the file the env var names, timed from the `PROCESS_START` origin captured at the top of `main`, mirroring the old client's `init_gpu_and_terminal_done` method that produced the recorded baseline.

### Deferred runtime metrics

While the client remains a display-only scaffold spike, input latency, firehose throughput, memory at 10 tabs, and scroll fps are reported `DEFERRED` rather than measured.

The spike has no stable input encoder with echo instrumentation, no multi-tab support, and no scroll frame counter, so those workloads cannot be driven yet. The rig records the exact live method for each so the launch gate (`scribe-38e.42`) can re-run it at cutover and enforce every threshold; a `FAIL` reopens the perf-rig bead.
