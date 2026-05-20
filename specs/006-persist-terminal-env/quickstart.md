# Quickstart — Manual Verification

Per `spec.md` QR-002 + constitution principle II, this feature uses manual quickstart as its primary verification path (with a small number of surgical Rust unit tests for high-risk isolated units). Each section below corresponds to one independent-test slice defined in `spec.md`. Run on Linux **and** macOS for full coverage; pick one for the first pass.

## Prereqs (one-time, both OSes)

- Build the workspace: `cargo build` (debug) or `just build` (release).
- `scribe-server`, `scribe-client`, `scribe-settings`, and `scribe-hook-helper` available on PATH or in `target/<profile>/`.
- macOS: login Keychain unlocked.
- Linux: a graphical session with `gnome-keyring` or `KWallet` running, or skip US3 Path A and exercise the failure path instead.
- The shell integration script for your shell is sourced (verify by checking `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID` are set in a fresh terminal).

## US1 — Restore my environment after an unexpected cold restart (P1)

Goal: SC-001, SC-002, SC-004.

1. Start Scribe. Open Settings → Terminal → General; enable **"Persist Environment"**. On macOS, allow the one-time keychain access prompt if it appears.
2. Open one terminal. In it run:
   ```bash
   export SCRIBE_TEST_TOKEN=secret-abc
   export PROJECT_ROOT=/tmp/sc-test
   export PATH=/opt/local/bin:$PATH
   unset SHLVL  # SHLVL should remain in the ExclusionSet — verifying SC-004
   cd /tmp
   ```
   Wait one prompt return (≥ 150 ms) for the debounce window to flush.
3. Force-kill the server: `pkill -9 scribe-server`. Confirm `scribe-client` reports a cold restart and respawns terminals from the saved layout.
4. In the restored terminal:
   ```bash
   echo "$SCRIBE_TEST_TOKEN" "$PROJECT_ROOT"
   echo "$PATH"
   echo "SHLVL=$SHLVL WINDOW=$WINDOWID DISPLAY=$DISPLAY"
   ```
   Expected:
   - `secret-abc /tmp/sc-test` — restored user-set vars (SC-001).
   - `PATH` begins with `/opt/local/bin:` — restored.
   - `SHLVL` is shell-default (e.g., `1`); `WINDOWID` / `DISPLAY` reflect the *current* session's values, not stale ones from the previous server process (SC-004; verifies the ExclusionSet correctly filtered out process/host-specific vars).
5. **Acceptance Scenario 2 (removal)**: unset one of the vars (`unset PROJECT_ROOT`), wait one prompt, force-kill, restore. `echo "${PROJECT_ROOT:-unset}"` should print `unset`.

Pass criteria: all echoes match expectations; zero manual re-exports performed (SC-002).

## US2 — Continuous, silent capture (P2)

Goal: SC-001 freshness + SC-003 no perceptible latency + SC-005 zero plaintext.

1. With the feature on and a terminal open, set + unset:
   ```bash
   export US2_A=1; export US2_B=2; unset US2_A
   ```
   Wait one prompt return.
2. Inspect the persisted envelope — do **not** attempt to read it as plaintext:
   ```bash
   ls -l "$XDG_STATE_HOME/scribe/restore/env/"*/*.envz
   file "$XDG_STATE_HOME/scribe/restore/env/"*/*.envz
   strings "$XDG_STATE_HOME/scribe/restore/env/"*/*.envz | grep -E 'US2_A|US2_B|^1$|^2$' || echo "PASS: no plaintext"
   ```
   Expected: file reports as binary data; `strings` scan does NOT reveal `US2_A`, `US2_B`, or their values (SC-005).
3. Force-kill and restore as in US1.
4. In the restored terminal:
   ```bash
   echo "${US2_A:-unset}" "${US2_B:-unset}"
   ```
   Expected: `unset 2` — `US2_A` is absent (Acceptance Scenario 2 of US2, SC-001).
5. **Latency check** (PR-001):
   ```bash
   # Baseline (feature OFF — toggle it off in Settings first)
   for i in 1 2 3 4 5; do printf .; done > /dev/null   # warmup
   time (for i in $(seq 1 200); do printf "p$i\n"; done) > /dev/null
   # Then turn the feature back ON, open a fresh terminal so the integration script re-sources, repeat:
   time (for i in $(seq 1 200); do printf "p$i\n"; done) > /dev/null
   ```
   Pass criterion: feature-on wall-clock is within 5 % of baseline over 200 iterations (target: indistinguishable). If a human can perceive a slowdown, investigate before declaring pass.

## US3 — Opt-in and sensitive-value protection (P3)

Goal: SC-005 (encryption-at-rest), SC-008 (preflight refuses with message), SC-009 (default OFF behavior unchanged).

### Path A — happy path (keystore present)

1. Fresh install / fresh config: open Settings → Terminal → General. The toggle should read OFF (SC-009).
2. With the toggle OFF, open a terminal and `export DEFAULT_OFF=1`; force-kill and restore. Confirm `echo "${DEFAULT_OFF:-unset}"` prints `unset` (default OFF means no behavior change; SC-009).
3. Enable the toggle. (macOS: allow the one-time keychain prompt.) The toggle stays ON; no error row appears.
4. Repeat the US1 export-then-restore flow; confirm restoration works (SC-001).

### Path B — failure path (keystore unavailable)

1. Disable the toggle.
2. Linux: kill the `gnome-keyring-daemon` / `kwalletd5` process (or unset `DBUS_SESSION_BUS_ADDRESS` for the Scribe server's process tree). macOS: lock the login keychain via Keychain Access.
3. Open Settings → Terminal → General; flip the toggle ON.
4. Expected: the toggle reverts to OFF; an inline red error row appears below it with the actionable platform-specific message from `contracts/config-and-settings-ui.md::Preflight error message map`. The on-disk env-store directory contains no new envelopes for this session (SC-008).

Pass criteria: default-OFF behavior unchanged; happy-path enable works; failure-path enable is refused-with-message; zero plaintext anywhere on disk.

## Restore timing (PR-001 < 100 ms)

1. Enable the feature; populate a terminal's env with ~50 user-set vars.
2. Force-kill the server. Time the interval from server exit to the moment the restored terminal renders its first prompt.
3. Repeat with the feature disabled (no envelope to apply); take the difference as the added cost.
4. Pass criterion: added cost ≤ 50 ms over five trials (under the spec's 100 ms cap).

Acceptable measurement: server-side metric if `scribe-server --metrics` is available; otherwise stopwatch on the visual prompt appearance.

## Failure-mode probes

- **Unsupported or disabled shell integration** (FR-010, SC-006): open a terminal under a shell whose integration script has not yet been updated with the env-delta block — *or* with shell integration disabled in `~/.config/scribe/config.toml`. With the feature on, run `export TEST_UNSUPPORTED=1`; wait one prompt return; force-kill `scribe-server` and let it restore. Expected: no envelope is ever created for that session under `restore/env/`; the restored terminal renders layout and CWD normally; `echo "${TEST_UNSUPPORTED:-unset}"` prints `unset` (no env restore occurred for that session); no `⚠` warning glyph appears on the pane; no error is recorded in the server `tracing` log. The terminal is fully functional throughout.
- **Disable transition**: with the feature on and envelopes present, toggle the setting OFF. Confirm every file under `$XDG_STATE_HOME/<flavor>/restore/env/` is deleted (FR-009 + R4.6).
- **Mid-session keystore failure** (Linux: `kill -STOP $(pgrep gnome-keyring-daemon)`; macOS: lock the login keychain). With the feature still on, `export TRIGGER_DEGRADE=1` and wait one prompt. Observe:
  - The status bar shows a `⚠` indicator on the affected pane with the tooltip "Environment capture paused: keystore unavailable. Retry from Settings → Terminal → General."
  - No new plaintext envelope appears.
  - Unlock the keystore; in Settings, toggle the feature off then on (this re-runs preflight). Expected: the status-bar indicator clears, and the next `export` is persisted again (FR-016).
- **Oversized value**: `export BIG=$(yes a | head -c 100000)` and wait one prompt. The persist still succeeds for the rest of the delta; a debug log records the skipped variable (FR-014, SC-007).
- **Clean Scribe quit**: with the feature on and envelopes present, quit Scribe via File → Quit. Confirm all `restore/env/<window_id>/` directories are emptied.

## Automated tests (if added per constitution gate 2 documented deferral)

The plan documents that these surgical Rust unit tests are added because a silent regression in any of them would be dangerous:

```bash
cargo test -p scribe-server env_store::delta            # delta computation against baseline
cargo test -p scribe-server env_store::delta::exclusion # ExclusionSet application
cargo test -p scribe-server env_store::envelope         # seal / open round-trip with sample key
cargo test -p scribe-server env_store::keystore::preflight # PreflightError variant mapping
```

No new integration test harness is introduced. Broader behavior is verified by the manual scenarios above.

## Sign-off

Each user-story section above must pass on **at least one** of {Linux, macOS} before declaring the feature complete; both platforms must pass before release. Record the platform(s) used and any deviations in the final completion report.
