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

`just install` builds and installs the stable `scribe` package, while `just install-dev` builds and installs an isolated `scribe-dev` package with renamed binaries, service unit, and share directory.

The Debian maintainer scripts branch on the package name so `scribe` manages `/run/user/{uid}/scribe`, `scribe-server`, and `/usr/share/scribe`, while `scribe-dev` manages `/run/user/{uid}/scribe-dev`, `scribe-dev-server`, and `/usr/share/scribe-dev`. Each package now ships the full Codex hook source set in that share directory, including `setup-codex-hooks.sh`, `detect-codex-question.sh`, and `codex-task-label.sh`, so `postinst` can seed `~/.codex/hooks` without missing-source failures. All `pgrep`/`pkill` calls in maintainer scripts use `-f` (full cmdline match against the absolute binary path) instead of `-x` (match against the kernel comm field), because Linux truncates comm to 15 characters (`TASK_COMM_LEN`) and dev-flavor binary names like `scribe-dev-server` (17 chars) and `scribe-dev-settings` (19 chars) exceed that limit. The `preinst` captures PIDs of the active flavor before install, and the `postinst` compares the running binaries (`/proc/PID/exe`) against the installed copies so only changed binaries are restarted after a successful hot-reload. Linux server restart decisions also compare a persisted `server-runtime-generation` stamp under `/run/user/{uid}/{app}/`; the stamp is now an opaque hash of launch-critical `postinst` behavior plus the installed user service unit, so changes to runtime environment inheritance or restart flow force a hot-reload even when `/usr/bin/scribe-server` is byte-identical. Linux hot-reload and client relaunches preserve the active GUI session variables (`DISPLAY`, `WAYLAND_DISPLAY`, `XDG_SESSION_TYPE`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`, `XAUTHORITY`) so the replacement server keeps clipboard and display access for child PTY sessions. `postinst` now prefers `systemctl --user show-environment` values for those variables and only falls back to the invoking shell when the user manager does not provide them. The server still uses [[server#Handoff]] for zero-downtime hot-reload; client and settings are normally relaunched only when their binary changed. See [[server#Handoff#Binary Change Detection]].

If Linux hot-reload fails because the handoff state version changed, `postinst` falls back to a true cold restart: it reloads the matching user unit, stops it, kills any detached flavor-specific server processes still holding the lock/socket, clears stale sockets, resets the failed unit state, and then starts the new server. The installer shows only a high-level warning, and it asks the user to save work only when the original server PID is still alive after the failed handoff attempt. Because the UI client exits on server disconnect, a previously running client is relaunched after this cold restart even when the client binary itself is unchanged. If that restart fails, the package script now skips client and settings relaunches instead of piling new processes onto a broken server.

Settings relaunches also wait for the old singleton to release its lock and socket, then escalate to SIGKILL before starting the replacement if the old process refuses to exit. `scribe-dev` additionally skips automatic Claude/Codex hook setup during install so the stable install's global hook configuration remains untouched.

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

Prompt bar state (`first_prompt`, `latest_prompt`, `prompt_count`) is client-side only and not part of `SessionList` or the handoff protocol. During hot restart reattach, [[crates/scribe-client/src/main.rs#App#apply_snapshot_prompt_state]] reads the cold restart snapshot saved by the previous client and copies prompt data to matching panes by `conversation_id`.

### Cold Restart Restore

When the server crashes or is killed and relaunched, all PTY sessions are lost. The client detects a cold restart by receiving an empty `SessionList` while a restore snapshot exists on disk, then replays the previous window layout.

The restore pipeline has three layers: [[crates/scribe-client/src/restore_state.rs#RestoreStore]] persists per-window snapshots and a global index under `$XDG_STATE_HOME/{flavor}/restore/`, [[crates/scribe-client/src/restore_replay.rs#snapshot_window_restore]] captures the current layout, and [[crates/scribe-client/src/restore_replay.rs#prepare_replay]] rebuilds the layout from a snapshot. Snapshots are saved on a debounced timer after every layout change. On explicit close or quit the snapshot is removed; on server crash it is preserved. Multiple windows are restored by having the first client claim the first index entry and spawn fresh processes for the rest.
