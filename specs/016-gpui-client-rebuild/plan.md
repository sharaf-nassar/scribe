# Plan: gpui-client-rebuild

## Architecture Approach

Rebuild the client as a new crate, `crates/scribe-client-gpui`, developed
side-by-side against the live server over the frozen IPC protocol, then cut
over (rename binary to `scribe-client`, delete the old crate) once the
launch gate passes. The server, protocol, `scribe-common`, `scribe-test`,
and `scribe-cli` are untouched.

**Foundation:** GPUI pinned to zed `v1.12.0`
(`f96212f2c50f54d93712fa130d6226b1ce7d76b5`) via git deps `gpui` +
`gpui_platform` (features `["font-kit", "x11", "wayland"]`). GPUI at this
pin renders through `gpui_wgpu` (wgpu 29). `[profile.dev.package.gpui]
opt-level = 3`.

**Terminal core:** adopt Zed's display-only terminal model wholesale (GPL
relicense is step-0, so code is copied, not reimplemented):

- A `Terminal` entity per pane in the style of Zed's
  `TerminalType::DisplayOnly` — owns the alacritty `Term`, a VTE
  `Processor`, and a `Content` snapshot; IPC bytes enter via
  `write_output()`, which advances the processor and emits `Wakeup`.
- Zed's `write_to_pty` path is replaced by an `IpcSink` that enqueues
  `ClientMessage::KeyInput` / mouse-report bytes onto the existing
  IPC-writer command channel.
- `TerminalElement` (cribbed from Zed's `terminal_element.rs`) paints from
  the `Content` snapshot: merged background quads + `shape_line` glyph runs
  with forced `cell_width`, underline/undercurl/strikethrough from cell
  flags, minimum-contrast adjustment. Also cribbed: Zed's `alacritty.rs`
  glue (`make_content`, cell/selection/mode conversions, listener) and
  `mappings/colors.rs`; `mappings/keys.rs`/`mouse.rs` serve as the shape
  for the ported Scribe encoders, which supersede them (Scribe's kitty
  keyboard chain is stricter than Zed's legacy-only table).
- alacritty: the client adopts Zed's fork (`zed-industries/alacritty` rev
  `4c129667`) to guarantee API match with cribbed code. The server keeps
  crates.io `0.26.0-rc1` — verified safe: `scribe-common` snapshot/replay
  types do not couple the two grids. (Alternative rejected: porting cribbed
  code onto stock 0.26.0-rc1 — saves nothing and risks subtle API drift.)
- `SessionReplay` = zstd-decompress → `write_output`. `ScreenSnapshot` =
  reset Term + replay `snapshot_to_ansi` output (same conversion already in
  `scribe-common`).

**Concurrency / event bridge** (core design, per spec review):

- The IPC thread (background Tokio runtime, unchanged pattern from
  `ipc_client.rs`) sends `UiEvent`s into an `mpsc` channel as today.
- A GPUI foreground task (spawned on the app context) drains that channel
  with Zed-style coalescing: batch up to 100 events or 4 ms, collapse
  redundant wakeups per pane, then apply batched `write_output` per
  terminal entity and `cx.notify()` once per dirty pane.
- Keystroke-before-output ordering: unchanged by construction — outbound
  `KeyInput`/`Resize` go straight from the input handler to the IPC-writer
  channel (Resize flushed first, as today); they never traverse the
  inbound drain. Sync-update frame preservation: the client-side
  `queue_output_frames` logic (one committed CSI-2026 burst per redraw,
  150 ms expiry, catch-up drain) ports into the drain task, in front of
  `write_output`.

**Chrome:** all UI (custom titlebar + integrated tab bar, status bar,
scrollbar with command marks, dividers, dialogs, palette, tooltips, AI
indicator, prompt bar) is built as GPUI elements/views with taffy layout,
GPUI shadows/gradients/animations. No gpui-component dependency for v1
(fewer moving parts; our widgets are bespoke anyway — decision noted as
revisitable). Splash screen is deleted (startup budget 500 ms).

**Settings:** `scribe-settings` GTK/wry webview delivery is deleted. The
settings UI is rebuilt as a GPUI window (second window in the client
process). The `settings.lock` + `settings.sock` singleton is absorbed by
the client: `scribe-client --settings` focuses/opens the settings window;
the CLI surface stays so external invocations keep working. (Alternative
rejected: separate GPUI settings binary — doubles GPUI link time and binary
size for no isolation benefit.)

**Cutover mechanics:** launch gate = parity checklist (US1/US2 + core
chrome) + perf budget met + visual E2E green. At cutover the new crate is
renamed `scribe-client` (binary name unchanged for packaging/postinst),
old `crates/scribe-client` and `crates/scribe-renderer` are deleted, and
the deletion sweep + lat.md rewrite follow as their own phase.

**Vulkan safety:** client startup probes Vulkan (hardware ICD → lavapipe
fallback); if even lavapipe fails, the client exits with a diagnostic
without touching sessions. Packaging-level guard: dpkg replaces the binary
at unpack (before `postinst`), so "keep the previous binary" must be
implemented as a **`preinst` stash**: `preinst` copies the current
`scribe-client` binary aside; `postinst` runs `--vulkan-probe` and, on
failure, restores the stashed binary, skips the client relaunch, and
surfaces a warning (sessions stay alive either way). The stash is removed
on probe success. A documented deb-downgrade procedure
(`apt install scribe=<prev>` / pinned previous release) is the functional
rollback for non-Vulkan cutover failures.

## Affected Components

| Component | Change |
|---|---|
| `crates/scribe-client-gpui` (NEW) | The rebuilt client; later renamed to `scribe-client`. |
| `crates/scribe-client` | Untouched during build; DELETED at cutover. |
| `crates/scribe-renderer` | Untouched during build; DELETED at cutover. ~900 LoC of pure logic (xterm-256 palette, box-drawing rasterizer, bold→bright/DIM/sRGB color semantics) moves into the new crate first, with tests. |
| `crates/scribe-settings` | GTK/wry webview + HTML/CSS/JS assets DELETED; config read/write/apply logic (`apply.rs`, server_action) moves into the new client's settings module. |
| `crates/scribe-common` | FROZEN (no protocol changes). |
| `crates/scribe-server` | FROZEN except `Cargo.toml` deb metadata (Depends, assets) and `postinst` (Vulkan probe guard) at cutover. |
| `crates/scribe-test`, `crates/scribe-cli` | FROZEN; CI proves they stay green. |
| Workspace `Cargo.toml` | + gpui/gpui_platform git deps, alacritty fork (client only), MSRV 1.95; − winit, wgpu, cosmic-text, GTK/wry (at cutover). License field → GPL-3.0-or-later (step-0). |
| `third_party/` | Unchanged (gpui consumed as git dep, not vendored, while upstream is healthy; vendoring is the contingency). |
| `docker/Dockerfile.visual`, `Dockerfile.func`, `dist/ci` | Rust 1.95 images, GPUI system deps, lavapipe; visual entrypoint gains `SCRIBE_DISABLE_ANIMATIONS=1`. |
| `dist/debian/*`, deb metadata | New Depends (`libvulkan1, mesa-vulkan-drivers, libwayland-client0, libxkbcommon-x11-0`, xcb libs; − `libgtk-4-1` at cutover), postinst Vulkan guard. |
| `LICENSE-*`, README, all `Cargo.toml` license fields | GPL-3.0-or-later migration (step-0), attribution notices for Zed (GPL) and GPUI (Apache-2.0). |
| `lat.md/` | client.md, rendering.md, settings.md, architecture.md rewritten post-cutover; test.md updated for new harness details. |
| `specs/016-gpui-client-rebuild/parity-inventory.md` (NEW) | The committed parity oracle (46 ClientMessage / 57 ServerMessage / subsystem checklist). |

## Data Model

No server-side or protocol data changes. Client-internal:

- **Entities:** `Terminal` (per pane: Term, processor, Content snapshot,
  command_records, prompt state, split-scroll state), `PaneTree` (port of
  `LayoutTree`: binary splits, ratios, directional focus),
  `WorkspaceTree` (port of `WindowLayout`: slots, tabs, accents, names),
  `AppState` (connection, dialogs, share/remote state). Each maps to a GPUI
  `Entity<T>` with views subscribing to wakeups.
- **Config:** same TOML files, same watcher semantics. Removed appearance
  keys (splash-related; any pipeline-specific constants) are silently
  ignored on load (no hard error), documented in a "removed keys" table in
  the parity inventory. `opacity` fate decided by spike (OQ12).
  New key: `animations` (bool, default true) — doubles as the reduce-motion
  user setting and the E2E determinism hook (env
  `SCRIBE_DISABLE_ANIMATIONS=1` overrides). This is a sanctioned exception
  to the "no new end-user features" Non-Goal: it exists to preserve the
  latency-purist experience and test determinism, per spec-review
  observations.
  **Side-by-side dual-writer rule:** during the parallel-dev period the old
  GTK settings app remains the sole authoritative config writer; the new
  GPUI settings window operates against a separate dev config path
  (`SCRIBE_CONFIG_DIR` override) until cutover, so two writers never race
  on the live TOML.
- **Restore store:** format unchanged (`RestoreStore` snapshots carry
  session ids + trees + geometry). Caveat: geometry persisted by the
  OS-decorated old client may restore mis-inset under the new custom
  titlebar — first launch after cutover runs a geometry-compat
  normalization (clamp + titlebar-inset adjustment), with a scripted
  assertion in the lifecycle E2E.
- **Out of scope (explicit):** crash reporting/telemetry (none today, none
  added), i18n (English-only today, unchanged), accessibility beyond what
  GPUI/AccessKit provides by default — chrome a11y audit is a filed
  follow-on, not part of this feature.
- **Golden fixtures (NEW, test-only):** byte-capture corpus for key/mouse
  encoding generated from the OLD client's encoder tables before deletion;
  stored under the new crate's test fixtures.

## API / Interface Changes

- **IPC protocol:** none (hard freeze; CI guard: `scribe-common` diff check
  in the epic's definition of done).
- **Binary surface:** `scribe-client` name, args, and env contract
  preserved at cutover (`--restore-child`, window adoption, dial env vars
  `SCRIBE_REMOTE_DIAL`/`SCRIBE_LAN_DIAL`). NEW: `--settings` (opens
  settings window), `--vulkan-probe` (exits 0/1; used by postinst).
- **`scribe-settings` binary:** DELETED at cutover. Anything invoking it
  (client gear icons, `.desktop` entries, docs) switches to
  `scribe-client --settings`. Settings socket protocol (`focus` command)
  preserved during transition, absorbed into the client's socket handling.
- **Breaking (accepted, no-legacy mandate):** GTK settings app gone;
  removed config keys inert; splash gone; native window decorations gone
  (custom titlebar); license GPL-3.0-or-later.
- **Keybindings:** full `Bindings` parser and all `KeyAction`/
  `LayoutAction` variants (50+) ported verbatim — named explicitly in the
  parity inventory.

## Testing Strategy

- **Golden byte-capture harness (US1):** unit tests assert the ported input
  encoder (kitty 4-level chain, CSI-u, legacy xterm, DECCKM/DECPAM, numpad
  SS3) and mouse reporter (X10/SGR-1006, modes 1000/1002/1003) produce
  byte-identical output to fixtures captured from the old client's tables.
  This is the "byte-identical" oracle.
- **`#[gpui::test]` headless suites (US6):** pane tree ops, workspace tree,
  selection model (cell/word/line, WRAPLINE), sync-frame queueing +
  150 ms expiry, URL/OSC8 detection, replay application, config
  load-with-removed-keys, reconnect topology rebuild. No display server
  needed; runs in `Dockerfile.func` CI.
- **scribe-test functional E2E:** unchanged and must stay green throughout
  (server-only; proves the freeze).
- **Visual E2E (US6):** `Dockerfile.visual` updated: Xvfb + lavapipe
  (`VK_ICD_FILENAMES` → lvp), Rust 1.95 image, GPUI system deps;
  `SCRIBE_DISABLE_ANIMATIONS=1` for deterministic screenshots; xdotool
  drive + scrot capture as today; X11 active-window guard semantics
  preserved (spike verifies XID access).
- **Perf gate (US* launch):** scripted A/B on the same machine: input
  latency (typometer-style or instrumented echo round-trip), `cat`
  firehose throughput, memory at 10 tabs, startup-to-first-frame, scroll
  fps with frame-drop counter. Thresholds from Clarification Q3. Run
  against the old client before deletion to record baselines; results
  committed alongside the parity inventory.
- **Session lifecycle (US2):** scripted E2E: attach → kill client →
  reattach (replay correctness), server `--upgrade` under a live GPUI
  client, cold-restart restore fan-out, geometry-compat restore. All
  lifecycle tests run against a disposable test server instance — never
  the user's live server (CLAUDE.md invariant).
- **Failure/degraded paths (US2, spec-review):** explicit tests for
  server-down at launch, socket vanishing mid-session, adoption failure,
  replay decode failure (pane shows error state, no crash), reconnect
  retry/timeout behavior.
- **Cutover abort path (Q1):** scripted packaging test in the visual
  Docker image with Vulkan removed: install cutover deb → `preinst` stash +
  `--vulkan-probe` failure → old binary restored, sessions alive, warning
  surfaced.
- **Color emoji (US3 headline):** visual-E2E screenshot asserts a grid
  emoji renders in color (not tinted) — added to the parity checklist as
  an automated item.
- **IME (US1):** manual parity-checklist item with a written procedure
  (compose text via ibus/fcitx on X11 and a Wayland compositor); no
  automated harness — explicitly a manual gate item.
- **Parity checklist:** every `parity-inventory.md` item gets a
  verification column (golden / gpui-test / visual-E2E / scripted-E2E /
  manual) — no item ships unverified; manual items require a checked-off
  review pass before cutover.

## Risks

| Risk | Mitigation |
|---|---|
| A capability spike fails (box drawing, fallback ordering, ligatures, opacity, X11 XID) | Spikes run first with authority to rewrite US3 criteria (Clarification Q7). Fallbacks per spike: box drawing → paint-quad overlay in TerminalElement keyed on codepoints (works regardless of text-system hooks); fallback ordering → explicit font-stack list per run if GPUI honors per-run fonts, else accept regression + document; ligatures → drop `ligatures` key; opacity → drop `opacity` key; XID → poll via `xdotool`-style EWMH from the guard's own connection using the window title/PID instead of XID. |
| GPUI pin has a blocking bug; upstream fix requires moving the pin | Contingency: vendor the 11 gpui crates into `third_party/` (existing convention) and cherry-pick; only move the pin deliberately as its own bead. |
| Perf regression vs old client (GPUI is heavier than a bespoke quad pipeline) | Perf gate with recorded baselines is launch-blocking; Zed proves the ceiling is high enough; profile with wgpu tools before optimizing. |
| The parity tail (remote/LAN, restore, notifications) drags | Parity inventory makes the tail visible and countable from day one; phases C–E are parallelizable across agents; launch gate excludes US3 cosmetics so the tail is correctness-only. |
| 015 lands late | Only phase E's remote/LAN/share beads depend on 015; everything else proceeds. If 015 stalls indefinitely, fall back to porting today's surfaces (explicit re-decision at the epic level). |
| Executor-model ordering bugs (keystroke latency, frame tearing) | Bridge design keeps outbound path separate from inbound drain (ordering by construction); sync-frame logic has dedicated gpui-tests; firehose + typing-under-load scripted test. |
| GPL flip is irreversible once external contributions arrive | Step-0 lands first, in its own commit, with README/notice; co-contributor (1 commit) acknowledged; no CLA needed (permissive → GPL is one-way compatible). |
| Settings rebuild scope-creeps | Feature set ported 1:1 from `settings.html/js` inventory; new capabilities explicitly out of scope. |
| Old client deleted before baselines/fixtures captured | Deletion phase explicitly depends on golden-fixture capture and perf baseline beads. |

## Sequencing

Phases become the bead DAG. Ordering is expressed as dependencies; items
within a phase are parallel unless noted. **Bold** items are the critical
path.

**Phase 0 — Foundations (no dependencies):**
- **Relicense project to GPL-3.0-or-later** (LICENSE files, all Cargo.toml
  license fields, README, attribution notices). Blocks every bead that
  copies Zed code.
- **Commit `parity-inventory.md`** (from the session parity inventory; adds
  verification-method column, removed-keys table, keybinding action list).
  Blocks phase-gate beads and the analyze-side checklists. Rows covering
  015 surfaces (share roster, control passing, LAN dialogs) are marked
  **provisional**; a dedicated "reconcile 015 surfaces into
  parity-inventory" bead, gated on 015 landing, finalizes them before the
  launch gate.
- Capture golden input/mouse fixtures + perf baselines from the old client.
  Blocks deletion phase and the perf gate.
- Toolchain: MSRV 1.95 (rust-toolchain, CI images), GPUI system deps in
  docker images, `[profile.dev.package.gpui] opt-level = 3` docs.

**Phase A — Spikes (depend on: relicense, toolchain):**
- **Scaffold spike: `scribe-client-gpui` crate, gpui deps pinned, one
  window, connects to live server, renders one pane's grid via cribbed
  display-only Terminal + TerminalElement.** Proves the whole bet; blocks
  all of phases B–F.
- Capability spikes (parallel, each may rewrite US3 criteria): box-drawing
  entry point; Nerd-Font fallback ordering; ligatures via `shape_line`;
  window opacity Wayland/X11; X11 window-handle/XID access for the focus
  guard. A closing "criteria reconciliation" bead folds results into
  spec.md + parity inventory.

**Phase B — Terminal core (depends on: scaffold spike):**
- IPC bridge: UiEvent drain task with 4 ms/100-event coalescing +
  per-pane wakeup collapse; outbound IpcSink (Resize-before-KeyInput).
- Sync-frame queueing port (CSI-2026 burst preservation, expiry, catch-up).
- Input encoder port (kitty chain, CSI-u, legacy, app modes, numpad) +
  golden tests. Mouse reporting port + golden tests.
- Cribbed terminal state: selection (+ smart selection), vi/copy mode,
  search, scrollback/viewport, URL/OSC8 detection + hover/dwell/open,
  bracketed paste detection, IME wiring, bell.
- Ported pure logic: xterm-256 palette, color semantics (bold→bright, DIM
  0.67, minimum-contrast, sRGB↔linear conversions, BrightForeground
  boost), box-drawing. **Dependency edge: the box-drawing bead depends on
  the box-drawing capability spike; the glyph-run painting bead depends on
  the ligature + fallback-ordering spikes and the criteria-reconciliation
  bead** — not just the scaffold spike.
- Session lifecycle: replay, snapshot, reconnect rebuild, adoption,
  TrimScrollback mark shifting.

**Phase C — Layout & chrome (depends on: phase B core rendering):**
- PaneTree + WorkspaceTree ports (+ gpui-tests), dividers + drag,
  directional focus, zoom, equalize, split-scroll.
- Custom titlebar + integrated tab bar (all behaviors: drag reorder,
  close, flash, AI dot, task labels, workspace badge, context-%).
- Status bar (all segments incl. sparklines), scrollbar + command marks
  (custom element), focus borders.
- Animation system usage: tab/focus/overlay transitions ≤150 ms,
  smooth scrolling, `animations` config key + env override.

**Phase D — Dialogs & overlays (depends on: phase C shell):**
- Command palette, context menu, tooltips.
- Dialog suite: close, update, paste confirmation, clipboard/OSC52,
  disallowed scheme, workspace notes modal + hover preview.
- AI indicator, prompt bar.

**Phase E — Integrations (depends on: phase B; remote/LAN items also
depend on: 015 landed):**
- Config watcher + runtime reload; keybindings parser + actions.
- Clipboard (arboard + OSC52 bridge + primary selection), notifications
  (zbus/notify-rust), server lifecycle (systemctl/launchd), window
  geometry persistence, X11 focus guard (per spike), drag-drop paths.
- Cold-restart restore (RestoreStore + `--restore-child`).
- Remote/LAN/share surfaces (per 015 final form): connect picker, LAN
  approval, lost control, share roster, control passing.

**Phase F — Settings window (depends on: phase C shell; parallel to D/E):**
- GPUI settings window: port feature set from settings.html/js inventory
  (appearance, keybindings, themes, workspace roots, AI indicator,
  updates/releases page); absorb singleton/focus socket; `--settings` flag.

**Phase G — Test & CI hardening (starts with phase B, completes after E):**
- gpui-test suites per Testing Strategy; visual E2E docker rework
  (lavapipe, 1.95, animations off); lifecycle scripted E2E; perf A/B rig.

**Phase H — Cutover (depends on: B–G complete, perf gate green, parity
checklist green):**
- **Launch-gate bead: run full parity checklist + perf gate; explicit
  go/no-go.**
- Rename crate/binary to `scribe-client`; packaging: deb Depends update,
  `preinst` binary stash + `postinst` Vulkan-probe guard/restore,
  `--vulkan-probe`; **remove the settings-singleton relaunch path from
  preinst/postinst** (`upgrade-settings-pid`, `restart_singleton_binary`
  for scribe-settings) and delete/redirect `scribe-settings.desktop`;
  `.desktop`/docs updates; documented deb-downgrade rollback procedure in
  the release notes.
- Disable/skip macOS build + notarize jobs (`dist/macos/*`) at cutover;
  file the macOS GPUI port as a tracked follow-on feature (deferred on
  purpose, not a red pipeline).
- Delete `crates/scribe-client` (old), `crates/scribe-renderer`,
  `scribe-settings` webview delivery; workspace dep purge (winit, wgpu,
  cosmic-text, GTK/wry).

**Phase I — Deletion sweep & docs (depends on: cutover):**
- Dead-code audit: cargo udeps-equivalent, grep for old-pipeline
  identifiers, unused config keys, orphaned assets/tests.
- lat.md rewrite (client, rendering, settings, architecture, test) +
  `lat check`.
- Update CLAUDE.md, AGENTS.md, and README build/run instructions (MSRV
  1.95, GPUI system deps, `opt-level=3` note, crate names, just targets) —
  named owner: this bead; MSRV/system-dep parts land earlier with the
  Phase 0 toolchain bead.
- US3 cosmetics that trailed cutover (if any) close here; follow-on
  register (image protocols, gpui-component adoption, macOS) filed as
  future feature stubs, not left as TODOs.

## Alignment fixes applied

- (B, must) Fixed the impossible postinst rollback: dpkg replaces the
  binary at unpack, so the guard is now a preinst stash + postinst
  probe/restore, plus a documented deb-downgrade procedure as the
  functional rollback.
- (B, must) Wired Phase B box-drawing / glyph-painting beads to their
  Phase A capability spikes and the criteria-reconciliation bead instead
  of only the scaffold spike.
- (B, must) Phase H now removes the settings-singleton relaunch path from
  preinst/postinst and deletes/redirects scribe-settings.desktop.
- (B, should) 015-derived parity-inventory rows marked provisional with a
  dedicated reconcile bead gated on 015 landing.
- (B, should) macOS packaging/CI explicitly disabled at cutover with a
  tracked follow-on, instead of silently breaking.
- (B, should) Geometry-compat normalization + test for old-client window
  geometry under the custom titlebar.
- (B, should) CLAUDE.md/AGENTS.md/README build-instruction ownership
  assigned (Phase I bead + Phase 0 toolchain bead).
- (B, should) Side-by-side dual-writer rule: old settings app stays sole
  config writer until cutover; new settings window uses a dev config path.
- (A, should) Q1 abort path now has a scripted packaging test (Vulkan-less
  Docker: stash → probe fail → restore, sessions alive).
- (A, should) Color emoji promoted to an automated visual-E2E checklist
  item; IME designated an explicit manual gate item with procedure.
- (A, should) Failure/degraded-path tests added (server down, socket gone,
  adoption failure, replay decode failure, reconnect timeout).
- (A, should) Crib scope clarified: alacritty.rs glue + mappings/colors
  cribbed; mappings/keys+mouse superseded by ported Scribe encoders.
- (A, should) sRGB↔linear + BrightForeground boost named in the ported
  color semantics; `animations` key reconciled against Non-Goals as a
  sanctioned exception; a11y/telemetry/i18n explicit out-of-scope lines
  added; lifecycle tests pinned to a disposable server (never the live
  one).
