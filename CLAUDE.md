# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```bash
# Build all crates (debug)
cargo build

# Build release (thin LTO, stripped)
cargo build --release

# Check without building (faster feedback)
cargo check

# Lint — extremely strict clippy config, see Cargo.toml workspace lints + clippy.toml
cargo clippy --workspace

# Format (edition 2024, max_width=100)
cargo fmt --all

# Run tests
cargo test --workspace

# Run a single test by name
cargo test --package scribe-server -- tests::workspace_name_is_sticky

# Run the server
cargo run --bin scribe-server

# Run the GPU client
cargo run --bin scribe-client

# Run the CLI test tool (raw stdin/stdout passthrough)
cargo run --bin scribe-cli
```

## Architecture

Scribe is a client-server terminal emulator: a long-lived PTY server manages shell sessions, and separate UI clients connect over Unix domain sockets.

### Crate Layout

```
crates/
├── scribe-common     # Shared types: protocol messages, framing, IDs, error, AI state
├── scribe-pty        # PTY I/O: AsyncPtyFd, OSC interceptor, metadata parser
├── scribe-server     # PTY server: session/workspace management, IPC, hot-reload handoff
├── scribe-client     # GPU client: winit + wgpu, multi-pane layout, input, splash screen
├── scribe-renderer   # GPU pipeline: glyph atlas (cosmic-text), colour palette, wgpu pipeline
└── scribe-cli        # Headless test CLI: raw stdin/stdout over IPC
```

### Data Flow

1. **Client → Server**: `ClientMessage` (key input, resize, create/close session) serialised as length-prefixed msgpack over Unix socket (`/run/user/{uid}/scribe/server.sock`)
2. **Server → Client**: `ServerMessage` (PTY output, screen snapshots, AI state, CWD/title changes) — same framing
3. **PTY read loop** (in `ipc_server.rs`): three parallel paths per read:
   - **Fast path**: raw bytes forwarded to client as `PtyOutput`
   - **State path**: bytes fed into `alacritty_terminal::Term` via VTE ANSI processor
   - **Metadata path**: bytes parsed by `OscInterceptor` for OSC 7 (CWD), OSC 0/2 (title), OSC 1337 (AI state), BEL

### Key Design Decisions

- **`alacritty_terminal`** is used for terminal emulation state (grid, scrollback, cursor) — not Alacritty's rendering
- **Dual VTE parsers**: `alacritty_terminal` ignores custom OSC 1337 `AiState`, so a separate `OscInterceptor` runs in parallel on the same byte stream
- **Hot-reload handoff** (`handoff.rs`): zero-downtime upgrades via `SCM_RIGHTS` — old server sends PTY master fds + serialised state to new server on `--upgrade`. Handoff socket: `/run/user/{uid}/scribe/handoff.sock`
- **Workspace auto-naming**: `WorkspaceManager` matches session CWD against configured `workspace_roots` to auto-name workspaces from the first path component after the root
- **Multi-pane layout**: binary tree (`LayoutTree`) with split/close/focus-cycle, divider drag resizing
- **GPU rendering**: single `TerminalRenderer` with shared glyph atlas renders all panes into one instance buffer per frame

### Lint Policy (Strict)

The workspace denies `unsafe_code` globally (only `scribe-pty` opts in with a crate-level allow). Clippy denies: pedantic, all restriction lints for `unwrap`/`expect`/`panic`/`indexing_slicing`/`todo`/`print_stdout`/`dbg_macro` and more. When `#[allow]` is used, a `reason = "..."` string is **required** (`allow_attributes_without_reason = "deny"`).

Thresholds in `clippy.toml`: cognitive complexity 15, function params 5, lines 80, nesting 4.

### Config

Server reads `~/.config/scribe/config.toml`:
```toml
[workspaces]
roots = ["~/work", "~/projects"]

[terminal]
scrollback_lines = 10000  # max 100_000
```

### IPC Security

Both the main IPC socket and handoff socket verify peer UID via `SO_PEERCRED` — connections from different users are rejected. Socket directories are created with 0700 permissions.
