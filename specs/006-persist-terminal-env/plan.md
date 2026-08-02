# Implementation Plan: Persist & Restore Terminal Environment Across Cold Restart

**Branch**: `006-persist-terminal-env` | **Date**: 2026-05-19 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/006-persist-terminal-env/spec.md`

## Summary

Add an opt-in, per-terminal environment-persistence feature. Detect changes to exported environment variables as they occur (via the existing structured hook channel, a new `EnvChanged` event kind emitted by each resident shell's prompt-time hook). Compute a per-session delta against a startup baseline captured after user startup and restore apply: at the integration tail for bash/nushell/PowerShell and at the first prompt for zsh/fish. Persist the delta to disk as an encrypted envelope (ChaCha20-Poly1305; AEAD data key in the OS secret store via the cross-platform `keyring` crate). On cold restart, the client passes the existing `LaunchRecord.launch_id` to the server as `env_envelope_id`; the server decrypts the envelope into a shell-specific temp file and a supported post-startup consumer applies it, preserving FR-008 precedence. Structured AI bash/zsh/fish launches consume the file after login and before `exec` but emit no baseline or prompt deltas; other AI shell kinds stage none. The feature is gated by one toggle in Settings → Terminal → General (default OFF) with a server-side preflight that refuses-with-message if the OS secret store is missing or misconfigured, and a runtime fail-safe that stops persisting (never plaintext) and surfaces a non-intrusive status-bar indicator. The existing handoff/upgrade path is unchanged — the PTY's file descriptor is preserved across handoff so env never resets there.

## Technical Context

**Language/Version**: Rust workspace (existing toolchain & edition; matches the rest of the repo).
**Primary Dependencies**: Existing — `alacritty_terminal`, `vte`, `rmp-serde`, `tokio`, `serde`, the Scribe crates (`scribe-common`, `scribe-server`, `scribe-client`, `scribe-pty`, `scribe-hook-helper`, `scribe-settings`). New — the cross-platform `keyring` crate (OS secret store wrapper) and an AEAD primitive (preferred: `ring`'s `aead::ChaCha20Poly1305`; fall back to the RustCrypto `chacha20poly1305` crate if `ring` is not already transitively present at implementation time). Versions pinned at implementation time after checking `Cargo.lock` and `crates.io`.
**Storage**: Encrypted env-delta envelopes at `$XDG_STATE_HOME/<flavor>/restore/env/<window_id>/<launch_id>.envz`, 0o700 dir / 0o600 file, atomic write-temp + rename, retained on crash and cleared on clean shutdown — same lifecycle as the existing per-window crash-recovery restore data. Per-envelope AEAD data key in the OS secret store (macOS login Keychain; Linux Secret Service `login` collection), namespaced by the install flavor so stable and `scribe-dev` cannot collide.
**Testing**: Manual quickstart per user story (constitution principle II default — `spec.md` QR-002 explicitly does not request automated tests). Targeted Rust unit tests are added only for high-risk isolated units where a silent regression would be dangerous: delta computation against baseline, exclusion-set application, envelope round-trip seal/open, and `PreflightError` mapping. No new integration harness.
**Target Platform**: `scribe-server` + `scribe-client` + `scribe-settings` on Linux (Wayland/X11 under `systemd --user`, requires a graphical session D-Bus or a system keyring backend) and macOS (launchd). Windows is out of scope (matches the existing two-platform Scribe distribution).
**Project Type**: Multi-crate Rust workspace (desktop terminal + daemon). No new top-level crate; the feature extends existing crates.
**Performance Goals**: Per-prompt hook overhead < 20 ms (well under any perceptible-latency threshold for human-paced commands). Persistence writes debounced/coalesced at 100 ms per session. Restore adds < 50 ms per terminal to ready time (under the 100 ms cap in PR-001). Measurement plan codified in `quickstart.md`.
**Constraints**: Encryption-at-rest mandatory when enabled — no plaintext fallback at enable time, at persist time, or after a runtime keystore failure. Default OFF. Preflight refuses-with-message on missing prerequisites. No live `scribe-server` restart required to enable or apply the feature (config field is applied via the existing `ConfigReloaded` flow; behavior changes apply at next session-create or restore). Handoff/`HandoffState` is **not** modified.
**Scale/Scope**: Up to 256 concurrent sessions (existing session-manager cap). Per-value cap 64 KiB; per-terminal cap 512 KiB; global cap 10 MiB. Oversized values degrade to skipped (FR-014, SC-007). Typical user-env footprint is 20–100 vars; the caps leave ~100× headroom.

All Technical Context items are resolved by Phase 0 (see `research.md`). No remaining NEEDS CLARIFICATION.

## Constitution Check

*GATE 1 (pre-research): PASS. GATE 2 (post-design): PASS. Both re-evaluated below; nothing changed during design.*

- **Code Quality** — **PASS**. The new `env_store` submodule of `scribe-server` is the single new abstraction; it owns encrypted envelope I/O, the AEAD wrapper, and the keystore handle. All other changes are surgical extensions of existing modules: one additive `HookEventKind::EnvChanged` variant (named MessagePack with `#[serde(default)]`); one additive `env_envelope_id: Option<String>` field on `CreateSession` (with `#[serde(default)]`); two new `Server`/`ClientMessage` variants for preflight; one new `ServerMessage::EnvStatus`; one nested `TerminalEnvPersistenceConfig` field on `TerminalConfig` (with `#[serde(default)]`); one new settings row in Terminal → General matching the directly-preceding row's pattern. No duplicated protocol/config parsing, no parallel persistence path, no cross-cutting helper. New crate deps (`keyring`, possibly an AEAD primitive) are confined to `env_store`.
- **Testing Strategy** — **PASS (with documented deferral)**. Each user story has an independent manual verification path in `quickstart.md`. The spec did not request automated tests; per constitution principle II the plan defers broad automation to manual quickstart with rationale. Surgical Rust unit tests are added only for delta computation, exclusion-set application, envelope round-trip, and `PreflightError` mapping. These are easy to isolate and high-risk if silently wrong (e.g., persisting the wrong delta, or surfacing the wrong preflight message).
- **User Experience Consistency** — **PASS**. The only new user-visible surfaces are (a) one toggle in the existing Terminal → General Settings section, immediately following — and matching the row pattern of — the feature-005 "Enhanced keyboard protocol (Kitty)" toggle, and (b) one warning glyph in the existing status-bar widget when runtime degradation occurs, reusing the existing palette warning slot and tooltip pattern. Restore itself remains silent and automatic, consistent with how window layout / CWD / AI state already come back after a restart. Preflight errors appear inline next to the toggle (no modal). No toast spam. No surprising auto-toggle behavior.
- **Performance** — **PASS**. Measurable budgets stated: < 20 ms per-prompt hook overhead, 100 ms debounce, < 50 ms restore add per terminal; size caps 64 KiB / 512 KiB / 10 MiB. Measurement plan in `quickstart.md` uses a 5-iteration warm-up + 200-iteration A/B wall-clock comparison for prompt latency, and a 5-trial stopwatch measurement of restore-to-first-prompt against an empty-envelope baseline.
- **Operational Safety** — **PASS**. Default OFF; preflight refuses enable when prerequisites are missing; runtime path fails safe (never plaintext); no live-server restart required. Worktree changes preserved (this plan touches only feature-scoped files). `lat.md/` updates planned and enumerated: `lat.md/server.md` (Sessions, Hook Channel, Shell Integration), `lat.md/client.md` (Restore Pipeline), `lat.md/settings.md` (Terminal page row). `lat check` runs before completion. Protocol/persistence changes carry explicit compatibility decisions: every new field is additive, named MessagePack with `#[serde(default)]`; no protocol version bump; `HANDOFF_VERSION` is unchanged (handoff is not touched); the on-disk envelope format carries an explicit `version: u8 = 1` byte for future evolution.

No violations identified at either gate → **Complexity Tracking is intentionally empty**.

## Project Structure

### Documentation (this feature)

```text
specs/006-persist-terminal-env/
├── plan.md                                 # This file
├── research.md                             # Phase 0 — decisions, agent reports, reconciliations
├── data-model.md                           # Phase 1 — entities, validation, transitions, ownership
├── quickstart.md                           # Phase 1 — per-story manual verification + perf check
├── contracts/                              # Phase 1 — boundary contracts
│   ├── protocol-additions.md
│   ├── hook-event-additions.md
│   └── config-and-settings-ui.md
├── checklists/
│   └── requirements.md                     # From /speckit-specify
├── spec.md                                 # From /speckit-specify
└── tasks.md                                # Phase 2 — /speckit-tasks (NOT created here)
```

### Source Code (repository root — existing workspace, no new crates)

```text
crates/
├── scribe-common/src/
│   ├── config.rs            # +TerminalEnvPersistenceConfig (nested on TerminalConfig, #[serde(default)])
│   ├── protocol.rs          # +EnvPreflight (client→server), +EnvPreflightResult, extend CreateSession with env_envelope_id, +EnvStatus
│   └── hook.rs              # +HookEventKind::EnvChanged { added, removed, baseline_ready }
├── scribe-server/src/
│   ├── env_store/           # NEW module — encrypted persistence
│   │   ├── mod.rs
│   │   ├── envelope.rs      # AEAD seal/open over MessagePack-named TerminalEnvDelta + versioned binary header
│   │   ├── keystore.rs      # `keyring` wrapper, flavor-aware identifiers, preflight, PreflightError
│   │   ├── delta.rs         # Delta computation vs StartupBaseline; ExclusionSet application; size caps
│   │   └── store.rs         # On-disk layout, atomic write-temp + rename, lifecycle (create/update/delete)
│   ├── session_manager.rs   # +baseline capture on first EnvChanged{baseline_ready}; +inject SCRIBE_RESTORE_ENV_DELTA_FILE on restore-driven spawns
│   ├── hook_ingress.rs      # +EnvChanged translation → in-memory delta update + debounced persist
│   ├── ipc_server.rs        # +handle EnvPreflight; +propagate env_envelope_id in CreateSession; +emit EnvStatus on degrade/recover; +delete envelopes on session/window close and on feature-disable transition
│   └── (no handoff changes)
├── scribe-client/src/
│   ├── restore_replay.rs    # +populate env_envelope_id = launch_id when re-issuing a LaunchRecord
│   ├── ipc_client.rs        # +route EnvStatus to the owning pane, trigger redraw
│   ├── pane.rs              # +env_status: Option<EnvStatusState>
│   └── status_bar.rs        # +render warning glyph when env_status == Degraded
├── scribe-hook-helper/src/
│   └── main.rs              # +--event env-delta subcommand with --added-json / --removed-json / --baseline-ready flags
├── scribe-settings/src/
│   ├── assets/
│   │   ├── settings.html    # +"Persist Environment" toggle row after Kitty toggle; +inline error row
│   │   ├── settings.js      # +preflight-then-save interception for the new toggle; +error-row population & auto-dismiss; +EnvStatus listener (optional)
│   │   └── settings.css     # +.setting-error styling (red, semi-transparent bg, left border)
│   └── apply.rs             # +"terminal.env_persistence.enabled" match arm; +preflight invocation before save
└── dist/                    # Shell integration scripts
    ├── bash                 # post-rc tail: apply restore, baseline, then PROMPT_COMMAND delta hook
    ├── zsh                  # first precmd after rc: apply restore + baseline before recurring precmd delta hook
    ├── fish                 # first fish_prompt after config: apply restore + baseline before recurring delta hook
    ├── nu                   # post-startup autoload tail: JSON restore + baseline, then pre_prompt delta hook
    └── pwsh                 # post-profile script tail: .ps1 restore + baseline, then prompt wrapper
```

**Structure Decision**: Extend the existing Rust workspace; no new crate. Encrypted persistence is encapsulated in a new `env_store` submodule of `scribe-server` (the owner of PTY lifecycle, hook ingress, and existing on-disk persistence). Change detection reuses the existing structured hook channel — no new IPC transport. Restore reuses the existing client launch-record replay pipeline — only the new `env_envelope_id` field is added on `CreateSession`. Settings reuses the existing webview configuration pattern, with one toggle row and one inline error row. This preserves constitution principle I (clear crate boundaries) and principle III (consistent UX).

## Complexity Tracking

No constitution violations identified at either gate. Table intentionally empty.

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| (none) | | |
