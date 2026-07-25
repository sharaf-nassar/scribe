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
  5 samples vs the 500 ms budget (~4×; old baseline 190 ms). The rig's other
  four metrics (input latency, cat-firehose throughput, memory@10 tabs, scroll
  fps) are unmeasurable by the current rig and were DEFERRED — the rig still
  treats the client as the display-only spike. → beads `scribe-38e.50`,
  `scribe-38e.51`.
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
- **D4 — No config watcher in the GPUI terminal window.** `load_config()` runs
  once at startup and `ConfigStore::reload_from_disk` is never called from the
  terminal window; only the settings window re-reads config. Live config reload
  parity (and the `ConfigReloaded` row) is unproven, despite task `.16` being
  closed as complete. → bead `scribe-38e.57`.

## Green (verified) surface

- All 46 client-message and 59 server-message rows whose method is
  golden/gpui-test/visual-E2E: covered and passing.
- Input/keybinding golden + gpui-test rows: passing.
- Removed config keys (9): deserialize-and-ignore behavior passing (gpui-test).
- Lifecycle & failure paths (cold-restart, server-down, socket-loss, reconnect,
  hot-reload, multi-window isolation, workspace-split, shell-integration,
  terminal-shortcuts, keybindings): green.

## Re-gate criteria

Re-run this gate when all of the following are resolved: `.50` (startup
≤500 ms), `.51` (rig drives all five metrics), `.52` (AI-indicator func green),
`.56` (opacity implemented) followed by `.53` (opacity re-verified), and `.57`
(config watcher wired, live reload proven). The IME manual procedure still
requires a human on a host with an input-method engine. Only then does `.42`
flip to GO and unblock cutover `.43`.
