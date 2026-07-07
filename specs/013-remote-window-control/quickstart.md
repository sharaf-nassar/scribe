# Quickstart: Remote Window Control over Tailscale

**Feature**: `013-remote-window-control` | **Date**: 2026-07-03
Manual validation guide — one scenario per user story plus performance
checks. See [contracts/](./contracts/) for exact message and UI contracts.

## Prerequisites

- Two machines (**A** = owning, **B** = connecting) on the same tailnet,
  both signed into the same Tailscale account, `tailscale status` healthy
  on both.
- Dev-flavor Scribe builds on both (`scribe-dev` identity, dev socket
  paths) so the production server is untouched. **Never restart the
  production Scribe server — all scenarios below run against dev-flavor
  instances only.**
- A third identity for negative tests if available: any tailnet device on a
  DIFFERENT account (or a tagged node). If unavailable, mark scenario 2.3
  as not-run in the completion report.
- Baseline: note A's tailnet IP (`tailscale ip -4`) and device names.

## Scenario 1 — Attach & control (User Story 1, SC-001/SC-002)

1. On A (dev instance): open a window, create 2–3 sessions with distinct
   scrollback (e.g. `seq 1 5000`, a TUI like `htop`, a shell mid-command).
2. On A: Settings → Remote → enable. Verify the status-bar
   remote-enabled indicator appears and the plain-language who-can-connect
   copy is shown at the toggle (UX-003).
3. On B: palette → "Connect to remote machine…" → pick A from the peer
   list (verify it lists by short name; verify manual entry also works).
4. Attach to A's window. **Expect** first full render ≤2 s (PR-002); all
   tabs/panes/scrollback/cursor present.
5. Fidelity check (SC-002): from a third shell on A run the snapshot
   tooling (`scribe-cli` snapshot against the dev socket) for one session;
   compare against the same session's content visible on B — identical
   grid, cursor, and scrollback tail.
6. On B: type in a shell, drive the TUI with the mouse, resize the window,
   scroll and search history, close one session and open a new one
   (FR-008); create a NEW window on A from the picker's "New window" and
   run a command in it (US1 scenario 6).
7. Host-action gates (FR-014): in a controlled session run an OSC 52 write
   (`printf '\e]52;c;%s\a' "$(printf hi | base64)"`) — the clipboard policy
   prompt appears on **B** and B's clipboard receives the payload; with
   `terminal.paste_confirmation` enabled on B, paste a multiline snippet
   into a non-bracketed-paste prompt — B shows the confirmation dialog;
   click a URL in scrollback — it opens in **B's** browser, never on A.
8. Timing (SC-001): from opening Scribe on B to controlling the window —
   under 30 s and ≤5 interactions.

## Scenario 2 — Enablement & authorization (User Story 2, SC-005)

1. Fresh config on A (remote table absent): verify nothing listens on the
   remote port — `ss -tlnp | grep 46061` empty (FR-001).
2. Enable remote on A: verify the listener binds ONLY tailnet addresses —
   `ss -tlnp` shows the 100.x/fd7a: bind, and a connect attempt from a
   non-tailnet interface (e.g. LAN IP) is refused/unreachable (FR-002).
3. Different-account device (if available): attempt connect. **Expect**
   refusal before any window list is shown, "not the same account" copy on
   the caller, and a `remote: refused reason=unauthorized` log line on A
   (FR-003, FR-017).
4. Stop tailscaled on A (`sudo systemctl stop tailscaled` or GUI quit):
   attempt connect from B. **Expect** a typed identity-unavailable refusal
   ("can't verify device identity") — the handshake reply arrives even
   though authorization can't run (fail closed, FR-015); local
   dev-instance use of A completely unaffected. Restart tailscaled.
5. While B controls a window: disable remote on A in Settings. **Expect**
   B severed within 2 seconds showing the delivered "remote access was
   turned off on A" notice (not a generic connection error), A's sessions
   running, `severed` log line, port closed (FR-016). A later cold connect
   from B shows the combined connection-failure copy (offline / not
   running / disabled — FR-004). Re-enable for later scenarios.
6. Version gate (FR-012): temporarily build B with
   `REMOTE_PROTOCOL_VERSION` bumped; connect. **Expect** typed refusal
   naming both versions. Revert.

## Scenario 3 — Takeover & return to local (User Story 3 + clarifications)

1. With A's window open locally and B attached to a different window:
   from B, attach to A's OPEN window. **Expect** on A: input stops, content
   freezes dimmed, banner "Controlled by <B's device> (<account>) — Take
   back control"; no live updates on A while B types (clarification:
   dimmed frozen view).
2. On A: hit the reclaim action. **Expect** A shows the full current state
   including everything B did; B now shows the dimmed-frozen banner state
   pointing at A (FR-007 symmetry, US3).
3. Gate re-routing after reclaim (FR-014): repeat the OSC 52 write from
   Scenario 1 step 7. **Expect** the prompt and clipboard payload now land
   on **A** — the capability/policy state followed the new controller.
4. Race check: trigger attach from B and reclaim on A near-simultaneously
   several times. **Expect** exactly one controller each round, the loser
   sees the taken-over copy, never two live controllers (edge case).

## Scenario 4 — Interruptions (User Story 4, SC-004)

1. While B controls a window, run `seq 1 100000` in a session and
   mid-stream cut B's network (toggle Wi-Fi or `tailscale down` on B).
   **Expect** B shows cancelable "Reconnecting to A…" (FR-011); A's
   sessions keep running (FR-010).
2. Restore B's network. **Expect** automatic reconnect; the rebuilt view
   matches the session's true current state — compare final lines against
   A's snapshot; no duplicated/missing regions (FR-011, SC-004).
3. Reclaim during an outage (FR-011, analysis C3): while B is offline and
   showing "Reconnecting…", reclaim the window locally on A; then restore
   B's network. **Expect** B's automatic reconnect does NOT take control
   back — B lands in the dimmed lost-control state naming A, and control
   moves only if B's user explicitly reclaims.
4. Cancel during an outage: verify the settled disconnected state with
   one-action reconnect.
5. Repeat a burst of ≥10 attach/interrupt/reattach cycles (loop of steps
   1–2): zero session deaths or corruption (SC-004 sampled here; the full
   100-cycle scripted run is executed in the Exit checklist).

## Performance checks (PR-001, PR-003, PR-004 / SC-003)

- **Keystroke latency (PR-001)**: on B, in an attached shell, run a
  paced-input latency sampler (e.g. key-echo timestamping script or
  240 fps phone-camera sampling of ≥20 keystrokes). Direct path (verify
  `tailscale status` shows a direct connection, not DERP): p95 ≤100 ms.
- **Idle overhead (PR-003)**: with remote enabled but no connections,
  compare A's dev-instance input latency/render feel and server CPU
  (`pidstat`) against remote-disabled baseline: no measurable difference.
- **Stalled consumer (PR-004)**: attach from B, `SIGSTOP` B's client (or
  drop its network without letting it reconnect), then generate sustained
  output on A (`yes | head -c 200M`). **Expect**: A's local dev windows
  stay fluid; server RSS for the remote connection stays bounded
  (watch `ps -o rss` over a minute — plateaus, not linear growth); on
  `SIGCONT`/reconnect B converges via fresh replay (D5).

## Exit checklist

- All scenarios above pass on Linux↔Linux and (if hardware available)
  macOS↔Linux pairs; note skipped legs in the completion report.
- Scripted 100-cycle attach/detach/interruption run completes with zero
  session terminations or corruption — this closes SC-004's full bar (a
  loop of Scenario 4 steps 1–2 driven by a shell script against the dev
  instances).
- Audit log contains accepted/refused/disconnect/severed lines matching
  the actions performed (FR-017).
- `lat.md` updated (protocol, server, client, settings sections) and
  `lat check` passes before completion.
