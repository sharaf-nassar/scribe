# Data Model: Paste Confirmation (Multiline / Control-Character)

This feature is client-side and adds no protocol/persistence schema. The
"entities" are in-memory client types plus one additive config field. No
database, no wire format.

## Config field (persisted)

### `TerminalConfig.paste_confirmation: bool`

- **Location**: `crates/scribe-common/src/config.rs`, `TerminalConfig`.
- **Serde**: `#[serde(default)]` → defaults to `false` when absent.
- **TOML key**: `terminal.paste_confirmation`.
- **Default**: `false` (feature off — opt-in).
- **Semantics**: when `true`, the client applies the paste gate (FR-001/FR-002);
  when `false`, the paste path is unchanged and the classifier is never invoked.
- **Compatibility**: additive; older/newer configs interoperate (research R7).

## In-memory types (client)

### `PasteRisk`

The pure classification result.

| Field | Type | Meaning |
|-------|------|---------|
| `has_line_break` | `bool` | content contains `\n` or `\r` (incl. a single trailing newline) |
| `has_control` | `bool` | content contains a control/escape byte that is NOT tab/LF/CR (C0 except `\t`, DEL, or C1) |

- Produced by `classify_paste(text: &str) -> Option<PasteRisk>`: returns
  `Some` iff `has_line_break || has_control`, else `None`.
- Pure, allocation-free, O(n). The unit-test seam (QR-002).
- Drives the reason line in the dialog (e.g. "12 lines · 3 control characters").

### `PasteTarget` (existing — reused)

Already defined at `crates/scribe-client/src/main.rs:262`. Reused unchanged as
the parked target so confirm delivers to the right pane with the right
wrapping.

| Field | Type | Meaning |
|-------|------|---------|
| `session_id` | `SessionId` | destination PTY session of the focused pane |
| `bracketed` | `bool` | focused pane has `TermMode::BRACKETED_PASTE` set, captured at request time |

### `PasteConfirmationDialog`

Modal state; cloned from `DisallowedSchemeDialog`. Parks the paste while
awaiting the user's choice.

| Field | Type | Meaning |
|-------|------|---------|
| `content` | `String` | the raw, unmodified paste text to resume on confirm |
| `target` | `PasteTarget` | parked destination + bracketed flag (research R2) |
| `risk` | `PasteRisk` | classification, for the reason line |
| `focused` | `ButtonIndex` | default `Cancel` (index 0) |
| `hovered` | `Option<usize>` | mouse-hover button index |
| `button_rects` | `[Rect; 2]` | cached hit rects |

- Stored on `App` as `paste_confirmation_dialog: Option<PasteConfirmationDialog>`.
- `body_lines()` builds: a reason line, a blank, then the caret-escaped,
  per-line-truncated preview (≤ `MAX_PREVIEW_LINES`, each ≤ `MAX_PREVIEW_COLS`),
  optionally a `… (+N more lines)` summary (research R4).
- Released (`take()`) when resolved → no retained-clipboard growth (PR-001).

### `PasteConfirmationAction`

| Variant | Effect |
|---------|--------|
| `Paste` | resume: send `content` via `try_send_single_paste`/`send_chunked_paste` with the parked `target` (bracketed wrap + chunk applied by the tail), bypassing the gate |
| `Cancel` | drop the dialog; send nothing (0 bytes to PTY) |

- `Cancel` is default focus and Esc-bound; `Enter` activates the focused button.

## State transitions (paste gate)

```text
paste requested (keybinding | context-menu | middle-click)
   → fetch text → send_paste_data(text)
       → target = prepare_paste_target()        (resolves bracketed)
       → IF !config.paste_confirmation  OR  target.bracketed
              OR classify_paste(text) == None
            → send immediately (unchanged path)
         ELSE
            → park (content, target, risk) in PasteConfirmationDialog; redraw
                → user: Paste  → resume send (bypass gate)
                → user: Cancel/Esc → drop (no send)
```

Decision truth table (when content is non-empty): see
`contracts/paste-confirmation.md`.
