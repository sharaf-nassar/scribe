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

### Lint Suppression Guard

New Rust lint suppressions are blocked by a committed baseline so contributors must fix the underlying warning instead of adding `#[allow]` or `#[expect]`.

`tools/check-no-new-lint-suppressions.sh` scans the staged, working, or CI target tree and compares the discovered suppression inventory against `tools/lint-suppressions-allowlist.txt`. That keeps the repo's three narrowly scoped unavoidable suppressions explicit while rejecting any drift. The guard runs in pre-commit, `just lint-suppressions`, and the normal pull-request quality workflow. `third_party/` is pruned from the scan so vendored upstream suppressions do not need allowlist entries.

### Vendored Third-Party Dependencies

The `third_party/` directory holds path-patched copies of external crates with outstanding upstream bugs, wired in via `[patch.crates-io]` in the root `Cargo.toml`.

The directory is excluded from the workspace (`exclude = ["third_party/*"]`) so workspace lints do not apply to vendored code.

Current entries:

- `third_party/unix-ancillary/` — local fork of `unix-ancillary 0.1.0`. Upstream 0.1.0 fails to compile on Apple targets because `ancillary.rs::set_cloexec` references `io::Result`/`io::Error` without importing `std::io`. The fork adds a cfg-gated `use std::io;` that mirrors the function's own cfg. Remove once a fixed release ships on crates.io.

### Package Install Flow

`just install` builds and installs the stable `scribe` package, while `just install-dev` builds and installs an isolated `scribe-dev` package with renamed binaries, service unit, and share directory.

The Debian maintainer scripts branch on the package name so `scribe` manages `/run/user/{uid}/scribe`, `scribe-server`, and `/usr/share/scribe`, while `scribe-dev` manages `/run/user/{uid}/scribe-dev`, `scribe-dev-server`, and `/usr/share/scribe-dev`. Each package now ships the full Codex hook source set in that share directory, including `setup-codex-hooks.sh`, `detect-claude-question.sh`, `detect-codex-question.sh`, and `codex-task-label.sh`, so `postinst` can seed `~/.codex/hooks` without missing-source failures. When installs run through a privileged helper, the scripts derive the desktop user from `SUDO_UID` or `PKEXEC_UID`, which keeps updater-driven `pkexec dpkg -i` installs targeting the real user session instead of root's `/run/user/0`. All `pgrep`/`pkill` calls in maintainer scripts use `-f` (full cmdline match against the absolute binary path) instead of `-x` (match against the kernel comm field), because Linux truncates comm to 15 characters (`TASK_COMM_LEN`) and dev-flavor binary names like `scribe-dev-server` (17 chars) and `scribe-dev-settings` (19 chars) exceed that limit. The `preinst` captures PIDs of the active flavor before install, and the `postinst` compares the running binaries (`/proc/PID/exe`) against the installed copies so only changed binaries are restarted after a successful hot-reload. Before any relaunch, `postinst` also migrates legacy prompt-bar color overrides in the flavor-specific `config.toml`: `prompt_bar_bg` is rewritten to `prompt_bar_second_row_bg`, and when an old `prompt_bar_first_row_bg` override is present the script remaps both saved colors through the old mixed-fill formulas so the new exact-fill prompt bar preserves the user's previous appearance instead of jumping to a harsher direct row fill. Linux server restart decisions also compare a persisted `server-runtime-generation` stamp under `/run/user/{uid}/{app}/`; the stamp is now an opaque hash of launch-critical `postinst` behavior plus the installed user service unit, so changes to runtime environment inheritance or restart flow force a hot-reload even when `/usr/bin/scribe-server` is byte-identical. Linux hot-reload and client relaunches preserve the active GUI session variables (`DISPLAY`, `WAYLAND_DISPLAY`, `XDG_SESSION_TYPE`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`, `XAUTHORITY`) so the replacement server keeps clipboard and display access for child PTY sessions. `postinst` now prefers `systemctl --user show-environment` values for those variables and only falls back to the invoking shell when the user manager does not provide them. The server still uses [[server#Handoff]] for zero-downtime hot-reload; client and settings are normally relaunched only when their binary changed. Client relaunches now wait for every recorded client PID to exit, escalate to SIGKILL when needed, and skip relaunch if an old client survives so a fresh replacement client cannot cold-restore a duplicate window before the server clears the old connection. See [[server#Handoff#Binary Change Detection]].

If Linux hot-reload fails because the handoff state version changed, `postinst` normally falls back to a true cold restart: it reloads the matching user unit, stops it, kills any detached flavor-specific server processes still holding the lock/socket, clears stale sockets, resets the failed unit state, and then starts the new server. The installer shows only a high-level warning, and it asks the user to save work only when the original server PID is still alive after the failed handoff attempt. Auto-update installs now set a runtime defer marker first, so that same failure path can leave the old server running, persist an `update-restart-required` flag, and skip client/settings relaunches until the UI explicitly approves the cold restart. Once approved, a detached client helper performs the cold restart and relaunches one fresh client after the previous windows exit, which preserves the existing cold-restore replay flow. If a non-deferred cold restart fails, the package script skips client and settings relaunches instead of piling new processes onto a broken server.

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

Sessions that were active during a [[server#Handoff]] retain their pre-handoff snapshot for the first attaching client. After a handoff the workspace tree stored by the old client may reference workspace IDs that differ from the new server's session workspace IDs; the client detects this mismatch (empty join between tree workspace order and session groups) and falls back to session-based reconstruction.

Prompt bar state (`first_prompt`, `latest_prompt`, `prompt_count`) is client-side only and not part of `SessionList` or the handoff protocol. During hot restart reattach, [[crates/scribe-client/src/main.rs#App#apply_snapshot_prompt_state]] reads the cold restart snapshot saved by the previous client and copies prompt data to matching panes by `conversation_id`.

### Cold Restart Restore

When the server crashes or is killed and relaunched, all PTY sessions are lost. The client detects a cold restart by receiving an empty `SessionList` while a restore snapshot exists on disk, then replays the previous window layout.

The restore pipeline has three layers: [[crates/scribe-client/src/restore_state.rs#RestoreStore]] persists per-window snapshots and a global index under `$XDG_STATE_HOME/{flavor}/restore/`, [[crates/scribe-client/src/restore_replay.rs#snapshot_window_restore]] captures the current layout, and [[crates/scribe-client/src/restore_replay.rs#prepare_replay]] rebuilds the layout from a snapshot. Snapshots are saved on a debounced timer after every layout change. On explicit close or quit the snapshot is removed; on server crash it is preserved. Multiple windows are restored by having the first client claim the first index entry and spawn `--restore-child` processes for the rest, so only the bootstrap client fans out additional windows. Because a true cold restart connects to a fresh server that already assigned new window IDs in `Welcome`, the client reapplies geometry from the claimed snapshot's original window ID before replaying panes. The claim step prunes stale index IDs whose per-window snapshot file is missing or unreadable before computing the remaining-window count, which prevents partial restore-state corruption from spawning duplicate fresh windows.
