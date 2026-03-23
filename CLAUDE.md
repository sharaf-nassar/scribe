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
├── scribe-settings   # Settings webview: wry window, HTML/CSS/JS assets, IPC handlers
└── scribe-cli        # Headless test CLI: raw stdin/stdout over IPC
```

### Data Flow

1. **Client → Server**: `ClientMessage` (key input, resize, create/close session) serialised as length-prefixed msgpack over Unix socket (`/run/user/{uid}/scribe/server.sock`)
2. **Server → Client**: `ServerMessage` (PTY output, screen snapshots, AI state, CWD/title changes) — same framing
3. **PTY read loop** (in `ipc_server.rs`): three parallel paths per read:
   - **Fast path**: raw bytes forwarded to client as `PtyOutput`
   - **State path**: bytes fed into `alacritty_terminal::Term` via VTE ANSI processor
   - **Metadata path**: bytes parsed by `OscInterceptor` for OSC 7 (CWD), OSC 0/2 (title), OSC 1337 (AI state), BEL

### UI Hierarchy

```
Window
├── Workspace A (screen region — workspaces split the window)
│   ├── Tab Bar
│   │   ├── [Workspace Badge] (only when 2+ workspaces open)
│   │   ├── [gap]
│   │   ├── Tab 1 (session)
│   │   └── Tab 2 (session, active)
│   ├── Content Area
│   │   ├── Pane 1 (split within the active tab)
│   │   └── Pane 2 (split within the active tab)
│   └── Status Bar
└── Workspace B (another screen region)
    ├── Tab Bar
    │   ├── [Workspace Badge]
    │   ├── [gap]
    │   └── Tab 1 (session)
    ├── Content Area
    │   └── Pane 1
    └── Status Bar
```

- **Workspace**: A region of the window. Creating a new workspace splits the window. Each workspace has its own tab bar, sessions, pane layout, and status bar.
- **Tab (Session)**: A shell session within a workspace, shown in that workspace's tab bar.
- **Pane**: A split within a tab's content area. Panes divide the active tab, not the workspace.
- **Workspace Badge**: Colored dot + workspace name shown in the tab bar. Only visible when 2+ workspaces are open. Separated from tabs by a gap.

Workspaces are never tabbed — they always occupy visible screen real estate side by side.

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

Unified config read by both server and client — `~/.config/scribe/config.toml`:
```toml
[appearance]
font = "JetBrains Mono"
font_size = 14.0
theme = "minimal-dark"   # or "tokyo-night", "catppuccin-mocha", "dracula", "solarized-dark", "custom"

[terminal]
scrollback_lines = 10000  # max 100_000

[workspaces]
roots = ["~/work", "~/projects"]
```

### Keyboard Shortcuts

- `Ctrl+Shift+\` — split vertical (side-by-side)
- `Ctrl+Shift+-` — split horizontal (top/bottom)
- `Ctrl+Shift+W` — close pane
- `Ctrl+Tab` — cycle focus to next pane
- `Ctrl+,` — open settings

### IPC Security

Both the main IPC socket and handoff socket verify peer UID via `SO_PEERCRED` — connections from different users are rejected. Socket directories are created with 0700 permissions.
