# Architecture

Scribe is a GPUI terminal client and a session-owning server joined by a frozen
local IPC protocol.

## Design Philosophy

The UI (client) and process manager (server) are separate OS processes connected over a Unix domain socket. Sessions survive client restarts, crashes, and upgrades because the server owns PTY lifetime independently.

## Crate Map

The workspace contains eight crates, each with a focused responsibility.

### scribe-common

Shared types used by every other crate: the IPC protocol, error definitions, configuration, theme system, and socket path conventions. This is a leaf dependency with no internal cross-crate references.

The protocol surface also carries the  /  message pair that backs the settings window's release-history panel.

### scribe-pty

Low-level  management: async file descriptor wrappers for zero-copy PTY I/O, OSC sequence interception running in parallel with alacritty_terminal's parser, and metadata extraction (CWD, title, AI state) from terminal output streams.

### scribe-server

Long-running daemon that owns PTY sessions, manages  with auto-naming and durable , coordinates  for zero-downtime upgrades via SCM_RIGHTS fd passing, and handles software . Also backs the  panel via the  and the new  message.

Feature 013 adds the owning side of : a Tailscale-gated TCP listener ( for LocalAPI identity) that lets another of the user's tailnet machines attach to a window, off by default.

### scribe-client

GPU frontend that renders , handles , manages , renders  from server snapshots, and speaks IPC.

It also owns the connecting side of  (feature 013): the connect picker, the auto-reconnect state machine, and the displaced/lost-control rendering for a window dialed on another tailnet machine.

### Retired renderer

The former standalone renderer is deleted. Its palette, shaping, and
box-drawing responsibilities now live in the GPUI client's terminal paint path.

### Integrated settings

The former `scribe-settings` binary is deleted. `scribe-client --settings`
opens the integrated GPUI window, which writes the same TOML configuration and
uses the same live watcher path.

The settings window also shows a  browser fed over IPC and a  sourced from the build-time workspace version.

Stateful actions that need an immediate server-side response (e.g. the Updates page's "Check Now" button) open a transient `server.sock` connection via  instead of going through the config file.

### scribe-cli

Thin CLI entry point that launches the client process.

### scribe-hook-helper

Tiny binary invoked by AI-tool hook adapter scripts and shell-integration scripts to emit one  to the server and exit 0.

Reads `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID` from env (both injected by Scribe into every PTY it spawns); no-ops silently when either is unset. After writing its frame it waits for the server's EOF inside the existing 100 ms total budget, keeping the Unix connection alive while macOS verifies the sender with `getpeereid`; exiting immediately after the write raced that check into `ENOTCONN` and dropped otherwise complete events. Argv carries only fixed selectors (`--provider`, `--event`, `--state`, `--fill-percent`, `--conversation-id`, `--baseline-ready`); every value-bearing field arrives as a JSON document on stdin under `--payload-stdin`. The per-provider adapters in `dist/ai-hook-*.sh` translate AI tool hook stdin JSON into that document, and the shell-integration scripts stream `{"added": …, "removed": …}` the same way to feed `HookEventKind::EnvChanged` into the same channel. `/proc/<pid>/cmdline` is world-readable and one argument cannot exceed `MAX_ARG_STRLEN` (128 KiB), so the previous argv form both exposed prompts and exported secrets to every local account and silently lost oversized payloads to `E2BIG` — with the caller ignoring the exit status, the event simply vanished. A field the document omits still falls back to its old `--flag`, which is what keeps shells that were running across the upgrade working until they restart. Accepted `--event` tokens are the clap `snake_case` renames of `EventKind`, and a mis-spelled one is dropped at `Cli::try_parse` with a silent exit 0 (FR-007) — no socket is ever opened — so a unit test scans every literal `--event=` and `--provider=` value under `dist/` and fails when a shipped script passes a token the helper would reject. See  for the full pipeline and `specs/003-ai-hook-channel/` for the design docs.

### scribe-test

Integration test harness with PTY capture, IPC helpers, and assertion utilities.

## Build Tooling

Scripts and helpers used during local development builds.

### lat.md Agent Tooling

Agent-facing lat.md discovery and validation are host-owned, not part of Scribe's runtime or source tree.

The repository keeps architecture and test intent under `lat.md/`, while the
active agent host provides `lat_search`, `lat_section`, `lat_locate`,
`lat_expand`, `lat_refs`, and `lat_check`. The host also owns the lifecycle
policy that reminds agents to search before work and validates references and
documentation sync when work ends.

Scribe does not register a second project-local Pi extension under
`.pi/extensions/`. Keeping one registration owner prevents duplicate tool
names and competing lifecycle hooks when the host already supplies the same
surface.

This boundary also keeps Pi extension APIs, TypeBox schemas, TUI renderers, and
their package versions outside the application repository. Removing the local
extension changes where agent commands are registered; it does not change the
`lat.md/` format or the requirement to keep those documents synchronized with
Scribe behavior.

### Rust Toolchain and CI Images

Scribe pins Rust 1.95 for local builds, release CI, and the functional and visual test images so GPUI builds use one supported compiler and native dependency set.

The Docker test images install GPUI's font, TLS, Vulkan, Wayland, X11/XKB,
zstd, and Clang prerequisites. The visual image also selects lavapipe's
software Vulkan ICD, keeping rendering tests usable on CI runners without a
hardware Vulkan driver.

### Restart Recipes

`just restart-server` and `just restart-server-release` invoke the server binary directly with `--upgrade` to trigger a zero-downtime hot-reload of the running server without rebuilding.

### Restart Approval Policy

Manual server restarts during active work require explicit user approval because even zero-downtime handoff attempts can still disrupt in-flight tasks and connected clients.

### Release Script

The shared `release.sh` wrapper keeps release-note generation noninteractive so terminal probes cannot pollute confirmation prompts.

`tools/release-me/release.sh` invokes Codex with CI/no-colour terminal environment and, when `setsid` is available, runs the background `codex exec` in a fresh session without a controlling terminal. This prevents Codex's `/dev/tty` colour and cursor probes from being answered into the parent script's later `read -rp` prompts.

The release matrix builds the GPUI client on Apple Silicon and Intel macOS
runners. Each macOS job stages the GPUI executable with its server and
helpers, signs the app bundle, creates its DMG once, submits it for
notarization, staples the accepted ticket, then minisigns the release
artifact. Linux `.deb` releases continue through the same artifact and
publish path.

Release CI disables `Swatinem/rust-cache` caching for `~/.cargo/bin` and uses a cache prefix that excludes older bin-cached archives. This prevents restored caches from replacing the runner's `cargo` shim with unrelated binaries before the release build starts.

### Lint Suppression Guard

New Rust lint suppressions are blocked by a committed baseline so contributors must fix the underlying warning instead of adding `#[allow]` or `#[expect]`.

`tools/check-no-new-lint-suppressions.sh` scans the staged, working, or CI target tree and compares the discovered suppression inventory against `tools/lint-suppressions-allowlist.txt`. That keeps the repo's three narrowly scoped unavoidable suppressions explicit while rejecting any drift. The guard runs in pre-commit, `just lint-suppressions`, and the normal pull-request quality workflow. `third_party/` is pruned from the scan so vendored upstream suppressions do not need allowlist entries.

In `--working-tree` mode, `git worktree list --porcelain` resolves every other linked worktree of the repo (such as an implement-ready task checkout under `.worktrees/`) and prunes those paths from the walk, since a sibling worktree's own tracked source is not part of this working tree's suppression inventory. Staged and range scans already exclude other worktrees because `git checkout-index` and `git archive` only ever materialize tracked content for the target ref, so this pruning is `--working-tree`-only. `tools/check-no-new-lint-suppressions-tests.sh` (run via `just lint-suppressions-tests`) is the regression coverage: it builds an isolated fixture repo with a nested linked worktree to prove that worktree's suppressions are ignored while a real new suppression in tracked source is still rejected.

### Reachability Gate

The GPUI client's unreachable surface is pinned by a committed baseline, so a feature that compiles and passes unit tests but is never constructed by the running binary shows up as a number instead of hiding behind a green test run.

The 016 reachability audit (`specs/016-gpui-client-rebuild/reachability-audit.md`) found the crate shipping far more implemented surface than `main.rs` could reach: most library modules were never imported, most `ServerMessage` variants fell into a `_ => {}` arm, and most `LayoutAction` variants fell into a `_ => tracing::debug!` arm. Both catch-alls are gone. Every `LayoutAction` now reaches a real handler, so the swallowed-action counter has been deleted along with its arm; inbound messages the client does not act on go through , which names the variant, increments a process counter, and logs at `warn`, so an unimplemented surface is observable at runtime rather than silent.  holds the exhaustive variant table that supplies those names, which makes adding a protocol variant a compile error until someone decides what the client does with it.

`tools/check-reachability.sh` re-derives the three metrics from source — library modules against the binary's transitive import closure, `ServerMessage` variants against , and `LayoutAction` variants against  — prints them as one `reachability: …` line, and compares the unreachable sets against `tools/reachability-baseline.txt`. Following library-to-library imports keeps live helpers such as `focus_border` and `palette` from appearing dead merely because `main.rs` reaches them through `ai_indicator` and `color`. The check fails when the unreachable set grows *and* when a baseline entry has become reachable, so the baseline can only shrink and every wiring bead has to record its progress. It runs in pre-commit (`reachability-baseline`) and in `just reachability`, which `just ready` invokes alongside the lint-suppression guard.

### GPUI Chrome Accessibility

The GPUI chrome accessibility audit records the custom titlebar, status chrome,
and settings accessibility blockers and their remediation beads.

`specs/016-gpui-client-rebuild/accessibility-audit.md` separates keyboard
operation from AccessKit semantics:
and  currently
track only root focus while their click targets lack roles, names, and state.
The audit assigns five defects to `scribe-38e.108` through `scribe-38e.112`;
closure requires keyboard-only and real screen-reader verification, plus a
debug accessibility-tree check for stable unique nodes.

The AI prompt strip and window status bar each expose one stable AccessKit
status node with a concise state label, leaving decorative spans anonymous.
Pane connection and error feedback share the window status node instead of
creating a redundant band. An actionable update label is a focusable button that accepts
pointer, Enter/Space, and AccessKit Click activation through the same dialog.

### Parity Inventory Gate

The 016 launch gate's parity metric is a row count read off `specs/016-gpui-client-rebuild/parity-inventory.md`, so every number in that document is derived from its own tables rather than typed, and a check fails the build when the two disagree.

The inventory gives each of the 203 rows a "Reachable from" cell that either names a live-path symbol or carries an em-dash `— (unwired …)` / `— (missing …)` marker. `tools/check-parity-inventory.sh` recounts all six tables from those cells and verifies the section headings' declared row counts, the six `**Reachability:**` footers, the roll-up table including its Total row, and the user-facing sentence with its percentage and its in-client figure. Before this gate the file had drifted 112 rows behind the binary — it still read 51 of 164 user-facing rows reachable while the true figure was 164 of 165 — because roughly 29 wiring beads had landed since the counts were written.

The same check cross-references the source, so the document cannot describe a client that does not exist: the two message tables must enumerate exactly the `ClientMessage` and `ServerMessage` variants of `crates/scribe-common/src/protocol.rs`, the keybinding table exactly the `Bindings` actions of `crates/scribe-client/src/input.rs`, and any `ServerMessage` row  does not handle must be annotated a settings-window row (the five variants the settings window's synchronous request/reply helper consumes). It runs in pre-commit (`parity-inventory`), in `just parity-inventory` which `just ready` invokes, and in the pull-request quality workflow.

#### Requirement Derivation

The row set is derived from `spec.md`'s requirement register, not from the legacy client's IPC surface, so the reachable-row total measures parity rather than the subset that happened to be tabulated.

Every acceptance criterion and porting obligation in `specs/016-gpui-client-rebuild/spec.md` carries a stable append-only id (`US<n>-<n>`, `PO-<n>`). `parity-inventory.md` carries a coverage index mapping each id onto the rows that carry it, plus a `Spec behaviour requirements` table holding the rows no message, keybinding or rendering row already carried. `tools/check-parity-inventory.sh` fails when an id has no carrying row, when the index names a row no table contains, or when it names an id the spec does not declare — so adding a requirement breaks the build until it has a row and a verdict.

This closed the systemic hole behind the 2026-07-27 NO-GO: nine spec requirements — mouse reporting, mouse-wheel scrolling, IME composition, cold-restart restore, the command-mark scrollbar, window geometry persistence, the desktop notification dispatcher, server lifecycle management, and file drag-and-drop — had never been enumerated, so no oracle scored them and the gate read 163 of 164 rows reachable while nine requirements were missing from the product. The escape hatch is narrow: tree, licensing and CI requirements (`US5-*`, `US6-*`) are marked `not a parity row` with the artifact that gates them, because no reachable client symbol can carry them.

Widening the census surfaced requirements with no live path. Pane dividers and the AI indicator's painted half are now wired; server-upgrade reattach and the remote connect picker remain missing. The AI gap was invisible to the module ratchet because `ai_indicator` is imported by `main.rs`; module-level reachability is a floor, not a substitute for a per-requirement row.

#### Go Threshold

Cutover requires every user-facing row to be reachable — zero unwired and zero missing — so the gate criterion is a ratio, not a tolerance, and `just parity-gate` scores it mechanically.

`tools/check-parity-inventory.sh --gate` runs the same derivation as the drift check and then exits non-zero while any row carries an unreachable marker, printing each offending row. It stays out of pre-commit and out of `just ready`, because it is meant to fail until the wiring beads it measures have landed; the drift check must stay green throughout. After FU-25 it scores 191 of 194 user-facing rows.

The threshold is derived rather than picked: `spec.md` Goal 1 is full reachable parity with no user-visible regression, and the inventory's definition of done makes a row done only when it names a live-path symbol and its verification method passes — so an unreachable row is a regression and the spec grants no budget for one. The denominator moves with the requirement register, and the only relief valve is descoping a requirement in `spec.md` with a recorded decision, which deletes its row and shrinks the denominator instead of lowering the bar. `plan.md` § "Phase H re-baseline" holds the statement of record.

### Vendored Third-Party Dependencies

The `third_party/` directory holds vendored copies of external crates that Scribe cannot consume as published, for two distinct reasons: an outstanding upstream bug, or a trust boundary that requires a reduced attack surface.

The directory is excluded from the workspace (`exclude = ["third_party/*"]`) so workspace lints do not apply to vendored code. Each entry carries its own `README.md` recording the upstream package, VCS revision, crates.io checksum, license files, fork delta, and named security ownership, plus a `DEPENDENCY-TREE.txt` pinning its transitive set. The attribution for every entry also appears in the repository `NOTICE`.

Bug workaround, wired in via `[patch.crates-io]` in the root `Cargo.toml`:

- `third_party/unix-ancillary/` — local fork of `unix-ancillary 0.1.0`. Upstream 0.1.0 fails to compile on Apple targets because `ancillary.rs::set_cloexec` references `io::Result`/`io::Error` without importing `std::io`. The fork adds a cfg-gated `use std::io;` that mirrors the function's own cfg. Remove once a fixed release ships on crates.io.

Trust-boundary forks, consumed as path dependencies rather than patches because their public API is deliberately not the upstream one. Both decode untrusted PTY bytes, forbid `unsafe`, drop every encoder and indirect-resource path, and take caller-owned limits, budgets, deadlines, and cancellation hooks. Replacing either with its stock crate, a C decoder, or a generic image library requires a new trust-boundary review — see [[terminal-images#Bounded Sixel Decoder]] and [[terminal-images#Bounded Kitty PNG Decoder]]:

- `third_party/icy-sixel-decoder/` — decoder-only fork of `icy_sixel 0.5.0`, MIT OR Apache-2.0, consumed by `scribe-server`.
- `third_party/image-png-decoder/` — decoder-only fork of `png 0.18.1` published locally as `scribe-png-decoder`, MIT OR Apache-2.0, consumed by `scribe-common`.

Scribe maintainers own CVE and RustSec review, upstream-diff review, and emergency patches for both decoder forks. Each upstream release and each regular dependency advisory audit triggers a diff against the pinned revision; audited decoder security fixes are ported, the adversarial Docker corpus, `cargo deny`, and the dependency tree are rerun, and the pin, checksum, fork delta, and licenses are updated together. Report a suspected vulnerability privately through a GitHub security advisory on this repository rather than a public issue.

### Package Install Flow

`just install` builds and installs the stable `scribe` package, while `just install-dev` builds and installs an isolated `scribe-dev` package with renamed binaries, service unit, and share directory.

The Debian maintainer scripts branch on the package name so `scribe` manages `/run/user/{uid}/scribe`, `scribe-server`, and `/usr/share/scribe`, while `scribe-dev` manages `/run/user/{uid}/scribe-dev`, `scribe-dev-server`, and `/usr/share/scribe-dev`. Each package ships `scribe-hook-helper`, Claude/Codex hook adapters, their setup scripts, and shell integration files in that share directory so `postinst` can seed supported AI hooks without missing-source failures. When installs run through a privileged helper, the scripts derive the desktop user from `SUDO_UID` or `PKEXEC_UID`, which keeps updater-driven `pkexec dpkg -i` installs targeting the real user session instead of root's `/run/user/0`. All `pgrep`/`pkill` calls in maintainer scripts use `-f` (full cmdline match against the absolute binary path) instead of `-x` (match against the kernel comm field), because Linux truncates comm to 15 characters (`TASK_COMM_LEN`) and dev-flavor binary names like `scribe-dev-server` (17 chars) and `scribe-dev-settings` (19 chars) exceed that limit. For client/settings PID capture, `preinst` seeds candidates with `pgrep -f` and then filters by `/proc/{pid}/exe`, because `/usr/bin/scribe-dev` is a prefix of the dev server and settings binary paths. The `preinst` captures PIDs of the active flavor before install, and the `postinst` compares the running binaries (`/proc/PID/exe`) against the installed copies so only changed binaries are restarted after a successful hot-reload. Before any relaunch, `postinst` also migrates legacy prompt-bar color overrides in the flavor-specific `config.toml`: `prompt_bar_bg` is rewritten to `prompt_bar_second_row_bg`, and when an old `prompt_bar_first_row_bg` override is present the script remaps both saved colors through the old mixed-fill formulas so the new exact-fill prompt bar preserves the user's previous appearance instead of jumping to a harsher direct row fill. Linux server restart decisions also compare a persisted `server-runtime-generation` stamp under `/run/user/{uid}/{app}/`; the stamp is now an opaque hash of launch-critical `postinst` behavior plus the installed user service unit, so changes to runtime environment inheritance or restart flow force a hot-reload even when `/usr/bin/scribe-server` is byte-identical. Linux hot-reload and client relaunches preserve the active GUI session variables (`DISPLAY`, `WAYLAND_DISPLAY`, `XDG_SESSION_TYPE`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`, `XAUTHORITY`) so the replacement server keeps clipboard and display access for child PTY sessions. `postinst` now prefers `systemctl --user show-environment` values for those variables and only falls back to the invoking shell when the user manager does not provide them. The server still uses  for zero-downtime hot-reload; client and settings are normally relaunched only when their binary changed. Client relaunches now wait for every recorded client PID to exit, escalate to SIGKILL when needed, and skip relaunch if an old client survives so a fresh replacement client cannot cold-restore a duplicate window before the server clears the old connection. The shared `wait_for_pid_exit` helper in `dist/debian/postinst` treats a PID in zombie (`Z`) state as exited (via `pid_is_zombie`, which reads `/proc/$pid/status`), because a zombie's task is already gone and SIGTERM/SIGKILL are silently dropped — without that check, an unreaped client (e.g. gnome-shell taking its time to `waitpid()` after `ServerDisconnected` exit) would falsely look "still alive" to `kill -0` and block the post-upgrade relaunch indefinitely. The same helper backs settings relaunches in `restart_singleton_binary`. See  and .

Every Debian configure checks for executable `bd` without running it. Lookup uses the target user manager's absolute PATH entries, that user's standard local, mise, Go, and Cargo tool directories, Linuxbrew, and system bin directories; absence only warns that the Beads board remains hidden and never fails install or changes the server runtime generation.

If Linux hot-reload fails because the handoff state version changed, `postinst` normally falls back to a true cold restart: it reloads the matching user unit, stops it, kills any detached flavor-specific server processes still holding the lock/socket, clears stale sockets, resets the failed unit state, and then starts the new server. The installer shows only a high-level warning, and it asks the user to save work only when the original server PID is still alive after the failed handoff attempt. Auto-update installs now set a runtime defer marker first, so that same failure path can leave the old server running, persist an `update-restart-required` flag, and skip client relaunches until the UI explicitly approves the cold restart. Once approved, the client starts a detached helper and asks every window to save and exit; the helper waits for those processes, cold-restarts the server, and launches one fresh client for the existing cold-restore fan-out. If a non-deferred cold restart fails, the package script skips client relaunches instead of piling new processes onto a broken server.

Every Debian configure also retries the retired workspace-annotation migration. The script accepts only the exact `scribe` or `scribe-dev` package identity, resolves that target user's XDG state root, and deletes only the matching flavor's legacy TOML artifact plus atomic-write temp siblings as the target UID. It never removes the flavor directory or sibling state. A cleanup-only, fail-closed `/proc` scan captures every exact server PID, process start time, and executable hash without changing the normal socket/`pgrep` lifecycle. Zero captured writers or writers already hashing to the installed binary authorize cleanup independently of service-start success. Older writers require successful hot/cold replacement and verified exit. An immediate rescan must then find no exact server or only installed-binary hashes. Unknown desktop UID, unreadable procfs, deferred/failed replacement of an older writer, exit timeout, unsafe symlinked flavor directory, unresolved state root, deletion error, or failed absence check leaves cleanup pending and emits a retry warning. An explicit migration revision participates in `server-runtime-generation`, forcing the first migration-bearing package through replacement even when the server binary is otherwise unchanged. No backup is created.

Auto-update package downloads are staged under the user runtime directory and Linux passes a verified, unlinked package fd to `pkexec dpkg`, so maintainer scripts install the same inode that minisign verified rather than reopening a mutable temp path.

Settings relaunches also wait for the old singleton to release its lock and socket, then escalate to SIGKILL before starting the replacement if the old process refuses to exit. `scribe-dev` additionally skips automatic Claude/Codex hook setup during install so the stable install's global hook configuration remains untouched.

At the GPUI cutover, `preinst` stashes the current client before dpkg unpacks
the replacement. `postinst` runs `scribe-client --vulkan-probe`; failure
restores the stash, warns the user, and skips relaunch without disturbing
server-owned sessions. The standalone settings desktop entry and its
maintainer-script relaunch path are retired; `scribe-client --settings` is the
supported entry point. Users can roll back other Debian upgrade failures with
`apt install scribe=<previous-version>` and pin that version until fixed.

## Data Flow

Terminal I/O flows through a well-defined pipeline from shell process to screen pixel.

### Write Path

User keystrokes travel from the client through IPC to the server, which writes them to the PTY master fd. The shell reads from the PTY slave and processes the input.

Keyboard-originated input is marked so the server can clear persisted attention states before the next reconnect or handoff snapshot.

Clipboard pastes follow the same path but may exceed the 4 KiB  message limit. The client chunks large pastes into multiple `KeyInput` messages, with bracketed-paste markers on the first and last chunks only.

### Read Path

Shell output flows from the PTY master fd through the server's ANSI processor () and metadata parser (), then is serialized as a  and sent to the attached client for GPU rendering.

OSC 52 clipboard reads and writes branch off this path through alacritty_terminal's `Event::ClipboardStore` / `Event::ClipboardLoad` callbacks and travel through the server-side policy engine plus a client round-trip to the host clipboard bridge. See  for the policy decision and the new  /  IPC pair.

### Reconnect Path

When a client reconnects, the server sends a full screen snapshot of every subscribed session. The client rebuilds its terminal grid from this snapshot without any visible gap.

Active AI state is restored from the `SessionList` response before `AttachSessions` so the  tracker is populated immediately. The same response also carries an AI provider hint for sessions whose visible attention state was already dismissed, so reconnect preserves provider-aware behavior without reviving the indicator. The per-session `AiStateChanged` messages from `send_stored_metadata` arrive later as an idempotent overwrite.

Sessions that were active during a  retain their pre-handoff snapshot for the first attaching client. After a handoff the workspace tree stored by the old client may reference workspace IDs that differ from the new server's session workspace IDs; the client detects this mismatch (empty join between tree workspace order and session groups) and falls back to session-based reconstruction.

Prompt bar state (`first_prompt`, `latest_prompt`, `prompt_count`) is client-side only and not part of `SessionList` or the handoff protocol. During hot restart reattach,  reads the cold restart snapshot saved by the previous client and copies prompt data to matching panes by `conversation_id`.

### Cold Restart Restore

When the server crashes or is killed and relaunched, all PTY sessions are lost. The client detects a cold restart by receiving an empty `SessionList` while a restore snapshot exists on disk, then replays the previous window layout.

The restore pipeline has three layers:  persists per-window snapshots and a global index under `$XDG_STATE_HOME/{flavor}/restore/`,  captures the current layout, and  rebuilds the layout from a snapshot. Snapshots are saved on a debounced timer after every layout change. On explicit close or quit the snapshot is removed; on server crash it is preserved. Multiple windows are restored by having the first client claim the first index entry and spawn `--restore-child` processes for the rest, so only the bootstrap client fans out additional windows. Because a true cold restart connects to a fresh server that already assigned new window IDs in `Welcome`, the client reapplies geometry from the claimed snapshot's original window ID before replaying panes, and feeds that same geometry into the replay so pane grids are sized from the saved logical dimensions instead of `window.inner_size()` — which is unreliable in the same synchronous block because `request_inner_size` and `set_maximized(true)` are async on most compositors and have not yet been acknowledged. The claim step prunes stale index IDs whose per-window snapshot file is missing or unreadable before computing the remaining-window count, which prevents partial restore-state corruption from spawning duplicate fresh windows.

### Remote Control Path

Feature 013 runs the same write/read pipeline between two machines: a connecting client dials the owning machine's tailnet TCP listener instead of the local Unix socket, exchanges the , then speaks the ordinary message catalogue.

The PTY still lives only on the owning machine, so `Hello` / `KeyInput` / `PtyOutput` / `SessionReplay` cross the tailnet unchanged once the handshake's WhoIs identity check and version gate pass. The differences from the local path are three: the owning side wraps each remote writer in a bounded  queue that drops backlog and resyncs via `SessionReplay` rather than stalling the authoritative Term; single-writer window ownership gains  so a claim can displace the current controller (or land in lost control), a displacement the owner then enforces by barring the lost connection from mutating or re-attaching its old window (); and a dropped link drives the client's  instead of the local server-relaunch recovery. Terminal contents leave the device only over the tailnet's encrypted transport to the user's own authenticated machine, and only while `remote.enabled`.
