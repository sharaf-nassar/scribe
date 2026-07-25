# GPUI client launch gate — go/no-go checklist (scribe-38e.42)

Decision date: 2026-07-24. Evaluated against final `main` (post Tier-4:
`67c10e8`, plus dev-packaging fix `cb872dc`). Driven by orchestrated
verification: the workspace unit/golden/gpui-test oracle, the visual-E2E and
func/lifecycle scripted-E2E Docker suites, the perf A/B rig (`tools/perf-ab-rig`),
and live manual driving of the new GPUI client (`/usr/bin/scribe-dev`, repackaged
to ship `scribe-client-gpui`) against the isolated `scribe-dev` server sandbox.

## Decision: 🔴 NO-GO

Cutover (`scribe-38e.43`+) must NOT begin. The client is functionally close —
the entire golden/gpui-test oracle and the visual-E2E suite are green — but three
hard gates fail and two parity rows could not be verified in this environment.

## Parity-inventory results by verification method

| Method | Rows | Oracle | Result |
| --- | --- | --- | --- |
| `golden` | KeyInput, ControlRequest, PtyOutput, TrimScrollback, terminal-shortcut bytes | workspace golden suite (part of 850 tests) | ✅ PASS (0 failures) |
| `gpui-test` | ~50 client/server/input/removed-key rows | headless `#[gpui::test]` suites (part of 850 tests) | ✅ PASS (0 failures) |
| `visual-E2E` | AI indicator, titles, dialogs, clipboard prompt, share roster, LAN overlays, box drawing, ligatures | visual-E2E Docker suite | ✅ PASS (all completed suites green: dialogs, overlays, command-palette, tooltips, paste-confirmation, color-emoji, reconnect) |
| `scripted-E2E` | session/window/remote/LAN/clipboard lifecycle, resize, focus guard | func + lifecycle Docker scripts | ⚠️ 11/13 PASS — **2 FAIL** (see B2) |
| `manual` | Bell, Opacity (+ bead-called-out IME, dialogs) | live driving of `scribe-dev` on `:0` | ⚠️ Dialogs PASS; Bell/Opacity PARTIAL; IME human-required (see below) |

Total: golden + gpui-test (850 tests) and visual-E2E rows fully green. The gate
fails on scripted-E2E (AI indicator), perf, and unverifiable manual rows.

## Blockers (each NO-GO on its own)

- **B1 — Perf gate FAIL.** Startup-to-first-frame measured 2030–2350 ms across
  5 samples vs the 500 ms budget (~4×; old baseline 190 ms). → bead
  `scribe-38e.50`.

  The other four metrics (input latency, cat-firehose throughput,
  memory@10 tabs, scroll fps) were originally recorded as DEFERRED because the
  rig still treated the client as the display-only spike. `scribe-38e.51` has
  since removed that limitation: both clients carry the shared probe
  (`crates/scribe-common/src/perf_probe.rs`) and `--live` drives and thresholds
  all five metrics. The three comparative metrics need a `--live --old-client
  <bin> --record-baseline` run to fill the machine-readable baseline block in
  `perf-baseline.md`; until then they report `NO-BASELINE` and the gate stays
  `INCOMPLETE` rather than `DEFERRED`.
- **B2 — Func-E2E FAIL ×2.** `ai-state-indicator` and `ai-context-thresholds`
  fail: a context value of 50 is not rendered as `50%` in the prompt bar. AI
  indicator / prompt-bar parity (`.33`) is not met. → bead `scribe-38e.52`.
- **B3 — Manual rows not fully verified.**
  - *Opacity* (`manual`): **CORRECTED 2026-07-24** — originally recorded as
    inconclusive/environment-masked. Re-verification on the real GPU (bead
    `scribe-38e.53`) disproves that: the client runs on the NVIDIA RTX 3090
    (confirmed via `nvidia-smi` and `/dev/nvidia*` mappings), the libEGL/DRI2
    warnings are harmless Mesa side-path noise, and `opacity = 1.0` vs `0.85`
    renders **byte-identical** (full-window diff bbox `None`, max channel delta
    0). `appearance.opacity` is simply **UNIMPLEMENTED** in the GPUI client:
    the root background is a hardcoded opaque `rgb(0x101318)`, `WindowOptions`
    never sets `WindowBackgroundAppearance::Transparent`, and
    `opacity_changed()` has no consumer. → fix bead `scribe-38e.56`;
    re-verify via `scribe-38e.53`.
  - *IME* (bead-called-out manual): no input-method engine on this host, so
    end-to-end CJK preedit/commit could not be exercised. The preedit *model*
    passes its 6 unit tests, but the manual procedure is unverified and requires
    a human on a machine with an IME.

## Manual item detail

| Item | Verdict | Evidence |
| --- | --- | --- |
| Dialogs | ✅ VERIFIED | Close-Scribe modal renders (Quit / Kill / Cancel; Cancel safe-default), Esc dismisses; visual dialog suite also green |
| Bell | ⚠️ PARTIAL | BEL ingested cleanly, correct focused-pane suppression, no crash/garbage. Unfocused attention badge not shown; audible ring unconfirmable (no audio) |
| Opacity | ❌ FAILED (corrected) | Pixel-identical at 1.0 vs 0.85 on the real RTX 3090 → feature unimplemented, not environment-masked. See B3 and bead `.56` |
| IME | ⛔ NEEDS HUMAN | No IME engine installed; 6 preedit-model unit tests pass as automated proxy |

## Additional defects found during gate driving (not parity rows)

- **D1 — zbus background panics.** Every client launch throws two non-fatal
  `zbus … no reactor running, must be called from the context of a Tokio 1.x
  runtime` background-thread panics. Window still renders. → bead
  `scribe-38e.54`.
- **D2 — new_tab no-op.** `ctrl+shift+t` did not spawn a second tab/session in
  the running client. Needs confirmation (may overlap B2's `.33` surface). →
  bead `scribe-38e.55`.
- **D3 — WITHDRAWN (was: local GPU/EGL broken).** Re-verification proved the
  host GPU stack is healthy: dual RTX 3090, driver 580.142, native NVIDIA
  Vulkan, and the client demonstrably renders on the 3090. The libEGL/DRI2
  warnings come from an unused Mesa EGL side path and are harmless. Perf and
  opacity results are therefore attributable to the client, not the host.
- **D4 — RESOLVED (was: no config watcher in the GPUI terminal window).** The
  terminal window now owns a `ConfigRuntime` that keeps the `notify` watcher
  alive and reloads on the GPUI foreground; theme, chrome, grid font, cell
  metrics, and keybindings reapply live and every reload emits
  `ClientMessage::ConfigReloaded`. Proven against the dev server with no
  restart (same pid, same X window, theme accent repainted). Covered by
  headless suites plus `tests/e2e/visual/config-reload.sh`. → bead
  `scribe-38e.57`.

## ⚠️ RETRACTED: "green surface" claim (corrected 2026-07-24)

This section previously asserted that all client/server-message rows with
`golden`/`gpui-test`/`visual-E2E` methods were "covered and passing", implying
parity. **That claim was wrong and is withdrawn.** Those suites validate pure
functions, not reachability: a module can pass every test while the running
client never calls it.

The reachability audit (`reachability-audit.md`, bead `scribe-38e.60`) measured
the truth across 173 rows:

| Verdict | Count | Share |
| --- | --- | --- |
| WIRED (reachable in the app) | 60 | 34.7% |
| UNWIRED (logic exists, nothing calls it) | 63 | 36.4% |
| MISSING (no implementation) | 50 | 28.9% |

Excluding the 9 removed-config-key rows (which assert *absence* of behavior),
**51 of 164 user-facing rows are reachable — 31%.**

Structural causes: `main.rs` imports 19 of 54 library modules; `run_reader`
handles 12 of 59 `ServerMessage` variants and ends in `_ => {}`;
`handle_layout_action` executes 9 of 35 `LayoutAction` variants; and
`Content.rows` is `Vec<String>` — the paint path has **no per-cell color**, so
box drawing, ligatures, and font fallback are all unreachable. Command palette,
context-menu, and dialog *events* are discarded, so those surfaces open but
their actions do nothing.

What genuinely IS verified green: the 9 removed-config-key rows, and the
lifecycle/failure-path scripted E2Es (cold-restart, server-down, socket-loss,
reconnect, hot-reload, multi-window isolation, workspace-split,
shell-integration, terminal-shortcuts, keybindings-validation) — these drive the
real app and therefore do prove reachability for what they cover.

## Re-scope pointer

Feature 016 was re-scoped in place on the strength of the retraction above —
see `spec.md` § "Re-scope — reachability re-baseline" and `plan.md`
§ "Re-sequenced remaining phases (post-reachability-audit)". The 016 task list
completed the *library port*; the remaining work is integration/wiring plus the
genuinely missing features, sequenced around the audit's fix units FU-1..FU-23
with FU-1 (cell-accurate paint path) first.

Two consequences bind this gate:

- **The parity metric is the reachable-row count**, regenerated from
  `parity-inventory.md`'s roll-up (currently **51 of 164** user-facing rows) by
  mechanical CI checks — never a green unit-test run.
- **Verification methods were upgraded** so no user-facing row can pass on
  headless unit tests alone: 27 IPC rows moved `gpui-test` → `scripted-E2E`;
  font fallback and all 54 named keybinding actions moved to `visual-E2E`
  driven by `xdotool`. `gpui-test` now applies only to the nine
  removed-config-key rows. The "Parity-inventory results by verification
  method" table above records the methods **as they stood at the gate run** and
  is superseded by `parity-inventory.md` for any re-gate.

## Re-gate criteria

Re-run this gate when all of the following are resolved: `.50` (startup
≤500 ms), `.51` (rig drives all five metrics), `.52` (AI-indicator func green),
`.56` (opacity implemented) followed by `.53` (opacity re-verified), and `.57`
(config watcher wired, live reload proven). The IME manual procedure still
requires a human on a host with an input-method engine.

Post-re-scope, those are necessary but no longer sufficient: the re-gate also
requires the reachable-row count to meet its explicit go threshold, with the
FU-1..FU-23 fix units closed to that threshold and the mechanical reachability
checks green. Only then does `.42` flip to GO and unblock cutover `.43`.
