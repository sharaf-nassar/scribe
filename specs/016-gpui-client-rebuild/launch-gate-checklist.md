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
  - *Opacity* (`manual`): 32-bit ARGB surface + clamp + no-restart confirmed,
    but no live translucency observed. This box's GPU/EGL path is broken
    (NVIDIA driver null → software fallback), so the result is inconclusive, not
    a confirmed pass. Needs a working-GPU display. → bead `scribe-38e.53`.
  - *IME* (bead-called-out manual): no input-method engine on this host, so
    end-to-end CJK preedit/commit could not be exercised. The preedit *model*
    passes its 6 unit tests, but the manual procedure is unverified and requires
    a human on a machine with an IME.

## Manual item detail

| Item | Verdict | Evidence |
| --- | --- | --- |
| Dialogs | ✅ VERIFIED | Close-Scribe modal renders (Quit / Kill / Cancel; Cancel safe-default), Esc dismisses; visual dialog suite also green |
| Bell | ⚠️ PARTIAL | BEL ingested cleanly, correct focused-pane suppression, no crash/garbage. Unfocused attention badge not shown; audible ring unconfirmable (no audio) |
| Opacity | ⚠️ PARTIAL | ARGB depth-32 surface + clamp + live-reload plumbing present; no visible bleed-through — masked by broken local GPU/EGL |
| IME | ⛔ NEEDS HUMAN | No IME engine installed; 6 preedit-model unit tests pass as automated proxy |

## Additional defects found during gate driving (not parity rows)

- **D1 — zbus background panics.** Every client launch throws two non-fatal
  `zbus … no reactor running, must be called from the context of a Tokio 1.x
  runtime` background-thread panics. Window still renders. → bead
  `scribe-38e.54`.
- **D2 — new_tab no-op.** `ctrl+shift+t` did not spawn a second tab/session in
  the running client. Needs confirmation (may overlap B2's `.33` surface). →
  bead `scribe-38e.55`.
- **D3 — local GPU/EGL broken** (environmental): NVIDIA `10de:2204` driver null,
  DRI2 screen creation fails → software rendering. Not a client defect, but it
  blocks clean local perf/opacity verification.

## Green (verified) surface

- All 46 client-message and 59 server-message rows whose method is
  golden/gpui-test/visual-E2E: covered and passing.
- Input/keybinding golden + gpui-test rows: passing.
- Removed config keys (9): deserialize-and-ignore behavior passing (gpui-test).
- Lifecycle & failure paths (cold-restart, server-down, socket-loss, reconnect,
  hot-reload, multi-window isolation, workspace-split, shell-integration,
  terminal-shortcuts, keybindings): green.

## Re-gate criteria

Re-run this gate when B1 (startup ≤500 ms + rig drives all five metrics), B2
(AI-indicator func green), and B3 (opacity + IME verified on capable hardware)
are resolved. Only then does `.42` flip to GO and unblock cutover `.43`.
