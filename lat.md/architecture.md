# Architecture

Scribe is a GPU-accelerated terminal emulator with a client-server split and first-class AI process awareness.

## Design Philosophy

The UI (client) and process manager (server) are separate OS processes connected over a Unix domain socket. Sessions survive client restarts, crashes, and upgrades because the server owns PTY lifetime independently.

## Crate Map

The workspace contains eight crates, each with a focused responsibility.

### scribe-common

Shared types used by every other crate: the IPC [[protocol]], error definitions, [[protocol#Screen Snapshots]], configuration, theme system, and socket path conventions. This is a leaf dependency with no internal cross-crate references.

### scribe-pty

Low-level [[pty]] management: async file descriptor wrappers for zero-copy PTY I/O, OSC sequence interception running in parallel with alacritty_terminal's parser, and metadata extraction (CWD, title, AI state) from terminal output streams.

### scribe-server

Long-running daemon that owns PTY sessions, manages [[server#Workspaces]] with auto-naming, coordinates [[server#Handoff]] for zero-downtime upgrades via SCM_RIGHTS fd passing, and handles software [[server#Updater]].

### scribe-client

GPU-accelerated frontend that renders terminal [[client#Panes]] via wgpu, handles keyboard/mouse [[client#Input]], manages a two-level [[client#Layout]] (window splits into workspaces, workspaces hold tabbed pane trees), and communicates with the server over IPC.

### scribe-renderer

[[rendering]] pipeline: glyph atlas powered by cosmic-text with ligature support, wgpu instanced-quad draw calls, procedural box-drawing rasterizer, xterm-256 colour palette, and chrome quad builders for UI elements.

### scribe-settings

[[settings]] UI: webview-based configuration editor for appearance, keybindings, themes, workspace roots, and AI indicator behaviour. Changes are saved to TOML and picked up by a file watcher.

### scribe-cli

Thin CLI entry point that launches the client process.

### scribe-test

Integration test harness with PTY capture, IPC helpers, and assertion utilities.

## Build Tooling

Scripts and helpers used during local development builds.

### Restart Recipes

`just restart-server` and `just restart-server-release` invoke the server binary directly with `--upgrade` to trigger a zero-downtime hot-reload of the running server without rebuilding.

### Restart Approval Policy

Manual server restarts during active work require explicit user approval because even zero-downtime handoff attempts can still disrupt in-flight tasks and connected clients.

### Package Install Flow

`just install` runs `just deb` (release build + `cargo deb`) then `sudo dpkg -i` which triggers smart reload via the deb postinst script.

The `preinst` captures PIDs of all running components and reasserts ownership on `/run/user/{uid}/scribe` before it writes upgrade state. The `postinst` SHA-256-compares each running binary (`/proc/PID/exe`) against the installed copy so only changed binaries are restarted after a successful hot-reload. The server still uses [[server#Handoff]] for zero-downtime hot-reload; client and settings are normally relaunched only when their binary changed. See [[server#Handoff#Binary Change Detection]].

If Linux hot-reload fails because the handoff state version changed, `postinst` falls back to a true cold restart: it reloads the user unit, stops it, kills any detached `scribe-server` processes still holding the lock/socket, clears stale sockets, resets the failed unit state, and then starts the new server. The installer shows only a high-level warning, and it asks the user to save work only when the original server PID is still alive after the failed handoff attempt. Because the UI client exits on server disconnect, a previously running client is relaunched after this cold restart even when the client binary itself is unchanged. If that restart fails, the package script now skips client and settings relaunches instead of piling new processes onto a broken server.

Settings relaunches also wait for the old singleton to release its lock and socket, then escalate to SIGKILL before starting the replacement if the old process refuses to exit. This prevents the new binary from exiting immediately after focusing an older instance.

## Data Flow

Terminal I/O flows through a well-defined pipeline from shell process to screen pixel.

### Write Path

User keystrokes travel from the client through IPC to the server, which writes them to the PTY master fd. The shell reads from the PTY slave and processes the input.

Keyboard-originated input is marked so the server can clear persisted attention states before the next reconnect or handoff snapshot.

Clipboard pastes follow the same path but may exceed the 4 KiB [[protocol#Client Messages#Terminal I/O]] message limit. The client chunks large pastes into multiple `KeyInput` messages, with bracketed-paste markers on the first and last chunks only.

### Read Path

Shell output flows from the PTY master fd through the server's ANSI processor ([[crates/scribe-pty/src/osc_interceptor.rs#OscInterceptor]]) and metadata parser ([[crates/scribe-pty/src/metadata.rs#MetadataParser]]), then is serialized as a [[protocol#Screen Snapshots]] and sent to the attached client for GPU rendering.

### Reconnect Path

When a client reconnects, the server sends a full screen snapshot of every subscribed session. The client rebuilds its terminal grid from this snapshot without any visible gap.

Active AI state is restored from the `SessionList` response before `AttachSessions` so the [[client#AI Indicator]] tracker is populated immediately. The same response also carries an AI provider hint for sessions whose visible attention state was already dismissed, so reconnect preserves provider-aware behavior without reviving the indicator. The per-session `AiStateChanged` messages from `send_stored_metadata` arrive later as an idempotent overwrite.

Sessions that were active during a [[server#Handoff]] retain their pre-handoff snapshot for the first attaching client.
