# GPUI client launch gate — go/no-go checklist (scribe-38e.42)

Decision date: 2026-07-27 (third run). Evaluated against `main` at `544f1a5`.
Driven by the workspace unit/golden/gpui-test oracle, the visual-E2E and
func/lifecycle scripted-E2E Docker suites (both images rebuilt from this tree),
the perf A/B rig (`tools/perf-ab-rig`) in `--live` mode against the isolated
`scribe-dev` server, the mechanical reachability ratchet
(`tools/check-reachability.sh`), the parity drift check and go-threshold scorer
(`just parity-inventory` / `just parity-gate`), and live manual driving of the
new GPUI client on `:0`.

Every binary under gate was built from this tree. The manual rows were driven
against `target/release/scribe-client-gpui` staged under the `scribe-dev`
runtime slug — the installed `/usr/bin/scribe-dev` dates from 2026-07-23 and no
longer matches, so it was deliberately not used. The production server
(`/usr/bin/scribe-server`, pid 492537, started 2026-07-21) was never restarted,
stopped, upgraded, or connected to; no client, rig or harness in this run
addressed the default socket.

This run supersedes the 2026-07-24 and 2026-07-27 (`250e3e3`) NO-GOs. **All
twelve blockers named across those two runs are confirmed resolved.** One
criterion still fails.

## Decision: 🔴 NO-GO

Cutover (`scribe-38e.43`+) must NOT begin.

The gate now fails on exactly one criterion, and it is the criterion the last
two runs existed to make scoreable: the parity go threshold. Every other oracle
is green, including all five perf metrics and both manual rows, which have never
all been green in the same run before.

- **`just parity-gate` scores 189 of 194 user-facing rows reachable (97%)
  against a threshold of 194 of 194 (100%).** Five `spec.md` requirements are
  implemented but unreachable from the running client: server-upgrade reattach,
  the AI indicator's painted half, pane dividers and drag-resize, the
  workspace-notes hover preview, and the remote connect picker overlay
  (FU-24..FU-28). Filed this run as beads `scribe-38e.99`..`scribe-38e.103`.

## Oracle results

| Oracle | How driven | Result |
| --- | --- | --- |
| Unit / golden / `#[gpui::test]` | `cargo test --workspace --no-fail-fast` | ✅ **967 passed, 0 failed, 0 ignored** across 20 suites (947 at the prior gate) |
| Visual E2E | every `tests/e2e/visual/*.sh` in a rebuilt `scribe-test-visual` image, each with the env its `just` recipe documents | ✅ **39 / 39 PASS, 0 FAIL** |
| Func / lifecycle scripted E2E | every `tests/e2e/func/*.sh` in a rebuilt `scribe-test-func` image | ✅ **13 / 13 PASS, 0 FAIL** |
| Perf A/B rig | `run-perf-ab.sh --live`, both clients built from this tree and probe-instrumented, same host and session, against `scribe-dev` | ✅ **PASS — all 5 of 5 metrics measured and inside budget** |
| Reachability ratchet | `tools/check-reachability.sh --working-tree` | ✅ **PASS** — 62/66 modules wired, 54/59 server messages handled, 36/36 layout actions handled |
| Parity drift check | `just parity-inventory` | ✅ **PASS** — 203 rows, 198 reachable, 3 unwired, 2 missing; all 48 spec register ids carried |
| Parity go threshold | `just parity-gate` | ❌ **NO-GO** — 189 of 194 user-facing rows reachable (97%); the threshold is 194 of 194 |
| Manual rows | live driving on `:0` against the `scribe-dev` server | ✅ Opacity **PASS**; IME **PASS** |

### Prior blockers — all twelve confirmed resolved

| Prior blocker | Status | Evidence from this run |
| --- | --- | --- |
| B1 — nine unreachable `spec.md` requirements (`.86`–`.90`) | ✅ RESOLVED | The ratchet moved from 53/65 to 62/66 wired modules. Each capability now has an app-level oracle that PASSes: `visual/mouse-reporting.sh` (ten phases: wheel paging, X10 and SGR-1006 reports), `visual/ime-preedit.sh`, `visual/scrollbar.sh`, `visual/cold-restart.sh`, `visual/notifications.sh`, `visual/drag-drop.sh`, `visual/server-lifecycle.sh`. `tools/reachability-baseline.txt` is down to four unwired modules, three of which are the FU-25..FU-27 rows below and one (`focus_border`) a dead duplicate |
| B2 — IME manual item FAILED | ✅ RESOLVED | Re-driven live this run and it composes — see "Manual item detail" |
| B3 — perf FAIL on 2 of 5 (`.91`, `.92`) | ✅ RESOLVED | All five metrics PASS this run; see "Perf detail". Both failures were measurement defects, diagnosed and recorded in `perf-baseline.md` |
| B4 — `parity-inventory.md` stale (`.93`) | ✅ RESOLVED | `just parity-inventory` PASSes: every marker cell, footer and roll-up number is re-derived from the source and agrees with it |
| B5 — parity denominator incomplete (`.94`) | ✅ RESOLVED | 48 `spec.md` register ids, every one carried by a row; the check fails when one is not |
| B6 — no numeric go threshold (`.95`) | ✅ RESOLVED | `plan.md` § "Phase H re-baseline" → "The go threshold" states it and `just parity-gate` scores it. It is scored below, and it is what fails |
| D1 — `visual/reconnect.sh` hung the harness (`.96`) | ✅ RESOLVED | Ran unattended, PASS in 11 s, container exited on its own |
| D2 — perf rig degraded silently (`.97`) | ✅ RESOLVED | The `--live` preflight ran and no `NO-BASELINE`, `NOT-MEASURED` or `INCOMPLETE` verdict appears anywhere in the report |

## The failing criterion — parity go threshold

`just parity-gate` (`tools/check-parity-inventory.sh --gate`) exits non-zero and
names each offending row:

```
parity inventory: 203 rows, 198 reachable, 3 unwired, 2 missing
  (194 user-facing, 188 reachable in-client, 48 spec requirements carried)

parity gate: NO-GO — 189 of 194 user-facing rows reachable (97%);
  the threshold is 194 of 194.
```

| Unreachable row | `spec.md` | Marker | Why it is not reachable | Bead |
| --- | --- | --- | --- | --- |
| Server-upgrade reattach | `US2-4` | missing | `main.rs::start_ipc_thread` awaits `run_connection` exactly once; when it returns nothing redials, so an `--upgrade` handoff leaves the window attached to nothing. `visual/reconnect.sh` relaunches the client *process* and does not cover it | `scribe-38e.99` |
| AI indicator borders and tab tint | `US4-1` | unwired | `ai_indicator::{tab_indicator_color, workspace_border_color, pane_border_edges, tick, needs_animation, clear_stale_processing}` have no caller outside their module; AI state is tracked and never painted | `scribe-38e.100` |
| Pane dividers and drag-resize | `US3-10` | wired | `TerminalView::render_dividers` paints `PaneShell::dividers`; the grid pointer path maps the divider drag to `PaneShell::set_pane_ratio` and republishes both grids | `scribe-38e.101` |
| Workspace notes hover preview | `US4-3` | unwired | `workspace_notes_preview.rs` has no reference outside `lib.rs` and its own tests. The notes *modal* is wired; the hover preview is not | `scribe-38e.102` |
| Remote connect picker overlay | `US4-4` | missing | `remote::RemoteConnect` models the picker and no GPUI view renders it; `refresh_remote_peers` surfaces a peer count on the status strip instead | `scribe-38e.103` |

`FU-24` (`scribe-38e.99`) is the serious one and should be sequenced first: the
client never redials, so it fails silently on every server upgrade — the exact
failure `US2` exists to prevent — and no suite in the gate observes it.

The threshold is not adjustable at gate time. `plan.md` derives it from
`spec.md` Goal 1 ("full, reachable feature parity … no user-visible regression
in functionality"): an unreachable user-facing row *is* a user-visible
regression, so the only consistent bar is all of them. The only relief valve is
descoping a requirement in `spec.md` with a recorded decision, which deletes its
register id and its row and shrinks the denominator rather than lowering the
bar.

## Perf detail — 5 of 5 PASS

`tools/perf-ab-rig/run-perf-ab.sh --live --new-client
target/release/scribe-client-gpui --old-client target/release/scribe-client
--scribe-test target/release/scribe-test`, run on `:0` against the isolated
`scribe-dev` server, which was never restarted. Full report:
[`perf-ab-report.md`](perf-ab-report.md).

| Metric | New client | Old client | Budget | Verdict |
| --- | --- | --- | --- | --- |
| Scribe-attributable startup | **29.612 ms** | n/a | ≤ 150 ms absolute | ✅ PASS |
| Startup to first frame (total) | **705.488 ms** (29.6 Scribe + 677.8 gpui bring-up) | **3238.264 ms** | no worse than old, +10% allowance (≤ 3562.090 ms) | ✅ PASS |
| Input latency (p50 echo) | **0.214 ms** | **0.248 ms** | no worse than old, +10% allowance | ✅ PASS |
| cat-firehose throughput | **17.701 MiB/s** | **10.785 MiB/s** | no worse than old, −10% allowance | ✅ PASS |
| Memory at 10 tabs | **276.191 MiB** | **352.367 MiB** | ≤ old + 20% | ✅ PASS |
| Scroll fps / dropped | **59.911 fps, 0.000 % dropped** | 39.992 fps, 7.781 % dropped | 60 fps, < 1 % dropped (absolute) | ✅ PASS |

No metric reported `NO-BASELINE`, `NOT-MEASURED` or `INCOMPLETE`, and the rig's
own summary line reads `overall gate verdict: PASS`. This is the first gate run
in which that is true. Both prior failures were measurement defects rather than
client regressions, and both are now documented in `perf-baseline.md`: input
latency was comparing the new client's in-process read stamp against the old
client's UI-thread backlog, and the scroll metric was reporting `xdotool`'s
synthetic key rate (a ~47 fps ceiling) for both clients while a stale probe
stamp inflated the measurement window. Under the unpaced in-pane writer the rig
now uses, the new client sustains the display's full 60 Hz with zero dropped
frames and the old client does not reach the target on the same workload.

## Visual E2E detail — 39/39 PASS

`ai-task-label`, `bell`, `clipboard-osc52`, `cold-restart`, `color-emoji`,
`config-reload`, `dialogs`, `drag-drop`, `find-overlay`, `ime-preedit`,
`lan-approval`, `mouse-reporting`, `notifications`, `overlay-actions`,
`overlays`, `pane-workspace-layout`, `paste-confirmation`, `prompt-marks`,
`reconnect`, `remote-control`, `scrollbar`, `server-lifecycle`,
`session-tooling`, `settings-entry`, `settings-trust`, `share-control`,
`tab-window-chords`, `terminal-viewport`, `terminal-zoom`, `titlebar`,
`update-dismiss`, `update-trigger`, `window-chrome-bands`, `window-lifecycle`,
`window-resize`, `workspace-ipc`, `workspace-notes`, `workspace-split`,
`x11-focus-guard`. (`update-common.sh` is a helper, not a test.)

Seven are new since the prior gate — `cold-restart`, `drag-drop`,
`ime-preedit`, `mouse-reporting`, `notifications`, `scrollbar` and
`server-lifecycle` — and they are the app-level oracles that close prior
blocker B1.

`reconnect` finished unattended in 11 s and its container exited on its own,
confirming the D1 fix (`.96`). `pane-workspace-layout` must be run with the env
its own header documents, because openbox grabs the shipped Ctrl+Alt+Left
default; without it phase 7 fails on the harness's window manager rather than on
the client. It has no dedicated `just` recipe, which is a trap for an unattended
sweep — see D4.

## Func / lifecycle E2E detail — 13/13 PASS

`ai-context-thresholds`, `ai-state-indicator`, `cold-restart`,
`failure-server-down`, `failure-socket-loss`, `hot-reload`,
`keybindings-validation`, `multi-window`, `reconnect`, `shell-integration`,
`smoke`, `terminal-shortcuts`, `workspace-split`.

`func/cold-restart.sh` still uses the daemon as the client stand-in (prior D3),
but the client's own restore path is now covered by `visual/cold-restart.sh`,
which kills the client, cold-restarts the server and relaunches it.

## Parity-inventory results by verification method

Every row's own stated method was exercised this run.

| Method | Rows | Oracle | Result |
| --- | --- | --- | --- |
| `visual-E2E` | 102 | visual Docker suite, `xdotool` and pixel assertions against the real window | ✅ 39/39 scripts PASS |
| `scripted-E2E` | 79 | func + visual Docker scripts driving the real client and server | ✅ all driving suites green (13/13 func, 39/39 visual) |
| `visual-E2E (+ golden bytes)` | 7 | as above, plus the retained encoder fixtures | ✅ PASS |
| `gpui-test` | 9 | the nine removed-configuration-key rows | ✅ PASS (within the 967) |
| `golden` | 5 | captured byte/serialization fixtures | ✅ PASS (within the 967) |
| `manual` | 1 | Opacity, driven live against `scribe-dev` | ✅ PASS |

The six method counts sum to the inventory's 203 rows.

A row's method passing is necessary but not sufficient: the five unreachable
rows above are carried by passing methods *and* are unreachable, which is what
the "Reachable from" column and the go threshold exist to catch.

## Manual item detail

| Item | Verdict | Evidence |
| --- | --- | --- |
| Opacity | ✅ **PASS** | Driven live on `:0` against the `scribe-dev` server, with the release binary staged under the `scribe-dev` slug. A 1400×1300 magenta backdrop *window* was placed behind the client (ImageMagick `display`; the desktop root was left untouched), the client window located by PID and captured cropped to its own geometry. `opacity = 1.0` → `srgb(30,30,30)` at all three sample points; `0.85` → `srgb(35,29,35)` with 1 001 460 differing pixels; `0.5` → `srgb(86,22,86)`, monotonic in the backdrop's magenta. Clamping is correct: `1.5` → `srgb(30,30,30)` (opaque; 2 007 px apart from 1.0, cursor blink only) and `-0.2` → `srgb(255,0,255)` (fully transparent, pure backdrop). All four edits applied **at the same pid (384578) and the same X window id (52428801)**, proving reload without restart. |
| IME | ✅ **PASS** | Driven live on `:0` per `plan.md`:223. Launched the staged client with `XMODIFIERS=@im=ibus`, echoed a marker for a clean prompt, switched the host engine with `ibus engine table:cangjie3` (confirmed active), then tapped `h`, `q`, `i` through XTEST. The client logged three `IME preedit updated` lines from `preedit.rs::replace_and_mark_text_in_range` — a method only the platform can call, and only through the handler `TerminalView::start_ime` registers — and the frame changed by 21 591 px: the capture shows the underlined preedit 我 at the cursor with the CangJie candidate list `竹手戈 (1/4)` open. `space` then produced `IME committed text bytes=3` and `committing IME text to the focused pane bytes=3`, and the pane's command line reads a single `我` with no `hqi` anywhere on it. The 2026-07-27 B2 failure does not reproduce. |
| Dialogs | ✅ PASS | Automated: `visual/dialogs.sh` PASS. No longer a manual row. |
| Bell | ✅ PASS | Automated: `visual/bell.sh` PASS. No longer a manual row. |

The host `ibus` engine was restored to `xkb:us::eng` after the IME row by a
shell trap that fires on failure as well as success, confirmed by `ibus engine`
afterwards. `~/.config/scribe-dev/config.toml` was backed up before the opacity
edits and restored byte-for-byte after them.

## Additional defects found during gate driving (not parity rows)

- **D4 — `pane-workspace-layout.sh` has no `just` recipe carrying its required
  env.** Its header documents seeding `SCRIBE_EXTRA_CONFIG` with
  `workspace_focus_left = "ctrl+alt+h"` and points at "the run command in `just
  e2e-visual`", but `e2e-visual` is the generic single-script recipe and passes
  no env. Run through it, the script fails phase 7 with `ctrl+alt+h never
  reached workspace_focus_left` — a harness misconfiguration that reads exactly
  like a product regression. Every other script needing non-default env has a
  named recipe; this one should too.
- **D5 — a stale build artifact from a deleted worktree can fail `cargo test
  --workspace`.** The first run of the suite this gate reported three failures
  in `scribe-client-gpui --lib`, all "read … golden fixture: NotFound". The
  fixtures exist and are tracked; the test binary was a cached artifact whose
  `CARGO_MANIFEST_DIR` was baked as
  `/home/mamba/work/scribe/.worktrees/orch-scribe-38e.93/…`, a path that no
  longer exists. `-p scribe-client-gpui --lib` alone rebuilt and passed, because
  workspace feature unification selects a different metadata hash and therefore
  a different cached artifact. `cargo clean -p scribe-client-gpui` followed by
  `cargo test --workspace` gave 967/0. Not a product defect, but it is a false
  failure that will recur for anyone running worktrees against a shared target
  directory, and `concat!(env!("CARGO_MANIFEST_DIR"), …)` in the fixture loaders
  is what makes it possible.
- **D6 — the IME path depends on `XMODIFIERS` reaching the client.** With
  `XMODIFIERS` unset the composition keys bypass the input method entirely and
  land on the shell as literal `hqi`, with zero preedit lines in the client log
  — byte-for-byte the 2026-07-27 B2 observation. A normal desktop session
  exports it (the host's own `ibus-daemon` carries `XMODIFIERS=@im=ibus`), so
  this is an artifact of launching from a bare shell rather than a client
  defect. It is recorded because from the outside it is indistinguishable from a
  real IME regression, and because the visual container does not export it
  either — `visual/ime-preedit.sh` passes there because ibus registers itself as
  the default XIM server.

## Re-gate criteria

Re-run this gate when the following holds:

1. **`just parity-gate` exits zero** — every user-facing row reachable, i.e.
   beads `scribe-38e.99`..`scribe-38e.103` (FU-24..FU-28) each wired to a live
   path with an oracle that drives the running app, or the corresponding
   requirement explicitly descoped in `spec.md` with a recorded decision. Wiring
   a module means deleting its line from `tools/reachability-baseline.txt` and
   bumping the matching count.

Nothing else is outstanding. The already-green evidence — 967 unit/golden/
gpui-test assertions, 39/39 visual, 13/13 func, the reachability ratchet, the
parity drift check, all five perf metrics, and both manual rows — must stay
green, and each new wiring needs an oracle that drives the running app rather
than the module. That distinction is what the last three runs were about: green
suites are necessary and never sufficient, and the decisive question remains
whether a user of the running client can reach each `spec.md` requirement. For
five of them the answer is still no.
