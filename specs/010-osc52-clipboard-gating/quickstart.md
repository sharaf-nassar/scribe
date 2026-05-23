# Quickstart — OSC 52 Clipboard Gating

**Branch**: `010-osc52-clipboard-gating`
**Date**: 2026-05-22
**Purpose**: Per-user-story manual verification.

These steps assume a Scribe build with this feature, a running
`scribe-server`, an attached `scribe-client` GUI window, and a
shell prompt inside one of its panes. No automated tests are
shipped with this feature (QR-002 + Constitution II); verification
is via the steps below.

`@osc52-write` and `@osc52-read` below are shell aliases for:

```bash
osc52_write() {
  local data="$1"; local sel="${2:-c}"
  printf '\x1b]52;%s;%s\x07' "$sel" "$(printf '%s' "$data" | base64 -w0)"
}
osc52_read() {
  local sel="${1:-c}"
  printf '\x1b]52;%s;?\x07' "$sel"
  # Press Ctrl+D then Enter inside cat - to see the bytes the shell
  # received; or pipe through `xxd` for a hex dump.
}
```

Verify each user story independently. The "Independent Test"
language in each block mirrors the spec.

---

## US1 — Block silent exfiltration by default (P1)

**Goal**: With default settings, no PTY-side program can read the
clipboard without user consent.

### US1-S1: clipboard read prompts under default policy

1. Copy a known string (e.g., `secret-string`) into the host
   clipboard using any host application.
2. In a Scribe pane, run `osc52_read c`.
3. Observe: a clipboard confirmation dialog appears in the pane.
   The default focus is on **Deny**.
4. Press **Escape** (or click Deny).
5. Observe: the PTY-side program receives an empty OSC 52 reply
   (or no reply within the 30 s server timeout). `secret-string`
   is **not** in the shell's read buffer.

### US1-S2: primary selection read also prompts

Repeat US1-S1 with `osc52_read p`. Same outcome on X11. On
Wayland/macOS the primary selection routes to the system
clipboard at arboard's layer; the prompt still appears.

### US1-S3: writes flow through silently by default

1. In the same Scribe pane, run `osc52_write 'hello-from-osc52'`.
2. Observe: no dialog appears (writes default to `Allow`).
3. Switch focus to another application; paste.
4. Observe: the pasted text is `hello-from-osc52`. The host
   clipboard reflects the OSC 52 write.

### US1-S4: SSH context exfil attempt

1. SSH into a remote host from inside a Scribe pane.
2. On the remote, run `osc52_read c`.
3. Same expectation as US1-S1: a dialog appears in the Scribe
   pane (the OSC 52 bytes round-trip through the local PTY
   regardless of SSH).

---

## US2 — Configurable clipboard policy (P2)

**Goal**: The user can override the default via settings and the
change takes effect on the next OSC 52 operation.

### US2-S1: open settings, change read mode to Allow

1. Open Scribe Settings via the menu / shortcut.
2. Navigate to Terminal → Clipboard (OSC 52).
3. Set the "Reads" dropdown to **Allow**.
4. Save / Apply.
5. In a Scribe pane, run `osc52_read c`.
6. Observe: no dialog. The PTY-side program receives the current
   host clipboard contents directly.

### US2-S2: change write mode to Deny

1. Settings → Terminal → Clipboard (OSC 52) → "Writes" → **Deny**.
2. Run `osc52_write 'should-not-land'`.
3. Switch focus, paste.
4. Observe: the previously copied value (from any other test) is
   still there. `should-not-land` did not write to the host
   clipboard.

### US2-S3: live policy change applies to the next request

1. Settings → set Reads to **Deny**. Apply.
2. Run `osc52_read c`. Observe: empty reply, no dialog.
3. Without restarting Scribe, set Reads to **Allow**. Apply.
4. Run `osc52_read c`. Observe: clipboard contents returned, no
   dialog.

### US2-S4: max write size enforcement

1. Settings → set Max write size to `1 KB` (test-only minimum).
   Apply.
2. Run `osc52_write "$(yes a | head -c 2048)"` (2 KB payload).
3. Switch focus, paste. Observe: the prior clipboard value is
   unchanged (the write was rejected at the size cap).
4. Run `osc52_write "$(yes a | head -c 512)"` (well under cap).
   Paste. Observe: the new value lands.

### US2-S5: 512 MiB upper bound enforced in settings UI

1. Settings → Max write size input. Try to enter `600 MB`.
2. Observe: the value is clamped to 512 MiB (the input's `max`
   attribute or a TOML clamp on save).

---

## US3 — Confirmation prompt + burst-decision reuse (P3)

**Goal**: In prompt mode, the user's first decision applies to a
burst of follow-up requests without re-prompting.

### US3-S1: single prompt, choose Allow once

1. Default policy (reads = Prompt).
2. Run `osc52_read c`. Dialog appears.
3. Click **Allow once**.
4. Observe: PTY-side program receives the clipboard contents.

### US3-S2: burst flurry inherits the decision

1. With the same Pane, immediately (< 500 ms) run
   `osc52_read c; osc52_read c; osc52_read c`.
2. Observe: no additional dialogs appear; all three reads
   succeed. (The first request inside the 500 ms window from
   US3-S1's decision reuses it; subsequent reads stay inside
   the burst window because each refreshes the activity
   timestamp.)
3. Wait ~1 s of pane idleness.
4. Run `osc52_read c` again. Observe: a fresh dialog appears
   (burst window has elapsed).

### US3-S3: cross-pane independence

1. Pane A: `osc52_read c`. Dialog appears; choose Allow once.
2. Pane B (different pane in the same window): `osc52_read c`.
3. Observe: a separate dialog appears in Pane B. The two
   decisions are independent.

### US3-S4: Always-allow persists the policy

1. Run `osc52_read c`. Dialog appears.
2. Click **Always allow**.
3. Open Settings → Terminal → Clipboard (OSC 52).
4. Observe: the Reads dropdown is now **Allow**.
5. Run `osc52_read c` again. Observe: no dialog, value
   returned directly.

---

## US4 — Host clipboard bridge for writes (P3)

**Goal**: An allowed OSC 52 write updates the host clipboard so
other applications can paste the value.

### US4-S1: write reaches host clipboard

1. Default policy (writes = Allow).
2. In a Scribe pane: `osc52_write 'bridge-test-1'`.
3. Switch focus to another application. Paste.
4. Observe: `bridge-test-1`.

### US4-S2: focus-gate-writes opt-in blocks unfocused writes

1. Settings → enable **Require focus for writes**. Apply.
2. Click another application's window so Scribe is unfocused.
3. From a script outside Scribe, send a delayed OSC 52 write
   into a pane:
   ```bash
   (sleep 2 && osc52_write 'should-not-land-focus-gated') > /dev/pts/N &
   ```
   (Replace `/dev/pts/N` with the pane's TTY device.)
4. Wait 3 s. Paste in the other application.
5. Observe: the previously copied value is unchanged.
6. Click Scribe to focus it. Run `osc52_write 'lands-now'`
   inside the pane.
7. Switch focus, paste. Observe: `lands-now`.

### US4-S3: oversize write rejected (matches US2-S4)

Already covered by US2-S4.

---

## Cross-cutting verifications

### Performance spot-check (PR-001, SC-002, SC-004)

1. With Reads = Allow (no prompt path), run
   `time osc52_read c` 100 times in a loop.
2. Total wall-clock for 100 reads (each round-trip is
   PTY → server → IPC → client (arboard) → IPC → server →
   PTY) should be ≪ 10 s — i.e., < 100 ms per round-trip on a
   warm system. This validates SC-002.
3. With Reads = Prompt, manually time how quickly the dialog
   appears after running `osc52_read c`. Should be visibly
   immediate (≤100 ms per SC-004); confirm by feel (frame-rate
   stable, no perceptible lag).

### Invalid OSC 52 payload handling (FR-012)

1. With default policy and writes = Allow, send a malformed
   base64 payload directly via printf so it bypasses the
   base64-encoding helper:
   ```bash
   printf '\x1b]52;c;not-valid-base64!@#$%^\x07'
   ```
2. Observe: Scribe does not crash. No client-side error dialog.
   The host clipboard contents are unchanged (the prior value
   from US4-S1 should still paste cleanly).
3. Repeat with an empty payload (`printf '\x1b]52;c;\x07'`).
   Same expected behavior — silent drop, no host-clipboard
   mutation.

Rationale: FR-012 ("invalid base64 silently dropped") is
enforced by alacritty_terminal's VTE parser layer before
`Event::ClipboardStore` is even fired. This scenario verifies
the upstream behavior holds end-to-end through Scribe's
pipeline.

### OSC 52 fallback-chain decomposition (FR-011)

1. Default policy (reads = Prompt). Copy a known string into
   the system clipboard via a host app.
2. From a Scribe pane, request the clipboard-then-primary
   fallback chain:
   ```bash
   printf '\x1b]52;cp;?\x07'
   ```
3. Observe: two confirmation dialogs appear in sequence (one
   for clipboard, one for primary selection), each independent.
   Dismiss both.
4. Optional: enable a server-side `tracing::debug!` log on the
   `SessionEvent::ClipboardLoad` arm during this verification
   to confirm two distinct events fire (one with
   `ClipboardType::Clipboard`, one with `ClipboardType::Selection`).

Rationale: confirms research decision 9 — alacritty decomposes
fallback chains into per-selection events; Scribe-side has no
chain-walking logic. If only one dialog appears for a `cp`
chain, research decision 9 is wrong and a Scribe-side chain
walker is needed.

### Cold-restart handoff (Operational Safety)

1. Run a Scribe session with this feature; trigger a write that
   lands in the host clipboard.
2. From another terminal: `scribe-server --upgrade` (per the
   user's prior explicit approval — DO NOT run this without
   confirming with the user; restarts the server, hand-off
   replays sessions).
3. After handoff completes, run another
   `osc52_write 'after-handoff'`.
4. Observe: still lands in the host clipboard. The new
   server's `ClipboardBurstState` initializes empty; the
   `ClipboardPolicyConfig` is re-read from disk.

### Headless / no-client scenario (FR-013, decision 7)

1. Detach all clients (close all windows; server keeps
   running).
2. From inside a session (via `scribe-cli` or a pre-existing
   PTY-side script), issue an OSC 52 read.
3. Observe: the read returns no payload (server has no client
   to bridge to, so the read is treated as headless-deny).
4. Re-attach a client; the next read prompts again (default
   policy).

### Mixed-version attach (decision 8 / C7)

1. Run the old-version (pre-feature) client against the
   new-version server.
2. The client's attach handshake reports
   `clipboard_gating: false`.
3. Inside a session, issue `osc52_read c`.
4. Observe: the server treats the session as headless for
   OSC 52 prompt purposes — silent deny. No client-side dialog
   (the old client has none).
5. Conversely, new client + old server: the bridge variants
   are never emitted; user-driven paste still works.

---

## Known limitations (documented in spec / research)

- **Headless writes**: A program writing to the clipboard
  during a detached interval loses the value. Documented in
  research decision 7 and visible to users via the Settings
  help text.
- **vte OSC raw-buffer memory at 512 MiB ceiling**: A single
  oversize write attempt allocates ~`cap_bytes` inside vte
  before Scribe rejects it. Visible only at the absolute
  upper bound of the user-exposed setting.
- **Wayland / macOS primary selection**: Maps to the system
  clipboard at the arboard layer. Documented in spec
  Assumptions.
- **OSC 5522** (kitty's out-of-band auth extension): Out of
  scope for v1. Future work if a concrete need arises.
