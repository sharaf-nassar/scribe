# E2E Testing Containers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build two Docker containers (functional + visual) for E2E testing Scribe, driven by a new `scribe-test` crate that provides a daemon + CLI architecture for bash test scripts.

**Architecture:** A `scribe-test` daemon holds a persistent IPC connection to `scribe-server`, buffering events. CLI subcommands talk to the daemon via a local Unix socket. The functional container uses CPU-rendered PNG screenshots; the visual container adds Xvfb + Mesa for pixel-level GPU client screenshots.

**Tech Stack:** Rust (workspace crate), tokio, clap, cosmic-text, png, Docker (debian:bookworm-slim)

**Spec:** `docs/superpowers/specs/2026-03-22-e2e-testing-containers-design.md`

**Lint policy:** This workspace denies `unsafe_code`, `unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `print_stdout`, `print_stderr`, `pedantic`, and more. Every `#[allow]` requires `reason = "..."`. Max function lines: 80, cognitive complexity: 15, nesting: 4, params: 5. See `Cargo.toml` workspace lints and `clippy.toml`.

**Testing policy:** Do NOT write unit tests unless explicitly requested. The E2E smoke test at the end IS the test.

**Deferred:** Detached session mode (`CreateSession { detached: bool }`, `AttachSession`) from the spec is deferred to a future plan. The daemon's persistent IPC connection solves the session-ownership problem for all test scenarios. Detached mode is independently useful for the server but not required for E2E testing.

**Exit code strategy:** `std::process::exit()` is disallowed by `clippy.toml`. Use `fn main() -> ExitCode` with a custom error type that distinguishes test failure (exit 1) from infrastructure error (exit 2). See Task 3 for the error type definition.

**Parallelization:** Tasks 1, 2, and 3 have no dependencies on each other and can be worked in parallel.

---

### Task 1: Add `RequestSnapshot` and `CreateWorkspace` handlers

**Files:**
- Modify: `crates/scribe-common/src/protocol.rs`
- Modify: `crates/scribe-server/src/ipc_server.rs`
- Modify: `crates/scribe-server/src/session_manager.rs`

This adds a new `ClientMessage` variant and its server-side handler, plus implements the existing but unhandled `CreateWorkspace` message. The `CreateWorkspace` handler is needed because `scribe-test session create` sends `CreateWorkspace` to get a `WorkspaceId` before creating a session.

- [ ] **Step 1: Add `RequestSnapshot` variant to `ClientMessage`**

In `crates/scribe-common/src/protocol.rs`, add to the `ClientMessage` enum:

```rust
RequestSnapshot {
    session_id: SessionId,
},
```

- [ ] **Step 2: Make `snapshot_term` helpers `pub(crate)` in `session_manager.rs`**

In `crates/scribe-server/src/session_manager.rs`, find the private helper functions `snapshot_term`, `convert_cell`, `convert_color`, `convert_flags`, `convert_cursor_style` (currently private, likely marked `#[allow(dead_code)]`). Change their visibility to `pub(crate)` so `ipc_server.rs` can call them directly. Do NOT duplicate this logic.

- [ ] **Step 3: Handle `RequestSnapshot` in `dispatch_message`**

In the `dispatch_message()` match block (around line 186), add:

```rust
ClientMessage::RequestSnapshot { session_id } => {
    handle_request_snapshot(session_id, &session_handles, &writer).await
}
```

The handler function:
1. Looks up `session_id` in `session_handles`
2. If not found, sends `ServerMessage::Error`
3. If found, calls `session_manager::snapshot_term()` (now `pub(crate)`) with the `SessionHandle`'s `term` lock
4. Sends `ServerMessage::ScreenSnapshot { session_id, snapshot }`

- [ ] **Step 4: Implement `CreateWorkspace` handler in `dispatch_message`**

Currently `CreateWorkspace` falls through to the unhandled catch-all in `dispatch_message()` (around line 222). Implement it:
1. Call `workspace_manager.write().await.create_workspace()` to get a new `WorkspaceId`
2. Send `ServerMessage::WorkspaceInfo { workspace_id, name: None, accent_color }` back to the client
3. The accent color comes from the workspace manager's color palette

This is required because `scribe-test session create` sends `CreateWorkspace` to obtain a `WorkspaceId` before creating a session.

- [ ] **Step 5: Verify build**

Run: `cargo check --workspace`
Run: `cargo clippy --workspace`
Run: `cargo test --workspace`

- [ ] **Step 6: Commit**

```
feat: add RequestSnapshot and CreateWorkspace IPC handlers

RequestSnapshot allows clients to request a ScreenSnapshot for any
session they own. CreateWorkspace creates a new workspace and returns
WorkspaceInfo. Both are needed for the E2E test harness.
```

---

### Task 2: Add `FromStr` to `SessionId` and `WorkspaceId`

**Files:**
- Modify: `crates/scribe-common/src/ids.rs`

The `scribe-test` CLI needs to parse session IDs from command-line arguments. Currently `SessionId` has `Display` (`session-{first8}`) but no `FromStr`. We need to accept the full UUID string as input (not the truncated display form, since that's lossy).

- [ ] **Step 1: Add `FromStr` impl to `SessionId`**

In `crates/scribe-common/src/ids.rs`:

```rust
impl std::str::FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(s)?;
        Ok(Self(uuid))
    }
}
```

- [ ] **Step 2: Add `FromStr` impl to `WorkspaceId`**

Same pattern for `WorkspaceId`.

- [ ] **Step 3: Add a `to_full_string()` method to both**

```rust
impl SessionId {
    /// Returns the full UUID string (for CLI serialization).
    pub fn to_full_string(&self) -> String {
        self.0.to_string()
    }
}
```

Same for `WorkspaceId`. This gives `scribe-test` a non-lossy string to print that can be parsed back via `FromStr`.

- [ ] **Step 4: Verify build**

Run: `cargo check --workspace && cargo clippy --workspace && cargo test --workspace`

- [ ] **Step 5: Commit**

```
feat: add FromStr and to_full_string to SessionId/WorkspaceId

Enables parsing session/workspace IDs from CLI arguments and printing
full (non-truncated) UUID strings for round-trip serialization.
```

---

### Task 3: Scaffold `scribe-test` crate

**Files:**
- Modify: `Cargo.toml` (workspace deps)
- Create: `crates/scribe-test/Cargo.toml`
- Create: `crates/scribe-test/src/main.rs`

- [ ] **Step 1: Add new dependencies to workspace `Cargo.toml`**

In the `[workspace.dependencies]` section of the root `Cargo.toml`, add:

```toml
clap = { version = "4", features = ["derive"] }
regex = "1"
```

- [ ] **Step 2: Create `crates/scribe-test/Cargo.toml`**

```toml
[package]
name = "scribe-test"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
description = "E2E test harness for Scribe terminal emulator"

[[bin]]
name = "scribe-test"
path = "src/main.rs"

[dependencies]
scribe-common.workspace = true
tokio.workspace = true
serde.workspace = true
serde_json.workspace = true
rmp-serde.workspace = true
clap.workspace = true
regex.workspace = true
cosmic-text.workspace = true
png.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
nix.workspace = true

[lints]
workspace = true
```

- [ ] **Step 3: Create `crates/scribe-test/src/main.rs` with clap skeleton**

```rust
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "scribe-test", about = "E2E test harness for Scribe")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the scribe-server process
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// Manage the test daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Manage sessions
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Send keystrokes to a session
    Send {
        session_id: String,
        data: String,
    },
    /// Resize a session's PTY
    Resize {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    /// Take a CPU-rendered PNG screenshot
    Screenshot {
        session_id: String,
        path: String,
    },
    /// Dump grid state as JSON
    Snapshot {
        session_id: String,
        path: String,
    },
    /// Wait for output matching a regex
    WaitOutput {
        session_id: String,
        pattern: String,
        #[arg(long, default_value = "5000")]
        timeout: u64,
    },
    /// Wait for CWD to change to a path
    WaitCwd {
        session_id: String,
        path: String,
        #[arg(long, default_value = "5000")]
        timeout: u64,
    },
    /// Wait for output quiescence
    WaitIdle {
        session_id: String,
        #[arg(long, default_value = "500")]
        ms: u64,
        #[arg(long, default_value = "5000")]
        timeout: u64,
    },
    /// Assert character at grid position (0-indexed)
    AssertCell {
        session_id: String,
        row: u16,
        col: u16,
        expected: char,
    },
    /// Assert cursor position (0-indexed)
    AssertCursor {
        session_id: String,
        row: u16,
        col: u16,
    },
    /// Wait for session exit and assert exit code
    AssertExit {
        session_id: String,
        code: i32,
        #[arg(long, default_value = "5000")]
        timeout: u64,
    },
}

#[derive(Subcommand)]
enum ServerAction {
    Start,
    Stop,
}

#[derive(Subcommand)]
enum DaemonAction {
    Start,
    Stop,
    /// Internal: runs the daemon event loop (not user-facing)
    Run,
}

#[derive(Subcommand)]
enum SessionAction {
    Create,
    Close { session_id: String },
}

/// Exit code distinction: test failure (1) vs infrastructure error (2).
enum TestError {
    TestFailure(String),
    InfraError(String),
}

impl From<TestError> for ExitCode {
    fn from(err: TestError) -> Self {
        match err {
            TestError::TestFailure(msg) => {
                let _ = writeln!(io::stderr(), "FAIL: {msg}");
                ExitCode::from(1)
            }
            TestError::InfraError(msg) => {
                let _ = writeln!(io::stderr(), "ERROR: {msg}");
                ExitCode::from(2)
            }
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => ExitCode::from(e),
    }
}

fn run(cli: Cli) -> Result<(), TestError> {
    // Dispatch will be implemented in subsequent tasks
    drop(cli);
    Ok(())
}
```

Note: `std::process::exit()` is disallowed by `clippy.toml`. All exit code logic flows through `fn main() -> ExitCode`. The `TestError` enum carries the 1 vs 2 distinction.

- [ ] **Step 4: Verify build**

Run: `cargo check --workspace && cargo clippy --workspace`

Fix any clippy issues. The skeleton may need `use std::process::ExitCode; use std::io::{self, Write};` and adjustments for unused fields.

- [ ] **Step 5: Commit**

```
feat: scaffold scribe-test crate with clap CLI

New workspace crate for E2E testing. Defines the full subcommand tree:
server start/stop, daemon start/stop, session create/close, send,
resize, screenshot, snapshot, wait-output, wait-cwd, wait-idle,
assert-cell, assert-cursor, assert-exit.
```

---

### Task 4: Build daemon-subcommand protocol

**Files:**
- Create: `crates/scribe-test/src/cmd_socket.rs`

This defines the request/response types for communication between CLI subcommands and the daemon, plus connection helpers. Uses the same length-prefixed msgpack framing as the server protocol (`scribe-common/src/framing.rs`).

- [ ] **Step 1: Create `cmd_socket.rs`**

Define the daemon protocol types:

```rust
use serde::{Deserialize, Serialize};
use scribe_common::ids::SessionId;
use scribe_common::screen::ScreenSnapshot;

/// Request from a CLI subcommand to the daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRequest {
    CreateSession,
    CloseSession { session_id: SessionId },
    Send { session_id: SessionId, data: Vec<u8> },
    Resize { session_id: SessionId, cols: u16, rows: u16 },
    RequestScreenshot { session_id: SessionId },
    RequestSnapshot { session_id: SessionId },
    WaitOutput { session_id: SessionId, pattern: String, timeout_ms: u64 },
    WaitCwd { session_id: SessionId, path: String, timeout_ms: u64 },
    WaitIdle { session_id: SessionId, quiet_ms: u64, timeout_ms: u64 },
    AssertCell { session_id: SessionId, row: u16, col: u16, expected: char },
    AssertCursor { session_id: SessionId, row: u16, col: u16 },
    AssertExit { session_id: SessionId, expected_code: i32, timeout_ms: u64 },
    Shutdown,
}

/// Response from the daemon to a CLI subcommand.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonResponse {
    Ok,
    SessionCreated { session_id: SessionId },
    ScreenshotData { snapshot: ScreenSnapshot },
    AssertFailed { message: String },
    Error { message: String },
}
```

Add helper functions:
- `daemon_socket_path() -> PathBuf` — returns `/run/user/{uid}/scribe/test-daemon.sock`
- `send_request(request: &DaemonRequest) -> Result<DaemonResponse, ...>` — connects to daemon socket, writes request via `write_message`, reads response via `read_message`, returns
- These reuse `scribe_common::framing::{read_message, write_message}`

- [ ] **Step 2: Verify build**

Run: `cargo check -p scribe-test && cargo clippy -p scribe-test`

- [ ] **Step 3: Commit**

```
feat(scribe-test): add daemon<->subcommand protocol

DaemonRequest/DaemonResponse types with msgpack framing over a local
Unix socket. Subcommands connect, send one request, receive one
response, disconnect.
```

---

### Task 5: Build IPC client to scribe-server

**Files:**
- Create: `crates/scribe-test/src/ipc.rs`

A thin async IPC client that connects to the scribe-server Unix socket, sends `ClientMessage`s, and reads `ServerMessage`s. Used by the daemon to maintain its persistent connection.

- [ ] **Step 1: Create `ipc.rs`**

Key functions:
- `connect() -> Result<(ReadHalf, WriteHalf), ...>` — connects to `server_socket_path()`, splits into read/write halves
- `send(writer: &Mutex<WriteHalf>, msg: &ClientMessage) -> Result<(), ...>` — serializes and sends via `write_message`
- `recv(reader: &mut ReadHalf) -> Result<ServerMessage, ...>` — reads via `read_message`

Use `tokio::net::UnixStream` for the connection. Reuse `scribe_common::framing` and `scribe_common::socket::server_socket_path()`.

- [ ] **Step 2: Verify build**

Run: `cargo check -p scribe-test && cargo clippy -p scribe-test`

- [ ] **Step 3: Commit**

```
feat(scribe-test): add IPC client for scribe-server

Async UnixStream client using scribe-common framing. Connects to the
server socket, sends ClientMessages, receives ServerMessages.
```

---

### Task 6: Build server lifecycle management

**Files:**
- Create: `crates/scribe-test/src/server.rs`

Manages starting and stopping the `scribe-server` process. `start` spawns the binary, polls for the socket, writes a PID file. `stop` reads the PID file and sends signals.

- [ ] **Step 1: Create `server.rs`**

`start()`:
1. Spawn `scribe-server` via `tokio::process::Command` with stdout/stderr redirected to `/dev/null` or a log file
2. Write PID to `/tmp/scribe-server.pid`
3. Poll for socket at `server_socket_path()` — check every 100ms, timeout after 5s
4. Return `Ok(())` when socket exists, or `Err` on timeout (exit code 2)

`stop()`:
1. Read PID from `/tmp/scribe-server.pid`
2. Send `SIGTERM` via `nix::sys::signal::kill`
3. Wait up to 3s for process to exit (poll `/proc/{pid}/` or `waitpid`)
4. If still alive after 3s, send `SIGKILL`
5. Remove PID file
6. Return `Ok(())`

- [ ] **Step 2: Verify build**

Run: `cargo check -p scribe-test && cargo clippy -p scribe-test`

- [ ] **Step 3: Commit**

```
feat(scribe-test): add server lifecycle management

Spawns scribe-server as a child process, polls for socket readiness,
manages PID file, and handles graceful + forced shutdown.
```

---

### Task 7: Build daemon core

**Files:**
- Create: `crates/scribe-test/src/daemon.rs`

The daemon is the most complex component. It:
1. Connects to scribe-server via IPC
2. Listens for subcommand connections on a local Unix socket
3. Continuously reads server messages, dispatching to per-session state
4. Handles subcommand requests by translating them to server messages and/or querying buffered state

- [ ] **Step 1: Define session state struct**

```rust
const MAX_OUTPUT_BUFFER: usize = 65_536; // 64KB ring buffer

enum SessionStatus {
    Running,
    Exited(Option<i32>),
}

struct SessionState {
    output_buffer: VecDeque<u8>,   // Ring buffer, max 64KB
    latest_snapshot: Option<ScreenSnapshot>,
    cwd: Option<PathBuf>,
    title: Option<String>,
    ai_state: Option<AiProcessState>,
    git_branch: Option<Option<String>>,
    status: SessionStatus,
    workspace_id: WorkspaceId,
}
```

The output ring buffer is a `VecDeque<u8>` capped at `MAX_OUTPUT_BUFFER`. After each `extend()`, check `.len()` and `drain(..overflow)` from the front if over capacity. This drain step is essential — `VecDeque` does not enforce capacity limits automatically.

- [ ] **Step 2: Implement daemon event loop**

The daemon runs three concurrent tasks via `tokio::select!`:
1. **Server message reader**: reads `ServerMessage`s from the server IPC connection, dispatches to `SessionState` maps
2. **Command listener**: accepts connections on the daemon socket, reads `DaemonRequest`, processes it, sends `DaemonResponse`
3. **Shutdown signal**: watches for `DaemonRequest::Shutdown` flag

Message dispatch logic — handle ALL `ServerMessage` variants:
- `PtyOutput { session_id, data }` → append to session's `output_buffer`, drain front if over 64KB, notify any waiting `wait-output`
- `ScreenSnapshot { session_id, snapshot }` → store as `latest_snapshot`
- `CwdChanged { session_id, cwd }` → store as `cwd`, notify any waiting `wait-cwd`
- `TitleChanged { session_id, title }` → store as `title`
- `AiStateChanged { session_id, ai_state }` → store as `ai_state`
- `GitBranch { session_id, branch }` → store as `git_branch`
- `SessionCreated` → create new `SessionState` entry
- `SessionExited { session_id, exit_code }` → set `status = Exited(exit_code)`, notify any waiting `assert-exit`
- `WorkspaceInfo`, `WorkspaceNamed` → store for workspace tracking
- `Bell`, `Error` → log via tracing

For wait conditions, use `tokio::sync::Notify` or `tokio::sync::watch` channels so subcommand handlers can block until the condition is met.

- [ ] **Step 3: Implement `start()` and `stop()` entry points**

`start()`:
1. Fork/daemonize the process (or spawn as background tokio task)
2. Connect to server via `ipc::connect()`
3. Bind daemon socket at `daemon_socket_path()`
4. Enter event loop
5. Write a PID file at `/tmp/scribe-test-daemon.pid`

`stop()`:
1. Connect to daemon socket
2. Send `DaemonRequest::Shutdown`
3. Wait for daemon to exit (poll PID)

For the daemon process: use `std::process::Command` to re-invoke `scribe-test daemon run` (a hidden subcommand) in the background. The `daemon run` subcommand runs the actual event loop. `daemon start` spawns it and waits for the socket to appear. `daemon stop` sends shutdown.

- [ ] **Step 4: Implement subcommand request handlers**

Each `DaemonRequest` maps to:
- `CreateSession` → send `CreateWorkspace` to server, wait for `WorkspaceInfo`, send `CreateSession { workspace_id }`, wait for `SessionCreated`, return `DaemonResponse::SessionCreated`
- `CloseSession` → send `CloseSession` to server, return `Ok`
- `Send` → send `KeyInput` to server, return `Ok`
- `Resize` → send `Resize` to server, return `Ok`
- `RequestScreenshot` → send `RequestSnapshot` to server, wait for `ScreenSnapshot`, return `ScreenshotData`
- `RequestSnapshot` → same as screenshot but returns raw data
- `WaitOutput` → check output buffer for regex match, if not found set up notify and wait with timeout
- `WaitCwd` → check current CWD, if not matching set up notify and wait with timeout
- `WaitIdle` → wait for no `PtyOutput` for `quiet_ms` duration (reset timer on each output)
- `AssertCell` → get latest snapshot (request one if none), check cell at `(row, col)`
- `AssertCursor` → get latest snapshot, check cursor position
- `AssertExit` → check if session already exited, if not wait with timeout
- `Shutdown` → close all sessions, disconnect, exit

- [ ] **Step 5: Verify build**

Run: `cargo check -p scribe-test && cargo clippy -p scribe-test`

- [ ] **Step 6: Commit**

```
feat(scribe-test): implement daemon with event loop

Persistent daemon holds IPC connection to scribe-server, buffers
per-session state (output ring buffer, snapshots, CWD, exit codes),
and handles subcommand requests via local Unix socket.
```

---

### Task 8: Wire up CLI subcommands

**Files:**
- Create: `crates/scribe-test/src/session.rs`
- Create: `crates/scribe-test/src/input.rs`
- Create: `crates/scribe-test/src/wait.rs`
- Create: `crates/scribe-test/src/assert.rs`
- Create: `crates/scribe-test/src/capture.rs`
- Modify: `crates/scribe-test/src/main.rs`

Each subcommand is a thin function that:
1. Parses CLI args (already done by clap)
2. Builds a `DaemonRequest`
3. Calls `cmd_socket::send_request()`
4. Handles the `DaemonResponse` (print output, set exit code)

- [ ] **Step 1: Create `session.rs`**

```rust
pub fn create() -> Result<(), ...> {
    let resp = cmd_socket::send_request(&DaemonRequest::CreateSession)?;
    match resp {
        DaemonResponse::SessionCreated { session_id } => {
            // Print full UUID to stdout for capture by $()
            writeln!(io::stdout(), "{}", session_id.to_full_string())?;
            Ok(())
        }
        DaemonResponse::Error { message } => Err(...)
    }
}

pub fn close(session_id: &str) -> Result<(), ...> {
    let id: SessionId = session_id.parse()?;
    let resp = cmd_socket::send_request(&DaemonRequest::CloseSession { session_id: id })?;
    // handle response
}
```

Note: `print_stdout` is denied by clippy. Use `writeln!(io::stdout(), ...)` instead.

- [ ] **Step 2: Create `input.rs`**

Key: escape parsing. `scribe-test send <id> 'hello\n'` must convert `\n` → `0x0A`, `\t` → `0x09`, `\x1b` → `0x1B`, `\xNN` → byte `NN`.

```rust
pub fn parse_escapes(input: &str) -> Vec<u8> {
    let mut result = Vec::new();
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push(b'\n'),
                Some('t') => result.push(b'\t'),
                Some('\\') => result.push(b'\\'),
                Some('x') => {
                    // Parse two hex digits
                    let hi = chars.next().and_then(|c| c.to_digit(16));
                    let lo = chars.next().and_then(|c| c.to_digit(16));
                    if let (Some(h), Some(l)) = (hi, lo) {
                        result.push((h * 16 + l) as u8);
                    }
                }
                Some(other) => {
                    result.push(b'\\');
                    // push other char's UTF-8 bytes
                }
                None => result.push(b'\\'),
            }
        } else {
            // push c's UTF-8 bytes
        }
    }
    result
}
```

`send()` calls `parse_escapes()`, builds `DaemonRequest::Send`, sends to daemon.
`resize()` builds `DaemonRequest::Resize`, sends to daemon.

- [ ] **Step 3: Create `wait.rs`**

Each function builds the corresponding `DaemonRequest` with timeout, sends to daemon, maps `DaemonResponse::Ok` to exit 0 and `DaemonResponse::Error` to exit 2 (infra).

- [ ] **Step 4: Create `assert.rs`**

Each function builds the corresponding `DaemonRequest`, sends to daemon. Maps:
- `DaemonResponse::Ok` → exit 0
- `DaemonResponse::AssertFailed { message }` → print message to stderr, exit 1
- `DaemonResponse::Error { message }` → print message to stderr, exit 2

- [ ] **Step 5: Create `capture.rs`**

`screenshot()`:
1. Sends `DaemonRequest::RequestScreenshot` to daemon
2. Gets `DaemonResponse::ScreenshotData { snapshot }`
3. Calls `render::render_to_png(&snapshot, &path)?` (Task 9)
4. Returns `Ok(())`

`snapshot()`:
1. Sends `DaemonRequest::RequestSnapshot` to daemon
2. Gets `DaemonResponse::ScreenshotData { snapshot }`
3. Serializes via `serde_json::to_string_pretty(&snapshot)?`
4. Writes to file at `path`

- [ ] **Step 6: Wire up `main.rs`**

Update `main.rs` to dispatch each `Command` variant to the corresponding function. Set process exit code based on result:
- `Ok(())` → exit 0
- `Err` with test failure → exit 1
- `Err` with infra error → exit 2

Add `mod` declarations for all modules:
```rust
mod assert;
mod capture;
mod cmd_socket;
mod daemon;
mod input;
mod ipc;
mod render;
mod server;
mod session;
mod wait;
```

- [ ] **Step 7: Verify build**

Run: `cargo check -p scribe-test && cargo clippy -p scribe-test`

- [ ] **Step 8: Commit**

```
feat(scribe-test): implement all CLI subcommands

session create/close, send with escape parsing, resize, wait-output,
wait-cwd, wait-idle, assert-cell, assert-cursor, assert-exit,
screenshot, snapshot. Each subcommand talks to the daemon via local
Unix socket.
```

---

### Task 9: Build CPU PNG renderer

**Files:**
- Create: `crates/scribe-test/src/render.rs`

Renders a `ScreenSnapshot` to a PNG file using `cosmic-text` for glyph rasterization and `png` for encoding. No GPU required.

- [ ] **Step 1: Define rendering constants**

```rust
const CELL_WIDTH: u32 = 10;   // pixels per cell width (adjust for font)
const CELL_HEIGHT: u32 = 20;  // pixels per cell height
const FONT_SIZE: f32 = 14.0;
```

These should produce a readable terminal screenshot. Adjust after testing with actual font metrics.

- [ ] **Step 2: Implement ANSI color palette**

```rust
/// Resolve a ScreenColor to RGBA. Uses `.get()` for palette lookup (indexing_slicing is denied).
fn resolve_color(color: &ScreenColor) -> [u8; 4] {
    match color {
        ScreenColor::Named(idx) => ANSI_PALETTE
            .get(*idx as usize)
            .copied()
            .unwrap_or([255, 255, 255, 255]),
        ScreenColor::Indexed(idx) => INDEXED_PALETTE
            .get(*idx as usize)
            .copied()
            .unwrap_or([255, 255, 255, 255]),
        ScreenColor::Rgb { r, g, b } => [*r, *g, *b, 255],
    }
}
```

Note: the `unwrap_or` fallback is intentional — invalid palette indices get white instead of crashing. The `#[allow(clippy::unwrap_used)]` is NOT needed here since `unwrap_or` is not `unwrap`.

Include the standard 16-color ANSI palette and 256-color indexed palette as static arrays. Reference the same palette data used in `scribe-renderer` if available, or use the standard xterm-256color values.

- [ ] **Step 3: Implement `render_to_png()` — decomposed into sub-functions**

**Critical:** The 80-line function limit and `indexing_slicing` deny mean this must be split into small, focused functions. Use `.get()` for all cell access.

Top-level orchestrator:
```rust
pub fn render_to_png(snapshot: &ScreenSnapshot, path: &Path) -> Result<(), ...> {
    let width = u32::from(snapshot.cols) * CELL_WIDTH;
    let height = u32::from(snapshot.rows) * CELL_HEIGHT;
    let mut pixels = vec![0u8; (width * height * 4) as usize];

    let mut font_system = FontSystem::new();
    let mut swash_cache = SwashCache::new();

    render_cells(snapshot, &mut pixels, width, &mut font_system, &mut swash_cache);

    if snapshot.cursor_visible {
        draw_cursor(&mut pixels, width, snapshot);
    }

    write_png(path, &pixels, width, height)
}
```

Cell rendering (separate function, stays under 80 lines):
```rust
fn render_cells(
    snapshot: &ScreenSnapshot,
    pixels: &mut [u8],
    width: u32,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
) {
    for (idx, cell) in snapshot.cells.iter().enumerate() {
        let col = (idx % snapshot.cols as usize) as u32;
        let row = (idx / snapshot.cols as usize) as u32;

        let (fg, bg) = resolve_cell_colors(cell);
        fill_rect(pixels, width, col * CELL_WIDTH, row * CELL_HEIGHT, CELL_WIDTH, CELL_HEIGHT, bg);

        if cell.c != ' ' && !cell.flags.hidden {
            render_glyph(font_system, swash_cache, pixels, width,
                cell.c, fg, col * CELL_WIDTH, row * CELL_HEIGHT,
                cell.flags.bold, cell.flags.italic);
        }
    }
}
```

Note: `snapshot.cells.iter().enumerate()` avoids indexing entirely. Each helper function (`fill_rect`, `render_glyph`, `draw_cursor`, `resolve_cell_colors`) should be its own function, each under 80 lines.

- [ ] **Step 4: Implement `write_png()` using the `png` crate**

```rust
fn write_png(path: &Path, pixels: &[u8], width: u32, height: u32) -> Result<(), ...> {
    let file = File::create(path)?;
    let w = io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(pixels)?;
    Ok(())
}
```

- [ ] **Step 5: Implement glyph rendering with `cosmic-text`**

Use `cosmic-text::FontSystem`, `cosmic-text::Buffer`, and `cosmic-text::SwashCache` to:
1. Create a `Buffer` with one line containing the character
2. Shape and layout the buffer
3. Use `SwashCache::get_image_uncached()` to get the glyph bitmap
4. Blit the glyph bitmap into the pixel buffer at the correct position

Reference how `scribe-renderer` uses `cosmic-text` for the atlas — the logic is similar but writes to a CPU buffer instead of a GPU texture.

- [ ] **Step 6: Verify build**

Run: `cargo check -p scribe-test && cargo clippy -p scribe-test`

- [ ] **Step 7: Commit**

```
feat(scribe-test): add CPU PNG renderer for terminal screenshots

Renders ScreenSnapshot to PNG using cosmic-text for glyph
rasterization and the png crate for encoding. Supports ANSI/256
color palette, cell flags (bold, italic, inverse, dim), and cursor
rendering. No GPU required.
```

---

### Task 10: Docker containers and entrypoints

**Files:**
- Create: `docker/Dockerfile.func`
- Create: `docker/Dockerfile.visual`
- Create: `docker/entrypoint-func.sh`
- Create: `docker/entrypoint-visual.sh`
- Create: `tests/e2e/smoke.sh`
- Modify: `.gitignore`

- [ ] **Step 1: Create `docker/Dockerfile.func`**

```dockerfile
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        fonts-jetbrains-mono \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy pre-built binaries
COPY target/release/scribe-server /usr/local/bin/scribe-server
COPY target/release/scribe-test /usr/local/bin/scribe-test
COPY docker/entrypoint-func.sh /entrypoint.sh

RUN chmod +x /entrypoint.sh /usr/local/bin/scribe-server /usr/local/bin/scribe-test

# Create output directory
RUN mkdir -p /output

ENTRYPOINT ["/entrypoint.sh"]
```

- [ ] **Step 2: Create `docker/entrypoint-func.sh`**

Use the entrypoint script from the spec (`docs/superpowers/specs/2026-03-22-e2e-testing-containers-design.md`, lines 188-213). Copy it exactly.

Ensure the script is executable and has proper error handling:
```bash
#!/bin/bash
set -euo pipefail

# Cleanup runs regardless of test outcome
cleanup() {
    scribe-test daemon stop || true
    scribe-test server stop || true
}
trap cleanup EXIT

# Create socket directory
UID_DIR="/run/user/$(id -u)/scribe"
mkdir -p "$UID_DIR"
chmod 700 "$UID_DIR"

# Start server and daemon
scribe-test server start
scribe-test daemon start

# Create default session
SESSION=$(scribe-test session create)
export SESSION

# Execute test script with timeout, capturing logs
EXIT_CODE=0
timeout 30 "$1" 2>&1 | tee /output/result.log || EXIT_CODE=$?

exit $EXIT_CODE
```

Note: `trap cleanup EXIT` ensures daemon and server are always stopped, even if a command fails under `set -e`. The `|| true` in cleanup prevents cascading failures.

- [ ] **Step 3: Create `docker/Dockerfile.visual`**

```dockerfile
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        fonts-jetbrains-mono \
        ca-certificates \
        xvfb \
        mesa-utils \
        libgl1-mesa-dri \
        scrot \
        xdotool \
        libvulkan1 \
        libegl1-mesa \
    && rm -rf /var/lib/apt/lists/*

# Copy pre-built binaries
COPY target/release/scribe-server /usr/local/bin/scribe-server
COPY target/release/scribe-client /usr/local/bin/scribe-client
COPY target/release/scribe-test /usr/local/bin/scribe-test
COPY docker/entrypoint-visual.sh /entrypoint.sh

RUN chmod +x /entrypoint.sh \
    /usr/local/bin/scribe-server \
    /usr/local/bin/scribe-client \
    /usr/local/bin/scribe-test

RUN mkdir -p /output

ENTRYPOINT ["/entrypoint.sh"]
```

- [ ] **Step 4: Create `docker/entrypoint-visual.sh`**

Use the entrypoint script from the spec (lines 239-286), but add the same `trap cleanup EXIT` pattern as the functional entrypoint. The cleanup function should also `kill $CLIENT_PID` and `kill $XVFB_PID` with `|| true`.

- [ ] **Step 5: Create `tests/e2e/smoke.sh`**

```bash
#!/bin/bash
set -e

# Basic smoke test: verify the shell starts and responds
scribe-test send "$SESSION" 'echo scribe-e2e-test\n'
scribe-test wait-output "$SESSION" "scribe-e2e-test"
scribe-test screenshot "$SESSION" /output/01-echo.png

# Verify CWD tracking
scribe-test send "$SESSION" 'cd /tmp\n'
scribe-test wait-cwd "$SESSION" "/tmp"
scribe-test screenshot "$SESSION" /output/02-cwd.png

# Verify resize
scribe-test resize "$SESSION" 80 24
scribe-test wait-idle "$SESSION" --ms 200
scribe-test send "$SESSION" 'tput cols\n'
scribe-test wait-output "$SESSION" "80"
scribe-test screenshot "$SESSION" /output/03-resize.png

echo "PASS: smoke test completed"
```

Make executable: `chmod +x tests/e2e/smoke.sh`

- [ ] **Step 6: Update `.gitignore`**

Add to `.gitignore`:
```
test-output/
```

- [ ] **Step 7: Verify Docker build**

```bash
cargo build --release
docker build -f docker/Dockerfile.func -t scribe-test-func .
```

If the build succeeds, the image is ready. Don't run it yet — we need the daemon + subcommands working first.

- [ ] **Step 8: Commit**

```
feat: add Docker containers and smoke test for E2E testing

Functional container (debian-slim + server + test harness) and visual
container (adds Xvfb + Mesa + client). Includes entrypoint scripts,
basic smoke test, and gitignore for test-output directory.
```

---

### Task 11: Integration test — run the smoke test

**Files:** None (verification only)

- [ ] **Step 1: Build release binaries**

Run: `cargo build --release`

Verify all three binaries exist:
- `target/release/scribe-server`
- `target/release/scribe-test`
- `target/release/scribe-client`

- [ ] **Step 2: Build functional Docker image**

Run: `docker build -f docker/Dockerfile.func -t scribe-test-func .`

- [ ] **Step 3: Run smoke test in functional container**

```bash
mkdir -p test-output
docker run --rm \
  -v ./tests/e2e:/tests \
  -v ./test-output:/output \
  scribe-test-func /tests/smoke.sh
```

Expected: exit code 0, `test-output/result.log` shows "PASS", PNG screenshots exist in `test-output/`.

- [ ] **Step 4: Inspect screenshots**

Read the generated PNG files to verify they show terminal content:
- `test-output/01-echo.png` — should show `echo scribe-e2e-test` and output
- `test-output/02-cwd.png` — should show terminal after `cd /tmp`
- `test-output/03-resize.png` — should show `80` from `tput cols`

- [ ] **Step 5: Build and test visual container (if GPU/display available)**

```bash
docker build -f docker/Dockerfile.visual -t scribe-test-visual .
docker run --rm \
  -v ./tests/e2e:/tests \
  -v ./test-output:/output \
  scribe-test-visual /tests/smoke.sh
```

This may fail if Mesa/Xvfb setup has issues — debug and fix. The visual container is expected to need iteration.

- [ ] **Step 6: Fix any issues found**

Debug failures by:
1. Reading `test-output/result.log`
2. Running the container interactively: `docker run --rm -it scribe-test-func bash`
3. Testing individual commands inside the container

- [ ] **Step 7: Commit any fixes**

```
fix: resolve E2E smoke test issues

[describe what was fixed]
```

---

### Task 12: Update CLAUDE.md with E2E testing commands

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add E2E testing section to CLAUDE.md**

Add under the Build & Development Commands section:

```markdown
## E2E Testing

```bash
# Build containers (after cargo build --release)
docker build -f docker/Dockerfile.func -t scribe-test-func .
docker build -f docker/Dockerfile.visual -t scribe-test-visual .

# Run functional E2E test
docker run --rm -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-func /tests/smoke.sh

# Run visual E2E test (requires software GPU)
docker run --rm -v ./tests/e2e:/tests -v ./test-output:/output scribe-test-visual /tests/smoke.sh

# Inspect results: check exit code, read test-output/result.log, view PNG screenshots
```
```

- [ ] **Step 2: Commit**

```
docs: add E2E testing commands to CLAUDE.md
```
