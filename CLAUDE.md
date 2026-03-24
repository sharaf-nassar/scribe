# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

A `justfile` provides the standard workflows. Run `just` to list all recipes.

```bash
# Common workflows
just build            # Debug build (all crates)
just build-release    # Release build (all crates)
just check            # Type-check without building
just clippy           # Lint (strict clippy config)
just fmt              # Format (edition 2024, max_width=100)
just test             # Run all tests
just ready            # Pre-commit gate: fmt + clippy + test

# Run
just server           # Run the server
just client           # Run the GPU client

# Package & install
just deb              # Build release .deb (builds full workspace)
just install          # Build, package, and install .deb (sudo)

# E2E testing
just e2e              # Full functional E2E suite (build + containerise + run all)
just e2e-func func/smoke.sh        # Run a single functional test
just e2e-visual visual/reconnect.sh # Run a single visual test (requires GPU)
just docker-func      # Rebuild functional test container
just docker-visual    # Rebuild visual test container
```

Raw cargo commands still work for one-off needs:

```bash
# Run a single test by name
cargo test --package scribe-server -- tests::workspace_name_is_sticky

# Run the CLI test tool (raw stdin/stdout passthrough)
cargo run --bin scribe-cli

# Inspect E2E results: check exit code, read test-output/result.log, view PNG screenshots
```

## E2E Testing

Two Docker containers for end-to-end testing. Use them to validate changes before committing.

### When to use which container

**Functional container (`scribe-test-func`)** — use for most changes:
- Server/protocol changes (`scribe-common`, `scribe-server`)
- IPC message handling, session management, workspace logic
- PTY I/O behavior, OSC parsing, metadata events
- Any change that doesn't touch the GPU renderer
- Fast: runs in ~3 seconds

**Visual container (`scribe-test-visual`)** — use when touching rendering:
- Renderer changes (`scribe-renderer` — wgpu pipeline, shaders, instance buffers)
- Glyph atlas / font rendering (`cosmic-text` integration)
- Color palette / theme changes
- Pane layout / split visual behavior
- Client input handling (`scribe-client`)
- Slower: ~5 seconds startup (Xvfb + GPU)
- **Requires `--gpus all`** to expose the GPU to the container

### Test directory layout

```
tests/e2e/
├── func/            # Functional tests (no GPU, fast)
│   ├── smoke.sh
│   ├── reconnect.sh
│   ├── keybindings-validation.sh
│   └── workspace-split.sh
└── visual/          # Visual tests (GPU + Xvfb + xdotool)
    ├── reconnect.sh
    └── workspace-split.sh
```

### How to run

```bash
# Fastest: run the full functional E2E suite (build + containerise + run all)
just e2e

# Or step by step:
just build-release                        # 1. Build release binaries
just docker-func                          # 2. Rebuild functional container
just e2e-func func/smoke.sh               # 3. Run a single test

# Visual tests (requires --gpus all):
just docker-visual
just e2e-visual visual/workspace-split.sh

# Inspect results:
# - Exit code: 0 = pass, 1 = test failure, 2 = infra error
# - Read test-output/result.log for stdout/stderr
# - Read test-output/*.png to visually verify terminal screenshots
```

### Writing test scripts

Test scripts are bash scripts that use `scribe-test` subcommands. The entrypoint pre-creates a session and exports `$SESSION`.

```bash
#!/bin/bash
set -e

# Send keystrokes (escapes parsed by scribe-test, not shell — use single quotes)
scribe-test send "$SESSION" 'echo hello\n'

# Wait for output matching a regex (5s default timeout)
scribe-test wait-output "$SESSION" "hello"

# Take a CPU-rendered PNG screenshot
scribe-test screenshot "$SESSION" /output/result.png

# Wait for no output for N ms (prompt detection heuristic)
scribe-test wait-idle "$SESSION" --ms 300

# Resize the PTY
scribe-test resize "$SESSION" 120 40

# Assert grid cell content (0-indexed row, col)
scribe-test assert-cell "$SESSION" 0 0 '$'

# Assert cursor position (0-indexed)
scribe-test assert-cursor "$SESSION" 1 0

# Dump raw grid state as JSON
scribe-test snapshot "$SESSION" /output/state.json
```

### Testing reconnection / session persistence

The daemon supports disconnect/reconnect testing via `session attach`:

```bash
SAVED_SESSION="$SESSION"
scribe-test daemon stop                      # disconnect from server
scribe-test daemon start                     # new connection
scribe-test session attach "$SAVED_SESSION"  # reattach to existing session
scribe-test send "$SAVED_SESSION" 'echo test\n'
scribe-test wait-output "$SAVED_SESSION" "test"
```

The reconnect test (`tests/e2e/func/reconnect.sh`) validates session survival across client disconnects.

### When to test

**MANDATORY**: After ANY code change that touches server, client, or protocol code, you MUST:
1. Run `just e2e` (builds release, rebuilds container, runs all functional tests)
2. Verify all pass before considering the change complete

Do NOT rely on `cargo test` alone — unit tests do not cover IPC, session lifecycle, reconnection, or screen content restoration. The E2E container tests are the source of truth for end-to-end correctness.

- After implementing a new feature: write a test script that exercises it
- After fixing a bug: write a test script that reproduces the scenario
- Before committing renderer changes: use the visual container and inspect pixel screenshots
- **After changing IPC, session lifecycle, or client startup**: run `func/reconnect.sh` to verify sessions survive disconnect/reconnect and screen content is restored
- **After changing client input, keybindings, or workspace layout**: run `visual/workspace-split.sh` to verify splits render correctly
- The smoke test (`tests/e2e/func/smoke.sh`) validates basic server->session->I/O->screenshot flow
- The reconnect test (`tests/e2e/func/reconnect.sh`) validates session persistence and reattachment
- The workspace split test (`tests/e2e/visual/workspace-split.sh`) validates keybinding-triggered workspace splits through the real client

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
│   └── Content Area
│       ├── Pane 1 (split within the active tab)
│       └── Pane 2 (split within the active tab)
├── Workspace B (another screen region)
│   ├── Tab Bar
│   │   ├── [Workspace Badge]
│   │   ├── [gap]
│   │   └── Tab 1 (session)
│   └── Content Area
│       └── Pane 1
└── Status Bar (single bar spanning full window width)
```

- **Workspace**: A region of the window. Creating a new workspace splits the window. Each workspace has its own tab bar, sessions, and pane layout.
- **Tab (Session)**: A shell session within a workspace, shown in that workspace's tab bar.
- **Pane**: A split within a tab's content area. Panes divide the active tab, not the workspace.
- **Workspace Badge**: Colored dot + workspace name shown in the tab bar. Only visible when 2+ workspaces are open. Separated from tabs by a gap.
- **Status Bar**: A single bar at the bottom of the window (not per-workspace). Shows: connection dot, focused workspace name, CWD, git branch, session count, hostname, and time. The workspace viewport is shrunk by `STATUS_BAR_HEIGHT` (24px) before workspace rects are computed, so the bar occupies the gap below all workspaces.

Workspaces are never tabbed — they always occupy visible screen real estate side by side.

### Key Design Decisions

- **`alacritty_terminal`** is used for terminal emulation state (grid, scrollback, cursor) — not Alacritty's rendering
- **Dual VTE parsers**: `alacritty_terminal` ignores custom OSC 1337 `AiState`, so a separate `OscInterceptor` runs in parallel on the same byte stream
- **Session persistence across UI restarts**: sessions live in a server-wide `LiveSessionRegistry` with detachable `ClientWriter`. When the UI disconnects, sessions keep running (PTY reader task continues feeding `Term`). On reconnect: client sends `ListSessions` → `AttachSessions`, server re-sets the writer and sends a `ScreenSnapshot` (converted to ANSI by the client to restore visible content). The `Term` state is always up-to-date because the reader task never stops.
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

**Panes**
- `Ctrl+Shift+\` — split pane vertical (side-by-side)
- `Ctrl+Shift+-` — split pane horizontal (top/bottom)
- `Ctrl+Shift+W` — close pane
- `Ctrl+Tab` — cycle focus to next pane
- `Ctrl+Alt+Left/Right/Up/Down` — focus pane in direction

**Workspaces**
- `Ctrl+Alt+\` — split workspace vertical (side-by-side)
- `Ctrl+Alt+-` — split workspace horizontal (top/bottom)

**Terminal Shortcuts** (configurable, send escape sequences to PTY)
- `Ctrl+Left/Right` — word left/right
- `Alt+Backspace` — delete word backward
- `Ctrl+Backspace` — delete word backward (ctrl)
- `Ctrl+Delete` — delete word forward
- `Ctrl+Home/End` — jump to line start/end
- `Alt+<char>` — sends ESC + char (readline: Alt+B/D/F/. etc.)
- `Shift+Arrow` — xterm modifier-encoded selection
- `Ctrl+Shift+Arrow` — xterm modifier-encoded selection by word
- `Shift+Tab` — backtab

**General**
- `Ctrl+Shift+N` — open new window
- `Ctrl+,` — open settings

### IPC Security

Both the main IPC socket and handoff socket verify peer UID via `SO_PEERCRED` — connections from different users are rejected. Socket directories are created with 0700 permissions.
