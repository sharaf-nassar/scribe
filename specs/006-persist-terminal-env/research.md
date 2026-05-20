# Phase 0 Research — Persist & Restore Terminal Environment Across Cold Restart

Four research streams resolved every open Technical Context item in `plan.md`. Each section follows the Spec Kit `Decision / Rationale / Alternatives considered` shape, with concrete code touchpoints.

Three cross-stream reconciliations are recorded inline (see `Cross-stream reconciliations` at the end) where two reports disagreed or where an internal contradiction needed correction before being accepted into the plan.

---

## R1 — Shell-integration mechanism, baseline capture & apply, performance budgets

### R1.1 Change detection — Decision: structured hook channel with a new `EnvChanged` kind

The change emit goes through the existing `scribe-hook-helper` → `ClientMessage::HookEvent` → `hook_ingress::handle` pipeline that already powers AI-tool state events. A new `HookEventKind::EnvChanged { added, removed, baseline_ready }` is added to `crates/scribe-common/src/hook.rs`. Each shell's prompt-time hook (precmd / PROMPT_COMMAND / fish_prompt / pre_prompt / prompt function) compares its current exported-env list to a per-session shell-local snapshot and invokes `scribe-hook-helper --provider=system --event=env-delta --added-json='{…}' --removed-json='[…]' [--baseline-ready]` with only what changed.

**Rationale**: reuses the same channel that already carries AI hook events with a hard 100 ms total deadline and silent fail-safe (helper exits 0 on any failure). Wire payload is small (delta-only), so per-prompt overhead stays imperceptible. Server-side ingress code path is the same one already feeding `MetadataEvent`s into the existing broadcast.

**Alternatives considered**:
- OSC-snapshot dumping every prompt — violates PR-001 by pushing whole env (PATH, LS_COLORS, all rc-driven vars) every prompt; the server would also need to diff snapshots itself, duplicating work shells can already do.
- `/proc/<pid>/environ` polling — Linux-only (macOS has no equivalent); race-prone; would skip macOS entirely, violating FR-010 graceful-degradation by platform.
- Hybrid OSC-baseline + hook-delta — extra complexity for no win; hook channel is sufficient for both.

**Code touchpoints**: `crates/scribe-common/src/hook.rs` (add variant); `crates/scribe-hook-helper/src/main.rs` (add `env-delta` event with `--added-json` / `--removed-json` / `--baseline-ready` flags); `crates/scribe-server/src/hook_ingress.rs` (translate `EnvChanged` → in-memory delta update + debounced persist); `dist/` shell-integration scripts (per-shell hook function emit). See `data-model.md` for entity shapes.

### R1.2 Baseline capture — Decision: shell integration emits a one-shot `baseline_ready` flag on its post-rc tail emit

At the very tail of the integration script (after rc files have been sourced, after the restore-delta file — if any — has been sourced; see R1.3), the script invokes `scribe-hook-helper --event=env-delta --added-json='<full exported-env snapshot>' --baseline-ready`. The server treats that one event as the `StartupBaseline` for the session and persists nothing yet; subsequent `EnvChanged` events with `baseline_ready: false` are folded into a `TerminalEnvDelta` against that baseline and persisted (debounced).

**Rationale**: the per-shell signal for "rc is done, the user has not yet typed anything" is exactly the tail of the integration script — by construction. Folding the baseline-ready signal into the existing `EnvChanged` variant (rather than introducing a separate `InitDone` kind) keeps the protocol surface minimal.

**Alternatives considered**:
- Separate `HookEventKind::InitDone` marker — extra variant for no semantic gain; the post-rc snapshot is itself the baseline payload.
- Detecting "shell ready" from the server (e.g. waiting for the first OSC 133;A) — fragile across shells, doesn't reliably correspond to rc completion (zsh emits OSC 133;A before some `precmd` hooks finish).

**Code touchpoints**: each `dist/` integration script gains a single tail emit; `crates/scribe-server/src/env_store/delta.rs` handles the `baseline_ready` branch (record baseline, clear any prior delta, do not persist on baseline event).

### R1.3 Delta apply at restore — Decision: server writes a per-spawn temp delta file; shell integration sources it AFTER rc, immediately before the baseline-ready emit

On a restore-driven PTY spawn (`CreateSession.env_envelope_id.is_some()`), the server decrypts the envelope, writes the resulting `export NAME=value` / `unset NAME` lines to a 0o600 temp file under `$XDG_RUNTIME_DIR/<flavor>/env-apply/<session_id>-<pid>.sh`, and injects its path via a new PTY env var `SCRIBE_RESTORE_ENV_DELTA_FILE`. At the tail of each shell's integration script (i.e., after rc has run), the script sources the file if the env var is present, then unlinks the file, then performs the baseline-ready emit. The server unlinks any unconsumed temp file after a short grace period.

**Rationale**: this is the only ordering that satisfies FR-008 ("captured user-set value wins"). Sourcing BEFORE rc would let rc files clobber the restored values (e.g., `~/.bashrc` resetting `PATH`). Sourcing AFTER rc preserves the user's working environment as the final state of init. The restored values then naturally become part of the post-restore `StartupBaseline` snapshot — which is correct: on a subsequent restart, only what the user changes from that point forward needs to be re-persisted (delta-only model stays consistent and idempotent).

**Alternatives considered**:
- Inject vars directly into `PtyOptions.env` at PTY spawn — the shell sees them as inherited at startup, then rc may overwrite them; the post-rc baseline captures the rc-mutated values, not the restored ones; FR-008 violated.
- Write `export` commands to the PTY's stdin after spawn — fragile timing, may interleave with user keystrokes, depends on shell having opened its line editor.
- Source before rc (R1 agent's original proposal) — REJECTED for FR-008 reasons noted above; this is the explicit correction recorded under `Cross-stream reconciliations`.

**Code touchpoints**: `crates/scribe-server/src/session_manager.rs#build_pty_options` (inject `SCRIBE_RESTORE_ENV_DELTA_FILE` when the spawn is restore-driven); `crates/scribe-server/src/env_store/store.rs` (decrypt + write temp file + schedule grace-period unlink); each `dist/` script (source-then-unlink block at tail, immediately before the baseline emit).

### R1.4 Performance budgets — concrete numbers

| Budget | Value | Basis |
|---|---|---|
| Per-prompt hook overhead | < 20 ms | Hook-helper cold-start ~15 ms (measured in feature 003); shell-side diff vs last-emitted-state ~< 1 ms for 20–100 vars. Imperceptible against typical 50–200 ms human command pacing. |
| Persistence debounce | 100 ms | Coalesces rapid bulk-export blocks (`source ./envrc`, `direnv` hooks) into one disk write; below human perception. |
| Restore-time add per terminal | < 50 ms | Keystore fetch ~5 ms warm + AEAD decrypt ~1 ms + write temp file ~1 ms + shell source < 5 ms. Well under the 100 ms cap (PR-001). |
| Per-value byte cap | 64 KiB | Covers `LS_COLORS` on verbose distros and long concatenated `PATH`s with headroom. Oversized values are skipped with a debug log (FR-014, SC-007). |
| Per-terminal byte cap | 512 KiB | ~100× typical user-env size; oversized deltas degrade FIFO with a warning log. |
| Global cap | 10 MiB | Hard ceiling across all sessions; LRU drop with warning. |

**Measurement plan** (codified in `quickstart.md`): five-iteration warm-up + 200-iteration A/B wall-clock comparison of feature-off vs feature-on prompt latency; pass if the feature-on mean + 2σ is within 5 % of the baseline. Separately: time from `scribe-server` exit to first prompt in the restored terminal over five trials; pass if the average added cost vs an empty-envelope baseline is < 50 ms.

**Alternatives considered**: per-keystroke writes (rejected — disk thrash); 10 ms debounce (rejected — too tight, contention on bulk-exports); 1 MiB per-terminal cap (rejected — bloat without coverage).

---

## R2 — OS keystore integration & encryption-at-rest

### R2.1 Crate choice — Decision: the cross-platform `keyring` crate

`keyring` abstracts macOS Keychain (`security-framework`) and Linux Secret Service (`secret-service` over D-Bus) behind a uniform `Entry::set_password / get_password / delete_password` API, with error variants (`NotFound`, `LockedKeyring`, `NoStorageAccess`, `InvalidInput`) that map cleanly to our `PreflightError` set. Exact crate version is pinned at implementation time by inspecting `crates.io` and the workspace `Cargo.toml`; the version reported by the research agent is treated as indicative only.

**Rationale**: a single abstraction halves the platform-specific test surface and concentrates failure-mode handling in one place. The crate is sync-only, which is acceptable: preflight runs at toggle time (user-initiated, < 200 ms expected), and runtime persistence calls are wrapped in `tokio::task::spawn_blocking` consistent with existing Scribe patterns for blocking I/O.

**Alternatives considered**:
- `security-framework` + `secret-service` directly — doubles platform code; harder to keep a uniform `PreflightError` mapping.
- Storing the env directly in the keystore (no envelope) — runs into per-item-size limits (notably on Linux Secret Service backends) and couples persistence size to keystore API quirks.

**Code touchpoints**: workspace `Cargo.toml` (`keyring` dep, version pinned at implementation); new `crates/scribe-server/src/env_store/keystore.rs`.

### R2.2 Encryption scheme — Decision: envelope encryption with ChaCha20-Poly1305 (via the `ring` crate)

A random 256-bit data-encryption key (DEK) per envelope, stored in the OS secret store under a flavor-aware identifier. The on-disk envelope is a fixed-format binary blob: `version: u8` (currently `1`), 7 bytes reserved, `nonce: [u8;12]`, then AEAD-sealed MessagePack (named-encoding) of `TerminalEnvDelta` with the Poly1305 tag appended by the AEAD construction itself. Atomic write via temp-file + rename, 0o600 permissions, identical to existing `restore_state.rs` patterns. `ring`'s use is contingent on confirming it is already in the workspace at implementation time; if not, we add it (single new crypto dep, FIPS-validated, no `unsafe` in the call sites we'd use).

**Rationale**: envelope encryption decouples the persistence size from keystore item limits and matches NIST guidance. ChaCha20-Poly1305 is constant-time on all architectures (no AES-NI dependence), simple to integrate via `ring::aead`, and standard (RFC 8439). Named MessagePack mirrors the existing handoff serialization style (`rmp_serde::to_vec_named`, post-commit `01458f7`).

**Alternatives considered**: AES-GCM (equivalent security, more legacy footguns in non-AES-NI environments); ring's higher-level `aead::seal_in_place` vs the lower-level `LessSafeKey` API (we'll use the safer wrapper); RustCrypto's `chacha20poly1305` crate (similar quality, but a second crypto dep when `ring` would suffice).

**Code touchpoints**: `crates/scribe-server/src/env_store/envelope.rs` (seal/open + serde wire format); `crates/scribe-server/src/env_store/store.rs` (atomic file I/O).

### R2.3 Per-platform keystore specifics

- **macOS**: target the default login keychain. Service identifier `com.scribe.server` (or `com.scribe.dev.server` for the dev flavor — derived from the existing `AppIdentity` helper). Account name `env-key-<window_id>-<launch_id>`. A locked keychain returns `LockedKeyring`; we classify and surface — we do NOT trigger a system unlock prompt (consistent with non-intrusive UX). If the prompt does appear (e.g. on first use within a fresh session), allowing it is fine and one-time.
- **Linux**: target the default `login` Secret Service collection over the user session D-Bus. Item attributes scoped by `service` + `flavor` + `window_id` + `launch_id` for searchability and clean removal. A missing session D-Bus (notably under SSH-only or a service launched outside `graphical-session.target` — see Scribe commit `3467920` "start scribe after graphical session") is detected before any keyring call and classified as `SecretServiceUnavailable`.
- **Identifier scheme**: every key is namespaced by the install flavor so stable and `scribe-dev` installs cannot collide.

### R2.4 Preflight semantics — Decision: low-cost sentinel set + delete; map errors to a small enum

```
enum PreflightError {
  KeychainLocked,           // macOS
  SecretServiceUnavailable, // Linux (no D-Bus / no Secret Service backend)
  KeystoreAccessDenied,     // either platform, permission denied
  Unknown(String),
}
```

Preflight writes a sentinel "preflight" item and immediately deletes it. Success ⇒ keystore is reachable, writable, and unlocked. Each error variant maps to a single actionable user-facing message (see `contracts/config-and-settings-ui.md`).

### R2.5 Runtime fail-safe — Decision: degrade-not-plaintext, surface non-intrusively, recover via re-toggle

Any keystore error during persist (`LockedKeyring`, `NoStorageAccess`, etc.) causes the server to (a) leave any existing envelope file untouched (it remains valid for restore), (b) NOT write any new file in plaintext or otherwise, (c) transition the session's runtime `EnvStatus` to `Degraded { reason }`, and (d) emit `ServerMessage::EnvStatus { session_id, state: Degraded { … } }` to the client. The status-bar surface (R4) renders a warning indicator. Recovery is by user re-toggle from Settings (re-runs preflight); a successful preflight transitions sessions back to `Active`. Automatic background re-probing is deliberately out of scope (avoids periodic keystore noise and unauthenticated retry storms).

---

## R3 — Storage layout & protocol integration

### R3.1 Storage owner — Decision: server-side

The server owns PTY lifecycles, the hook ingress that observes env changes, the keystore interface, and existing on-disk persistence patterns (handoff state, workspace notes). Putting encrypted envelopes anywhere else would either (a) push the encryption interface onto the client, doubling the cryptographic surface, or (b) force a round-trip protocol just to read or update a delta. Server ownership also keeps the fail-safe path local: if encryption can't proceed, the server simply stops persisting and emits an `EnvStatus` event.

### R3.2 Storage layout

Envelopes live at `$XDG_STATE_HOME/<flavor>/restore/env/<window_id>/<launch_id>.envz`, alongside the existing `restore/windows/<window_id>.toml`. Mode 0o700 on each enclosing directory, 0o600 on each file. Atomic creation uses the same write-temp-then-rename pattern with private-mode chains as `crates/scribe-client/src/restore_state.rs`. Per-launch concurrency is by-file (independent files = no shared lock); within a single launch, persistence is single-threaded (the per-session debounced timer task), so no lock is needed.

Cleanup: deleted on clean session close (matching the same trigger that deletes the `LaunchRecord`), on clean Scribe quit (mirroring the existing restore-state lifecycle), and on feature disable. Retained on crash. Empty `<window_id>/` directories are pruned lazily on next startup.

### R3.3 Restore association — Decision: extend `ClientMessage::CreateSession` with an optional `env_envelope_id` field

The client passes the `LaunchRecord.launch_id` (already a stable identifier present in `restore_state.rs`) when re-issuing a launch during cold restart. The server reads the file at `restore/env/<window_id>/<env_envelope_id>.envz`, decrypts it, and applies it through the `SCRIBE_RESTORE_ENV_DELTA_FILE` mechanism (R1.3). If the envelope is missing, unreadable, or fails decryption, the session is spawned without an applied delta and (for the decryption-fail case) the session transitions to `EnvStatus::Degraded`.

**Alternatives considered**:
- Add a new `RestoreEnv` protocol message — extra round-trip without benefit.
- Use `session_id` for keying — `session_id` is minted server-side on spawn; the client doesn't know it before issuing `CreateSession`.

### R3.4 Protocol additions (full list)

- `ClientMessage::EnvPreflight` (no fields). Sent at toggle-on.
- `ServerMessage::EnvPreflightResult { ok: bool, error: Option<PreflightError> }`. Sent in reply.
- `ClientMessage::CreateSession` gains `env_envelope_id: Option<String>` with `#[serde(default)]`. Additive.
- `ServerMessage::EnvStatus { session_id, state: EnvStatusState }` where `state` is `Active | Degraded { reason: String }`. Sent on transitions only.
- `HookEventKind::EnvChanged { added, removed, baseline_ready }` (additive variant in `crates/scribe-common/src/hook.rs`).

All payloads use named MessagePack with `#[serde(default)]` on every new field so older peers tolerate them gracefully. No protocol version bump needed. `HANDOFF_VERSION` is **not** modified (handoff scope is excluded — see R3.5).

### R3.5 Handoff exclusion — confirmed

Handoff passes the PTY's master file descriptor between server instances via `SCM_RIGHTS`; the child shell process is NOT respawned, so its in-process environment is preserved across the handoff by construction. Env-store data is intentionally **not** included in `HandoffState`. There is no risk of double-application: the apply path runs only when `CreateSession.env_envelope_id` is `Some`, which only happens during the cold-restart replay (client-issued from `restore_replay`), never during handoff (the new server reconstructs sessions from `from_handoff_state`, a separate code path).

### R3.6 Lifecycle

- **Create** envelope: on the first persist cycle after a `baseline_ready: true` event has been processed for the session.
- **Update** envelope: each 100 ms debounce window in which at least one `EnvChanged { baseline_ready: false }` was processed.
- **Delete** envelope: on clean session close (`ipc_server.rs` close-session handler), on clean Scribe quit (final per-window sweep), on feature-disable transition (sweep all envelopes), and on detection of corrupted envelope (best-effort cleanup with warning log).
- **Retain** envelope: only when the corresponding `LaunchRecord` would also be retained — i.e., the client crashed or was killed before clean shutdown.

---

## R4 — Settings UI integration

### R4.1 Insertion point and HTML pattern

A new row goes into `crates/scribe-settings/src/assets/settings.html` immediately after the existing "Enhanced keyboard protocol (Kitty)" toggle in the Terminal → General section. The new row reuses the existing `.setting-row` / `.setting-info` / `.toggle` structure, with `data-key="terminal.env_persistence.enabled"` and an id of `env-persistence-toggle`. A sibling `.setting-row#env-persistence-error-row` is added immediately after, hidden by default, used to surface preflight failure inline.

### R4.2 Config field shape — Decision: nested `terminal.env_persistence.enabled`

Add a `TerminalEnvPersistenceConfig { enabled: bool }` nested struct on `TerminalConfig` with `#[serde(default)]` (default false), mirroring the existing `clipboard` / `ai_integration` / `ai_session` nested-config pattern. This leaves room for future fields (e.g. user-defined exclusion overrides) without polluting the flat namespace.

### R4.3 Settings → server flow

The change reaches `scribe-server` via the existing path: `settings.js` `sendChange` → IPC `setting_changed` → `apply.rs` dispatches via `apply_terminal_key` (new match arm) → updates `config.terminal.env_persistence.enabled` → `save_config()` writes `config.toml` → the existing file-watcher pipeline emits `ConfigReloaded` to the server. The on-change handler in `ipc_server.rs` either spins up the env-store machinery (on transition to enabled) or shuts it down and deletes existing envelopes (on transition to disabled).

### R4.4 Preflight UX

The toggle's click handler is the only one in `settings.js` that runs a preflight. On enable attempt:
1. Disable further clicks; do not yet flip the visual state.
2. Send `ClientMessage::EnvPreflight`.
3. On `ok: true` — flip to ON, call `sendChange(...)`, hide error row.
4. On `ok: false` — keep OFF, populate `#env-persistence-error-row` with the message mapped from `error`, auto-dismiss after 6 s.

Reopening Settings with the feature already on does NOT re-run preflight; the toggle simply reflects the persisted config. Runtime degradation (FR-016) is surfaced separately (R4.5).

### R4.5 Runtime degradation surface — Decision: persistent status-bar warning glyph

When the server emits `ServerMessage::EnvStatus { state: Degraded { reason } }` for a session, the client adds a small warning glyph (`⚠`) to the affected pane's status bar — immediately right of the existing command-status indicator — using the existing warning color slot. Hover tooltip: "Environment capture paused: keystore unavailable. Retry from Settings → Terminal → General." No toast, no modal, no banner. The glyph clears when the server emits an `EnvStatus { state: Active }` for that session (after the user re-toggles and preflight succeeds).

### R4.6 Disable transition — Decision: delete all envelopes immediately

When the user toggles the feature OFF, the server stops all per-session persist timers and deletes every envelope under `restore/env/`. Rationale: the user has explicitly signaled they no longer want env data persisted; leaving stale envelopes would be a privacy surprise on re-enable.

### R4.7 Copy

Toggle label: **"Persist Environment"**. Sublabel: **"Capture and restore terminal environment across cold restart (requires OS secret store)"**. Preflight error messages are platform-specific and reproduced in `contracts/config-and-settings-ui.md`. Status-bar tooltip on degraded: **"Environment capture paused: keystore unavailable. Retry from Settings → Terminal → General."**

---

## Cross-stream reconciliations

1. **Delta-apply ordering (R1 internal contradiction → corrected)**: R1.3 in the agent report proposed sourcing the restore-delta file *before* rc, which would let rc files clobber restored user values and violate FR-008. The plan adopts the inverse order — source *after* rc has run, *before* the `baseline_ready` emit — so restored user values become the final state of init and the post-restore baseline includes them.
2. **`keyring` crate version (R2)**: pinned at implementation time by inspecting `crates.io` and the workspace `Cargo.toml` rather than committing to the version the agent reported, which has not been independently verified.
3. **`EnvStatus` ownership (R3 vs R4)**: protocol-level shape (`ServerMessage::EnvStatus { … }`) is documented in R3 / `contracts/protocol-additions.md`; the client-side surface is owned by R4 / status bar. Both reports agree; this entry only confirms the split.
