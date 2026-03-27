<p align="center"><img src="dist/scribe-icon-256.png" width="128" /></p>

<h1 align="center">Scribe</h1>

<p align="center">A GPU-accelerated terminal emulator with a client-server architecture and first-class AI awareness.</p>

<p align="center">
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-1.87%2B-orange" alt="Rust: 1.87+" /></a>
</p>

---

## Features

- [Client-Server Architecture](#client-server-architecture)
- [Zero-Downtime Upgrades](#zero-downtime-upgrades)
- [AI / LLM Process Awareness](#ai--llm-process-awareness)
- [GPU-Accelerated Rendering](#gpu-accelerated-rendering)
- [Workspaces](#workspaces)
- [Panes](#panes)
- [Tabs](#tabs)
- [Session Persistence](#session-persistence)
- [Themes](#themes)
- [Configurable Keybindings](#configurable-keybindings)
- [Settings UI](#settings-ui)
- [Scrollbar](#scrollbar)
- [Multi-Window Support](#multi-window-support)
- [IPC Security](#ipc-security)

## Why Scribe?

Most terminal emulators bundle the UI and the process manager into one inseparable application. Crash the terminal, lose every shell. Update the terminal, lose every shell. Scribe separates these concerns: the server runs headless and owns your PTY sessions; the GPU client connects on demand. Your shells, SSH tunnels, and long-running builds persist independent of what happens to the client.

**Sessions that survive reality.** The server never stops reading from the PTY. Disconnect, close the laptop, come back days later — on reconnect, Scribe sends you a full screen snapshot of exactly where you left off. No "session expired" dialogs. No lost scrollback.

**See your AI agent's state at a glance.** Scribe natively parses OSC 1337 escape sequences from AI coding tools. The status bar shows whether your agent is idle, processing, waiting for approval, or errored — plus the active tool, model, and context window usage. No more window-switching to find out why your agent stopped.

**Zero-downtime upgrades.** Upgrading Scribe doesn't interrupt your sessions. The running server hands off all PTY file descriptors to the new process via `SCM_RIGHTS` — your shells don't even notice the version changed.

**Smooth rendering at any scale.** Every frame runs on the GPU via wgpu (Vulkan, Metal, or OpenGL ES). A shared glyph atlas keeps text crisp at any DPI, and all visible panes render in a single draw call — keeping Scribe responsive even with dozens of panes across multiple workspaces.

## Quick Start

### Prerequisites

- Rust 1.87+
- Linux: `libgtk-4-dev`, `libvulkan-dev`
- macOS: Xcode Command Line Tools

### Build from source

```bash
git clone https://github.com/scribe-terminal/scribe
cd scribe
just build-release
```

### Run

```bash
just server &   # start the PTY server
just client     # launch the GPU client
```

### Install on Linux (Debian/Ubuntu)

```bash
just install    # builds, packages .deb, and installs
```

### Install on macOS

```bash
just dmg        # builds .app bundle and .dmg installer
```

## Configuration

Scribe reads its configuration from `~/.config/scribe/config.toml`. Both the server and client share the same config file.

```toml
[appearance]
font = "JetBrains Mono"
font_size = 14.0
theme = "catppuccin-mocha"
opacity = 1.0
cursor_shape = "block"

[terminal]
scrollback_lines = 10000
copy_on_select = true
claude_code_integration = true

[workspaces]
roots = ["~/work", "~/projects"]
```

Open the graphical settings editor with `Ctrl+,` to modify configuration without editing the file directly.

## Keyboard Shortcuts

### Panes

| Action | Default Shortcut |
|--------|-----------------|
| Split vertical | `Ctrl+Shift+\` |
| Split horizontal | `Ctrl+Shift+-` |
| Close pane | `Ctrl+Shift+W` |
| Cycle focus | `Ctrl+Tab` |
| Focus direction | `Ctrl+Alt+Arrow` |

### Workspaces

| Action | Default Shortcut |
|--------|-----------------|
| Split workspace vertical | `Ctrl+Alt+\` |
| Split workspace horizontal | `Ctrl+Alt+-` |
| Cycle workspace | `Ctrl+Alt+Tab` |

### Tabs

| Action | Default Shortcut |
|--------|-----------------|
| New tab | `Ctrl+Shift+T` |
| Close tab | `Ctrl+Shift+Q` |
| Next/Previous tab | `Ctrl+PageDown/Up` |
| Select tab 1-9 | `Ctrl+1-9` |

### General

| Action | Default Shortcut |
|--------|-----------------|
| Copy | `Ctrl+Shift+C` |
| Paste | `Ctrl+Shift+V` |
| Zoom in/out/reset | `Ctrl+=/-/0` |
| Settings | `Ctrl+,` |
| New window | `Ctrl+Shift+N` |

All keybindings are fully configurable in `config.toml` under `[keybindings]`.

## Feature Details

### Client-Server Architecture

Scribe separates the terminal into two processes: `scribe-server` manages PTY sessions, and `scribe-client` provides the GPU-rendered UI. They communicate over a Unix domain socket (`/run/user/{uid}/scribe/server.sock`) using length-prefixed msgpack serialization. The server runs as a systemd user service. Clients are stateless and replaceable — crash one, start another, and reattach to your sessions instantly.

### Zero-Downtime Upgrades

When a new server binary is available, the running server hands off all PTY file descriptors and serialized session state to the new process via `SCM_RIGHTS` on a dedicated handoff socket (`/run/user/{uid}/scribe/handoff.sock`). The new server reconstructs sessions, workspaces, and pane layouts from the handoff state. Supports up to 256 PTY file descriptors and 16 MiB of serialized state. Triggered via `scribe-server --upgrade`.

### AI / LLM Process Awareness

Scribe natively parses OSC 1337 escape sequences emitted by AI coding tools like Claude Code. It tracks four AI states: idle/prompt, processing, waiting for permission, and error. Metadata includes the active tool, agent name, model, and context window usage percentage. The status bar surfaces this information in real time, giving developers instant visibility into their AI agent's state without switching windows.

### GPU-Accelerated Rendering

Built on wgpu with backends for Vulkan, Metal, and OpenGL ES. Text is shaped by cosmic-text with a shared glyph atlas across all panes. All visible panes are rendered in a single instance-buffered draw call per frame. Supports font ligatures, variable font weight, configurable cursor shapes (block/underline/bar), and cursor blink.

### Workspaces

Workspaces split the window into independent regions, each with its own tab bar and pane layout. They are always visible side by side — never tabbed or hidden. Auto-naming matches session working directories against configured `workspace_roots` to name workspaces from the project directory. Workspace badges (colored dots) appear in the tab bar when multiple workspaces are open.

### Panes

Panes split the active tab's content area using a binary tree layout. Split vertically (side-by-side) or horizontally (top/bottom). Navigate between panes with directional focus (`Ctrl+Alt+Arrow`) or cycle through them with `Ctrl+Tab`. Dividers are draggable for custom sizing.

### Tabs

Each workspace has its own tab bar with independent sessions. Create, close, and switch tabs with keyboard shortcuts. Direct tab selection via `Ctrl+1` through `Ctrl+9`. Tabs display the session title derived from OSC 0/2 escape sequences.

### Session Persistence

Sessions are owned by the server and survive client disconnects. The PTY reader task continues feeding the terminal state even when no client is attached. On reconnect, the client sends `AttachSessions` and receives a `ScreenSnapshot` — a full ANSI-encoded dump of the visible terminal content. Scrollback, cursor position, and all terminal state are preserved.

### Themes

Ships with 5 curated presets (Minimal Dark, Tokyo Night, Catppuccin Mocha, Dracula, Solarized Dark) plus 187 community presets imported from popular terminal color schemes. Themes control both terminal ANSI colors and UI chrome (tab bar, status bar, dividers, accent colors). Custom themes can be defined inline in `config.toml` or loaded from external `.toml` files.

### Configurable Keybindings

Every keyboard shortcut is configurable in `config.toml` under `[keybindings]`. Each action supports up to 5 alternative key combinations. Accepts both single strings and arrays: `close_pane = "ctrl+shift+w"` or `close_pane = ["ctrl+shift+w", "ctrl+w"]`. Over 30 bindable actions covering panes, workspaces, tabs, clipboard, zoom, and terminal navigation.

### Settings UI

A standalone settings window (`scribe-settings`) opens with `Ctrl+,`. Built as a webview using wry with HTML/CSS/JS. Changes are applied live without restarting. Singleton-enforced — opening settings twice focuses the existing window. Covers appearance, terminal behavior, keybindings, and workspace configuration.

### Scrollbar

macOS-style overlay scrollbar that fades in on scroll activity and fades out after 1.5 seconds of inactivity. Supports click-to-jump and drag-to-scroll. Minimal visual footprint with smooth opacity transitions (0.3s fade duration).

### Multi-Window Support

Open additional terminal windows with `Ctrl+Shift+N`. Each window connects independently to the server. The server tracks window ownership for session management. Windows share the same session pool — a session can be moved between windows.

### IPC Security

Both the main IPC socket and the handoff socket verify the connecting peer's UID via `SO_PEERCRED`. Connections from different users are rejected. Socket directories are created with `0700` permissions. All sockets are located under `/run/user/{uid}/scribe/`.

## Architecture

```
crates/
├── scribe-common     # Shared types: protocol, config, themes, IDs
├── scribe-pty        # PTY I/O, OSC interceptor, metadata parser
├── scribe-server     # Session/workspace management, IPC, handoff
├── scribe-client     # GPU client: winit + wgpu, pane layout, input
├── scribe-renderer   # GPU pipeline: glyph atlas, color palette, wgpu
├── scribe-settings   # Settings webview: wry, HTML/CSS/JS assets
├── scribe-cli        # Headless test CLI: raw stdin/stdout over IPC
└── scribe-test       # E2E test harness with subcommands
```

The client sends `ClientMessage` (key input, resize, session operations) serialized as length-prefixed msgpack over a Unix domain socket. The server responds with `ServerMessage` (PTY output, screen snapshots, metadata changes) using the same framing. Terminal emulation state is managed server-side by `alacritty_terminal`, with a parallel OSC interceptor for AI-specific escape sequences.

## License

Scribe is dual-licensed under [MIT](https://opensource.org/licenses/MIT) and [Apache 2.0](https://www.apache.org/licenses/LICENSE-2.0).
