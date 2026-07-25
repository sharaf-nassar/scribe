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

### AI Indicator E2E

Two scripts covering the AI state indicator and its context-window percentage, both driving the [[server#Hook Channel]] rather than OSC 1337 and reading the result back through [[crates/scribe-test/src/capture.rs#ai_chrome]].

Transport and readback are the two things these scripts get right that a naive version cannot. AI state, prompt text, and context % travel over the hook channel — OSC 1337 parsing for them was removed by spec 003 FR-022 — so the scripts run `scribe-hook-helper` inside the session shell, where the server exports `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID`. Readback cannot use a screen snapshot: [[crates/scribe-test/src/capture.rs#snapshot]] returns the server's PTY grid, and the prompt bar and tab label are client chrome that never appears in it. `scribe-test ai-chrome` renders the session's live AI state through [[common#AI Context Chrome]] instead, emitting one `prompt-bar:` line whenever a percentage exists and one `tab:` line only from the warn band up.

Both scripts park the shell in `read` after firing hook events. A returning shell prompt (OSC 133;A) tells the server the AI tool exited, and [[crates/scribe-server/src/ipc_server.rs#send_metadata_event]] then synthesizes an `AiStateCleared` that would wipe the state the helper just set. Parking the shell reproduces production, where hooks fire while the AI tool owns the foreground; releasing the parked shell is how each phase resets to a clean slate.

#### AI Context Thresholds E2E

Seven-phase test in `tests/e2e/func/ai-context-thresholds.sh` validating prompt-bar and tab inline % across all threshold bands for Claude and Codex.

Claude phases set `processing` plus a prompt and a context refresh of 50/72/91. Phase 1 asserts `50%` renders on exactly one chrome surface and Phase 4 reads that as the tab inline being suppressed below `warn=70`; phases 2 and 3 assert the Warn/Danger values render on two surfaces (prompt bar + tab inline). Codex phases repeat the same provider-symmetric checks at 51/73/92.

#### AI State Indicator E2E

Seven-phase test in `tests/e2e/func/ai-state-indicator.sh` covering the state machine end of the same channel.

It cycles all five `AiState` variants without corrupting the grid, asserts a context refresh of 42 reaches the AI chrome, confirms a legacy OSC 1337 payload is still consumed silently with the text on either side of it preserved, drives rapid transitions without deadlock, asserts `state_cleared` empties the chrome, and closes a session while an AI state is active.

### Session Lifecycle E2E

Scripted lifecycle coverage proving the GPUI client survives detach, hot-reload, and full cold restart against a disposable test server — never the user's live server (the CLAUDE.md invariant), as the harness runs its own `scribe-test server`.

In every script the `scribe-test` daemon is the client stand-in: `daemon stop` is the client going away and `daemon start` is a fresh client that must re-attach. [[crates/scribe-test/src/daemon.rs#run]] sends `Hello { window_id: None }` so [[crates/scribe-server/src/ipc_server.rs#resolve_window_assignment]] adopts the unconnected window-with-sessions, which is what makes every re-attach flow below possible.

`tests/e2e/func/reconnect.sh` covers plain detach/reattach: run a command, start a background job, `daemon stop`, `daemon start`, `session attach`, then assert fresh input works and the background job survived the disconnect.

`tests/e2e/func/hot-reload.sh` covers server `--upgrade` under a live client: it snapshots a session, stops the daemon, runs [[crates/scribe-test/src/server.rs#upgrade]] (fd handoff to the new server), then reconnects and asserts the session, its background job, and its on-screen scrollback all survived the graceful handoff.

`tests/e2e/func/cold-restart.sh` covers cold-restart restore fan-out plus geometry-compat restore. It opens three sessions with distinct markers, resizes one to 132x50, starts a background job, then fully cold-quits the client (`daemon stop`) while the server keeps the sessions. A fresh `daemon start` must fan out and re-attach all three panes; the script asserts each pane replayed and accepts input, the resized pane still reports 132 cols, and the background job survived the restart.

### Failure-Path E2E

Scripted degraded-path coverage proving the client fails loudly (never hangs) when the server is unavailable or vanishes mid-session, and recovers cleanly once it returns. Both scripts drive the disposable test server only.

`tests/e2e/func/failure-server-down.sh` covers server-down-at-launch and adoption failure. With the server stopped, `daemon start` must return non-zero within its bounded socket wait rather than block, because [[crates/scribe-test/src/daemon.rs#run]] fails its initial `ipc::connect()` and the client socket never appears. It then recovers, and — on a fresh daemon with no cached `SessionCreated` — asserts that adopting a nonexistent session id errors (server denies, [[crates/scribe-test/src/daemon.rs#handle_attach_session]] times out) without crashing the still-usable client.

`tests/e2e/func/failure-socket-loss.sh` covers a mid-session server crash. A SIGTERM `server stop` drops the client's IPC with no upgrade handoff; the daemon's server-reader loop ends, so it tears down and removes its command socket. The script polls until commands fail (proving loss detection, not a hang), reconnects to a freshly started server, asserts the crashed session is gone (PTYs died with the server, so re-adopt fails — the deliberate contrast with hot-reload), and confirms a fresh session works end to end.

## Visual E2E Tests

Visual end-to-end tests run the real `scribe-client-gpui` window headlessly (`docker/Dockerfile.visual`) and assert against screenshots written to `/output`.

`docker/entrypoint-visual.sh` starts Xvfb, an `openbox` window manager, `scribe-server`, the daemon, and the GPUI client, then runs the test script. The image pins `VK_ICD_FILENAMES` to lavapipe's software Vulkan ICD (shipped in `mesa-vulkan-drivers`) so the client renders deterministically with no GPU, and sets `SCRIBE_DISABLE_ANIMATIONS=1` so consecutive frames are byte-identical. Tests drive the client through `xdotool`/`xclip` and capture frames with `scrot`. An optional `SCRIBE_EXTRA_CONFIG` env var seeds `config.toml` before the client starts so a test can exercise opt-in settings (e.g. `terminal.paste_confirmation`).

The client's stderr is redirected to `/output/client.log` and its pid and log path are exported as `SCRIBE_CLIENT_PID` / `SCRIBE_CLIENT_LOG`, so a script can assert on runtime behaviour that leaves no pixels behind and can prove the process never restarted. `RUST_LOG` defaults to `scribe_server=info,scribe_client_gpui=info` so those client lines are actually emitted.

The GPUI client sets its X11 `WM_NAME`/`_NET_WM_NAME` to `Scribe` via [[crates/scribe-client-gpui/src/main.rs#open_window]] so `xdotool search --name "Scribe"` can locate the window for focus and capture.

`openbox` is required, not cosmetic: the active-window guard (mirroring [[crates/scribe-client/src/x11_focus.rs#X11FocusGuard]]) suppresses synthetic key input whenever `_NET_ACTIVE_WINDOW` does not name the client window, and only a window manager sets that root property under Xvfb. Without a WM, `xdotool`-driven visual tests cannot type.

### Color emoji renders in color

`tests/e2e/visual/color-emoji.sh` proves color emoji render in color rather than as monochrome/tinted glyphs — the US3 headline parity item promoted to an automated visual check.

It prints a grid of solid color-block and pictographic emoji, screenshots the frame, and asserts via ImageMagick's HSL saturation channel that a strongly-saturated pixel count clears a floor. A monochrome/tinted fallback tints every glyph the pale foreground color, so its saturated-pixel count collapses to near zero.

`tests/e2e/visual/paste-confirmation.sh` verifies the spec-011 paste gate ([[client#Dialogs#Paste Confirmation Dialog]]): a single-line paste carrying control/escape bytes pops the confirmation with a caret-escaped preview (`^[`), while a plain single line and a tab-separated line paste straight through without a dialog.

### Config live reload

`tests/e2e/visual/config-reload.sh` is the scripted oracle for the `ConfigReloaded` parity row: it edits `config.toml` under an already-running client, the user-visible scenario the headless suites cannot reach.

The script screenshots the baseline window, rewrites the config with a new theme, font size, `line_padding`, opacity, and command-palette combo in one save, then asserts four things in order: the client logged a `config hot-reloaded` line it had not logged before (the watcher fired and [[client#Client#Config Watching#GPUI Config Port#Terminal Window Reload Wiring]] ran), the client pid is unchanged (a reload, not a restart), the captured frame is no longer pixel-identical (the new theme and font actually reached the paint path), and the newly bound `ctrl+shift+o` opens the command palette even though that combo did not exist when the window started.

Asserting on the log rather than on pixels alone is deliberate: the status bar's sparklines resample on a timer, so a screenshot diff on its own could pass without any reload having happened.

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

## GPUI Sync Frame Queue

Unit tests for the ported [[client#GPUI Client Spike#IPC Bridge#Sync Frame Queueing]] — [[crates/scribe-client-gpui/src/sync_frames.rs#SyncFrameQueue]] sitting in front of [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal#feed_output]] — proving `CSI ? 2026` commit boundaries survive IPC chunking and that expiry and catch-up match the winit client.

### Splits committed burst across IPC boundaries

A synchronized-update frame chunked across four IPC messages (BSU split mid-escape, body in two parts, ESU last) is reassembled by [[crates/scribe-client-gpui/src/sync_frames.rs#SyncFrameQueue#queue_output_frames]] into exactly one committed burst, so a single [[crates/scribe-client-gpui/src/sync_frames.rs#drain_all_committed]] hands the terminal the whole frame with its original markers intact.

### Preserves per-commit boundaries

A tail frame followed by two distinct sync commits drains as three separate frames, so each `CSI ? 2026` commit reaches `feed_output` as its own burst rather than being concatenated.

### Presents one burst per redraw when caught up

With a backlog below [[crates/scribe-client-gpui/src/sync_frames.rs#OUTPUT_FRAME_CATCH_UP_THRESHOLD]], [[crates/scribe-client-gpui/src/sync_frames.rs#drain_until_frame]] applies one committed burst then stops with [[crates/scribe-client-gpui/src/sync_frames.rs#QueueState]] `HasMore`, so light traffic animates incrementally one frame per redraw.

### Drains through backlog past threshold

Once the queue depth exceeds the catch-up threshold, a single `drain_until_frame` replays every backlogged burst to the latest frame and reports `Drained`, so stale frames never pile up under a firehose.

### Flushes raw sync update on expiry

An unterminated `CSI ? 2026 h` arms a 150 ms raw deadline via [[crates/scribe-client-gpui/src/sync_frames.rs#RAW_SYNC_TIMEOUT]]; [[crates/scribe-client-gpui/src/sync_frames.rs#SyncFrameQueue#flush_raw_timeout]] commits nothing before the deadline and, at it, appends the BSU-stripped bytes as a frame so the buffered output still reaches the terminal.

### Flushes parser sync update on expiry

A committed frame that opens but never closes a synchronized update arms the VTE parser's own timeout; [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal#flush_parser_sync_timeout]] commits the held bytes at the deadline and clears the parser timeout.

### Split sync frame reaches terminal whole

Driving a four-way-split synchronized frame through the queue into a real [[crates/scribe-client-gpui/src/terminal.rs#DisplayOnlyTerminal]] renders the committed content, proving the queue never advances the VTE processor with a torn frame.

## GPUI URL Detection

Unit tests for the GPUI client's ported [[client#GPUI Client Spike#GPUI URL Detection Port]] scanner — [[crates/scribe-client-gpui/src/url_detect.rs#PaneUrlCache]] over Zed's Alacritty fork — proving byte-for-byte parity with the winit detector across hard-break joins and OSC 8 handling.

### Explicit hyperlink segment geometry

[[crates/scribe-client-gpui/src/url_detect.rs#segments_from_cells]] collapses a multi-row OSC 8 run into exact per-row [[crates/scribe-client-gpui/src/url_detect.rs#RowSegment]]s, and `Osc8CellRange::contains` hit-tests a partial middle row by its own segment bounds rather than a bounding rectangle, so hover coverage stays exact.

## GPUI Terminal Selection

Unit tests for the ported [[client#GPUI Client Spike#GPUI Terminal Selection Port]] state — [[crates/scribe-client-gpui/src/selection.rs]] and its vi-mode wrapper — proving cell/word/line granularity, `WRAPLINE`-aware extraction, and copy-on-select over Zed's Alacritty fork.

### Cell selection extracts a substring

[[crates/scribe-client-gpui/src/selection.rs#extract_text]] over a single-row cell range returns exactly the covered characters.

### Reversed cell selection normalizes

A range whose start is after its end extracts the same text as the forward range, because [[crates/scribe-client-gpui/src/selection.rs#SelectionRange#normalized]] orders the endpoints first.

### Word bounds snap to word characters

[[crates/scribe-client-gpui/src/selection.rs#word_bounds_at]] expands a cursor inside a token to the full word, treating `_` and other identifier punctuation as word characters.

### Word bounds on a delimiter select one cell

A cursor resting on a whitespace delimiter yields a single-cell word range rather than swallowing an adjacent word.

### Line bounds span the full row

[[crates/scribe-client-gpui/src/selection.rs#line_bounds_at]] returns the first through last column of the logical line for a non-wrapped row.

### WRAPLINE joins a wrapped row without a newline

[[crates/scribe-client-gpui/src/selection.rs#extract_text]] joins a row that ends with the `WRAPLINE` flag to its continuation row without inserting a newline.

### Hard line break inserts a newline

A selection spanning two rows separated by a hard line break (no `WRAPLINE`) is extracted with a `\n` between them.

### Word bounds follow a wrapped line

[[crates/scribe-client-gpui/src/selection.rs#word_bounds_at]] crosses a `WRAPLINE` boundary so a word split across two screen rows selects as one token.

### Line bounds span a wrapped logical line

[[crates/scribe-client-gpui/src/selection.rs#line_bounds_at]] follows `WRAPLINE` flags to cover every screen row of a wrapped logical line.

### Contains-cell honors selection shape

[[crates/scribe-client-gpui/src/selection.rs#SelectionRange#contains_cell]] includes only the partial first/last rows and every full middle row of a multi-row selection.

### Selection state copies on select

[[crates/scribe-client-gpui/src/selection.rs#SelectionState#copy_text]] returns the selected text after a cell/word/line gesture and `None` for an empty selection.

### Word drag extends by whole words

[[crates/scribe-client-gpui/src/selection.rs#SelectionState#drag_to]] in word mode extends the range by whole words from the double-click anchor to the drag point.

### Pixel mapping resolves grid cells

[[crates/scribe-client-gpui/src/selection.rs#pixel_to_grid]] maps a pointer pixel inside the content area to the correct grid cell and rejects pixels above the content area.

### Vi mode toggles and moves the cursor

[[crates/scribe-client-gpui/src/vi_mode.rs#toggle_vi_mode]] enters copy mode, [[crates/scribe-client-gpui/src/vi_mode.rs#vi_motion]] moves the vi cursor, and motions are no-ops while vi mode is inactive.

## GPUI Animation Policy

Unit tests for [[client#GPUI Client Spike#GPUI Animation System]] — [[crates/scribe-client-gpui/src/animation.rs#AnimationSettings]] — proving the config/override motion policy resolves correctly, transitions clamp to the 150 ms budget, and the disabled path yields a zero duration for byte-identical screenshots.

### Config default enables motion

With `appearance.animations` true and no environment override, [[crates/scribe-client-gpui/src/animation.rs#AnimationSettings#resolve_with_env]] leaves motion enabled.

### Config false disables motion

Setting `appearance.animations` to false disables motion even without the environment override, so the config key alone acts as the reduce-motion user setting.

### Truthy env override forces motion off

A truthy `SCRIBE_DISABLE_ANIMATIONS` value (`1`, `true`, `yes`, `on`, case- and whitespace-insensitive) force-disables motion even when the config bool is true, the E2E determinism hook.

### Falsy env value leaves config in charge

A falsy, empty, or unparseable override value leaves the config bool in charge, so a stray `SCRIBE_DISABLE_ANIMATIONS=` never silently kills motion.

### Enabled duration clamps to 150 ms

[[crates/scribe-client-gpui/src/animation.rs#AnimationSettings#duration]] clamps an over-budget request to the 150 ms `MAX_TRANSITION` cap and passes a within-budget request through unchanged.

### Disabled duration is zero

When motion is disabled, `duration` returns `Duration::ZERO` and [[crates/scribe-client-gpui/src/animation.rs#AnimationSettings#transition]] builds a zero-length animation, so GPUI paints the end state on the first frame.

## GPUI Terminal Search

Unit tests for [[crates/scribe-client-gpui/src/search.rs#TerminalSearch]], the ported regex find-in-terminal state, proving whole-grid match collection and forward/backward cycling with wraparound.

### Cycles matches with wraparound

[[crates/scribe-client-gpui/src/search.rs#TerminalSearch#select_next]] and `select_prev` advance the highlighted match in reading order and wrap at both ends of the match list.

### Match endpoints cover the whole hit

A collected [[crates/scribe-client-gpui/src/search.rs#SearchMatch]] reports inclusive start and end cells that span the entire matched run.

### Empty and unmatched queries stay valid

An empty query, a valid regex with no matches, and an invalid regex are handled without panicking — the first two yield an empty search and the last returns `None`.

## GPUI Smart Selection

Unit tests for [[crates/scribe-client-gpui/src/smart_selection.rs#CompiledSmartSelection]], the ported iTerm2-style regex matcher, proving precision ranking, capture-parameter expansion, and rule-compilation errors.

### Highest-precision rule wins

[[crates/scribe-client-gpui/src/smart_selection.rs#CompiledSmartSelection#candidate_at]] returns the highest-precision rule's match when several rules overlap the cursor.

### Legacy capture parameters expand

[[crates/scribe-client-gpui/src/smart_selection.rs#SmartSelectionCandidate#resolved_actions]] expands a legacy `\0` parameter to the full matched text and labels the action by rule and kind.

### Invalid regex reports an error

A rule whose regex fails to compile is recorded in [[crates/scribe-client-gpui/src/smart_selection.rs#CompiledSmartSelection]]'s `errors` rather than aborting compilation.

## Drag-drop path insertion

Unit tests for [[crates/scribe-client-gpui/src/drag_drop.rs#quote_path_for_shell]], the ported shell-aware quoting for dropped file paths, proving each shell's escaping and the trailing-space insertion payload match the legacy client byte-for-byte.

### POSIX quoting escapes single quotes

[[crates/scribe-client-gpui/src/drag_drop.rs#quote_posix_string]] wraps the path in single quotes and rewrites embedded quotes as `'"'"'`, leaving quote-free paths simply single-quoted.

### Fish quoting escapes backslash and quote

[[crates/scribe-client-gpui/src/drag_drop.rs#quote_fish_string]] escapes backslash and single-quote with a backslash inside the single-quoted string, matching fish's quoting rules.

### PowerShell quoting doubles single quotes

[[crates/scribe-client-gpui/src/drag_drop.rs#quote_powershell_string]] doubles each single quote inside the single-quoted string, the only escape PowerShell needs.

### Nushell raw-string fencing

[[crates/scribe-client-gpui/src/drag_drop.rs#quote_nushell_string]] uses a plain single-quoted string when no quote is present and otherwise emits a raw string, widening the `#` fence until it no longer collides with the path.

### Shell dispatch selects quoter

[[crates/scribe-client-gpui/src/drag_drop.rs#quote_path_for_shell]] routes to the fish, PowerShell, or nushell quoter by shell name and falls back to POSIX quoting for anything else.

### Insertion appends trailing space

[[crates/scribe-client-gpui/src/drag_drop.rs#dropped_path_insertion]] appends a single trailing space to the quoted path so the shell treats it as a complete, separated argument.

## Window geometry compat

Unit tests for [[crates/scribe-client-gpui/src/window_state.rs#normalize_legacy_geometry]], the first-launch geometry-compat normalization proving old-client window geometry restores correctly inset under the new custom titlebar. This is the scripted assertion required by the lifecycle acceptance criteria.

### Legacy geometry gains titlebar inset

An unnormalized legacy geometry grows in height by [[crates/scribe-client-gpui/src/window_state.rs#CUSTOM_TITLEBAR_HEIGHT]] so the terminal area below the in-window titlebar keeps its old size, while position and monitor survive unchanged.

### Normalization is idempotent

Running [[crates/scribe-client-gpui/src/window_state.rs#normalize_legacy_geometry]] a second time on already-normalized geometry returns it unchanged, so a save-and-reload never insets twice.

### Maximized geometry keeps its size

A maximized legacy geometry keeps its stored size (the compositor overrides it on restore) but is still marked normalized.

### Out-of-range legacy size is clamped

A hostile or corrupt oversized geometry is clamped into the accepted range so the restored window stays usable, satisfying [[crates/scribe-client-gpui/src/window_state.rs#geometry_size_is_sane]].

### Default geometry is already normalized

A freshly-created [[crates/scribe-client-gpui/src/window_state.rs#WindowGeometry]] is already in the new coordinate system, so normalization is a no-op on it.

### Legacy TOML lacks the normalized flag

A `state.toml` written by the old client has no `titlebar_normalized` key; it deserializes to `false` (via `serde(default)`) and therefore triggers the one-time normalization.

### Sanity range rejects extremes

[[crates/scribe-client-gpui/src/window_state.rs#geometry_size_is_sane]] rejects zero, too-small, and too-large edges and accepts the range boundaries.

## X11 focus guard

Unit tests for [[crates/scribe-client-gpui/src/x11_focus.rs#ReactivationDebounce]], the pure reactivation state machine backing the ported X11 focus guard, proving the suppression semantics that the visual E2E exercises against the live `_NET_ACTIVE_WINDOW`.

### Inactive window suppresses input

[[crates/scribe-client-gpui/src/x11_focus.rs#ReactivationDebounce#observe]] suppresses keyboard input whenever our window is not the active window (a compositor overlay is up).

### Reactivation debounce suppresses stray keys

After an inactive→active transition, `observe` keeps suppressing for [[crates/scribe-client-gpui/src/x11_focus.rs#REACTIVATION_DEBOUNCE]] so a stray keystroke that arrives as the overlay closes is caught, then resumes passing input once the window elapses.

### Steady active window allows input

A window that has been continuously active is never suppressed by `observe`.

### Genuine focus event clears debounce

[[crates/scribe-client-gpui/src/x11_focus.rs#ReactivationDebounce#clear]] drops the debounce on a real focus event (which overlays never send), so input flows immediately after a genuine refocus.

### Poll transition arms debounce

[[crates/scribe-client-gpui/src/x11_focus.rs#ReactivationDebounce#note_active]] arms the debounce when the periodic poll observes the inactive→active transition, so a key seen just afterward is still suppressed.

## Server lifecycle

Unit tests for [[crates/scribe-client-gpui/src/server_lifecycle.rs#stale_server_reason]], the pure staleness decision behind the ported local-server refresh path, proving path drift and rebuild detection without a live socket.

### Path drift marks server stale

A running server whose executable path differs from the installed binary is reported stale so the caller refreshes it.

### Newer installed binary marks server stale

A running server that started before the installed binary's modification time is reported stale (an in-place rebuild landed).

### Matching fresh server is not stale

A running server at the same path that started after the installed binary's modification time is not stale.

### Unknown timestamps are not stale

When neither the process start time nor the installed modification time is known, the server is treated as fresh rather than force-refreshed.

## GPUI OSC 52 Clipboard Bridge

Unit coverage for the ported host clipboard bridge ([[client#GPUI Client Spike#GPUI Platform Integrations Port#GPUI Clipboard and OSC 52 Bridge]]): OSC 52 routing, the FR-019 focus gate, primary-selection read/write with AI cleanup, and reply-message construction.

An in-memory `FakeClipboard` stands in for the live arboard handle so the read+write roundtrip runs without a display server; the arboard-backed E2E stays a manual / launch-gate parity item.

### Write-read roundtrip on the system clipboard

A payload written through [[crates/scribe-client-gpui/src/clipboard.rs#bridge_write]] to the system clipboard reads back verbatim through [[crates/scribe-client-gpui/src/clipboard.rs#bridge_read]] — the scripted OSC 52 bridge roundtrip at the unit level.

### Primary and system selections stay independent

Writes to `ClipboardSelection::Primary` and `ClipboardSelection::Clipboard` land in separate buffers and each reads back its own value, proving the per-selection routing.

### Unavailable backend reports a bridge error

Both [[crates/scribe-client-gpui/src/clipboard.rs#bridge_read]] and [[crates/scribe-client-gpui/src/clipboard.rs#bridge_write]] collapse a dead backend onto `BridgeError::Unavailable` so the server maps it to an empty OSC 52 reply.

### Focus gate drops only enabled unfocused writes

[[crates/scribe-client-gpui/src/clipboard.rs#FocusGate#drops_write]] returns true only when `focus_gate_writes` is enabled and the window is unfocused, and false for the other three combinations.

### Gated write is a silent no-op

A gated write on an unfocused window returns `Ok(())` without mutating the clipboard, while the same write on a focused window goes through — the FR-019 anti-hijack behavior.

### Read reply wraps the payload

[[crates/scribe-client-gpui/src/clipboard.rs#read_reply]] performs the host read and wraps the value in `ClientMessage::ClipboardBridgeReadReply` under the originating `request_id`.

### Read reply forwards a bridge error

When the backend is unavailable, [[crates/scribe-client-gpui/src/clipboard.rs#read_reply]] still emits a `ClipboardBridgeReadReply` carrying the `Err(BridgeError)` payload rather than dropping the request.

### Prompt response echoes id and decision

[[crates/scribe-client-gpui/src/clipboard.rs#prompt_response]] builds `ClientMessage::ClipboardPromptResponse` echoing the prompt's `request_id` and the user's decision.

### Primary read skips empty content

[[crates/scribe-client-gpui/src/clipboard.rs#read_primary]] returns `None` for an absent or empty primary selection so a middle-click paste is skipped, and `Some(text)` when content is present.

### Primary write applies cleanup

[[crates/scribe-client-gpui/src/clipboard.rs#set_primary]] runs the AI copy-cleanup transforms (dedent, unwrap) before writing to the primary selection when cleanup is enabled.

### Primary write is verbatim when cleanup off

[[crates/scribe-client-gpui/src/clipboard.rs#set_primary]] skips empty input entirely and writes the raw text unchanged when cleanup is disabled.

## GPUI Notification Dispatcher

Unit coverage for the platform-independent notification dispatcher logic ([[client#GPUI Client Spike#GPUI Platform Integrations Port#GPUI Notification Dispatcher]]): the `replaces_id` coalescing state machine and the freedesktop `expire_timeout` mapping.

The zbus transport and click-to-focus wiring are verified by the manual parity checklist.

### Timeout mode maps to expire_timeout

[[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#expire_timeout_millis]] maps `SystemDefault` to `-1`, `Never` to `0`, and `Custom` to `timeout_secs * 1000` (saturating on overflow).

### Same session reuses replaces_id

Repeated shows for one session reuse the live notification id via [[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifState#replaces_for]] and [[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifState#record_shown]], keeping exactly one live toast.

### Expired toast reallocation drops stale mapping

When the daemon allocates a fresh id despite a non-zero `replaces` (the prior toast expired), `record_shown` drops the stale reverse mapping so a later click cannot mis-route.

### Session close removes both mappings

[[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifState#take_session]] returns and clears the session's id once, then `None` thereafter, leaving no dangling id.

### Daemon closed signal clears mappings

[[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifState#on_closed]] drops both mappings for a closed notification id and no-ops on an unknown id.

### Shutdown closes every live toast

[[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifState#live_ids]] enumerates every live id for the shutdown close-all and [[crates/scribe-client-gpui/src/notification_dispatcher/mod.rs#NotifState#clear]] empties the state afterward.

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

## GPUI Client Headless Suites

The `#[gpui::test]` and golden suites in `scribe-client-gpui` are the primary correctness oracle for client-internal logic. They need no display server and every landed suite maps to a `parity-inventory.md` verification row.

These suites run under `just test` (and the `Dockerfile.func` image's Rust toolchain). This section consolidates the headless coverage the `gpui-client-rebuild` epic (`scribe-38e`) accumulates: each row of the map below ties a suite to the parity-inventory verification row it satisfies, and the coverage frontier lists the parity rows whose suites unblock as their feature beads land.

| Headless suite | Spec section | Parity-inventory row(s) |
| --- | --- | --- |
| Pane tree entity ops | [[client#GPUI Client Spike#GPUI Layout Entities#Pane Tree Model]] | Input/keybinding "Pane layout" (gpui-test) |
| Pane split-tree logic | [[test#GPUI Client Headless Suites#Pane split-tree logic]] | Input/keybinding "Pane layout" (gpui-test) |
| Workspace tree entity ops | [[client#GPUI Client Spike#GPUI Layout Entities#Workspace Tree Model]] | `CreateWorkspace`, `MoveSession`, `ReportWorkspaceTree` (gpui-test) |
| Input byte encoder golden | [[client#Input#GPUI Input Encoder Port]] | `KeyInput`, Terminal shortcuts (golden) |
| Keybindings dispatch | [[test#GPUI Client Headless Suites#GPUI keybindings dispatch]] | Pane/Workspace/Tab/Navigation/View keybinding actions (gpui-test) |
| Config load with removed keys | [[test#GPUI Client Headless Suites#Config load with removed keys]] | "Removed configuration keys" rows (gpui-test) |
| Config live reload | [[test#GPUI Client Headless Suites#Config live reload]] | `ConfigReloaded` live reload (scripted-E2E) |
| URL/OSC8 detection | [[test#GPUI URL Detection]] | hover/dwell/open surface (gpui-test) |
| IPC bridge ordering | [[test#GPUI IPC Bridge]] | Executor-model ordering risk (gpui-test) |
| Remote connect picker | [[test#GPUI Client Headless Suites#GPUI remote connect picker]] | `ListRemotePeers`, `ListLanPeers`, `RemotePeerList` remote connect picker (gpui-test) |
| Remote handshake | [[test#GPUI Client Headless Suites#GPUI remote handshake]] | `RemoteHandshake` preamble + dial-env spawn (gpui-test) |
| Lost control banner | [[test#GPUI Client Headless Suites#GPUI lost control banner]] | `WindowTakenOver` displaced-client reclaim (gpui-test) |
| LAN device approval | [[test#GPUI Client Headless Suites#GPUI LAN device approval]] | `LanApprovalRequest`/`LanApprovalDecision` prompt (gpui-test) |
| Window sharing | [[test#GPUI Client Headless Suites#GPUI window sharing]] | `ShareRoster`, `ControlClaim`/`ControlRequest`/`ControlGrant` (gpui-test) |
| Pane dividers | [[test#GPUI Pane Dividers]] | "Pane divider drag-resize" chrome (gpui-test) |
| Focus borders | [[test#GPUI Focus Borders]] | "Focused pane/workspace border" chrome (manual + gpui-test) |
| Split-scroll | [[test#GPUI Split-Scroll]] | "Split-scroll live-bottom pin" AI-pane chrome (gpui-test) |
| Font zoom | [[test#GPUI Font Zoom]] | "Zoom in/out/reset" View keybinding actions (gpui-test) |
| OSC 52 clipboard bridge | [[test#GPUI OSC 52 Clipboard Bridge]] | `ClipboardPromptResponse`, `ClipboardBridgeReadReply`, `ClipboardBridgeWrite`, `ClipboardBridgeReadRequest` OSC 52 bridge (scripted-E2E) |
| Notification dispatcher | [[test#GPUI Notification Dispatcher]] | Notification `replaces_id` coalescing + click-to-focus (gpui-test + manual) |

### Coverage frontier

Testing-Strategy suites not yet consolidated are blocked on their feature beads landing in `scribe-38e`. Each is tracked here against its parity row so the launch-gate bead (`scribe-38e.42`) can confirm the headless oracle is complete before cutover.

Pending headless suites and the parity rows they will satisfy:

- Selection model (cell/word/line, WRAPLINE) — terminal selection surface.
- Sync-frame queueing + 150 ms expiry — CSI-2026 burst preservation.
- Replay application — `SessionReplay` reconnect restore.
- Reconnect topology rebuild — `WorkspaceInfo` layout restore beyond the existing [[client#GPUI Client Spike#GPUI Layout Entities#Workspace Tree Model]] `from_tree` path.
- Degraded/failure paths — server-down at launch, socket vanish mid-session, adoption failure, replay decode failure (pane error state, no crash), reconnect retry/timeout.

### Pane split-tree logic

The pure [[crates/scribe-client-gpui/src/layout.rs#LayoutTree]] split-tree drives the "Pane layout" keybinding actions (`close_pane`, `cycle_pane`, `focus_left`/`right`/`up`/`down`) beneath the [[client#GPUI Client Spike#GPUI Layout Entities#Pane Tree Model]] entity wrapper, so its navigation and mutation logic is asserted directly without a GPUI context.

Over a 2x2 pane grid the suite exercises the surface the entity tests do not reach directly: [[crates/scribe-client-gpui/src/layout.rs#LayoutTree#find_pane_in_direction]] resolves a direct neighbor on all four axes and wraps to the opposite edge along the same column when none exists; [[crates/scribe-client-gpui/src/layout.rs#LayoutTree#next_pane]] cycles panes in depth-first order and wraps past the last leaf; [[crates/scribe-client-gpui/src/layout.rs#LayoutTree#swap_panes]] exchanges two leaf positions; and [[crates/scribe-client-gpui/src/layout.rs#LayoutTree#close_pane]] promotes a closed pane's sibling while refusing to remove the sole remaining leaf.

### GPUI keybindings dispatch

Verifies the ported [[crates/scribe-client-gpui/src/keybindings.rs#Bindings]] parser and [[crates/scribe-client-gpui/src/keybindings.rs#translate_key_action]] dispatch so no configured shortcut regresses across the GPUI cutover.

Driving each action from its default binding, the suite asserts every one of the 50+ [[crates/scribe-client-gpui/src/keybindings.rs#LayoutAction]] variants resolves to its named value, that command-palette/settings/find produce the right [[crates/scribe-client-gpui/src/keybindings.rs#KeyAction]], and that the seven terminal shortcuts emit their fixed escape sequences. It also checks combo parsing (`cmd`/`super` → platform modifier, named keys, rejected garbage), exact-modifier matching that ignores the GPUI function flag and is case-insensitive on the base character, key-down-only gating (press and repeat match, release does not), and that invalid combos are skipped without aborting the parse.

### Config load with removed keys

Confirms a config carrying every removed appearance key deserializes without error and leaves the GPUI-consumed surface intact, satisfying the parity inventory's "Removed configuration keys" rows.

The test parses the removed-keys TOML into [[crates/scribe-common/src/config.rs#ScribeConfig]], asserts the live appearance fields (font, font size, theme) parsed correctly, then resolves the full [[crates/scribe-client-gpui/src/config.rs#ClientConfig]] snapshot and checks the theme, derived chrome colors, and parsed bindings all populate — proving the removed keys are inert and never reach the paint path.

### Config live reload

A scripted reload confirms that edits to theme, font, and keybindings reapply live without a restart, backing the `ConfigReloaded` parity row.

Building a [[crates/scribe-client-gpui/src/config.rs#ClientConfig]] from an initial config and calling [[crates/scribe-client-gpui/src/config.rs#ClientConfig#reload]] with an edited config, the test asserts the returned [[crates/scribe-client-gpui/src/config.rs#ConfigReloadPlan]] flags the theme and font as changed, the resolved theme/chrome and font metrics actually updated, and the re-parsed [[crates/scribe-client-gpui/src/keybindings.rs#Bindings]] reflect the new combo. Companion cases assert an opacity-only edit is scoped to `opacity_changed` and an identical config reports no change.

Those cases prove the plan is computed correctly, but not that a running window ever asks for one. The child cases below cover the runtime path that closes that gap — watcher signal, foreground poll, painted font, and the outbound `ConfigReloaded` — and [[test#Visual E2E Tests#Config live reload]] drives the whole chain against a real window.

#### Watcher signal collapses a burst

Confirms [[crates/scribe-client-gpui/src/config.rs#ConfigChangeSignal]] turns the several `notify` events one editor save emits into exactly one reload, and reports nothing when the file is untouched.

The test polls a fresh signal (no reload due), fires three `signal()` calls to stand in for the delete/create/modify sequence a save-by-rename produces, and asserts a single [[crates/scribe-client-gpui/src/config.rs#ConfigChangeSignal#take_change]] consumes all three and the next poll is clean. This is the property that lets the foreground poll on a timer instead of waking per filesystem event without either missing a save or reloading three times per save.

#### Runtime applies a watcher-signalled edit

Drives the full foreground path — signal, poll, reload, read back — over [[crates/scribe-client-gpui/src/config.rs#ConfigRuntime]], proving a window only reloads when the watcher fires and that every live surface swaps in one step.

Using [[crates/scribe-client-gpui/src/config.rs#ConfigRuntime#detached]] so no real config directory is touched, the test asserts an unsignalled poll does not reload, then signals and applies an edit changing theme, font, opacity, and the command-palette combo at once. It checks the plan flags all three surfaces, the resolved theme and font actually updated, [[crates/scribe-client-gpui/src/config.rs#ConfigRuntime#opacity]] carries the new value to the opacity hook, the re-parsed [[crates/scribe-client-gpui/src/config.rs#ConfigRuntime#bindings]] expose the new palette key, and the consumed signal leaves no second reload queued.

#### Grid font tracks the live appearance config

Verifies [[crates/scribe-client-gpui/src/terminal_element.rs#GridFont]] derives the grid's painted metrics from `[appearance]` so a font edit changes pixels rather than only the stored config.

The test builds metrics from an edited appearance block and asserts the family, size, `line_padding`-inclusive row height, and the cell advance reported to the server all follow the config. A companion assertion drives `font_size = 0` and checks the value is clamped to the floor, so a bad edit degrades to a small grid instead of collapsing the window to nothing.

#### Reload announces ConfigReloaded

Backs the `ConfigReloaded` parity row at the protocol boundary: the reload path must put the message on the wire, ordered ahead of whatever the user types next.

Driving [[crates/scribe-client-gpui/src/ipc_bridge.rs#IpcSink#config_reloaded]] followed by a `KeyInput` on the same ordered writer channel, the test asserts a `ClientMessage::ConfigReloaded` is dequeued first. Ordering is the point: the server must have re-read the config before it interprets the next keystroke, otherwise a policy edit applies a keypress late.

### GPUI remote connect picker

Verifies the ported [[crates/scribe-client-gpui/src/remote.rs#RemoteConnect]] picker state machine — the transport-free core of the winit [[crates/scribe-client/src/remote_connect.rs#RemoteConnect]] — so the multi-machine connect flow behaves identically over the frozen IPC protocol.

The suite drives [[crates/scribe-client-gpui/src/remote.rs#RemoteConnect#set_peers]] and [[crates/scribe-client-gpui/src/remote.rs#RemoteConnect#set_lan_peers]] to assert the tailnet/LAN merge: a dual-reachable machine collapses to one LAN-preferred row with an "also Tailscale" hint, an incompatible-version LAN peer is dropped, and online peers sort before offline. It then walks the step transitions through [[crates/scribe-client-gpui/src/remote.rs#RemoteConnect#handle_key]] — a manual `host:port` entry winning over the highlighted peer, a probe dialing over the row's transport, and the window step producing `Attach`/`NewWindow` [[crates/scribe-client-gpui/src/remote.rs#RemoteConnectAction]] intents with feature-015 share occupancy. Finally it checks the typed failure copy for tailnet/LAN refusals, the awaiting-approval overlay swap, and the [[crates/scribe-client-gpui/src/remote.rs#ReconnectOverlay]] key/click actions, all read back through the flattened [[crates/scribe-client-gpui/src/remote.rs#PickerView]].

### GPUI remote handshake

Exercises the ported dial preamble [[crates/scribe-client-gpui/src/remote_handshake.rs#perform_remote_handshake]] over an in-memory `tokio::io::duplex` pair against a scripted fake server, proving the frozen `RemoteHandshake` / `RemoteHandshakeReply` exchange maps to the right [[crates/scribe-client-gpui/src/remote.rs#RemoteConnectOutcome]].

The scripted server reads the client's first frame, asserts it is a well-formed [[crates/scribe-common/src/protocol.rs#ClientMessage]] `RemoteHandshake` at the negotiated version, then replies: an accepted reply yields `Accepted`, a typed refusal propagates, a reason-less refusal and any non-reply frame and an EOF all merge into `ConnectionFailure`. Companion parser cases lock the [[crates/scribe-client-gpui/src/remote_handshake.rs#parse_dial_target]] grammar (`host`, `host:port`, bad-port fallback, bare IPv6 literal) and the `SCRIBE_REMOTE_WINDOW` / takeover-flag parsing without mutating process env.

### GPUI lost control banner

Confirms the ported [[crates/scribe-client-gpui/src/lost_control.rs#LostControlState]] — the transport-agnostic displaced-client state from the winit [[crates/scribe-client/src/lost_control.rs#LostControlState]] — names the new controller and gates reclaim to Enter only.

The suite asserts [[crates/scribe-client-gpui/src/lost_control.rs#LostControlState#headline]] renders `Controlled by <device> (<account>)` and that reclaim fires on `Enter` while every other key stays suppressed, matching the FR-009b banner copy and one-action reclaim obligation.

### GPUI LAN device approval

Confirms the ported [[crates/scribe-client-gpui/src/lan_approval.rs#LanApprovalDialog]] state — the model half of the winit [[crates/scribe-client/src/lan_approval.rs#LanApprovalDialog]] — keeps the safe Decline-default focus and word-wraps the approval body.

The suite asserts Decline is the initial focus (so an unexpected prompt never silently grants trust), that focus cycles between the two buttons, and that [[crates/scribe-client-gpui/src/lan_approval.rs#LanApprovalDialog#body_lines]] lists the requesting device, its trusted network, and its fingerprint words wrapped within the dialog width, adding the name-collision hint only when flagged.

### GPUI window sharing

Confirms the ported feature-015 sharing surfaces — [[crates/scribe-client-gpui/src/share.rs#ShareState]] and the control overlays from the winit [[crates/scribe-client/src/share_view.rs#ShareState]] — derive roster roles correctly and lower control passing onto the frozen v3 protocol.

The suite checks roster-derived multi/holder/label state and [[crates/scribe-client-gpui/src/share.rs#participant_label]] formatting, the [[crates/scribe-client-gpui/src/share.rs#ControlHint]] expiry window, and that a viewer's take-control and a [[crates/scribe-client-gpui/src/share.rs#ControlRequestPrompt]] answer lower through [[crates/scribe-client-gpui/src/share.rs#ControlIntent]] to `ControlClaim` / `ControlRequest` / `ControlGrant` [[crates/scribe-common/src/protocol.rs#ClientMessage]] messages.

## GPUI Pane Dividers

Covers the pure divider geometry in [[crates/scribe-client-gpui/src/divider.rs#collect_dividers]] and its drag-resize math — the renderer-independent core the GPUI solid-quad overlay consumes so pane resize handles behave identically to the winit client.

### Horizontal split divider is a centered vertical line

A side-by-side (`SplitDirection::Horizontal`) split produces one 1px-wide vertical divider centered on the boundary between the two child rects, carrying the first subtree's leaf as its `first_pane`.

### Vertical split divider is a centered horizontal line

A stacked (`SplitDirection::Vertical`) split produces one 1px-tall horizontal divider centered on the boundary, spanning the full width and honoring the split ratio.

### Nested splits yield one divider per split node

A tree with an outer split whose second child is itself a split emits exactly one divider per internal split node, so every resize boundary is hittable.

### Hit test honors 4px tolerance

[[crates/scribe-client-gpui/src/divider.rs#hit_test_divider]] matches a mouse within the [[crates/scribe-client-gpui/src/divider.rs#HIT_TOLERANCE]] 4px band around a 1px line and misses beyond it, so thin dividers stay easy to grab.

### Drag maps position to clamped ratio

[[crates/scribe-client-gpui/src/divider.rs#start_drag]] captures the parent extent and origin, and [[crates/scribe-client-gpui/src/divider.rs#drag_ratio]] maps a drag position to a `[0.1, 0.9]`-clamped ratio so a resize can never collapse a pane.

### Drag on degenerate parent extent falls back to half

A drag whose captured parent extent is zero returns a neutral 0.5 ratio instead of dividing by zero, keeping the layout stable during a zero-area transient.

### Viewport insets clip vertical dividers below the tab bar

[[crates/scribe-client-gpui/src/divider.rs#apply_viewport_insets]] clips a vertical divider below the tab bar and insets its top/bottom edges by the content padding when they touch the viewport boundary.

## GPUI Focus Borders

Covers the focus-border edge geometry in [[crates/scribe-client-gpui/src/focus_border.rs#border_edges]] — the four accent strips the GPUI paint path fills for a focused pane or workspace, kept pure so the corner-overlap math is verifiable without a window.

### Border edges frame the rect without corner overlap

`border_edges` returns full-width top/bottom strips and vertically inset left/right strips at the [[crates/scribe-client-gpui/src/focus_border.rs#FOCUS_BORDER_WIDTH]] 2px width, so the four quads frame the rect without double-painting the corners.

### Border side strips clamp on tiny rects

On a rect shorter than twice the border width, the left/right strip heights clamp to zero instead of going negative, so a tiny pane never produces an inverted quad.

## GPUI Split-Scroll

Covers the split-scroll live-bottom logic in [[crates/scribe-client-gpui/src/split_scroll.rs#split_scroll_eligible]] — eligibility, pin sizing, cursor-anchored translation, logical-line alignment, and viewport geometry — the AI-pane pinned-prompt behavior ported renderer-independent from the winit client.

### Eligible only for scrolled AI panes on the normal screen

[[crates/scribe-client-gpui/src/split_scroll.rs#split_scroll_eligible]] activates only when the pin is enabled, the pane runs a supported AI provider, the view is scrolled up, and the pane is on the normal screen — never on the alternate screen, encoding the alt-screen exclusion.

### Pin rows fit the AI prompt block or clamp on tiny screens

[[crates/scribe-client-gpui/src/split_scroll.rs#compute_pin_rows]] reserves the AI prompt block height when the screen has room and clamps to a `MIN_PIN_ROWS` floor and `screen - MIN_PIN_ROWS` ceiling on small screens so the top portion never vanishes.

### Cursor-anchored translation keeps the prompt visible

[[crates/scribe-client-gpui/src/split_scroll.rs#live_cell_y_translation]] shifts live cells so the cursor row lands on the last screen row, keeping an AI tool's prompt visible in the pin even when it draws in the upper half, and saturating to zero when the cursor is already at or past the bottom.

### Geometry stacks top divider and pinned bottom

[[crates/scribe-client-gpui/src/split_scroll.rs#compute_geometry]] stacks a scrollback top portion, a 1px divider, and a pinned bottom of the requested height, docking the jump-to-bottom chip inside the top portion where [[crates/scribe-client-gpui/src/split_scroll.rs#hit_test_jump_btn]] resolves it.

### Pin height clamps to the content rect

A pin height larger than the content rect collapses the top portion to zero rather than overflowing, so an oversized pin request stays inside the pane.

### Pin alignment absorbs soft-wrapped logical lines

[[crates/scribe-client-gpui/src/split_scroll.rs#align_pin_rows_to_logical_lines]] expands the pin upward across `WRAPLINE`-flagged rows so the split never starts mid-way through a soft-wrapped logical line, and leaves the requested rows unchanged when there is no wrap.

## GPUI Command Scrollbar

Covers the bespoke command-mark scrollbar in [[crates/scribe-client-gpui/src/scrollbar.rs#build_scrollbar_render]] — thumb geometry, fade/hover-widen animation, click/drag scroll math, and command-status tick placement with trim-shift — the renderer-independent core the GPUI paint path lowers onto quads.

### No scrollback yields no thumb

[[crates/scribe-client-gpui/src/scrollbar.rs#compute_thumb]] returns nothing and [[crates/scribe-client-gpui/src/scrollbar.rs#hit_test_scrollbar]] never matches when the pane has zero scrollback rows, so an unscrolled pane shows no overlay.

### Thumb sizes and positions from the viewport

`compute_thumb` sizes the thumb from the visible-to-total row ratio (floored at [[crates/scribe-client-gpui/src/scrollbar.rs#MIN_THUMB_HEIGHT]]) and positions it down the track from the display offset, right-aligned inside the pane with the fixed inset.

### Track click maps to a scroll offset

[[crates/scribe-client-gpui/src/scrollbar.rs#offset_from_track_click]] maps a click at the track top to the oldest scrollback and a click at the bottom to the live view, with mid-track clicks landing part-way up the history.

### Drag maps vertical delta to offset

[[crates/scribe-client-gpui/src/scrollbar.rs#offset_from_drag]] converts the vertical drag delta from the captured start into a new display offset — dragging down scrolls toward the live bottom and dragging up toward the top of history.

### Hit zone widens the right edge threefold

`hit_test_scrollbar` accepts points inside a `3x`-width band anchored to the pane's right edge (the [[crates/scribe-client-gpui/src/scrollbar.rs#HIT_ZONE_MULTIPLIER]] padding) and rejects points left of the band or above the track top.

### Thumb hit test tracks the thumb rect

[[crates/scribe-client-gpui/src/scrollbar.rs#hit_test_thumb]] matches only points inside the computed thumb rectangle, so a point elsewhere on the track (for click-to-jump) is distinguished from a point on the thumb (for drag).

### Command ticks colour by status and shift with trim

`build_scrollbar_render` colours each tick by its [[crates/scribe-client-gpui/src/scrollbar.rs#CommandStatus]] — theme green for success, red for failure, neutral for unknown — orders them by `abs_pos`, and re-places them after a trim shifts positions.

### Stale mark position clamps inside the track

A mark whose `abs_pos` is stale (larger than the post-resize history) clamps to the track bounds so it never renders outside the scrollbar, absorbing the transient between a resize and the next trim shift.

### Invisible scrollbar renders nothing

`build_scrollbar_render` returns `None` while the fade opacity is zero, so a rested scrollbar emits no thumb or ticks even with scrollback and marks present.

### Fade idles then fades over the configured windows

[[crates/scribe-client-gpui/src/scrollbar.rs#ScrollbarState#tick_fade_at]] holds full opacity through the 1.5s idle delay after a scroll, ramps opacity down across the 0.3s fade window, and settles to invisible past it.

### Hover holds opacity and widens the thumb

`on_hover_enter` pins full opacity and clears the fade timer; `build_scrollbar_render` retargets the width wider and `tick_fade_at` lerps the display width toward it, while `on_hover_leave` re-arms the fade and relaxes the target.

### Mark colours fall back without an ANSI palette

[[crates/scribe-client-gpui/src/scrollbar.rs#CommandMarkColors#from_ansi]] reads the theme's ANSI green (index 2) and red (index 1) for the success/failure tick hues, so themed palettes drive the tick colours directly.

## GPUI Font Zoom

Covers the runtime font-zoom math in [[crates/scribe-client-gpui/src/zoom.rs#ZoomState]] — the in/out/reset point delta the GPUI shell applies over the configured font size, isolated so clamping and the size floor are verifiable without a window.

### Zoom steps clamp to the point range

Repeated [[crates/scribe-client-gpui/src/zoom.rs#ZoomState#zoom_in]] and [[crates/scribe-client-gpui/src/zoom.rs#ZoomState#zoom_out]] calls saturate at the `+7` / `-7` point bounds rather than overflowing the level.

### Reset returns to the configured size

[[crates/scribe-client-gpui/src/zoom.rs#ZoomState#reset]] returns the level to zero so [[crates/scribe-client-gpui/src/zoom.rs#ZoomState#effective_font_size]] yields the unmodified configured size.

### Effective size applies the delta and honors the floor

`effective_font_size` adds the zoom delta to the base size and floors the result at the 6pt minimum so extreme zoom-out still renders legible cells.

## GPUI Status Bar

Unit tests for [[crates/scribe-client-gpui/src/status_bar.rs#build_model]], the ported window-status-bar segment model, proving every parity segment (connection, command/env glyphs, sparklines, labels, remote/share surfaces, update CTA) builds with the right text and colour without a live window.

### Connection dot reflects connection state

[[crates/scribe-client-gpui/src/status_bar.rs#build_left]] paints the connection dot with the connected (ANSI green) colour when attached and the disconnected (ANSI red) colour otherwise.

### Command status glyphs distinguish outcomes

[[crates/scribe-client-gpui/src/status_bar.rs#CommandStatus]] maps Success to a check, Failure to a cross, and Unknown to a dimmed `?` that is never failure-styled, while an absent status renders no glyph.

### Env warning fires only when degraded

The feature-006 env-capture warning glyph is emitted only for `EnvStatusState::Degraded`; `Active` and absent states render nothing.

### Sparkline maps percentage to block height

[[crates/scribe-client-gpui/src/status_bar.rs#sparkline_char]] maps 0–100% onto the eight block glyphs, clamps non-finite input to the lowest bar, and the network variant saturates at 100 MB/s.

### Usage color escalates with load

[[crates/scribe-client-gpui/src/status_bar.rs#usage_color]] returns green below 60%, yellow from 60–85%, and red at or above 85%.

### Network rate formats to four columns

[[crates/scribe-client-gpui/src/status_bar.rs#format_bytes_rate_fixed]] renders byte rates right-aligned in exactly four columns across the B/K/M and `>1G` ranges.

### CWD shortens home to tilde

[[crates/scribe-client-gpui/src/status_bar.rs#shorten_cwd]] replaces a `$HOME` prefix with `~` and leaves paths outside home untouched.

### Right side stitches enabled segments in order

[[crates/scribe-client-gpui/src/status_bar.rs#build_right]] emits git branch, session count (singular/plural), tmux, host, and clock segments in order, each gated on its input being present.

### Remote control summary tallies windows per device

[[crates/scribe-client-gpui/src/status_bar.rs#build_remote_control_summary]] deduplicates controllers by device in first-seen order and pluralises the per-device window count (FR-009b).

### Share presence badge names the control holder

The feature-015 presence badge reports the attached-participant count and names the current control holder, or states no one holds control when unheld.

### Centered update CTA reflects progress state

[[crates/scribe-client-gpui/src/status_bar.rs#build_center]] resolves the centred CTA label and clickability from the update-available version and each `UpdateProgressState`, returning nothing when no update is pending.

### Sparklines pad short history to fixed width

[[crates/scribe-client-gpui/src/status_bar.rs#build_right]] left-pads a short CPU/GPU history to the fixed eight-bar width and renders the CPU, MEM, GPU, and network groups when their config flags are on.

## GPUI Settings Window

Unit tests for the GPUI settings window that replaces the deleted `scribe-settings` GTK/wry app, proving the rebuilt surface stays 1:1 with the old page inventory and that a second launch hands off focus rather than opening a duplicate. See [[settings#GPUI Settings Window]].

### Per-page parity checklist

Every page in [[crates/scribe-client-gpui/src/settings/model.rs#page_controls]] exposes controls, and every config-backed control routes cleanly through the ported [[crates/scribe-client-gpui/src/settings/apply.rs#apply_config_key]] with the value the window reads for it, so no editable setting regresses versus `settings.html/js`.

### Keybinding coverage

The keybindings page lists every action the apply path routes under `keybindings.*` (the full 50+ set), and each action's current combos read back through [[crates/scribe-client-gpui/src/settings/values.rs#keybinding_combos]] without panicking, so no shortcut silently disappears.

### Singleton focus handoff

[[crates/scribe-client-gpui/src/settings/singleton.rs#acquire_at]] makes the first launch the primary; a second launch against the same paths sends a `focus` command with the anchor and returns `AlreadyRunning`.

The primary then accepts the handoff connection, verifies the peer UID, and reads back that exact focus command — proving the second launch focuses the running window instead of opening a duplicate.
