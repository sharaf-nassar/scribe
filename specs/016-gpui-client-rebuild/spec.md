# Spec: gpui-client-rebuild

## Problem Statement

Scribe's client UI feels dated next to modern terminals (iTerm, Ghostty, Warp,
Zed). The root cause is architectural: the entire client renders through a
single home-grown wgpu instanced-quad pipeline sampling one alpha-mask glyph
atlas. That pipeline cannot express color emoji, text decorations
(underline/undercurl/strikethrough), drop shadows, gradients, blur, smooth
scrolling, motion easing, or inline images — and every dialog is a flat,
sharp-cornered rectangle. Closing these gaps piecemeal means rebuilding a UI
framework one feature at a time.

We are instead rebuilding `scribe-client` from scratch on **GPUI** (Zed's UI
framework, pinned at zed `v1.12.0`), which provides all of the above natively
and ships a production terminal built on the same `alacritty_terminal` grid
library Scribe already uses. The server, IPC protocol, and `scribe-common`
stay unchanged; the client process is the entire rebuild surface. Scribe's
client-server split makes this safe: sessions survive client restarts by
design, so the new client is developed side-by-side against the live server
and swapped when it reaches parity.

This is a replacement, not a port with fallbacks: **no backward compatibility,
no legacy client support**. The old client and `scribe-renderer` are deleted
at cutover, along with all code that only existed to serve them.

- **For whom:** every Scribe user (daily-driver terminal users, AI-workflow
  users relying on Scribe's differentiating features).
- **Why now:** the UI gap list is long enough that incremental patching costs
  more than a rebuild; the GPUI ecosystem reached a usable state (wgpu
  renderer, published pin, verified local build); the project accepted
  GPL-3.0 relicensing, unlocking direct reuse of Zed's terminal code.

## Goals

1. **Full feature parity** with the current client per the parity inventory
   (46 `ClientMessage` variants sent, 57 `ServerMessage` variants handled, all
   subsystems in the inventory: terminal core, layout, chrome, dialogs,
   config, integrations). No user-visible regression in functionality.
2. **Modern UI capabilities from day one of cutover:**
   - Color emoji rendered in the grid (not tinted silhouettes).
   - Underline, double underline, colored undercurl, strikethrough.
   - Drop shadows and rounded corners on all overlays/dialogs.
   - Animated transitions (tab switch, focus change, overlay open/close)
     and smooth scrolling.
   - Custom titlebar with integrated tab bar (no stacked OS decoration +
     tab bar).
   - Hover/pressed states on all clickable chrome.
3. **Maximal code reuse from Zed** (GPL-3.0 relicense approved): crib
   `terminal.rs` display-only state management, `terminal_element.rs`
   painting, `mappings/` (keys/mouse/colors), `terminal_scrollbar.rs`,
   `alacritty.rs` glue.
4. **Aggressive deletion:** `scribe-renderer` crate removed; winit event loop
   and all wgpu plumbing in `scribe-client` removed; every module, config
   key, protocol shim, and test that served only the old client removed. No
   dead code left behind. `cargo build --workspace` clean with no unused-dep
   or dead-code warnings tied to the migration.
5. **Testing continuity:** `scribe-test` (server-only) untouched and green;
   visual E2E harness runs the GPUI client under Xvfb + software Vulkan and
   produces screenshots; new `#[gpui::test]` headless logic tests for
   client-side state (layout tree, selection, input encoding).
6. **License migration:** whole project relicensed to GPL-3.0-or-later
   (LICENSE files, all `Cargo.toml` license fields, README, any headers),
   with proper attribution for vendored/cribbed Apache-2.0 and GPL sources.
7. **De-risked sequencing:** early spikes prove the highest-uncertainty items
   (one-pane client against live server; terminal ligatures; Nerd-Font-first
   fallback ordering in GPUI's text system) before parity work fans out.

## Non-Goals

- **No server, protocol, or scribe-common changes** beyond what the client
  swap strictly requires (target: zero; the IPC contract is frozen for this
  feature). `specs/015-multi-machine-sharing` is in flight on the same
  server — this feature must not break it.
- **No terminal image protocols** (Sixel / Kitty graphics) in this feature.
  The rebuild makes them tractable later (GPUI has image elements), but
  parity + modernization is the scope. (Candidate follow-on feature.)
- **No backward compatibility:** no feature flag to run the old client, no
  transitional dual-render mode in the shipped binary, no config
  compatibility shims for removed appearance keys (removed keys are removed;
  config load must not error on their presence, but they do nothing).
- **No new end-user features** beyond the modernization list — feature work
  (new dialogs, new AI surfaces) waits until after cutover.
- **No new settings-app features** — the settings app is rebuilt as a GPUI
  window (Clarification Q6) but its feature set is ported 1:1; new settings
  capabilities are out of scope.
- **No Windows support work.** Linux (Wayland + X11) and macOS to the extent
  the current client supports them. (Open Question: current macOS status
  must be confirmed and matched, not extended.)

## User Stories

### US1 — Terminal parity for the daily driver
As a Scribe user, I want the new client to behave identically to the old one
for core terminal work, so that cutover is invisible to my muscle memory.

**Acceptance criteria:**
- All key input paths produce byte-identical sequences to the old client:
  kitty keyboard protocol (4-level priority chain, CSI-u), legacy xterm
  fallback, DECCKM/DECPAM application modes, numpad SS3 table.
- Mouse reporting (X10/SGR-1006, modes 1000/1002/1003) byte-identical.
- Selection (cell/word/line, copy-on-select), smart selection actions,
  search overlay with match cycling, URL/path detection with Ctrl+click
  open, OSC 8 hyperlinks with hover tooltip and disallowed-scheme gate all
  work as today.
- Scrollback, viewport scrolling, `TrimScrollback` mark shifting, split-
  scroll (pinned live bottom in AI panes) work as today.
- Bracketed paste, paste confirmation, OSC 52 clipboard gating dialogs work
  as today.
- IME/preedit composition works on Wayland and X11.
- Sync-update frames (CSI ?2026) never tear: one committed burst per redraw,
  150 ms expiry flush, catch-up drain — matching current
  `queue_output_frames` semantics.

### US2 — Session lifecycle parity
As a Scribe user, I want reattach, reconnect, cold-restart restore, and
server upgrades to work exactly as today, so that the multiplexer promise
(sessions outlive clients) holds.

**Acceptance criteria:**
- `SessionReplay` (zstd ANSI) rebuilds panes correctly on reattach via the
  display-only `write_output` path.
- `ScreenSnapshot` handling, reconnect topology reconstruction from
  `SessionList`, and window adoption (`Hello { takeover }`) work as today.
- Cold-restart restore (`RestoreStore`, `--restore-child` fan-out) restores
  windows, workspaces, tabs, and pane trees.
- Zero-downtime server upgrade (`--upgrade` handoff) leaves the new client
  attached and rendering.

### US3 — Modern visual polish
As a Scribe user, I want the client to look and feel like a current-
generation terminal, so that the UI no longer reads as dated.

**Acceptance criteria:**
- Emoji render in color in the grid. Every terminal glyph run supplies
  `FontFallbacks::from_fonts` with Scribe's Nerd Font entries before generic
  symbol and emoji fallbacks, preserving the current fallback ordering.
- Procedural box drawing for U+2500–U+259F remains independent of terminal
  font coverage: those cells bypass text shaping and paint alpha-mask quads
  after backgrounds and before text glyphs.
- Terminal ligatures honor `appearance.ligatures`: same-style runs use
  `shape_line` with the grid `cell_width`, and disable `calt` only when the
  setting is false, so later cell origins stay aligned.
- Undercurl (wavy, colored), underline, double underline, strikethrough
  render from cell flags.
- Every overlay (command palette, context menu, all dialogs, tooltips) has
  rounded corners, a drop shadow, and hover + pressed states on buttons.
- Tab switches, overlay open/close, and focus changes animate (fast,
  interruptible easing; no animation exceeds 150 ms).
- Scrolling is pixel-smooth (wheel and touchpad), including scrollback.
- The window uses a custom titlebar with the tab bar integrated; native
  decorations are gone on Linux. `appearance.opacity` updates live on Wayland
  and X11 by repainting alpha-aware terminal and chrome backgrounds on a
  transparent GPUI surface; text and controls stay opaque.
- On X11, the focus guard retains its exact EWMH comparison by extracting the
  GPUI `Xcb` raw-window-handle XID and comparing it with `_NET_ACTIVE_WINDOW`.

### US4 — Differentiating features preserved
As an AI-workflow user, I want Scribe's differentiators intact, so the
rebuild doesn't trade identity for polish.

**Acceptance criteria:**
- AI indicator (pulsing borders, priority ordering, stale-clear), prompt bar
  (elapsed timer, context meter, dismiss/copy), task labels in tabs, tab
  context-% suffix all work as today.
- Command-boundary marks render on the scrollbar (success/failure ticks,
  absolute-position shift on trim) — built as a custom GPUI scrollbar
  element (Zed has no equivalent).
- Workspace system (accent colors, badges, notes modal + hover preview,
  workspace splits) works as today.
- Remote/LAN: remote connect picker, LAN approval dialog (fingerprint
  words), lost-control banner, share roster/control passing all work as
  today.
- Status bar segments (connection, command status, env warning, CWD, git
  branch, session count, time, CPU/mem/GPU/net sparklines, update CTA)
  present.

### US5 — Clean tree for the maintainer
As the maintainer, I want the old rendering stack fully excised, so that no
dead code, unused deps, or stale docs remain.

**Acceptance criteria:**
- `crates/scribe-renderer` no longer exists; `winit`, `wgpu`, `cosmic-text`,
  and other old-client-only deps are gone from the workspace `Cargo.toml`
  (except where GPUI transitively provides its own).
- `scribe-settings`' GTK/wry webview delivery (GTK dep, HTML/CSS/JS assets)
  no longer exists; settings ship as a GPUI window with the same feature
  set (Clarification Q6).
- No module in the new client is unreferenced; `cargo udeps`-style check (or
  equivalent) is clean; grep for old-pipeline identifiers
  (`CellInstance`, `solid_quad`, `terminal.wgsl`) returns nothing.
- Ported pure logic (xterm-256 palette, procedural box-drawing, color
  semantics: bold→bright, DIM 0.67, contrast rules) lives in clearly named
  new-client modules with their old tests carried over.
- `lat.md/` fully reflects the new architecture (client.md, rendering.md,
  architecture.md rewritten; stale sections deleted); `lat check` passes.
- LICENSE files, every `Cargo.toml` license field, and README reflect
  GPL-3.0-or-later; vendored Zed code carries its attribution.

### US6 — Testing continuity for CI
As the maintainer, I want CI to keep meaningful coverage through the swap,
so regressions are caught during the rebuild, not after cutover.

**Acceptance criteria:**
- `scribe-test` functional E2E suite passes unchanged (server-only).
- Visual E2E: the GPUI client runs headlessly in Docker (Xvfb or headless
  Wayland + lavapipe software Vulkan), is drivable by `xdotool`, honors the
  active-window guard semantics, and produces deterministic screenshots to
  `/output`.
- Client logic tests exist as `#[gpui::test]` headless tests for: layout
  tree operations, workspace tree, selection model, input encoding tables,
  sync-frame queueing, URL detection.

## Constraints

- **Pin:** zed `v1.12.0`, commit `f96212f2c50f54d93712fa130d6226b1ce7d76b5`.
  Consume `gpui` + `gpui_platform` as git deps pinned to that rev
  (`gpui_platform` is not on crates.io). Vendoring posture: pinned rev now;
  the project must be prepared to vendor/fork (existing `third_party/`
  convention) if upstream access becomes a problem.
- **Renderer:** GPUI at this pin renders via `gpui_wgpu` (wgpu 29; blade is
  gone). Linux features: `["font-kit", "x11", "wayland"]`.
- **Toolchain:** MSRV Rust 1.95.0 (verified: 1.94 fails on `cold_path`;
  `cargo check` against the pin passes locally on 1.95.0). CI images and
  developer docs must move to 1.95.0.
- **System deps (Linux):** `libfontconfig-dev libssl-dev libvulkan1
  libwayland-dev libx11-xcb-dev libxkbcommon-x11-dev libzstd-dev clang`;
  runtime needs a Vulkan ICD (lavapipe acceptable for CI).
- **Build ergonomics:** set `[profile.dev.package.gpui] opt-level = 3`
  (gpui-component precedent); budget multi-minute cold builds.
- **License:** GPL-3.0-or-later for the whole project (user-approved). This
  is what permits copying Zed's `terminal`/`terminal_view` (GPL-3.0) code.
  GPUI itself is Apache-2.0; attribution obligations for both must be met.
- **alacritty_terminal version:** Zed pins its own fork
  (`zed-industries/alacritty` rev `4c129667`, 0.26.1-dev); Scribe uses
  crates.io `0.26.0-rc1`. Cribbed code must either adopt Zed's fork or be
  diffed against stock — decide during plan (leaning: adopt Zed's fork in
  the client to guarantee API match; server keeps its own pin unless
  identical grids are required — verify `scribe-common` snapshot types
  don't couple the two).
- **Architecture invariants:**
  - Server, IPC protocol (`scribe-common/src/protocol.rs`), and
    `scribe-test` are frozen surfaces.
  - The new client is `TerminalType::DisplayOnly`-style: a Terminal entity
    fed by IPC bytes via `write_output`; `write_to_pty` equivalents send
    `KeyInput` over the socket.
  - Keystroke-before-output ordering (Resize flushed before KeyInput; PTY
    coalescing that never queues keystrokes behind output) must be
    preserved in the GPUI executor model — mirror Zed's 4 ms / 100-event
    wakeup coalescing.
  - Kitty keyboard protocol is ported from Scribe's `input.rs` (Zed lacks
    it; GPUI exposes raw key + modifiers + repeat + IME, which is
    sufficient).
- **Porting obligations (framework-independent logic that must survive):**
  xterm-256 palette (~200 LoC), procedural box-drawing rasterizer (~480 LoC
  — Zed relies on fonts; Scribe's edge-to-edge rasterizer is a quality
  feature to keep — see Open Questions for the GPUI integration mechanism),
  bold→bright promotion / DIM factor / sRGB-linear conversions / brightness
  boost, Scribe font fallback ordering, command-mark scrollbar, sync-frame
  splitter client logic, X11 active-window guard, window geometry
  persistence, desktop notification dispatcher, server lifecycle management
  (`systemctl --user` / launchd), remote & LAN dial subprocess spawning.
- **Parallel feature in flight:** `specs/015-multi-machine-sharing` (branch
  `015-multi-machine-sharing`) touches server + client. Sequencing risk is
  real: parity inventory includes 015's client surfaces (share roster,
  control passing, LAN dialogs). Coordinate cutover after 015 lands or
  absorb its client surfaces into parity scope (Open Question).
- **Dev/test environment:** rebuild happens in a side-by-side binary
  (working name `scribe-client-gpui` until cutover renames it) so the old
  client keeps working throughout; NEVER restart the user's running Scribe
  server without explicit approval.

## Open Questions

> Items 1, 2, 7, 8, and 13 were **resolved at the clarify gate** — see
> `## Clarifications`. Items 3–6 and 9–12 carry into planning (3–6 via the
> spike gate, 9–12 as plan decisions).

1. **Settings app delivery:** today `scribe-settings` is a separate
   GTK/webview process with its own Unix-socket singleton. Options:
   (a) keep it unchanged for this feature (smallest scope, but it will not
   match the new visual language), (b) rebuild it as a GPUI window in the
   new client (bigger scope, deletes a whole crate + GTK dep). Which?
2. **macOS scope:** the current client has macOS pathways (FSEvents config
   watcher, launchd lifecycle, notify-rust). Is macOS a supported target of
   the rebuild at cutover, or Linux-first with macOS following? (GPUI's
   macOS support is first-class, but every platform doubles E2E work.)
3. **alacritty fork adoption:** adopt `zed-industries/alacritty` fork in the
   client only, or workspace-wide (server too)? Does `scribe-common`'s
   snapshot/replay code compile against both grids? Needs a quick spike.
4. **Box drawing under GPUI:** the procedural rasterizer needs a rendering
   entry point — options: custom glyph provider into GPUI's text system (if
   such a hook exists at the pin), a paint-quad overlay pass in the terminal
   element keyed on box-drawing codepoints, or trusting font coverage (Zed's
   approach — rejected? confirm). Mechanism must be validated in a spike.
5. **Ligatures:** does GPUI's `shape_line` with forced `cell_width` produce
   correct multi-column terminal ligatures with the user's configured font?
   Spike before committing to parity of the `ligatures` config key.
6. **Font fallback ordering:** GPUI's text system does its own fallback; can
   Scribe's Nerd-Font-before-symbol-fonts ordering be expressed (custom
   fallback list per font stack)? If not, what regresses?
7. **015 sequencing:** land 015 first and then cut over (parity includes its
   surfaces), or freeze 015's client surface now? Affects epic ordering.
8. **Splash screen:** keep a splash (as a GPUI image element) or delete the
   concept (GPUI startup may be fast enough)? Leaning: delete.
9. **Zoom semantics:** current zoom is font-size scaling per window; GPUI
   has its own scale/rem model. Same behavior, or adopt GPUI-native zoom?
10. **Old-config appearance keys:** several `AppearanceConfig` keys are
    pipeline-specific (e.g. `scrollbar_width` hover-lerp constants baked in
    code, `prompt_bar_*` colors). Which keys survive, which are silently
    ignored, and is a one-time "unknown key" warning wanted? (No compat
    shims per Non-Goals; but config files must not hard-error.)
11. **Visual E2E determinism:** GPUI animations + software Vulkan — do we
    need a global "reduce motion / disable animations" test hook to keep
    screenshots deterministic? (Likely yes; decide where it lives —
    env var vs config key.)
12. **Window transparency/opacity:** GPUI window transparency support on
    Wayland/X11 at the pin — confirmed? The `opacity` config key's fate
    depends on it.
13. **Performance budget:** what is the acceptance threshold for input
    latency and sustained-output throughput vs the old client (e.g. no
    worse than old client on `cat` firehose test, `vtebench`)? Numbers
    needed for the analyze gate.

## Spec Review

Six parallel review passes (requirements, gaps, ambiguity, feasibility,
scope, stakeholders). Findings merged; cross-dimension hits ranked first.

### Critical Questions (answer before planning)

1. **Cutover safety: what happens to users the auto-updater strands?** The
   server auto-updater ships client binaries and `postinst` silently kills
   and relaunches the client on upgrade. GPUI requires a working Vulkan ICD;
   users on VMs, forwarded X, or old GPUs would receive a client that cannot
   start, with no rollback path — and the spec forbids a legacy fallback.
   Also: Debian `Depends` (currently `libgtk-4-1, libvulkan1`) must grow
   GPUI's runtime libs. Need: minimum-GPU/Vulkan policy (is lavapipe
   software rendering an acceptable end-user fallback?), a pre-flight check
   or staged rollout in the updater, and packaging scope acknowledged.
   — flagged by: gaps, stakeholders, requirements.
2. **Parity has no in-repo oracle.** Every "works as today" criterion
   references a parity inventory that lives in session history, not the
   repo. Decision: commit the full inventory as a spec artifact
   (checklist), and define the verification method per class —
   golden byte-capture corpus for input/mouse encoding ("byte-identical"
   currently has no defined harness), manual checklist vs automated test
   for chrome/dialogs. — flagged by: requirements, ambiguity, scope.
3. **Performance budget needs numbers now.** "No user-visible regression"
   is unenforceable without thresholds: input latency, sustained throughput
   (`cat` firehose / vtebench vs old client), memory per pane at N tabs,
   startup time (also decides the splash question). — flagged by:
   requirements, ambiguity, scope.
4. **015-multi-machine-sharing sequencing.** Its client surfaces (share
   roster, control passing, LAN dialogs) are simultaneously in-flight and
   in this feature's parity list. Decide: land 015 first (parity includes
   its final surfaces) or freeze 015's client surface now. Affects epic
   ordering and the parity oracle. — flagged by: all six dimensions.
5. **Phasing: this is 3-4 features wearing one epic.** (a) The GPL-3.0
   relicense must land as step-0 — before any Zed code is copied, not as a
   cutover bullet. (b) Split acceptance criteria into launch-blocking
   (US1/US2 correctness, key encoding, session lifecycle) vs post-cutover
   polish (animations, shadows — US3 cosmetics) or cutover waits on
   cosmetics. (c) The deletion sweep (US5) should trail cutover as its own
   phase, not gate it. Confirm this phasing. — flagged by: scope,
   ambiguity.
6. **Two scope-sizing decisions are still open:** settings app (keep GTK
   webview as-is vs rebuild as GPUI window — swings scope by a crate) and
   macOS (supported at cutover vs Linux-first — doubles the E2E matrix).
   These bound the MVP and cannot stay Open Questions into planning.
   — flagged by: scope, ambiguity, stakeholders.
7. **Spike gate before parity criteria freeze.** Five acceptance criteria
   assume unproven GPUI capabilities: procedural box-drawing entry point,
   Nerd-Font-first fallback ordering, terminal ligatures via `shape_line`,
   window opacity on Wayland/X11, and the X11 active-window guard needing
   the window XID (does GPUI expose raw window handles?). The spike results
   must be allowed to *rewrite* US3 criteria, not just inform them.
   — flagged by: feasibility, requirements, ambiguity.

### Non-Blocking Observations

- **IPC-thread ↔ GPUI-executor bridge is the core concurrency design** and
  must be fully specified in plan.md (channel topology, coalescing,
  keystroke-before-output ordering in GPUI's executor model) — design work,
  not a human decision (gaps, feasibility).
- A user-facing "reduce motion / disable animations" setting would resolve
  both the latency-purist conflict and visual-E2E determinism (OQ11) with
  one mechanism (stakeholders, requirements).
- Promote to testable criteria: legacy config loads without error (removed
  keys inert); reconnect/replay failure and timeout behavior; degraded
  chrome states (server down, socket gone, adoption failure).
- Keybinding customization (50+ configurable actions) should be named
  explicitly in the parity checklist, not folded into "config" (gaps).
- Accessibility posture unstated: GPUI ships AccessKit; state a11y
  intent (or explicit out-of-scope) for chrome (gaps).
- Crash reporting / telemetry / i18n: state explicit out-of-scope lines
  (gaps).
- Relicense is a communications event: README/NOTICE/changelog owner
  needed; acknowledge co-contributor attribution in the relicense commit
  (stakeholders).
- scribe-cli assumed unaffected (depends only on scribe-common) — verify
  once in plan (stakeholders).
- Verified during review: scribe-common snapshot/replay types are decoupled
  from alacritty grid types, so the alacritty-fork decision (OQ3) is
  client-local and lower-risk than feared (feasibility).
- "Pixel-smooth" scrolling needs a frame-budget number (e.g. sustained
  60 fps, dropped-frame ceiling) to be testable (requirements).

## Clarifications

Human answers at the clarify gate (2026-07-23). No constitution.md exists;
the human chose to proceed without one.

**Q1: Cutover safety — Vulkan-less users and the auto-updater?**
A: **Pre-flight Vulkan probe with lavapipe fallback.** The client (or its
launcher) probes for a working Vulkan ICD at startup; falls back to lavapipe
software rendering when no hardware ICD works. `mesa-vulkan-drivers` (or
distro equivalent) joins Debian `Depends` alongside GPUI's runtime libs. The
cutover release's `postinst`/updater aborts the client swap (keeps sessions
alive; surfaces an error) only if even software Vulkan cannot initialize.
→ Reflected in Constraints (packaging) and US2.

**Q2: Parity oracle?**
A: **Both.** The full parity inventory becomes a committed spec artifact
(`parity-inventory.md`); input/mouse encoding is verified by a golden
byte-capture harness (old client's encoder tables as fixtures); chrome and
dialogs verified by a per-item checklist tied to the inventory.
→ Reflected in Goals 1 and US1/US6.

**Q3: Performance budget?**
A: **Accepted as proposed:** input latency and `cat`-firehose throughput no
worse than the old client on the same machine/session; memory ≤ old client
+20% at 10 tabs; startup ≤ 500 ms to first frame; sustained 60 fps scroll
with <1% dropped frames. The splash screen is deleted if startup meets the
500 ms budget (resolves OQ8, leaning confirmed).

**Q4: 015 sequencing?**
A: **Land 015 first.** The parity target includes 015's final client
surfaces. The epic's cutover-critical beads depend on 015 landing; early
phases (relicense, spikes, scaffold) may proceed in parallel.

**Q5: Phasing?**
A: **Confirmed:** step-0 GPL-3.0 relicense lands before any Zed code enters
the tree; phase-1 launch gate is US1/US2 correctness + core chrome (US3
cosmetics may trail cutover); phase-2 deletion sweep (US5) follows cutover
as its own phase.

**Q6: Settings app and macOS scope?**
A: **Settings: rebuild as a GPUI window (in scope).** Keeping the GTK
webview would contradict the no-legacy mandate — `scribe-settings`' GTK/wry
webview delivery is deleted; its feature set (appearance, keybindings,
themes, workspace roots, AI indicator config, releases page) is ported 1:1
to a GPUI settings window. The settings singleton behavior (lock file +
focus handoff socket) is preserved or absorbed into the client process
(plan decides which). **macOS: Linux-first**; macOS follows post-cutover.

**Q7: Spike gate?**
A: **Yes.** The five capability spikes (box-drawing entry point, Nerd-Font
fallback ordering, ligatures, window opacity, X11 window handle) run first
and their results may rewrite the affected US3 acceptance criteria before
parity work fans out.
