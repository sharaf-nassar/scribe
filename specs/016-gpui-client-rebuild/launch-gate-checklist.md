# GPUI client launch gate — go/no-go checklist (scribe-38e.42)

Decision date: 2026-07-27. Evaluated against `main` at `394ce63`. Driven by the
workspace unit/golden/gpui-test oracle, the visual-E2E and func/lifecycle
scripted-E2E Docker suites (both images rebuilt from this tree), the perf A/B
rig (`tools/perf-ab-rig`), the mechanical reachability ratchet
(`tools/check-reachability.sh`), a per-row re-verification of
`parity-inventory.md` against the source, and live manual driving of the new
GPUI client against the isolated `scribe-dev` server sandbox.

`/usr/bin/scribe-dev` was confirmed byte-identical (`md5sum`) to
`target/release/scribe-client-gpui` built from this tree, so the manual rows
were driven against exactly the code under gate. The production server
(pid 492537) was never restarted, stopped, or upgraded.

This run supersedes the 2026-07-24 NO-GO. **All four blockers named there are
confirmed resolved.** The gate nevertheless fails again, on different ground
that no previous run had measured.

## Decision: 🔴 NO-GO

Cutover (`scribe-38e.43`+) must NOT begin.

Every suite the gate has historically relied on is green — 947 unit/golden/
gpui-test assertions, 31 of 31 visual-E2E scripts, 13 of 13 func/lifecycle
scripts, and a green reachability ratchet. The gate fails because those suites
do not cover the failures, exactly as the 2026-07-24 retraction warned:

- **Nine `spec.md` requirements are implemented, unit-tested, and unreachable
  in the running client** — and not one of them was ever enumerated as a
  `parity-inventory.md` row, so no oracle scores them (B1). The IME half of
  this is now *empirically* demonstrated, not merely inferred (B2).
- **Two of five perf metrics FAIL** against baselines that were, for the first
  time, actually captured from a probe-instrumented old client (B3).

## Oracle results

| Oracle | How driven | Result |
| --- | --- | --- |
| Unit / golden / `#[gpui::test]` | `cargo test --workspace --no-fail-fast` | ✅ **947 passed, 0 failed, 0 ignored** across 20 suites (850 at the prior gate) |
| Visual E2E | every `tests/e2e/visual/*.sh` in a rebuilt `scribe-test-visual` image, each with the env its `just` recipe specifies | ✅ **31 / 31 PASS, 0 FAIL** |
| Func / lifecycle scripted E2E | every `tests/e2e/func/*.sh` in a rebuilt `scribe-test-func` image | ✅ **13 / 13 PASS, 0 FAIL** |
| Perf A/B rig | `run-perf-ab.sh --live --record-baseline`, both clients probe-instrumented, same host and session | ❌ **FAIL** — 2 of 5 metrics (B3) |
| Reachability ratchet | `tools/check-reachability.sh --working-tree` | ✅ **PASS** — 53/65 modules wired, 54/59 server messages handled, 36/36 layout actions handled |
| Parity rows | all 173 rows re-verified against the source at HEAD | ⚠️ 163/164 reachable, but the artifact is stale (B4) and the denominator is incomplete (B5) |
| Manual rows | live driving of `scribe-dev` on `:0` | ⚠️ Opacity **PASS**; IME **FAIL** (B2) |

### Prior blockers — all four confirmed resolved

| Prior blocker | Status | Evidence from this run |
| --- | --- | --- |
| B1 — startup perf (`.50`, `.83`) | ✅ RESOLVED | Scribe-attributable startup **26.8 ms** vs the 150 ms absolute budget; total first frame **615.2 ms** vs the old client's **4572.0 ms**. Metric verdict PASS. |
| B1 — perf-rig completeness (`.51`) | ✅ RESOLVED | The rig drove and thresholded **all five** Q3 metrics, and `--record-baseline` filled every previously empty slot in `perf-baseline.md`. |
| B2 — AI indicator context % (`.52`) | ✅ RESOLVED | `func/ai-state-indicator.sh` and `func/ai-context-thresholds.sh` both PASS (they were the two failures at the prior gate). |
| B3 — opacity (`.56`, then `.53`) | ✅ RESOLVED | Re-driven live this run — see "Manual item detail". |
| D4 — config watcher (`.57`) | ✅ RESOLVED | `visual/config-reload.sh` PASS. |

## Blockers (each NO-GO on its own)

### B1 — Nine `spec.md` requirements are unreachable in the running client

Each capability below is a numbered `spec.md` requirement with a complete,
unit-tested implementation in `crates/scribe-client-gpui/src/`. Each is
referenced by **nothing** on any live path of the shipped binary, so no user can
reach it. None is a `parity-inventory.md` row, which is why every oracle is
green.

| Lost capability | `spec.md` | Module | Live-path callers | Old client |
| --- | --- | --- | --- | --- |
| Mouse reporting (X10/SGR-1006, modes 1000/1002/1003) | :152 | `mouse_reporting.rs` | 0 — no `encode_mouse*` in `main.rs` or any binary submodule | wired |
| Mouse-wheel scrolling | :152 | — | **0 — `ScrollWheel`/`ScrollDelta`/`scroll_wheel` appear zero times in the whole crate**; `main.rs` registers only `on_mouse_down`/`on_mouse_move`/`on_mouse_up`/`on_key_down` | `main.rs:2188` → `handle_mouse_wheel` |
| IME / preedit composition | :161 | `preedit.rs` (507 lines, full `gpui::EntityInputHandler`) | 0 — `set_input_handler` is called nowhere in the crate | `preedit.rs` wired |
| Cold-restart restore (`RestoreStore`, `--restore-child` fan-out) | :176 | `restore_state.rs` | 0 on a live path — only `restore_replay.rs` refers to it, for one rounding helper | 25 call sites |
| Command-mark scrollbar | :217–218 | `scrollbar.rs` | 0 — zero `scrollbar` references outside the module | wired |
| Window geometry persistence | :321 | `window_state.rs` | 0 on a live path — its only caller is `restore_state.rs`, itself unwired | 14 call sites |
| Desktop notification dispatcher | :322 | `notification_dispatcher.rs` | 0 — no `spawn_dispatcher`/`NotifReq` on a live path | `notifications::NotificationTracker` → `maybe_fire_notification` |
| Server lifecycle management | :322 | `server_lifecycle.rs` | 0 — `main.rs:4563` connects directly via `UnixStream::connect(server_socket_path())`, with no autostart and no stale-socket diagnosis | wired |
| File drag-and-drop | — | `drag_drop.rs` | 0 — no `on_drop`/`ExternalPaths` handler | n/a |

`window_state.rs` → `restore_state.rs` → `restore_replay.rs` is a closed island:
nothing in it is reachable from `main.rs`.

The mechanical ratchet already lists all of these as unwired modules in
`tools/reachability-baseline.txt`. Of the twelve entries there, four are
harmless dead duplicates whose behaviour was re-implemented inline on the live
path — `divider` and `focus_border` → `main.rs::pane_border`; `palette` →
`color.rs::TerminalColors`; `workspace_notes_preview` →
`workspace_notes_modal::WorkspaceNotesModalView`. The other eight are the
capability losses above.

**Why the ratchet passes anyway:** it is a *ratchet*, not a threshold. It fails
only when the unreachable set grows, and is satisfied while each entry stays
listed. It was never a parity gate, and no parity gate caught these because
they are absent from the inventory (B5).

Several of these sit under beads that closed on the *port* rather than the
wiring: `.15` (scrollbar), `.17` (notifications), `.23` (mouse reporting),
`.26` (IME preedit), `.30` (server lifecycle, geometry, drag-drop), `.31`
(cold-restart restore).

### B2 — IME manual gate item FAILS (no longer merely unverifiable)

`plan.md`:223 designates IME an explicit manual gate item with a written
procedure ("compose text via ibus/fcitx on X11 and a Wayland compositor"), and
`spec.md`:161 requires it. The 2026-07-24 run recorded ⛔ NEEDS HUMAN because
the host had no input-method engine.

That is no longer the case: the host now runs `ibus-daemon --panel disable
--xim` with `table:cangjie3`, `table:cangjie5` and `table:cangjie-big`
installed, so the procedure is drivable. **It was driven, and it fails.**

Procedure and observation: launched `/usr/bin/scribe-dev` on `:0`, located its
window by PID (the host has 15+ pre-existing windows named `Scribe`, so a
name-based lookup is unsafe here), activated it, echoed a marker to get a clean
prompt, switched the engine with `ibus engine table:cangjie3` (confirmed
active), then sent `a`, `b`, `c` through XTEST with 0.4 s spacing.

A working CangJie integration intercepts those keystrokes into a composition
buffer and shows preedit plus a candidate list; the raw letters never reach the
PTY. The captured frame instead shows the shell prompt with a literal **`abc`**
on the command line — the keys bypassed the input method entirely. No preedit
overlay, no candidate window, and zero IME/preedit lines in the client log.

This matches the source exactly: `preedit.rs` ships a complete
`gpui::EntityInputHandler` (`Ime`, `PreeditMachine`, `compute_overlay`,
`replace_text_in_range`, `replace_and_mark_text_in_range`) and
`set_input_handler` is never called, so GPUI has no route by which to deliver
marked or committed text. `preedit.rs` has exactly one commit in its history
(`70953cc`, bead `.26` "port IME preedit, bracketed paste, and bell") and has
never acquired a referent.

The verdict moved from "unverifiable" to "fails" because the host gained an IME,
not because the code changed.

### B3 — Perf: two of five metrics FAIL

Measured live, both clients probe-instrumented and built from this tree, same
host and session, `--record-baseline`. This is the first run in which the three
comparative metrics had real baselines (see D2 for why earlier runs did not).

| Metric | New client | Old client | Budget | Verdict |
| --- | --- | --- | --- | --- |
| Scribe-attributable startup | **26.814 ms** | n/a | ≤ 150 ms absolute | ✅ PASS |
| Startup to first frame (total) | **615.175 ms** (26.8 Scribe + 589.0 gpui bring-up) | **4571.975 ms** | no worse than old, +10% allowance | ✅ PASS |
| Input latency (p50 echo) | **0.209 ms** | **0.032 ms** | no worse than old, +10% allowance | ❌ **FAIL (~6.5×)** |
| cat-firehose throughput | **17.623 MiB/s** | **0.232 MiB/s** | no worse than old, −10% allowance | ✅ PASS (~76×) |
| Memory at 10 tabs | **237.934 MiB** | **465.738 MiB** | ≤ old + 20% | ✅ PASS |
| Scroll fps / dropped | **29.364 fps, 13.592 % dropped** | n/a (absolute target) | 60 fps, < 1 % dropped | ❌ **FAIL** |

Recorded baselines now in `perf-baseline.md`:
`startup_first_frame_ms=4571.975`, `input_latency_p50_ms=0.032`,
`firehose_bytes_per_sec=243217.780`, `memory_rss_kb=476916`.

**Scroll** is an absolute target with no baseline dependency, so its FAIL is
unambiguous: under an 8 s driven paging workload the client sustains under half
the target frame rate and drops 13–16× the permitted share. Reproduced across
three independent runs (29.322 / 14.217 %, 29.207 / 15.851 %, 29.364 /
13.592 %). Note the rig scores scroll for the **new client only**
(`measure_scroll "$NEW_CLIENT"`), so it is not yet established whether the old
client clears 60 fps under the identical workload — that is the first
investigation step, not a reason to discount the FAIL.

**Input latency** regressed ~6.5× against the old client measured through the
same probe at the same points. Both absolute numbers are sub-millisecond, so
this is unlikely to be perceptible, but the Q3 criterion is comparative and it
is missed by a wide margin.

Startup, throughput, and memory are all comfortable passes — the rebuild is
dramatically better on those three.

### B4 — `parity-inventory.md` no longer describes the binary

The gate's parity metric is defined (`plan.md` § "Phase H re-baseline") as the
reachable-row count read off `parity-inventory.md`. That number cannot currently
be read from the file:

- **13 rows still carry `— (unwired)` / `— (missing)` markers, all stale.** All
  13 verify as reachable at HEAD: `ClipboardPromptResponse` and
  `ClipboardBridgeReadReply` (via `IpcSink::clipboard_answer`, called from
  `run_bridge_job` / `answer_clipboard_prompt`); `PromptMark`, `ScrollBottom`,
  `ClipboardPromptRequest`, `ClipboardBridgeWrite`, `ClipboardBridgeReadRequest`
  (named arms in `dispatch_server_message`); `copy`, `paste`, `prompt_jump_up`,
  `prompt_jump_down`, `jump_to_failure` (`handle_layout_action` is now
  exhaustive over all 36 variants — no catch-all remains); and `Opacity`
  (`opacity::{clamp_opacity, opaque_slot, surface}`, 13 uses on the render path).
- **The roll-up table still reports the `f56ef95` figures** — 60/173 reachable,
  "51 of 164 user-facing rows (31%)". Re-measured against the source at HEAD the
  true figure is **163 of 164**. The sole exception is `HookEvent`, whose named
  symbol lives in `scribe-hook-helper` and is out-of-client by design.
- **All five per-section `**Reachability:**` footers are stale.** The Rendering
  and window footer still reads "0 of 5 rows name a live-path symbol; 3 are
  unwired and 2 are missing"; four of five now do.
- **The reader prose (≈ lines 137–141) is stale**: it claims the live reader
  "matches exactly twelve variants and ends in a `_ => {}` catch-all".
  `dispatch_server_message` now names 54 of 59 and routes the rest to
  `unhandled_server_message`.
- Two inline annotations are stale: `command_palette` is described as
  "degenerate: `CommandPaletteEvent::Execute(_)` is discarded" (it is routed to
  `TerminalView::execute_palette_action`), and the LAN/sharing boundary note
  describes the legacy client's dispatch.

This is documentation drift, not missing wiring — but the gate metric is a
number read from this file, and that number is wrong by 112 rows.

### B5 — The parity denominator omits nine spec requirements

B4 is fixable by editing. B5 is not. `parity-inventory.md` enumerates 173 rows
across client messages, server messages, keybinding actions, rendering/window
surfaces, and removed configuration keys. It never enumerated mouse reporting,
mouse-wheel scrolling, IME composition, the command-mark scrollbar, cold-restart
restore, window geometry persistence, the desktop notification dispatcher,
server lifecycle management, or drag-and-drop — every one a `spec.md`
requirement (B1).

`reachability-audit.md` inherited the same blind spot: it audited "every row of
`parity-inventory.md`", so a requirement with no row could not surface in its
173-row census. **"163 of 164 rows reachable" is therefore a measure of the
tabulated subset, not of parity.** Both artifacts must be extended to the full
`spec.md` requirement set before the reachable-row count means what the gate
needs it to mean.

### B6 — The go threshold is undefined

`plan.md` § "Phase H re-baseline" re-baselines the gate on reachable-row count
"with an explicit go threshold", and the prior revision of this checklist
required "the reachable-row count to meet its explicit go threshold". No numeric
threshold is stated in `plan.md`, `spec.md`, `parity-inventory.md`, or
`reachability-audit.md`. A criterion that names no number cannot be evaluated;
one must be set before the next run.

## Parity-inventory results by verification method

Methods are as they now stand in `parity-inventory.md` (post method-upgrade).
Every row's own stated method was exercised this run.

| Method | Rows | Oracle | Result |
| --- | --- | --- | --- |
| `scripted-E2E` | 77 | func + visual Docker scripts driving the real client and server | ✅ all driving suites green (13/13 func, 31/31 visual) |
| `visual-E2E` | 75 | visual Docker suite, `xdotool` against the real window | ✅ 31/31 scripts PASS |
| `visual-E2E (+ golden bytes)` | 7 | as above, plus the retained encoder fixtures | ✅ PASS |
| `gpui-test` | 9 | the nine removed-configuration-key rows only | ✅ PASS (within the 947) |
| `golden` | 4 | captured byte/serialization fixtures | ✅ PASS (within the 947) |
| `manual` | 1 | Opacity, driven live against `scribe-dev` | ✅ PASS |

Reachability was additionally re-verified per row against the source at HEAD:
163 of 164 user-facing rows name a symbol genuinely on a live path — subject to
B4 (the file does not say so) and B5 (the row set is incomplete).

## Visual E2E detail — 31/31 PASS

`ai-task-label`, `bell`, `clipboard-osc52`, `color-emoji`, `config-reload`,
`dialogs`, `find-overlay`, `lan-approval`, `overlay-actions`, `overlays`,
`pane-workspace-layout`, `paste-confirmation`, `prompt-marks`, `reconnect`,
`remote-control`, `session-tooling`, `settings-entry`, `settings-trust`,
`share-control`, `tab-window-chords`, `terminal-viewport`, `terminal-zoom`,
`titlebar`, `update-trigger`, `update-dismiss`, `window-chrome-bands`,
`window-lifecycle`, `workspace-ipc`, `workspace-notes`, `workspace-split`,
`x11-focus-guard`. (`update-common.sh` is a helper, not a test.)

`reconnect` passed all four phases but required manual intervention to finish —
see D1.

## Func / lifecycle E2E detail — 13/13 PASS

`ai-context-thresholds`, `ai-state-indicator`, `cold-restart`,
`failure-server-down`, `failure-socket-loss`, `hot-reload`,
`keybindings-validation`, `multi-window`, `reconnect`, `shell-integration`,
`smoke`, `terminal-shortcuts`, `workspace-split`.

The first two were the prior gate's two failures and are now green.
`cold-restart` passes but does not exercise the client — see D3.

## Manual item detail

| Item | Verdict | Evidence |
| --- | --- | --- |
| Opacity | ✅ **PASS** | Driven live against `scribe-dev`. A magenta backdrop window was placed behind the client (ImageMagick `display`; `xsetroot` was deliberately avoided so the user's desktop root was untouched), the client window located by PID, and the screen captured cropped to the window geometry. `opacity = 1.0` → `srgb(30,30,30)`; `0.85` → `srgb(35,29,35)` at the same three sample points, i.e. magenta bleeding through, with 484 811 differing pixels and a max channel delta of 138. Clamping is correct and harmless: `1.5` → `srgb(30,30,30)` (opaque), `-0.2` → `srgb(255,0,255)` (fully transparent, pure backdrop). All four edits applied **live at the same pid and the same X window id**, proving reload without restart. |
| IME | ❌ **FAIL** | See B2 — literal `abc` reached the shell with CangJie active. |
| Dialogs | ✅ PASS | Now automated: `visual/dialogs.sh` PASS (close dialog Quit/Kill/Cancel with Cancel default; OSC 52 clipboard dialog with payload preview). No longer a manual row. |
| Bell | ✅ PASS | Now automated: `visual/bell.sh` PASS, asserting the `WM_HINTS` urgency flag. No longer a manual row. |

## Additional defects found during gate driving (not parity rows)

- **D1 — `tests/e2e/visual/reconnect.sh` hangs the harness after passing.** All
  four phases report PASS, then the container never exits: the script relaunches
  its client with `scribe-client-gpui &` and never kills it, so the orphan holds
  the entrypoint's `tee /output/result.log` pipe open indefinitely
  (`entrypoint-visual.sh`: `timeout "$TEST_TIMEOUT" "$1" 2>&1 | tee
  /output/result.log`). `TEST_TIMEOUT` governs the script, which has already
  exited, so nothing ever breaks the pipe. Every other relaunching script
  (`tab-window-chords.sh`, `pane-workspace-layout.sh`) ends with
  `kill "$SCRIBE_CLIENT_PID"`. The 153 s recorded above includes ~140 s of hang
  ended by killing the orphan by hand. This blocks unattended CI runs.
- **D2 — the perf rig silently degrades, producing three misleading
  `NO-BASELINE` verdicts.** Two independent causes. (a) Without `scribe-test` on
  `PATH` the rig logs "cannot seed a session for the client" and continues
  anyway. (b) The report's own "Reproducing" section documents
  `--old-client /usr/bin/scribe-client`, but the **installed** old client
  predates the shared probe (`strings /usr/bin/scribe-client | grep
  SCRIBE_PERF_PROBE` is empty), so `start_client`'s `[[ -s "$PROBE_FILE" ]]`
  guard never fires and every workload phase reports "client … never reached a
  first frame". Only `target/release/scribe-client` carries the probe. Both
  failures present as missing baselines rather than as a missing prerequisite.
  Re-running with `--scribe-test target/release/scribe-test --old-client
  target/release/scribe-client` produced all four baselines on the first try.
- **D3 — `func/cold-restart.sh` does not exercise the client.** It states
  plainly that "the daemon is the client stand-in", using `scribe-test daemon
  stop`/`start` for the cold quit and relaunch. It therefore passes while
  `restore_state.rs` is unreachable (B1), proving the server's session survival
  rather than the client's restore path.

## Re-gate criteria

Re-run this gate when all of the following hold:

1. **B1** — each of the nine unreachable capabilities is either wired to a live
   path with an oracle that drives the running app, or explicitly descoped in
   `spec.md` with a recorded decision. Wiring one means deleting its line from
   `tools/reachability-baseline.txt` and bumping the matching count.
2. **B2** — `set_input_handler` is called on the live window, and the
   ibus/CangJie procedure composes and commits CJK text into a pane instead of
   leaking raw letters to the PTY.
3. **B3** — scroll sustains 60 fps with < 1 % dropped under the rig's 8 s paging
   workload, and input-latency p50 returns to within 10 % of the old client.
4. **B4** — `parity-inventory.md` regenerated so its cells, footers, roll-up,
   and prose match the binary.
5. **B5** — `parity-inventory.md` and `reachability-audit.md` extended to the
   full `spec.md` requirement set, so the reachable-row count measures parity
   rather than the tabulated subset.
6. **B6** — a numeric go threshold for the reachable-row count written into
   `plan.md`.

The already-green evidence (947 unit/golden/gpui-test, 31/31 visual, 13/13 func,
the reachability ratchet, startup/throughput/memory perf, opacity) must stay
green. But — as the 2026-07-24 retraction established and this run confirms
again — green suites are necessary and never sufficient. The decisive question
is whether a user of the running client can reach each `spec.md` requirement,
and for nine of them the answer is still no.
