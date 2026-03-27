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

[[crates/scribe-test/src/wait.rs#wait_output]] sends `WaitOutput { pattern, timeout_ms }` and blocks until the daemon matches the regex against the output ring buffer. [[crates/scribe-test/src/wait.rs#wait_cwd]] sends `WaitCwd { path, timeout_ms }` and blocks until the session's CWD matches. [[crates/scribe-test/src/wait.rs#wait_idle]] sends `WaitIdle { quiet_ms, timeout_ms }` and blocks until no output has arrived for `quiet_ms` milliseconds.

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

## Installer Script Regression Harness

Offline shell harness for Debian `preinst` and `postinst` behavior so packaging regressions can be caught without touching the live user session.

`tests/install/postinst-regressions.sh` copies the installer scripts into a temp directory, rewrites `/run/user/...` paths into that temp runtime tree, and injects fake `systemctl`, `sudo`, `kill`, `pkill`, `pgrep`, and `sha256sum` binaries through `PATH`. The harness currently covers state-dir ownership in `preinst`, `daemon-reload` on successful hot-reload, truthful reporting when fallback server restart fails, and force-killing lingering singleton settings processes before relaunch.
