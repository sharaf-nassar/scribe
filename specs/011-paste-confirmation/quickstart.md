# Quickstart / Manual Verification: Paste Confirmation

Per QR-002 / Constitution II, each user story is verified by an independent
manual scenario (no automated tests added). Run against a dev build of the
client. The pure `classify_paste` function is the unit-test seam if automated
coverage is later requested.

## Setup

1. Build/run the client per the project's standard dev flow (do NOT restart the
   live server without explicit approval — Constitution / CLAUDE.md).
2. Enable the feature: Settings → Terminal → toggle **"Confirm risky pastes"**
   ON (it is OFF by default — verify that first; see US3).
3. Prepare two contexts to paste into:
   - **Unbracketed**: an app that does NOT enable bracketed paste — e.g. a bare
     `cat` (reading stdin), `sh` without bracketed-paste support, or a minimal
     REPL. (Confirm by checking the dialog *does* appear in US1.)
   - **Bracketed**: a normal `zsh`/`fish`/`bash 4.4+` prompt or `vim` insert
     mode (these enable bracketed paste).

## US1 — Catch an accidental multi-line paste before it runs (P1)

1. Copy a 3-line block (e.g. three shell commands).
2. With the setting ON, paste into the **unbracketed** context.
3. **Expect**: a confirmation dialog appears *before* any line runs, showing a
   reason line with the line count and a readable preview, with **Cancel**
   focused by default.
4. Press **Esc** (or click Cancel). **Expect**: zero bytes sent — the shell did
   not run anything.
5. Repeat; this time choose **Paste** (Enter). **Expect**: all three lines are
   delivered exactly as copied (compare against the same paste with the feature
   OFF — byte-identical, SC-002).
6. Paste the same block into the **bracketed** context. **Expect**: NO dialog;
   content pasted as today (SC-004).
7. Paste a single-line string with no control bytes (a long URL) into the
   unbracketed context. **Expect**: NO dialog (US1 acceptance #5).

## US2 — Catch hidden control/escape characters (P2)

1. Put a single-line string containing an embedded `ESC` byte on the clipboard
   (e.g. copy a raw terminal control sequence, or use a small `printf` piped to
   the clipboard tool).
2. With the setting ON, paste into the **unbracketed** context.
3. **Expect**: the dialog appears even though there is no line break; the reason
   names control characters; the preview shows the control byte in caret
   notation (e.g. `^[`) — verify NO raw control byte reaches the terminal/dialog
   (SC-008).
4. Paste a single line whose only non-printing characters are **tabs**.
   **Expect**: NO dialog (tabs alone do not trigger — US2 acceptance #3).

## US3 — Discoverable, live, and uniform across paste sources (P3)

1. With a fresh config, open Settings → Terminal. **Expect**: the
   **"Confirm risky pastes"** toggle is present, has helper text mentioning the
   bracketed-paste deferral, and is **OFF** by default.
2. Toggle it ON. Without restarting the client, paste risky content in the
   unbracketed context. **Expect**: the dialog now fires (live reload, SC-007).
3. Toggle it OFF. **Expect**: the next paste sends directly, no dialog.
4. With the setting ON, paste risky content via each entry point into the
   unbracketed context: (a) the paste keybinding, (b) right-click → **Paste**,
   (c) middle-click primary-selection paste (Linux). **Expect**: the dialog
   fires identically for all three (SC-006).
5. With the setting ON, middle-click-paste a primary selection **larger than
   4 KiB** and choose **Paste**. **Expect**: the full content is delivered with
   no truncation (validates the unified chunking path — research R1).

## Edge cases

- **Originating pane closed while the dialog is open**: trigger the
  confirmation with a risky paste in an unbracketed pane, close that
  pane/session, then choose **Paste**. **Expect**: the paste is dropped safely —
  no crash and nothing delivered to a different pane (FR-010 / spec Edge Cases).
- **Setting toggled off while a dialog is open**: trigger the confirmation, then
  turn the setting OFF in Settings, then choose **Paste**. **Expect**: the
  in-flight decision is still honored (the parked paste sends); only *subsequent*
  pastes skip the gate.

## Out-of-scope checks (should be UNAFFECTED)

- Drag-and-drop a file into the terminal with the setting ON. **Expect**:
  unchanged shell-quoted path insertion, **no** confirmation dialog (FR-013).
- Copy-on-select and OSC 52 clipboard writes behave exactly as before (FR-014).

## Performance check (PR-001 / Constitution IV)

- **Disabled-path zero cost (SC-005)** — confirmed *by inspection*: the gate
  short-circuits on `!self.config.terminal.paste_confirmation` before
  `classify_paste` is ever called, so a disabled paste runs the exact prior
  code path. Optionally corroborate by pasting a large block with the setting
  OFF and observing no perceptible change vs. baseline.
- **Enabled large paste** — paste a multi-megabyte block with the setting ON:
  the dialog appears without a perceptible stall, the preview is truncated
  (≤ 8 lines × ≤ 56 cols), and after the dialog resolves there is no lingering
  memory growth attributable to the parked content.
