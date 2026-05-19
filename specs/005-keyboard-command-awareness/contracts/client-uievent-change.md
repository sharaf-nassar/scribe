# Contract: Client `UiEvent::PromptMark` Change (internal)

**Not a wire contract.** `UiEvent` is a `scribe-client`-internal enum delivered from the IPC
reader task to the UI loop; it is never serialized. Documented here because it is the precise
seam where the feature's data currently dies.

## Before

```text
ServerMessage::PromptMark { session_id, kind, click_events, exit_code }   # wire — already complete
        │
        ▼  ipc_client.rs dispatch
UiEvent::PromptMark { session_id, kind, click_events }                    # exit_code DROPPED here
```

## After

```text
UiEvent::PromptMark { session_id, kind, click_events, exit_code: Option<i32> }   # forwarded
```

- `ServerMessage::PromptMark` (`scribe-common/src/protocol.rs`) is **unchanged** — it already
  carries `exit_code: Option<i32>` over msgpack. No protocol/version change.
- Change is limited to: the `UiEvent::PromptMark` variant definition and the one dispatch arm
  that destructures and re-emits it (stop dropping `exit_code`), plus every match site that
  destructures `UiEvent::PromptMark` (compiler-enforced exhaustiveness ⇒ no silent miss).

## Consumer obligation

`handle_prompt_mark` consumes `exit_code` to drive the `CommandRecord` state machine
(`A` opens `Unknown`; `D` resolves `0→Success`, `≠0→Failure`, `None→Unknown`). `exit_code`
MUST be treated as advisory: absent/`None` ⇒ `Unknown`, **never** `Failure` (FR-012/SC-006).

## Compatibility

- No persisted-state impact: command records are ephemeral per-attach and are not written to
  the cold-restart snapshot; on reattach they start empty (non-misleading per FR-014).
- No cross-binary concern: server and client need not be co-versioned for this change (the
  wire field already exists in shipped protocol).
