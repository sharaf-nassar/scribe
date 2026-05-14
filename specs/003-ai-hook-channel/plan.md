# Implementation Plan: AI Hook Channel

**Branch**: `003-ai-hook-channel` | **Date**: 2026-05-13 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/003-ai-hook-channel/spec.md`

## Summary

Replace the dead OSC-over-`/dev/tty` signaling path used by AI tool hook subprocesses (and the Claude statusline subprocess) with a structured IPC channel served by `scribe-server`. The channel reuses the existing length-prefixed msgpack framing already used by the client IPC (`crates/scribe-common/src/framing.rs`) over a shared Unix domain socket, with new transient `ClientMessage::Hook*` variants. Provider-specific hook adapter scripts become thin: each translates the AI tool's hook stdin JSON into one call to a shared emitter helper that connects, writes one event, and exits 0 unconditionally. The Stop-hook idle/waiting heuristic moves out of duplicated shell scripts into one Rust classifier in `scribe-server`. Scribe identifies each PTY via a new `SCRIBE_SESSION_ID` env var injected at PTY spawn (the existing `SessionId` UUID); adapters cannot run outside Scribe because they discover the channel only via that env var, satisfying FR-003 silent no-op. OSC 1337 parsing for AI hook-originated events is deleted from `scribe-pty/src/metadata.rs`; the shell preexec pre-arm sentinel parsing is retained per FR-023.

## Technical Context

**Language/Version**: Rust 2024 edition, MSRV 1.87 (per workspace `Cargo.toml:8-10`). Adapter scripts are POSIX `/bin/sh`. The shared emitter helper is a small Rust binary shipped from the workspace (see Research Decision 1 in `research.md`).
**Primary Dependencies**: `tokio` (async runtime), `serde` + `rmp-serde` (msgpack framing — already a workspace dependency used by `scribe-common::framing`), `nix`/`libc` for the helper's Unix-socket connect (or `tokio::net::UnixStream` if we use a full Tokio binary; see research.md decision on helper packaging).
**Storage**: None. Hook events are transient. Existing per-`SessionId` `AiProcessState` records in `LiveSessionRegistry` (`crates/scribe-server/src/ipc_server.rs:106`) are the only mutated state.
**Testing**: `cargo test` for Rust unit/integration tests (server-side classifier, ingress dispatch, env-var injection). The `crates/scribe-test` harness (`crates/scribe-test/src/ipc.rs:18-42`) for end-to-end "spawn server → emit hook event → assert `ServerMessage::AiStateChanged` reaches the client" assertions. Offline shell regressions in `tests/install/` (new file `tests/install/ipc-hook-regressions.sh` modeled after the existing `postinst-regressions.sh` and `codex-context-regressions.sh`).
**Target Platform**: Linux (primary) and macOS (per existing `dist/macos/build-dmg.sh` packaging). Windows is not a Scribe target.
**Project Type**: Multi-crate Rust workspace; this feature touches `scribe-common`, `scribe-server`, `scribe-pty`, plus a new helper crate, plus `dist/` shell adapters.
**Performance Goals**: ≤200 ms p95 from hook fire to UI repaint (Spec SC-002). Realistic budget for the channel itself is ≤10 ms p95 (subprocess startup + Unix socket connect + write + server consume), leaving 190 ms of headroom for client-side redraw.
**Constraints**:
- Adapter scripts and the shared helper MUST NOT write to stdout or stderr (FR-008, FR-009).
- Adapter scripts and the helper MUST exit 0 in every code path, regardless of channel state (FR-007).
- Helper MUST NOT open `/dev/tty` (FR-010).
- Helper emission step (connect + write + close) MUST complete inside a short, bounded budget (FR-012). The plan sets this to 100 ms.
- No backward compatibility / no fallback channel (Spec Clarifications + FR-020 through FR-022).
**Scale/Scope**: Three supported providers (Claude Code, Codex, Auggie). Event rate is bounded by hook event frequency: typically 1–20 events per minute per active AI session under interactive use; bursts up to ~100/min during a tool-call-heavy turn. Concurrent active AI sessions: usually 1–4 panes in a single Scribe instance. Total Scribe IPC traffic is dominated by `PtyOutput`, not hook events.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

The project constitution (`.specify/memory/constitution.md`) is the unfilled speckit template with placeholder principle names ("[PRINCIPLE_1_NAME]", etc.) and no enacted articles. No constitutional gates are defined.

Project conventions that act as de-facto gates in lieu of constitutional articles, all sourced from `CLAUDE.md` and existing code/lat.md style:

- **`lat.md/` must stay in sync with code** — every code section touched here must have its `lat.md/` page updated, and `lat check` must pass before merge. Tracked as a final implementation task (see `tasks.md` once generated).
- **No `#[allow]`/`#[expect]` lint suppressions outside the allow-listed baseline** (`tools/check-no-new-lint-suppressions.sh`). New code must not add lint suppressions.
- **Server restarts require explicit user approval** (CLAUDE.md). The plan's verification step does not auto-restart the server.

**Gate result**: PASS by default (no articles to violate). Pre-design and post-design re-check both trivially PASS.

## Project Structure

### Documentation (this feature)

```text
specs/003-ai-hook-channel/
├── plan.md                          # This file
├── research.md                      # Phase 0 — Decisions on packaging, transport, naming
├── data-model.md                    # Phase 1 — HookEvent variants, payload fields, mapping table
├── quickstart.md                    # Phase 1 — Manual verification per user story
├── contracts/
│   ├── wire-protocol.md             # New ClientMessage variants on the server socket
│   ├── env-vars.md                  # SCRIBE_HOOK_SOCK, SCRIBE_SESSION_ID semantics
│   └── helper-cli.md                # Shared emitter helper invocation contract
└── checklists/
    └── requirements.md              # (Existing — written by /speckit-specify)
```

### Source Code (repository root)

```text
crates/
├── scribe-common/
│   └── src/
│       └── protocol.rs              # ADD: ClientMessage::HookEvent { … } + sub-payload enum
│
├── scribe-server/
│   ├── src/
│   │   ├── ipc_server.rs            # MODIFY: dispatch ClientMessage::HookEvent into hook_ingress
│   │   ├── session_manager.rs       # MODIFY: inject SCRIBE_HOOK_SOCK + SCRIBE_SESSION_ID at :538
│   │   ├── shell_integration.rs     # MODIFY: propagate the two env vars at :72
│   │   ├── hook_ingress.rs          # NEW: HookEvent → MetadataEvent translation, FR-013 routing
│   │   └── stop_classifier.rs       # NEW: server-side idle/waiting heuristic (FR-013a)
│   └── Cargo.toml                   # MODIFY: deb assets list (add helper binary; drop obsolete shell scripts)
│
├── scribe-pty/
│   └── src/
│       └── metadata.rs              # MODIFY: REMOVE AI hook OSC parsing per FR-022; keep pre-arm
│
└── scribe-hook-helper/              # NEW CRATE: the shared emitter binary
    ├── Cargo.toml
    └── src/
        └── main.rs                  # ~150 lines: parse args, build HookEvent, msgpack-frame, connect, write, exit 0

dist/
├── ai-hook-claude.sh                # NEW: thin adapter — read stdin, exec scribe-hook-helper
├── ai-hook-codex.sh                 # NEW: thin adapter
├── ai-hook-auggie.sh                # NEW: thin adapter
├── ai-hook-statusline.sh            # NEW: thin adapter replacing scribe-claude-statusline.sh
├── setup-claude-hooks.sh            # REWRITE: register the new adapter, remove all printf>/dev/tty
├── setup-codex-hooks.sh             # REWRITE
├── setup-auggie-hooks.sh            # REWRITE
├── codex-hook-common.sh             # DELETE
├── codex-prompt-state.sh            # DELETE
├── codex-task-label.sh              # DELETE (replaced by adapter)
├── detect-claude-question.sh        # DELETE (replaced by stop_classifier.rs)
├── detect-codex-question.sh         # DELETE
├── detect-codex-context.sh          # DELETE (replaced by adapter)
├── scribe-claude-statusline.sh      # DELETE (replaced by ai-hook-statusline.sh)
└── debian/postinst                  # MODIFY: install hook helper binary; drop obsolete scripts

tests/
└── install/
    └── ipc-hook-regressions.sh      # NEW: offline regressions for the new adapter scripts + helper

lat.md/
├── architecture.md                  # UPDATE: crate map adds scribe-hook-helper
├── pty.md                           # UPDATE: REMOVE "OSC 1337 — AI State / Prompt / Context Refresh"
│                                    #         and "Claude Picker Truncation Filter" notes referring
│                                    #         to AI hook OSCs; KEEP "OSC 1337 — Pre-Arm Sentinel".
├── server.md                        # UPDATE: ADD "Hook Channel" section under Server; remove
│                                    #         OSC-driven hook ingestion references
├── protocol.md                      # UPDATE: document new ClientMessage::HookEvent variants
├── common.md                        # UPDATE: AI State entry; note source is hook channel not OSC
└── test.md                          # UPDATE: new regression harness entry
```

**Structure Decision**: Multi-crate Rust workspace augmented by a new `scribe-hook-helper` binary crate and four replacement shell adapters. The new code lands in `scribe-server` (ingress + classifier), `scribe-common` (protocol variant), and `scribe-hook-helper` (emitter). The bulk of the diff is **deletions**: six shell scripts removed, OSC 1337 AI-hook parsing removed from `scribe-pty/src/metadata.rs`, the heuristic shell scripts replaced by ~80 lines of Rust. The substantive new surface is one ingress module, one classifier module, one CLI helper, and three (plus one statusline) ~10-line adapter scripts.

## Complexity Tracking

> **Fill ONLY if Constitution Check has violations that must be justified**

No constitutional violations to track. The constitution is the unfilled template; no gates apply. The most likely "complexity" question is "why a new helper binary instead of inlining the emit logic into each shell adapter" — that decision is justified in `research.md` Decision 1 and rests on FR-008/FR-009 (no stdout/stderr noise), FR-012 (bounded latency), and the cost of correctly implementing a length-prefixed msgpack write from POSIX shell.
