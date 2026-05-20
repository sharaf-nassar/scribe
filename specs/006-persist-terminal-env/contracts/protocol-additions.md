# Contract: Protocol Additions

Wire-level additions to `ClientMessage` / `ServerMessage` in `crates/scribe-common/src/protocol.rs`. Every new field uses **named MessagePack** with `#[serde(default)]` so old peers tolerate them without a protocol version bump. The existing handoff wire format is **not** modified — `HANDOFF_VERSION` stays at its current value.

## ClientMessage::EnvPreflight (new)

Direction: client → server. No payload.

Sent when the user attempts to enable the feature toggle in Settings → Terminal → General. The server replies with exactly one `ServerMessage::EnvPreflightResult`.

## ServerMessage::EnvPreflightResult (new)

```text
EnvPreflightResult {
  ok: bool,
  error: Option<PreflightError>,    #[serde(default)]
}

enum PreflightError {
  KeychainLocked,
  SecretServiceUnavailable,
  KeystoreAccessDenied,
  Unknown(String),
}
```

Semantics:

- `ok == true` ⇒ the OS secret store is reachable and usable for our identifier; the settings layer commits the toggle.
- `ok == false` ⇒ `error` is `Some`; settings reverts the toggle visual state and renders the user-facing message mapped from the variant (see `config-and-settings-ui.md`).

## ClientMessage::CreateSession (extended)

```text
CreateSession {
  workspace_id:     WorkspaceId,
  split_direction:  Option<LayoutDirection>,
  cwd:              Option<PathBuf>,
  size:             Option<TerminalSize>,
  command:          Option<Vec<String>>,
  env_envelope_id:  Option<String>,    #[serde(default)]    // NEW — restore association
}
```

`env_envelope_id`:

- Carries the `LaunchRecord.launch_id` from the client's restore TOML when the session is being re-issued by `restore_replay`.
- Server lookup path: `$XDG_STATE_HOME/<flavor>/restore/env/<window_id>/<env_envelope_id>.envz`.
- On a successful decrypt the server materializes a temp delta file (see `hook-event-additions.md::SCRIBE_RESTORE_ENV_DELTA_FILE`) and injects its path as a PTY env var so the shell integration can source it after rc.
- On any failure (missing file, corrupt header, decrypt error, keystore unavailable) the session continues without an applied delta; the failure transitions the session's `EnvStatus` to `Degraded` and an `EnvStatus` event is emitted.

Backward compatibility: clients omitting the field and servers not yet knowing it both default to `None`. No version bump.

## ServerMessage::EnvStatus (new)

```text
EnvStatus {
  session_id: SessionId,
  state:      EnvStatusState,
}

enum EnvStatusState {
  Active,
  Degraded { reason: String },
}
```

Direction: server → client. Sent on transitions only — not periodic. Emitted when:

- A session's env capture transitions to `Degraded` because of a keystore error during persist or decrypt.
- A previously-degraded session's env capture transitions back to `Active` after a user-initiated re-toggle and successful preflight.

Drives the status-bar warning glyph on the affected pane (see `config-and-settings-ui.md::Status Bar Surface`).

## Serialization discipline

- All payloads use the existing `rmp_serde::to_vec_named` / `from_slice` codec already established in `scribe-server/src/handoff.rs` (post-commit `01458f7`).
- Every new field MUST carry `#[serde(default)]`. Every new enum variant SHOULD avoid mandatory fields; if mandatory fields are unavoidable, they must be additive only.
- No change to existing handoff serialization; `HANDOFF_VERSION` is **not** bumped because `HandoffState` is not modified.

## Out of scope for this contract

- `HookEventKind::EnvChanged` is documented in `hook-event-additions.md`, not here.
- Config field shape (`terminal.env_persistence.enabled`) is documented in `config-and-settings-ui.md`.
