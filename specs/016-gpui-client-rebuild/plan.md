# Plan: gpui-client-rebuild

## Architecture Approach

Rebuild the client as a new crate, `crates/scribe-client`, developed
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
  the `Content` snapshot: merged background quads, then a procedural
  paint-quad overlay for U+2500–U+259F, then `shape_line` glyph runs with
  forced `cell_width` and Scribe-ordered `FontFallbacks`. The glyph path uses
  `FontFeatures::disable_ligatures()` only when `appearance.ligatures` is
  false; it also renders underline/undercurl/strikethrough from cell flags
  and applies minimum-contrast adjustment. Also cribbed: Zed's `alacritty.rs`
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
revisitable). Splash screen is deleted (startup budget: Scribe-attributable
startup under its absolute cap and total first frame no worse than the old
client end-to-end, per the spec's Q3 re-scope).

**Settings:** `scribe-settings` GTK/wry webview delivery is deleted. The
settings UI is rebuilt as a GPUI window (second window in the client
process). The `settings.lock` + `settings.sock` singleton is absorbed by
the client: `scribe-client --settings` focuses/opens the settings window;
the CLI surface stays so external invocations keep working. (Alternative
rejected: separate GPUI settings binary — doubles GPUI link time and binary
size for no isolation benefit.)

**Cutover mechanics:** launch gate = parity checklist (US1/US2 + core
chrome) + perf budget met + visual E2E green. **Re-baselined post-audit:** the
parity half of that gate is measured as the *reachable*-row count from
`parity-inventory.md`, not as a green unit-test run — see "Re-sequenced
remaining phases (post-reachability-audit)" below. At cutover the new crate is
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
| `crates/scribe-client` (NEW) | The rebuilt client; later renamed to `scribe-client`. |
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
| `specs/016-gpui-client-rebuild/parity-inventory.md` (NEW) | The committed parity oracle (46 ClientMessage / 59 ServerMessage / 54 named keybinding actions / rendering + spec-behaviour + removed-key rows), each row carrying a mandatory "Reachable from" live-path symbol, plus a coverage index binding every `spec.md` requirement-register id to a carrying row. |

## Data Model

No server-side or protocol data changes. Client-internal:

- **Entities:** `Terminal` (per pane: Term, processor, Content snapshot,
  command_records, prompt state, split-scroll state), `PaneTree` (port of
  `LayoutTree`: binary splits, ratios, directional focus),
  `WorkspaceTree` (port of `WindowLayout`: slots, tabs, accents, names),
  `AppState` (connection, dialogs, share/remote state). Each maps to a GPUI
  `Entity<T>` with views subscribing to wakeups.
- **Config:** same TOML files, same watcher semantics. `appearance.ligatures`
  remains a boolean terminal-run shaping control and `appearance.opacity`
  remains a live `0.0..=1.0` root-background alpha control on a transparent
  GPUI surface. Removed appearance
  keys (splash-related; any pipeline-specific constants) are silently
  ignored on load (no hard error), documented in a "removed keys" table in
  the parity inventory.
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
  drive + scrot capture as today. The X11 focus guard extracts GPUI's Xcb XID
  and preserves the direct `_NET_ACTIVE_WINDOW` comparison; non-X11 backends
  do not enable it.
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
  review pass before cutover. **Re-baselined post-audit:** every item also
  carries a mandatory "Reachable from" column naming the live-path symbol that
  calls it, and an item with no such symbol cannot be marked done however many
  tests pass. `gpui-test` is retained only for the nine removed-config-key
  rows; the 27 headless-only IPC rows moved to `scripted-E2E`, and font
  fallback plus all 54 named keybinding actions moved to `visual-E2E` driven by
  `xdotool` against the real window.

## Risks

| Risk | Mitigation |
|---|---|
| Capability mechanisms regress at a later GPUI pin | The Phase A probes resolved the current pin: paint-quad overlay for box drawing, ordered `FontFallbacks`, forced-width `shape_line` ligatures, transparent-surface alpha repaint for opacity, and direct Xcb XID access. Any pin move reruns those probes before criteria change. |
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
- **Scaffold spike: `scribe-client` crate, gpui deps pinned, one
  window, connects to live server, renders one pane's grid via cribbed
  display-only Terminal + TerminalElement.** Proves the whole bet; blocks
  all of phases B–F.
- Capability spikes are resolved at the pinned GPUI revision: box drawing
  uses a `TerminalElement` paint-quad overlay; terminal runs carry ordered
  Nerd-Font-first `FontFallbacks`; ligatures use forced-width `shape_line`;
  opacity uses transparent-surface alpha repaint on Wayland/X11; and the X11
  focus guard uses the raw Xcb XID. Their criteria are reconciled in the spec
  and parity inventory before Phase B fans out.

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
  boost), box-drawing. The box-drawing renderer emits a paint-quad alpha-mask
  overlay for U+2500–U+259F, while normal terminal runs use ordered fallbacks
  and forced-width ligature shaping. **Dependency edge: the box-drawing bead
  depends on the box-drawing capability spike; the glyph-run painting bead
  depends on the ligature + fallback-ordering spikes and the
  criteria-reconciliation bead** — not just the scaffold spike.
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
  geometry persistence, X11 focus guard using GPUI's Xcb XID, drag-drop paths.
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

## Re-sequenced remaining phases (post-reachability-audit)

**Status of the original sequencing: Phases 0 through G delivered the library
port, not the product.** Their beads are closed and the code they produced is
real and unit-tested, but 35 of the crate's 54 library modules are outside
`main.rs`'s import closure and are never constructed by the running client. The
reachability audit (`reachability-audit.md`, at `f56ef95`) measured 173 parity
rows as 60 WIRED / 63 UNWIRED / 50 MISSING — **51 of 164 user-facing rows
reachable (31%)**.

The remaining work is therefore **integration/wiring plus the genuinely missing
features**, sequenced around the audit's fix units FU-1..FU-23 rather than
around new subsystems. Phases R0–R2 below replace what is left of B–G and run
before Phase H. Nothing here touches the server, the IPC protocol, or
`scribe-common` — those freezes stand unchanged.

### Structural causes being fixed

1. `main.rs` imports 19 of 54 library modules; the rest are reachable only from
   `lib.rs` and their own tests.
2. `main.rs::run_reader` matches 12 of 59 `ServerMessage` variants and ends in
   `_ => {}`; everything else is silently dropped on the wire.
3. `main.rs::handle_layout_action` executes 9 of 35 `LayoutAction` variants and
   sends the other 26 to a `tracing::debug!` catch-all.
4. `terminal.rs::Content` carries `rows: Vec<String>`, so the paint path has
   **no per-cell colour at all**.
5. Overlay events (`CommandPaletteEvent::Execute`, `ContextMenuEvent::Selected`,
   `DialogEvent::Chosen`) are discarded, so those surfaces open but do nothing.

### Phase R0 — reachable paint path and terminal chrome (P0)

- **FU-1 Cell-accurate paint path — first, and blocking.** `Content` must carry
  per-cell fg/bg/attrs instead of `Vec<String>`, and `TerminalElement::paint`
  must paint them. Every other rendering unit depends on this: box drawing
  (paint-quad overlay), ligatures (`shape_line` runs), and font fallback
  (`FontFallbacks` per run) all need per-cell style before they have anywhere
  to attach. Rows: Box drawing, Font fallback, Ligatures. **Do not start the
  other rendering units before FU-1 lands.**
- FU-2 Terminal chrome from server metadata — `TitleChanged`, `CwdChanged`,
  `GitBranch`, `WorkspaceNamed`, `SessionContextChanged`, `EnvStatus`, and the
  hardcoded `None`s in `main.rs::build_status_model`. Parallel to FU-3.
- FU-3 AI tab labels — `TaskLabelChanged`/`Cleared`,
  `CodexTaskLabelChanged`/`Cleared`. Parallel to FU-2.
- FU-4 Opacity — **already covered by bead `.56`**, which landed at `771794d`
  *after* the audit baseline. Do not re-file; re-verify through bead `.53`
  before flipping the row's "Reachable from" cell.
- **Gate-methodology beads run alongside R0**, because they are the drift
  guard for everything after: replace `run_reader`'s `_ => {}` with an explicit
  `warn`-level arm; add the three mechanical CI greps (reader arm set vs the
  inventory, `ClientMessage` construction inside `main.rs`'s import closure,
  `lib.rs` modules either in the closure or carrying an unwired marker naming
  their bead); and replace the dispatch catch-alls with an
  `unimplemented_action(action)` helper that warns and increments a counter a
  scripted run asserts is zero.

### Phase R1 — core interaction (P1, depends on: R0 gate-methodology beads)

- **FU-12 Command palette / context menu action delivery — first within R1.**
  `CommandPaletteEvent::Execute(_)` and `ContextMenuEvent::Selected(_)` are
  discarded, so both overlays are inert. Several later units assume this
  delivery mechanism exists. No parity row names it directly; it is
  nonetheless a prerequisite.
- FU-5 Pane tree — 8 pane-layout actions. **Covered by bead `.58`.**
- FU-6 Workspace tree — 6 workspace-layout actions **(covered by `.58`)** plus
  `CreateWorkspace`, `CloseWorkspace`, `MoveSession`, `ReportWorkspaceTree`,
  `WorkspaceInfo`, which `.58` does **not** name. **Covered by bead `.66`**,
  asserted on the wire by `tests/e2e/visual/workspace-ipc.sh`.
- FU-7 Scrollback navigation and marks — `scroll_up/down/top/bottom`
  **(covered by bead `.59`)** plus `prompt_jump_up/down`, `jump_to_failure`,
  `PromptMark`, `ScrollBottom`, which are not covered.
- FU-8 Clipboard and selection — `copy`, `paste`, the four OSC 52 bridge/prompt
  rows, and routing `DialogEvent::Chosen` to a real response. Selection
  groundwork is **partly covered by bead `.59`** (`smart_selection`).
- FU-9 Find overlay — `find`, `SearchRequest`, `SearchResults`.
- FU-10 Zoom — `zoom_in`, `zoom_out`, `zoom_reset`.
- FU-11 close_tab chord and new_window. **Covered by bead `.61`.**

### Phase R2 — window, lifecycle, and the unreachable 013/014/015 surface (P2)

Window and lifecycle first (they gate cutover behaviour), then the remote/LAN/
sharing tail, which is the largest single block of unreachable rows:

- FU-13 Window lifecycle — `CloseWindow`, `QuitAll`, `WindowClosed`,
  `QuitRequested`, `ListWindows`, `WindowList`, `FocusChanged`.
- FU-14 Update surfaces in the terminal window — `TriggerUpdate`,
  `DismissUpdate`, `UpdateAvailable`, `UpdateProgress`. (`CheckForUpdates`,
  `ListReleases`, `UpdateCheckResult`, `ReleaseList` are already reachable, but
  only from the `--settings` window.)
- FU-15 X11 focus guard — start `x11_focus.rs` from `open_window`.
- FU-20 Subscribe / snapshot tooling; FU-21 workspace notes on a real
  workspace (de-demo the fabricated `WorkspaceId` and route the reply);
  FU-22 Bell; FU-23 in-app settings entry point.
- FU-16 Remote (tailnet), FU-17 LAN (mTLS) dial and approval, FU-18 trusted
  devices/networks in the settings window, FU-19 sharing and control. The whole
  of features 013/014/015 is currently unreachable from the GPUI client;
  FU-18's transport helpers already exist in `settings/server_action.rs` and
  only need settings-page controls that call them.

### In-flight bead coverage (do not double-count or re-file)

| Bead | Fix unit | Rows already claimed |
| --- | --- | --- |
| `.56` (opacity) | FU-4 | Opacity — landed `771794d`, re-verify via `.53` |
| `.58` (pane/workspace) | FU-5, part of FU-6 | 8 pane-layout + 6 workspace-layout actions; **not** `CreateWorkspace`, `CloseWorkspace`, `MoveSession`, `ReportWorkspaceTree`, `WorkspaceInfo` |
| `.66` (workspace IPC) | rest of FU-6 | `CreateWorkspace`, `CloseWorkspace`, `MoveSession`, `ReportWorkspaceTree`, `WorkspaceInfo` |
| `.59` (vi / smart-selection / split-scroll) | part of FU-7, FU-8 | `scroll_up/down/top/bottom`; selection groundwork for `copy` |
| `.61` (close_tab / new_window) | FU-11 | `close_tab`, `new_window` |
| `.62` (status-band layout) | supports FU-2 | status-band chrome the FU-2 metadata rows render into. Not named by the audit (filed after it); it lands the surface, not the server-metadata ingestion |

Everything else in FU-1..FU-23 is currently unfiled.

### Phase H re-baseline

The launch gate `scribe-38e.42` is re-baselined on **reachable-row count**, not
the unit-test count. The green workspace unit-test suite (850 tests at the gate run) proves logic; it
proved nothing about whether the running client constructs the tested modules,
which is how the original gate produced a false parity reading. The gate metric
is the reachable-row total from `parity-inventory.md`'s roll-up, with an
explicit go threshold. That total is not typed into the document: `just
parity-inventory` (`tools/check-parity-inventory.sh`, also a pre-commit hook
and a quality-workflow step) re-derives every count in the file from its
per-row marker cells and cross-checks the tables against the protocol enums and
the live dispatcher, so reading the number off the file is safe. It stood at
51/164 user-facing rows when this gate was written.

**The denominator is derived from `spec.md`, not from the legacy client's IPC
surface** (bead `scribe-38e.94`, 2026-07-27). Every acceptance criterion and
porting obligation carries a stable register id, the inventory carries a
coverage index over those ids, and the same check fails when an id has no
carrying row — so a requirement can no longer be absent from the metric. Adding
that derivation grew the inventory from 174 to 203 rows (194 user-facing) and
moved the figure to 188/194 in-client, 189/194 reachable, because five
requirements that had never been tabulated turn out not to be reachable
(FU-24..FU-28 in `reachability-audit.md`). Cutover (`scribe-38e.43`+) stays blocked until that
threshold is met alongside the existing perf and manual re-gate criteria in
`launch-gate-checklist.md`.

#### The go threshold

**Go requires every user-facing row to be reachable: `Unwired = 0` and
`Missing = 0` on the roll-up's Total row — 194 of 194 user-facing rows (100%)
at the register's current size, 203 of 203 including the removed-configuration-
key rows.** Anything less is a NO-GO on this criterion. Score it mechanically
with `just parity-gate` (`tools/check-parity-inventory.sh --gate`), which
re-derives the counts exactly as the pre-commit check does and then exits
non-zero while any user-facing row is unreachable. At the 2026-07-27 gate the
score was 189/194, five rows short.

The number is derived, not chosen. Goal 1 of `spec.md` is "full, reachable
feature parity … no user-visible regression in functionality", and
`parity-inventory.md`'s "Definition of done" makes a row done only when its
"Reachable from" cell names a live-path symbol *and* its verification method
passes. An unreachable user-facing row is therefore a user-visible regression
by definition, so the only threshold consistent with the spec is all of them.
No sub-100% figure is derivable: the spec grants no regression budget, and
picking one would be a silent amendment of Goal 1.

Two clarifications keep the bar honest rather than merely strict:

- **The denominator moves; the bar does not.** 194 is today's count, not a
  constant. Adding a requirement to `spec.md`'s register adds a row and raises
  the denominator (the check fails until it does), and the threshold stays
  "all of them". Cite the ratio, not the literal, when the register grows.
- **Descoping is the only relief valve, and it shrinks the denominator instead
  of lowering the bar.** Re-gate criterion B1 already allows a capability to be
  "explicitly descoped in `spec.md` with a recorded decision"; that deletes the
  register id and its row, so the surviving rows must still all be reachable.
  A descope is a human decision recorded in the spec — never a threshold
  adjustment made at gate time.

Meeting the threshold is necessary, not sufficient: reachability is a
per-row structural fact, and the gate still requires each row's own
verification method to pass plus the perf, IME and manual criteria in
`launch-gate-checklist.md`. `tools/check-reachability.sh` remains a ratchet
below this threshold — it fails only when the unreachable set grows, and it
scores modules rather than requirements, so it can pass while a requirement
row is unreachable (`ai_indicator` did exactly that).

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
