# Contract: Hook Channel & Shell Integration Additions

Additions to `crates/scribe-common/src/hook.rs`, `crates/scribe-hook-helper/`, the server's hook ingress, and each shell integration script under `dist/`. All additions are additive named-MessagePack variants with `#[serde(default)]` discipline (see `protocol-additions.md`).

## HookEventKind::EnvChanged (new variant)

```text
EnvChanged {
  added:           Vec<(String, String)>,   #[serde(default)]   // (name, current value) since the shell's last emit
  removed:         Vec<String>,             #[serde(default)]   // names unset since the shell's last emit
  baseline_ready:  bool,                    #[serde(default)]   // true exactly once per session — on the post-rc tail emit
}
```

Emitted by each shell's prompt-time hook (or, for the baseline-ready event, by the tail of the integration script itself) via `scribe-hook-helper`.

Two scenarios:

1. **Baseline emit (once per session, at the tail of the integration script — after rc has run and after the restore-delta file, if any, has been sourced)**:
   - `added` = full snapshot of the current exported environment.
   - `removed` = empty.
   - `baseline_ready = true`.
   - Server action: record the snapshot as the session's `StartupBaseline`; do not persist; clear any prior delta for that session.

2. **Subsequent emits (each prompt return)**:
   - `added` / `removed` = the shell's observed delta since its own previous emit.
   - `baseline_ready = false`.
   - Server action: fold the delta into the session's `TerminalEnvDelta` (filter through the `ExclusionSet`), restart the 100 ms persist debounce timer.

## `scribe-hook-helper` invocation

New subcommand:

```bash
scribe-hook-helper \
  --provider=system \
  --event=env-delta \
  --added-json='{"NAME":"value","OTHER":"value2"}' \
  --removed-json='["UNSET_NAME"]' \
  [--baseline-ready]
```

- The helper retains its existing 100 ms total deadline and its existing exit-0-on-any-failure contract — it never blocks the shell.
- `--provider=system` distinguishes these events from AI-provider events so the existing translation map in `hook_ingress::handle` routes them correctly.
- Either of `--added-json` or `--removed-json` may be omitted (defaulting to `{}` / `[]`).

## Server-side translation (`crates/scribe-server/src/hook_ingress.rs`)

The existing `translate(provider, kind)` step gains a branch for `HookEventKind::EnvChanged`:

- If `baseline_ready == true`: emit `MetadataEvent::EnvBaselineCaptured { vars: added }` (or equivalent in-server notification) → `session_manager` records the `StartupBaseline`. Do not enqueue persistence.
- If `baseline_ready == false`: fold `added` / `removed` into the session's `TerminalEnvDelta` (after `ExclusionSet` filtering) → reset/start the per-session 100 ms persist timer.

The on-disk envelope is written by the persist timer's task, not synchronously inside `hook_ingress::handle`.

## Per-shell integration changes

Every supported shell's integration script (under `dist/`) gains three additions, in this order, at the tail of the script:

1. **Source the restore-delta file if present** (FR-008: applied AFTER rc has run):

   ```bash
   # Bash / Zsh
   if [[ -n "${SCRIBE_RESTORE_ENV_DELTA_FILE:-}" && -f "$SCRIBE_RESTORE_ENV_DELTA_FILE" ]]; then
     # shellcheck disable=SC1090
     source "$SCRIBE_RESTORE_ENV_DELTA_FILE"
     rm -f "$SCRIBE_RESTORE_ENV_DELTA_FILE" 2>/dev/null || true
   fi
   ```

   Equivalent forms for Fish (`set -q SCRIBE_RESTORE_ENV_DELTA_FILE; and test -f …; and builtin source …`), Nushell (`if ($env.SCRIBE_RESTORE_ENV_DELTA_FILE? | is-not-empty) and ($env.SCRIBE_RESTORE_ENV_DELTA_FILE | path exists) { source $env.SCRIBE_RESTORE_ENV_DELTA_FILE }`), and PowerShell (`if ($env:SCRIBE_RESTORE_ENV_DELTA_FILE -and (Test-Path $env:SCRIBE_RESTORE_ENV_DELTA_FILE)) { . $env:SCRIBE_RESTORE_ENV_DELTA_FILE }`).

2. **Register the prompt-time hook**: per shell, compute and emit the delta-since-last-emit on each prompt return. The shell maintains its own per-session in-process snapshot of "last emitted state" (an associative-array equivalent), compares it against the current exported environment, and invokes the helper with the diff.

3. **One-shot baseline emit**: at the very end of the integration script, compute the full current exported environment and invoke the helper with `--added-json='<snapshot>' --baseline-ready`. This is the signal the server uses to capture `StartupBaseline`.

## `SCRIBE_RESTORE_ENV_DELTA_FILE` (new PTY env var)

Set by `crates/scribe-server/src/session_manager.rs#build_pty_options` **only** when the spawn is restore-driven (`CreateSession.env_envelope_id.is_some()` and the corresponding envelope decrypted successfully).

- Value: absolute path to a per-spawn temp file.
- Contents: shell-source-compatible statements — `export NAME=value` for each entry in the decrypted `TerminalEnvDelta::added` and `unset NAME` for each entry in `removed`. Values are shell-quoted to handle spaces, newlines, and other special characters safely.
- Location: `$XDG_RUNTIME_DIR/<flavor>/env-apply/<session_id>-<pid>.sh` (per-user runtime dir; ephemeral by design).
- Permissions: 0o600 on the file; 0o700 on the enclosing directory.
- Lifecycle: the shell integration unlinks the file immediately after sourcing it; the server unlinks any unconsumed file after a short grace period (defensive cleanup).

## Performance contract (re-summarized from research.md R1.4)

- Per-prompt overhead from `EnvChanged` emit ≤ 20 ms (helper cold-start + shell-side diff combined). Imperceptible against human-paced command latency.
- Persist debounce: 100 ms per session. Coalesces bulk `export` blocks into a single envelope write.
- Restore-time-to-first-prompt add ≤ 50 ms per terminal (keystore fetch + decrypt + temp file + shell source). Under PR-001's 100 ms cap.

## Out of scope for this contract

- Encryption/AEAD wire format is owned by `env_store::envelope` and described in `data-model.md::EnvEnvelope`.
- Settings UI behavior is in `config-and-settings-ui.md`.
- `ServerMessage::EnvStatus` shape is in `protocol-additions.md`.
