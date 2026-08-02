# Contract: Hook Channel & Shell Integration Additions

Additions to `crates/scribe-common/src/hook.rs`, `crates/scribe-hook-helper/`, the server's hook ingress, and each shell integration script under `dist/`. All additions are additive named-MessagePack variants with `#[serde(default)]` discipline (see `protocol-additions.md`).

## HookEventKind::EnvChanged (new variant)

```text
EnvChanged {
  added:           Vec<(String, String)>,   #[serde(default)]   // (name, current value) since the shell's last emit
  removed:         Vec<String>,             #[serde(default)]   // names unset since the shell's last emit
  baseline_ready:  bool,                    #[serde(default)]   // true once for a capture-capable resident shell, after startup
}
```

Emitted by each capture-capable resident shell via `scribe-hook-helper`. AI tabs exec before a prompt and intentionally emit neither a baseline nor prompt-time deltas.

Two scenarios:

1. **Baseline emit (once per capture-capable resident session, after rc/profile processing and after the restore-delta file, if any, has been applied)**:
   - `added` = full snapshot of the current exported environment.
   - `removed` = empty.
   - `baseline_ready = true`.
   - Server action: record the snapshot as the session's `StartupBaseline`; do not persist; clear any prior delta for that session.

2. **Subsequent emits (each prompt return)**:
   - `added` / `removed` = the shell's observed delta since its own previous emit.
   - `baseline_ready = false`.
   - Server action: fold the delta into the session's `TerminalEnvDelta` (filter through the `ExclusionSet`), restart the 100 ms persist debounce timer.

## `scribe-hook-helper` invocation

Current invocation (value-bearing payload on stdin; older value flags remain compatibility fallbacks):

```bash
printf '%s' '{"added":{"NAME":"value"},"removed":["UNSET_NAME"]}' \
  | scribe-hook-helper \
      --provider=system \
      --event=env_delta \
      --payload-stdin \
      [--baseline-ready]
```

- The helper retains its existing 100 ms total deadline and its existing exit-0-on-any-failure contract — it never blocks the shell.
- `--provider=system` distinguishes these events from AI-provider events so the existing translation map in `hook_ingress::handle` routes them correctly.
- `--added-json` / `--removed-json` remain accepted when a long-lived pre-upgrade shell calls the older contract; stdin fields win when present.

## Server-side translation (`crates/scribe-server/src/hook_ingress.rs`)

The existing `translate(provider, kind)` step gains a branch for `HookEventKind::EnvChanged`:

- If `baseline_ready == true`: emit `MetadataEvent::EnvBaselineCaptured { vars: added }` (or equivalent in-server notification) → `session_manager` records the `StartupBaseline`. Do not enqueue persistence.
- If `baseline_ready == false`: fold `added` / `removed` into the session's `TerminalEnvDelta` (after `ExclusionSet` filtering) → reset/start the per-session 100 ms persist timer.

The on-disk envelope is written by the persist timer's task, not synchronously inside `hook_ingress::handle`.

## Per-shell integration changes

Every supported resident shell performs the same three logical operations after user startup, but the trigger is shell-specific:

1. **Apply the restore-delta file if present** (FR-008: applied AFTER rc/profile processing). Bash, nushell, and PowerShell do this at the integration tail. Zsh and fish register a self-removing first-prompt initializer from their pre-rc bootstrap; that initializer applies and deletes the file after user startup, captures the baseline, then enables recurring delta observation.

   ```bash
   # Bash / Zsh
   if [[ -n "${SCRIBE_RESTORE_ENV_DELTA_FILE:-}" && -f "$SCRIBE_RESTORE_ENV_DELTA_FILE" ]]; then
     # shellcheck disable=SC1090
     source "$SCRIBE_RESTORE_ENV_DELTA_FILE"
     rm -f "$SCRIBE_RESTORE_ENV_DELTA_FILE" 2>/dev/null || true
   fi
   ```

   Equivalent forms exist for Fish (`builtin source`), Nushell (read JSON then `load-env`/`hide-env`), and PowerShell (dot-source a `.ps1` file).

2. **Capture the baseline**: compute the full post-startup, post-restore exported environment and invoke the helper with `baseline_ready = true`. This is the signal the server uses to capture `StartupBaseline`.

3. **Register the prompt-time hook**: compute and emit the delta since the baseline/previous emit on each later prompt return. The shell keeps an in-process snapshot, compares it against the current exported environment, and invokes the helper only for a non-empty diff.

Structured AI launches are a separate consumer path. Bash's `-lic` preamble sources `SCRIBE_INTEGRATION_SCRIPT` in AI mode after login startup; that script applies and removes the restore file before returning to the preamble. Zsh and fish load only a pre-rc AI guard, then their server-built post-login preamble applies and removes the file. All three exec the provider immediately afterward, so prompt OSC marks, baseline emission, and per-prompt env-delta emission are moot. Nushell, PowerShell, and unknown AI launches have no restore consumer and never stage a file.

## `SCRIBE_RESTORE_ENV_DELTA_FILE` (new PTY env var)

Set by `crates/scribe-server/src/session_manager.rs#build_pty_options` **only** when persistence is enabled, the spawn is restore-driven (`CreateSession.env_envelope_id.is_some()`), the corresponding envelope decrypted successfully, and the launch kind has a restore consumer.

- Value: absolute path to a per-spawn temp file.
- Contents: target-shell dialect selected before rendering: POSIX `export`/`unset` for bash/zsh, Fish `set -gx`/`set -e`, a JSON added/removed object for Nushell, or PowerShell env assignment/removal. Values use that dialect's escaping.
- Location: `$XDG_RUNTIME_DIR/<flavor>/env-apply/<session_id>-<pid>.<ext>` with `.sh`, `.fish`, `.json`, or `.ps1` as appropriate (per-user runtime dir; ephemeral by design).
- Permissions: 0o600 on the file; 0o700 on the enclosing directory.
- Lifecycle: the shell integration or AI preamble unlinks the file immediately after applying it; the server unlinks any unconsumed file after a short grace period (defensive cleanup).

## Performance contract (re-summarized from research.md R1.4)

- Per-prompt overhead from `EnvChanged` emit ≤ 20 ms (helper cold-start + shell-side diff combined). Imperceptible against human-paced command latency.
- Persist debounce: 100 ms per session. Coalesces bulk `export` blocks into a single envelope write.
- Restore-time-to-first-prompt add ≤ 50 ms per terminal (keystore fetch + decrypt + temp file + shell source). Under PR-001's 100 ms cap.

## Out of scope for this contract

- Encryption/AEAD wire format is owned by `env_store::envelope` and described in `data-model.md::EnvEnvelope`.
- Settings UI behavior is in `config-and-settings-ui.md`.
- `ServerMessage::EnvStatus` shape is in `protocol-additions.md`.
