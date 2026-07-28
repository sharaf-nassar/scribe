# GPUI client launch gate — go/no-go checklist (scribe-38e.42)

Decision date: 2026-07-28 (fourth run).

This run evaluates the working tree based on `main` at
`dd5b1ed62e2803beaf166b5d3e67fa6a18431da4`. The commit containing this
artifact is the launch-gate candidate. It supersedes the three prior NO-GO
runs.

## Decision: GO

Cutover work (`scribe-38e.43` and later) may begin.

Every launch-blocking oracle passes. All 194 user-facing parity rows are
reachable, every row's declared verification method is green, all five
performance metrics are inside budget, and both live manual rows pass.

There are no launch blockers or waived failures.

## Candidate identity

Two byte-identified GPUI binaries drove the run:

| Candidate | SHA-256 | Use |
| --- | --- | --- |
| User-supplied `/usr/bin/scribe-dev` | `1b63ce9241b34fefbb7ced370af0146a276a07b5be3a1bef50bb6a768afbd4e2` | Live perf A/B, opacity, and IME on `:0` |
| `target/release/scribe-client` after the gate fixes | `b3791f69b23b8515ed9354aded13dce16f3f3a841078e6461fd0d66bea955f75` | Rebuilt visual image and source-level quality gates |

The source-only delta found during this run is confined to server-upgrade
reattachment plus E2E oracle corrections. The rebuilt source candidate drove
the affected black-box cases and the complete 41-case visual suite.

The stable install was not used. `/usr/bin/scribe-server` (PID `3390291`) and
`/usr/bin/scribe-dev-server --upgrade` (PID `482730`) were never restarted,
stopped, upgraded, or replaced.

## Required checklist

- [x] Workspace unit, golden, and `#[gpui::test]` suites pass.
- [x] Every visual E2E script passes in a rebuilt image.
- [x] Every functional/lifecycle E2E script passes in a rebuilt image.
- [x] Mechanical reachability ratchet passes.
- [x] Parity inventory has no drift, unwired rows, or missing requirements.
- [x] Numeric parity launch threshold passes.
- [x] All five live A/B performance metrics are measured and pass.
- [x] Opacity is verified live, including hot reload and clamping.
- [x] IME preedit and commit are verified live through a real input method.
- [x] `lat check` passes after behavior and oracle documentation updates.
- [x] No launch blocker remains.

## Oracle summary

| Oracle | Command / method | Result |
| --- | --- | --- |
| Unit / golden / GPUI | `cargo test --workspace --no-fail-fast` | **PASS — 967 passed, 0 failed, 0 ignored across 20 suites** |
| Visual E2E | all executable `tests/e2e/visual/*.sh`, with each script's documented environment | **PASS — 41/41** |
| Functional/lifecycle E2E | all `tests/e2e/func/*.sh` | **PASS — 13/13** |
| Perf A/B | `tools/perf-ab-rig/run-perf-ab.sh --live` | **PASS — 5/5 measured and inside budget** |
| Reachability | `tools/check-reachability.sh --working-tree` | **PASS — 65/67 modules, 54/59 server messages, 36/36 layout actions** |
| Parity drift | `just parity-inventory` | **PASS — 203/203 reachable, 0 unwired, 0 missing; all 48 requirement ids carried** |
| Parity threshold | `just parity-gate` | **GO — 194/194 user-facing rows reachable (100%)** |
| Manual | live installed GPUI client on `:0` | **PASS — opacity and IME** |
| Documentation graph | `lat check` | **PASS** |

## Parity threshold

`just parity-gate` reports:

```text
parity inventory: 203 rows, 203 reachable, 0 unwired, 0 missing
  (194 user-facing, 193 reachable in-client, 48 spec requirements carried)
parity gate: GO — 194 of 194 user-facing rows reachable (100%),
  0 unwired, 0 missing
```

The threshold is fixed at every user-facing requirement. No requirement was
waived or descoped during gate driving.

Every inventory row's own verification method ran:

| Method | Gate evidence |
| --- | --- |
| `visual-E2E` | complete 41-script visual suite |
| `scripted-E2E` | complete 13-script functional suite plus visual scripts |
| `visual-E2E (+ golden bytes)` | visual suite plus workspace golden tests |
| `gpui-test` | workspace test suite |
| `golden` | workspace test suite |
| `manual` | live opacity row |

## Performance — 5/5 PASS

The live rig used `/usr/bin/scribe-dev`, the current legacy release client, and
`target/release/scribe-test`, staged under the isolated `scribe-dev` runtime
slug. It attached to the existing dev server and never addressed the stable
socket. Full evidence is in
[`perf-ab-report.md`](perf-ab-report.md).

| Metric | New client | Old client | Budget | Verdict |
| --- | --- | --- | --- | --- |
| Scribe-attributable startup | **28.145 ms** | n/a | ≤ 150 ms | PASS |
| Startup to first frame | **815.491 ms** | **3200.862 ms** | no worse than old, 10% allowance | PASS |
| Input latency, p50 echo | **0.216 ms** | **0.236 ms** | no worse than old, 10% allowance | PASS |
| Cat-firehose throughput | **17.268 MiB/s** | **11.249 MiB/s** | no worse than old, 10% allowance | PASS |
| Memory at 10 tabs | **270.883 MiB** | **275.355 MiB** | ≤ old + 20% | PASS |
| Scroll FPS / dropped | **59.706 FPS, 0.416%** | 38.722 FPS, 7.186% | sustained 60 FPS within allowance, <1% dropped | PASS |

No metric reported `NO-BASELINE`, `NOT-MEASURED`, or `INCOMPLETE`. The rig's
own final verdict is `PASS`.

## Visual E2E — 41/41 PASS

The complete suite passed after rebuilding `scribe-test-visual` from this
working tree:

`ai-indicator`, `ai-task-label`, `bell`, `clipboard-osc52`, `cold-restart`,
`color-emoji`, `config-reload`, `dialogs`, `drag-drop`, `find-overlay`,
`ime-preedit`, `lan-approval`, `mouse-reporting`, `notifications`,
`overlay-actions`, `overlays`, `pane-workspace-layout`,
`paste-confirmation`, `prompt-marks`, `reconnect`, `remote-control`,
`scrollbar`, `server-lifecycle`, `server-upgrade-reattach`,
`session-tooling`, `settings-entry`, `settings-trust`, `share-control`,
`tab-window-chords`, `terminal-viewport`, `terminal-zoom`, `titlebar`,
`update-dismiss`, `update-trigger`, `window-chrome-bands`,
`window-lifecycle`, `window-resize`, `workspace-ipc`, `workspace-notes`,
`workspace-split`, and `x11-focus-guard`.

`update-common.sh` is a helper, not a test.

## Functional/lifecycle E2E — 13/13 PASS

The complete suite passed after rebuilding `scribe-test-func`:

`ai-context-thresholds`, `ai-state-indicator`, `cold-restart`,
`failure-server-down`, `failure-socket-loss`, `hot-reload`,
`keybindings-validation`, `multi-window`, `reconnect`, `shell-integration`,
`smoke`, `terminal-shortcuts`, and `workspace-split`.

## Manual evidence

Both manual rows used the installed `/usr/bin/scribe-dev` supplied for this
gate. Temporary XDG config/state/cache directories isolated the run; the real
`~/.config/scribe-dev/config.toml` remained unchanged at SHA-256
`a020141fd656c29b6bcb023b1eb66e3847179cde2b5f805fc83defe934ec83fe`.

### Opacity — PASS

A 1400×1300 magenta ImageMagick window was stacked behind the client. All
edits hot-reloaded at the same client PID (`1951988`) and X window
(`52428801`):

| Config value | Three body samples | Evidence |
| --- | --- | --- |
| `1.0` | `srgb(14,14,16)` | opaque baseline |
| `0.85` | `srgb(20,14,22)` | 710,858 pixels differ from baseline; magenta increases |
| `0.5` | `srgb(74,10,76)` | 710,877 pixels differ from 0.85; magenta increases again |
| `1.5` | `srgb(14,14,16)` | clamps to opaque; 1,514-pixel cursor-only difference |
| `-0.2` | `srgb(255,0,255)` | clamps to fully transparent backdrop |

The client logged each `config reload: opacity applied` value and
`config hot-reloaded ... opacity=true`; the process and window did not restart.

### IME — PASS

The same installed client launched with `XMODIFIERS=@im=ibus`. After a clean
prompt marker, the host engine switched to `table:cangjie3`; XTEST sent `h`,
`q`, `i`.

- Three `IME preedit updated bytes=3` events reached the GPUI input handler.
- The live capture showed underlined preedit `我` and the CangJie candidate list
  `竹手戈 (1/4)`.
- Space produced `IME committed text bytes=3` and
  `committing IME text to the focused pane bytes=3`.
- The shell line contained one `我`; literal `hqi` did not leak.

The host engine was restored to `xkb:us::eng` before cleanup.

## Defects found and resolved during this run

Gate driving exposed one product defect and two oracle defects. All were fixed
and re-exercised in isolation and in the complete suite.

### Server-upgrade reattachment

The GPUI client redialed and rebuilt topology after a real handoff, but its
locally remembered pane set was treated as already attached on the replacement
connection. Attach grants are connection-local.

The reconnect path now replays every still-visible `AttachSessions` entry after
the replacement connection's first `SessionList`, retaining each pane's exact
grid dimensions before `Resize` and `Subscribe`. The original process then
accepted input and painted output after the handoff.

### Remote-control injection target

The remote picker probe connected through `share-tap`, becoming the tap's
newest injection target. Later synthetic takeover notices missed the GPUI
client. The probe now splices directly to the tap's upstream server socket, and
the test dismisses both picker stages before later command-palette input.

### Upgrade oracle lifecycle

The old post-upgrade assertion used the separate `scribe-test` daemon after its
server stream had closed. The oracle now types a sentinel through the original
GPUI window and requires a substantial body repaint after reconnect, directly
testing the process that survived the handoff.

## Safety and cleanup

- No host server was restarted or upgraded.
- Stable Scribe runtime/config/state were not used.
- Gate-owned client and backdrop processes were terminated by exact session.
- Host `ibus` engine is `xkb:us::eng`.
- User `scribe-dev` config hash is unchanged.
- Disposable Docker servers performed their own isolated lifecycle tests.

## Final disposition

**GO.** `scribe-38e.42` is complete. No blocker remains; `scribe-38e.43`
(cutover) and `scribe-38e.44` (macOS packaging follow-on) may proceed.
