# Contract: Config Field & Settings UI

The single new user-facing surface for feature 006: one toggle row in Settings → Terminal → General, backed by one nested config field, with an inline preflight-failure message and a persistent runtime degradation indicator in the status bar.

## Config field (`crates/scribe-common/src/config.rs`)

A nested struct on `TerminalConfig`, mirroring the existing `clipboard` / `ai_integration` / `ai_session` nested-config pattern:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TerminalEnvPersistenceConfig {
    #[serde(default)]
    pub enabled: bool,
}

pub struct TerminalConfig {
    // …existing fields…
    #[serde(default)]
    pub env_persistence: TerminalEnvPersistenceConfig,
}
```

On-disk shape in `~/.config/scribe/config.toml`:

```toml
[terminal.env_persistence]
enabled = false
```

Default OFF. Loaded by the existing config flow. The change reaches `scribe-server` via the existing file-watcher → `ConfigReloaded` IPC. No server restart required.

## Settings → server flow

1. User clicks the toggle in `settings.html`.
2. The toggle's click handler in `settings.js` is intercepted (this is the only setting that requires a preflight). On enable attempt the handler does NOT immediately flip the visual state — it sends `ClientMessage::EnvPreflight` and waits for `ServerMessage::EnvPreflightResult`.
3. On `ok: true` the handler flips the toggle to ON and calls the existing `sendChange("terminal.env_persistence.enabled", true)`, which sends the standard `setting_changed` IPC.
4. `apply.rs::apply_terminal_key` gains an arm matching `"terminal.env_persistence.enabled"` that deserializes the bool, writes to `config.terminal.env_persistence.enabled`, and saves the config via the existing `save_config` path.
5. The file watcher → `ConfigReloaded` IPC delivers the change to the server.
6. On enabled-transition the server initializes the `env_store` machinery for active sessions (debounced persist timers + envelope I/O). On disabled-transition the server stops all per-session persist timers and deletes every envelope under `restore/env/`.

On a disable click the preflight is **not** run; disable is unconditional and immediate.

## Settings HTML row (insertion point: end of Terminal → General, immediately after the "Enhanced keyboard protocol (Kitty)" toggle)

```html
<div class="setting-row">
  <div class="setting-info">
    <div class="setting-label">Persist Environment</div>
    <div class="setting-desc">Capture and restore terminal environment across cold restart (requires OS secret store)</div>
  </div>
  <div class="toggle off" data-key="terminal.env_persistence.enabled" id="env-persistence-toggle">
    <div class="toggle-knob"></div>
  </div>
</div>
<div class="setting-row" id="env-persistence-error-row" style="display: none;">
  <div class="setting-error" id="env-persistence-error"></div>
</div>
```

Styling for `.setting-error` (add to `settings.css`):

```css
.setting-error {
  color: #f87171;
  font-size: 0.9rem;
  padding: 8px 12px;
  border-radius: 4px;
  background: rgba(248, 113, 113, 0.1);
  border-left: 3px solid #f87171;
  line-height: 1.4;
}
```

Behavior:

- The error row is hidden by default.
- On a failed preflight the row is populated with the message for the returned `PreflightError` variant and revealed.
- Auto-dismiss after 6 s. Clicking the toggle again dismisses the row immediately (and starts a fresh preflight if the click is an enable attempt).
- No modal, no toast, no banner.

## Preflight error message map

| `PreflightError` variant | User-facing message |
|---|---|
| `KeychainLocked` (macOS) | "macOS Keychain is locked. Open Keychain Access, unlock the login keychain, then try again." |
| `SecretServiceUnavailable` (Linux) | "Secret service is not available. Install and start a system keyring (e.g. gnome-keyring or KWallet) and ensure Scribe launched in a graphical session, then try again." |
| `KeystoreAccessDenied` | "The OS secret store denied access. Check your keychain permissions for Scribe and try again." |
| `Unknown(reason)` | "Could not access the OS secret store: {reason}. Try again, or see the docs for troubleshooting." |

All messages are 1–2 sentences, actionable, with a concrete next step.

## Status-bar surface (`crates/scribe-client/src/status_bar.rs` and `pane.rs`)

- Add `env_status: Option<EnvStatusState>` to `Pane`.
- Add a matching field on `StatusBarData`; populate from `pane.env_status` in the existing pane-data gathering path.
- In the existing command-status drawing path, when `env_status == Some(Degraded { .. })`, draw a warning glyph (`⚠`, U+26A0) immediately to the right of the command-status indicator, using the existing palette's warning slot.
- Tooltip on hover: **"Environment capture paused: keystore unavailable. Retry from Settings → Terminal → General."**
- The glyph clears when the server emits `EnvStatus { state: Active }` for that session (no client-driven retry — the user re-toggles).
- No modal, no toast, no banner.

## IPC routing of `EnvStatus` (`crates/scribe-client/src/ipc_client.rs`)

On receipt of `ServerMessage::EnvStatus { session_id, state }`:

1. Find the pane whose session matches `session_id`.
2. Update `pane.env_status = Some(state)`.
3. Trigger a redraw of the affected pane's status bar.

If no matching pane is found (e.g., the session was just closed), the event is silently dropped.

## Disable transition behavior (FR-009 + research.md R4.6)

When the user toggles the feature OFF (or when the server otherwise sees the config transition to `enabled: false`):

1. All per-session persist timers are stopped.
2. Every envelope file under `$XDG_STATE_HOME/<flavor>/restore/env/` is deleted.
3. Each session's `EnvStatus` is dropped (the client clears any warning glyph because no further `EnvStatus` events will arrive for that session).
4. No further `EnvChanged` events are processed for persistence (they are still observable in-memory but never written to disk).

## Out of scope for this contract

- Protocol message shapes are in `protocol-additions.md`.
- Hook channel / shell integration / `SCRIBE_RESTORE_ENV_DELTA_FILE` mechanics are in `hook-event-additions.md`.
- Encryption envelope format is in `data-model.md::EnvEnvelope`.
