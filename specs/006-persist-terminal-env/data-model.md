# Data Model — Persist & Restore Terminal Environment

Entities, fields, validation rules, ownership, and state transitions for feature 006. Implementation-agnostic but precise enough to drive `/speckit-tasks` and the contracts in `contracts/`.

## Entities

### TerminalEnvDelta

The per-terminal collection of exported environment variables that differ from the `StartupBaseline` at the latest observed point. The unit that is persisted and later restored.

**Fields**

| Name | Type | Notes |
|---|---|---|
| `added` | `Map<String, String>` | Variable name → current value, for variables that the user/session added or modified relative to the baseline. |
| `removed` | `Set<String>` | Variable names the user/session unset relative to the baseline. |
| `last_updated_at` | `Instant` | Server wall time of the last update; in-memory only, never serialized. |

**Validation**

- Each name conforms to POSIX env-var name rules (`[A-Za-z_][A-Za-z0-9_]*`).
- Names belonging to the `ExclusionSet` are filtered out before serialization.
- Each value ≤ 64 KiB (per-value cap; oversized values are dropped from the delta with a debug log, the rest of the delta remains intact).
- Total serialized size ≤ 512 KiB (per-terminal cap); excess entries are dropped FIFO with a warning log.

**State transitions**

- `Empty` → `HasEntries` on the first `EnvChanged { baseline_ready: false }` after the baseline has been captured for the session.
- `HasEntries` → `HasEntries` on each subsequent `EnvChanged`: `added` entries update; an entry that appears in the event's `removed` is removed from `added` and inserted into `removed`.
- `HasEntries` → `Empty` on session close, clean Scribe quit, or feature-disable transition.

### StartupBaseline

The per-terminal snapshot of the exported environment captured immediately after the shell's rc/initialization completes. Used as the reference for `TerminalEnvDelta`.

**Fields**

| Name | Type | Notes |
|---|---|---|
| `vars` | `Map<String, String>` | Variable name → value at the moment of capture. |
| `captured_at` | `Instant` | Server wall time; for diagnostics only. |

**Validation**

- Captured exactly once per session, on the `EnvChanged { baseline_ready: true }` event.
- Never persisted to disk — strictly in-memory on the server.

**State transitions**

- `Unknown` → `Captured` on the post-rc emit. Immutable thereafter for the session's life.

### EnvChangeEvent

A single hook-channel event reporting set/modify/unset since the shell's previous emit. Drives delta updates.

**Fields**

| Name | Type | Notes |
|---|---|---|
| `session_id` | `SessionId` | Owning session (resolved by the existing hook-channel discovery via `SCRIBE_SESSION_ID`). |
| `added` | `Vec<(String, String)>` | Names + current values for added/modified vars since the shell's last emit. |
| `removed` | `Vec<String>` | Names unset since the shell's last emit. |
| `baseline_ready` | `bool` | `true` exactly once per session — on the post-rc tail emit. `false` on all subsequent emits. |

**Validation**

- Arrives via the existing `ClientMessage::HookEvent` channel; same 100 ms total deadline as other hook events; silently dropped if `session_id` is not in the live session registry (consistent with `hook_ingress::handle`).
- Treated as advisory/best-effort. Each event is itself a delta-since-last-emit from the shell; the server folds the events cumulatively into `TerminalEnvDelta`. A dropped event delays the next persist cycle but does not corrupt state — subsequent events still carry the names the shell observed change.

### EnvEnvelope

The encrypted on-disk representation of a `TerminalEnvDelta`.

**On-disk binary layout** (little-endian where applicable, fixed-width header)

| Offset | Field | Type | Notes |
|---|---|---|---|
| 0 | `version` | `u8` | Currently `1`. Reserved for future format changes. |
| 1 | `reserved` | `[u8; 7]` | All zeros. Padding for 8-byte header alignment. |
| 8 | `nonce` | `[u8; 12]` | Per-write random AEAD nonce. |
| 20 | `ciphertext` | `Vec<u8>` | AEAD-sealed MessagePack-named encoding of `TerminalEnvDelta`. |
| (end) | `tag` | `[u8; 16]` | Poly1305 authentication tag (appended by the AEAD seal). |

**Validation**

- File path: `$XDG_STATE_HOME/<flavor>/restore/env/<window_id>/<launch_id>.envz`.
- Permissions: 0o700 on each enclosing directory, 0o600 on the file. Set on the temp file before rename.
- Atomic create/update: write-to-temp-then-rename (same pattern as `crates/scribe-client/src/restore_state.rs`).
- AEAD primitive: ChaCha20-Poly1305 (RFC 8439). Data-encryption key fetched from the OS secret store by `keystore_identifier_for(flavor, window_id, launch_id)`.

**State transitions**

- `Absent` → `Present` on the first persist cycle after `StartupBaseline` is captured AND at least one `EnvChanged { baseline_ready: false }` has been processed.
- `Present` → `Present (updated)` on each 100 ms debounce window with at least one new `EnvChanged`.
- `Present` → `Absent` on clean session close, clean Scribe quit, feature-disable transition, or unrecoverable read/decrypt error (best-effort delete with warning log).

### RestoreAssociation

The linkage from a restored launch (client-side state) to its persisted `EnvEnvelope` (server-side).

**Fields**

| Name | Type | Notes |
|---|---|---|
| `launch_id` | `String` | The existing `LaunchRecord.launch_id`. Reused unchanged as the envelope file basename and as the `env_envelope_id` value passed to the server in `CreateSession`. |

**Validation**

- Unique per `(window_id, launch_id)` tuple.
- Stable across cold restart: persisted in the existing client restore TOML (`restore/index.toml` and per-window TOMLs).

**State transitions**

- None — `launch_id` is an identifier; it does not change once minted.

### FeatureSetting

The single user-facing toggle that gates capture, persistence, and restore.

**Fields**

| Name | Type | Notes |
|---|---|---|
| `enabled` | `bool` | Config key `terminal.env_persistence.enabled`. Stored under `[terminal.env_persistence]` in `config.toml`. Default `false`. |

**Validation**

- Stored in the existing config file; applied via the existing `ConfigReloaded` flow; no server restart required.
- The `false` → `true` transition is allowed only after a successful `EnvPreflightResult { ok: true }`.
- The `true` → `false` transition is unconditional and immediate.

**State transitions**

- `false` → `true` on user toggle followed by successful preflight.
- `true` → `false` on user toggle. (Note: a server-side runtime fail-safe — see `EnvStatus` — does **not** flip the persisted setting back to `false`; it transitions the *runtime state* to `Degraded` so the user can retry by re-toggling.)

### KeystorePreflight

The one-shot prerequisite check run when the user attempts to enable the `FeatureSetting`.

**Fields**

| Name | Type | Notes |
|---|---|---|
| `result` | `PreflightResult` | One of `Ok`, `KeychainLocked` (macOS), `SecretServiceUnavailable` (Linux), `KeystoreAccessDenied`, `Unknown(String)`. |

**Validation**

- Implementation: a low-cost `set` + `delete` of a sentinel keystore item scoped to the install flavor, via the `keyring` crate. Any error is classified into the variants above; unmapped errors fall into `Unknown` with the underlying message preserved for diagnostics.
- No retries within one preflight call. The user re-toggles to retry.

**State transitions**

- One-shot per enable attempt.

### EnvStatus (runtime)

The per-session runtime indicator of whether env capture is healthy or degraded.

**Fields**

| Name | Type | Notes |
|---|---|---|
| `state` | `EnvStatusState` | `Active` or `Degraded { reason: String }`. |

**Validation**

- Server is authoritative. Default `Active` for any session while the feature is on. Transitions to `Degraded` when an encryption attempt fails for keystore reasons. Returns to `Active` only on a subsequent successful preflight (typically triggered by the user toggling off → on).
- `reason` is short, human-readable, and safe to surface in a tooltip.

**State transitions**

- `Active` → `Degraded { reason }` on a keystore failure during persist.
- `Degraded` → `Active` on a successful preflight after re-toggle.

### ExclusionSet

The fixed (and code-owned) set of variable names that are never captured or restored, because they are Scribe-internal or process/host/session-specific.

**Initial membership** (reviewed and finalized during implementation; documented here so reviewers can challenge):

- **Scribe-injected**: `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, `SCRIBE_HOOK_SOCK`, `SCRIBE_SESSION_ID`, `SCRIBE_RESTORE_ENV_DELTA_FILE`.
- **Terminal identification** (Scribe injects fresh values): `TERM`, `COLORTERM`.
- **Display / desktop session**: `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`, `XDG_SESSION_TYPE`, `XDG_SESSION_ID`, `XDG_SESSION_CLASS`, `DESKTOP_SESSION`.
- **Auth sockets and multiplexer markers**: `SSH_AUTH_SOCK`, `SSH_AGENT_PID`, `SSH_CLIENT`, `SSH_CONNECTION`, `SSH_TTY`, `TMUX`, `TMUX_PANE`, `STY` (GNU screen).
- **Process-id-bearing / shell-managed**: `WINDOWID`, `_` (last-command), `OLDPWD`, `SHLVL`.
- **Locale/auth tickets with host-specific lifetime**: `KRB5CCNAME`, `GPG_TTY`.

**Validation**

- Applied in `env_store::delta` before serialization. Matched names are filtered out of the persisted `TerminalEnvDelta` even if the shell reports them as added/removed.
- Static at compile time. Future user-overridable allow/deny lists are explicitly out of scope for the MVP (Assumptions in `spec.md`).

## Relationships

```text
Window ─┐
        │
        └─ LaunchRecord ───── env_envelope_id (= launch_id) ───── EnvEnvelope (on disk)
                                                                       │
                                                                       └ (decrypt) ─── TerminalEnvDelta
                                                                                              │
                                                                                              ├─ added : user/session-set vars
                                                                                              ├─ removed : user/session-unset vars
                                                                                              └ filtered by ExclusionSet
Server-side per session:
  Session ─── StartupBaseline (in-memory, immutable post-capture)
            ├ TerminalEnvDelta (in-memory, mutable; debounced-persisted as EnvEnvelope)
            └ EnvStatus (Active | Degraded)
```

## Ownership

| Entity | Owner | Lifetime |
|---|---|---|
| `TerminalEnvDelta` | `scribe-server` (in-memory) | per-session |
| `StartupBaseline` | `scribe-server` (in-memory) | per-session; never on disk |
| `EnvChangeEvent` | `scribe-server` (hook ingress) | per-event |
| `EnvEnvelope` | `scribe-server` (on-disk under XDG_STATE) | until session close / clean quit / disable |
| `RestoreAssociation` (`launch_id`) | `scribe-client` (in restore TOML) | until clean session/window close |
| `FeatureSetting` | `scribe-common` config + `~/.config/scribe/config.toml` | until user toggle |
| `KeystorePreflight` | `scribe-server` | transient per enable attempt |
| `EnvStatus` | `scribe-server` (broadcast to client) | per-session runtime |
| `ExclusionSet` | `scribe-server::env_store` (constant) | static |

## Invariants

1. **No plaintext on disk, ever.** If encryption cannot be performed, the envelope is not written. (FR-007, FR-016, SC-005.)
2. **The persisted delta represents the latest observed state, including removals.** Append-only is not allowed. (FR-004.)
3. **Cross-application is impossible.** An envelope at `<window_id>/<launch_id>.envz` is applied only to the session spawned with `env_envelope_id = launch_id` and that same `window_id`. (FR-005.)
4. **Default OFF is observable.** With default settings, no envelope is written, no `EnvStatus` is emitted, and behavior is byte-identical to today. (FR-009, SC-009.)
5. **Handoff is untouched.** No env-store side effects on `--upgrade`; the apply path runs only on cold-restart `CreateSession` with `env_envelope_id = Some`. (Spec scope assumption; R3.5 in `research.md`.)
6. **FR-008 precedence holds by construction.** The delta is sourced *after* rc completes, *before* the baseline-ready emit, so user-set restored values win against rc defaults and become part of the post-restore baseline. (R1.3 in `research.md`.)
