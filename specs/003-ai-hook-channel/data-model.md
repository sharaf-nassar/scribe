# Data Model: AI Hook Channel

**Phase 1 output for [plan.md](./plan.md). Maps the spec's Key Entities to concrete types and shows how they thread through existing Scribe state.**

---

## Entity overview

| Entity | Crate | Lifetime | Notes |
|---|---|---|---|
| `HookEvent` | `scribe-common::hook` (new module) | Per-event message on the wire | Single struct wrapping routing fields + payload |
| `HookEventKind` | `scribe-common::hook` (new module) | Variant inside `HookEvent` | One variant per event the helper can emit |
| `ClientMessage::HookEvent(...)` | `scribe-common::protocol` | Wire frame | New top-level variant on the existing enum |
| `AiProvider` | `scribe-common::ai_state` (existing) | — | Reused unchanged: `ClaudeCode`, `CodexCode`, `Auggie` |
| `AiState` | `scribe-common::ai_state` (existing) | — | Reused unchanged: `IdlePrompt`, `Processing`, `WaitingForInput`, `PermissionPrompt`, `Error` |
| `SessionId` | `scribe-common::ids` (existing) | Per PTY | UUID-v4 minted at `session_manager.rs:298`; survives AI-tool restart in same pane |
| `AiProcessState` | `scribe-common::ai_state` (existing) | Per `SessionId` × provider | Mutated by hook events; broadcast as `ServerMessage::AiStateChanged` |

---

## `HookEvent` — the wire message

```rust
// crates/scribe-common/src/hook.rs (new module)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    /// Routing key: which Scribe PTY this event belongs to.
    /// Read by the helper from `SCRIBE_SESSION_ID` env var.
    pub session_id: SessionId,

    /// Which AI tool produced this event. Claimed by the helper from its
    /// `--provider` CLI argument; the server uses it for vocabulary routing
    /// (e.g. task-label channel selection) and drops events whose provider is
    /// not recognized by the current build (FR-014).
    pub provider: AiProvider,

    /// Event payload.
    pub kind: HookEventKind,
}
```

**Fields**:

- `session_id` (mandatory): `SessionId`. Routes the event to the right pane via `LiveSessionRegistry` (`ipc_server.rs:106`). Stable across AI-tool restarts in the same pane.
- `provider` (mandatory): `AiProvider`. Identifies which provider emitted; used by the server for provider-specific routing (e.g. `CodexTaskLabelChanged` vs `TaskLabelChanged`).
- `kind` (mandatory): `HookEventKind` — see below.

**Validation rules** (enforced server-side in `hook_ingress::handle`):

- `session_id` MUST match a live session in `LiveSessionRegistry`. Mismatch → drop event silently.
- `provider` MUST be a value the current build recognizes. Mismatch → drop event silently (FR-014).
- `kind` MUST deserialize to a known variant; unknown variants are rejected by msgpack-named deserialization automatically.

---

## `HookEventKind` — event payload variants

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum HookEventKind {
    /// Adapter observed a structured state transition (PreToolUse:AskUserQuestion,
    /// Notification:permission_prompt, etc.) and is reporting the new AiState
    /// directly. The server applies it without running the classifier.
    StateChanged {
        state: AiState,
        conversation_id: Option<String>,
    },

    /// Adapter observed the AI tool's Stop hook firing. Carries the assistant's
    /// last-message text; the server's stop_classifier maps it to either
    /// AiState::IdlePrompt or AiState::WaitingForInput (FR-013a).
    SessionStopped {
        last_message: String,                 // <= LAST_MESSAGE_CAP_BYTES
        conversation_id: Option<String>,
    },

    /// AI session ended. Clears AiProcessState for this (session_id, provider).
    StateCleared,

    /// User submitted a prompt. Feeds the prompt-bar; bounded at PROMPT_TEXT_CAP_BYTES.
    PromptReceived {
        text: String,                         // <= PROMPT_TEXT_CAP_BYTES
        conversation_id: Option<String>,
    },

    /// Tab-strip task label updated. Bounded at TASK_LABEL_CAP_BYTES.
    TaskLabelChanged {
        label: String,                        // <= TASK_LABEL_CAP_BYTES
    },

    /// Tab-strip task label cleared without changing AiState.
    TaskLabelCleared,

    /// Context-window fill percentage (0..=100). Out-of-range values clamp.
    ContextChanged {
        fill_percent: u8,                     // server clamps to 0..=100
    },
}
```

**Caps** (constants defined in `scribe-common::hook`):

| Constant | Value | Source |
|---|---|---|
| `LAST_MESSAGE_CAP_BYTES` | 16 384 | New. Sized to comfortably hold the heuristic's "last ~20 non-empty lines" window. |
| `PROMPT_TEXT_CAP_BYTES` | 256 | Matches existing `OSC 1337 — Prompt Text` cap (`lat.md/pty.md:79-86`). |
| `TASK_LABEL_CAP_BYTES` | 256 | Matches existing `OSC 1337` task-label parser cap. |

**Field validation**:

- `state` (in `StateChanged`): MUST be a valid `AiState` value. Adapters can only emit `WaitingForInput` (via `PreToolUse:AskUserQuestion`), `PermissionPrompt` (via `Notification:permission_prompt`), `Error` (via `Notification:error`), or `Processing` (via `UserPromptSubmit`). `IdlePrompt` only flows via `SessionStopped` after classification.
- `conversation_id`: opaque string from the AI tool; truncated at 256 bytes by the server when present (matches existing OSC parser behavior).
- `last_message`: server truncates at `LAST_MESSAGE_CAP_BYTES` if oversized (FR-016).
- `text`, `label`: server truncates at the respective cap.
- `fill_percent`: server clamps to 0..=100. Out-of-range values are not rejected (matches existing `OSC 1337 — AI Context Refresh` behavior at `metadata.rs:parse_named_ai_context`).

---

## `ClientMessage::HookEvent` — wire-protocol wrapper

```rust
// crates/scribe-common/src/protocol.rs (modification — new variant on existing enum)

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    // ... existing variants (KeyInput, Resize, …, ListReleases) …

    /// Transient one-shot connection from a hook subprocess. Carries one
    /// HookEvent. Server consumes, dispatches via hook_ingress, drops the
    /// connection. No Welcome required, no reply expected by default.
    HookEvent(HookEvent),
}
```

The variant follows the exact pattern of the existing transient `CheckForUpdates` and `ListReleases` variants (`ipc_server.rs:519-533`).

---

## Lifecycle: hook fire → UI repaint

```
AI tool hook subprocess
        │
        │  exec dist/ai-hook-<provider>.sh < stdin-json
        ▼
Adapter script (POSIX sh)
        │  exec scribe-hook-helper --provider=X --event=Y --field=…
        ▼
scribe-hook-helper (Rust binary)
        │  read SCRIBE_SESSION_ID, SCRIBE_HOOK_SOCK from env
        │  build HookEvent { session_id, provider, kind }
        │  framing::write_message(ClientMessage::HookEvent(...))
        ▼
scribe-server accept loop  (ipc_server.rs:357)
        │  decode ClientMessage::HookEvent → dispatch transient branch
        ▼
hook_ingress::handle  (new module)
        │  validate session_id, provider
        │  classify if SessionStopped (delegate to stop_classifier)
        │  translate to MetadataEvent
        ▼
existing send_metadata_event  (ipc_server.rs:2615)
        │  mutate AiProcessState via persist_session_metadata / merge_partial_ai_state
        │  broadcast ServerMessage::AiStateChanged (or related)
        ▼
scribe-client receives ServerMessage
        │  update pane state, tab indicator, prompt bar
        ▼
GPU repaint  (scribe-client::App::draw)
```

The new path replaces the OSC byte ingress (red below) with a Unix-socket ingress (green). Everything **downstream** of `send_metadata_event` is unchanged.

```
BEFORE (deleted):  PTY bytes ─▶ osc_interceptor ─▶ metadata.rs::parse_named_ai_state ─▶ MetadataEvent ─▶ send_metadata_event
AFTER  (new):      Unix sock ─▶ hook_ingress      ─▶ stop_classifier  (only for SessionStopped) ─▶ MetadataEvent ─▶ send_metadata_event
```

---

## State transitions (the existing AI state machine, unchanged)

The `AiState` enum has five values; transitions are driven by `HookEventKind` variants as follows:

| From | Event | To |
|------|-------|----|
| any | `StateChanged { state: Processing }` (from `UserPromptSubmit` adapter path) | `Processing` |
| `Processing` | `StateChanged { state: WaitingForInput }` (from `PreToolUse:AskUserQuestion`) | `WaitingForInput` |
| `WaitingForInput` | `StateChanged { state: Processing }` (from `PostToolUse:AskUserQuestion`) | `Processing` |
| any | `StateChanged { state: PermissionPrompt }` (from `Notification:permission_prompt`) | `PermissionPrompt` |
| any | `StateChanged { state: Error }` (from `Notification:error`) | `Error` |
| `Processing` | `SessionStopped` → classifier → `WaitingForInput` | `WaitingForInput` |
| `Processing` | `SessionStopped` → classifier → `IdlePrompt` | `IdlePrompt` |
| any | `StateCleared` | (no live state; equivalent to `AiStateCleared` today) |

Non-state-changing events:

| Event | Effect |
|-------|--------|
| `PromptReceived` | Updates pane prompt-bar; does **not** change AiState (matches existing `OSC 1337 — Prompt Text` behavior) |
| `TaskLabelChanged` | Updates tab-strip label; does **not** change AiState |
| `TaskLabelCleared` | Clears tab-strip label; does **not** change AiState |
| `ContextChanged` | Updates `AiProcessState::context` only; does **not** change `AiState` (matches existing `send_ai_context_change` at `ipc_server.rs:2509-2532`) |

---

## Mapping table: `HookEventKind` → existing server output

The hook ingress translates each `HookEventKind` into the existing `MetadataEvent` types so the unmodified `send_metadata_event` (`ipc_server.rs:2615-2657`) and `convert_metadata_event` (`:2664-2713`) pipeline handles broadcast to clients.

| `HookEventKind` | Server-side translation | Existing downstream `ServerMessage` |
|---|---|---|
| `StateChanged { state, conversation_id }` | `MetadataEvent::AiStateChanged { provider, state, conversation_id }` | `ServerMessage::AiStateChanged` |
| `SessionStopped { last_message, conversation_id }` | classifier → `MetadataEvent::AiStateChanged { state: IdlePrompt or WaitingForInput, … }` | `ServerMessage::AiStateChanged` |
| `StateCleared` | `MetadataEvent::AiStateCleared { provider }` | `ServerMessage::AiStateCleared` |
| `PromptReceived { text, conversation_id }` | `MetadataEvent::PromptReceived { provider, text, conversation_id }` | `ServerMessage::PromptReceived` |
| `TaskLabelChanged { label }` (provider != Codex) | `MetadataEvent::TaskLabelChanged { label }` | `ServerMessage::TaskLabelChanged` |
| `TaskLabelChanged { label }` (provider == Codex) | `MetadataEvent::CodexTaskLabelChanged { label }` | `ServerMessage::CodexTaskLabelChanged` |
| `TaskLabelCleared` (provider != Codex) | `MetadataEvent::TaskLabelCleared` | `ServerMessage::TaskLabelCleared` |
| `TaskLabelCleared` (provider == Codex) | `MetadataEvent::CodexTaskLabelCleared` | `ServerMessage::CodexTaskLabelCleared` |
| `ContextChanged { fill_percent }` | `MetadataEvent::AiContextChanged { provider, context: fill_percent }` then existing `send_ai_context_change` re-broadcast | `ServerMessage::AiStateChanged` (with updated context) |

The dual `TaskLabel` / `CodexTaskLabel` channel mirrors today's PTY parser (`metadata.rs` keeps separate variants for Codex's task-label channel). Once Auggie/Claude task labels and Codex task labels can converge into one channel (out of scope here), the split can collapse.

---

## What is **not** carried in `HookEvent`

For clarity, several fields that travel via OSC today are intentionally absent from the new channel because the new channel doesn't need them:

- **`agent` / `tool` / `model`**: optional metadata fields on `AiProcessState`. Today's OSC payload includes them; the new channel can omit them in v1 because they're not displayed in any UI surface that the indicators or prompt bar consume. Future addition is trivial — add fields to `StateChanged`.
- **`bell` event**: OSC BEL is preserved on the PTY byte path; not a hook event.
- **`prompt mark`** (`OSC 133`): emitted by shell integration in a real shell; not in scope (FR-023 retention applies to all shell-integration OSCs, not just the pre-arm sentinel).
- **`cwd` / `title`**: shell-integration sourced; out of scope.

---

## Invariants

1. **`SessionId` is the routing key**. `provider` is metadata. A hook event with mismatched session_id is dropped silently.
2. **One event per helper invocation**. The helper does not batch. Adapter scripts that need to emit two distinct events fork the helper twice (rare; usually one event per hook fire).
3. **The helper exits 0 in every branch** — connect failure, write failure, timeout, missing env vars, malformed args. All failures are silent. No retries.
4. **Server side never returns an error to the hook subprocess**. Even validation failures (unknown session_id, oversize payload) are handled by truncate-or-drop, never by responding with an error message; the hook subprocess has already moved on.
5. **`HookEvent` is forward-compatible**. The tagged-msgpack representation tolerates new `HookEventKind` variants in newer Scribe builds connected by older helpers, and vice versa, as long as the wire format respects the existing serde-msgpack rules used throughout `scribe-common::protocol`.
