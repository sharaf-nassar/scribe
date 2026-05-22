# Quickstart — OSC 8 Hyperlink Manual Verification

**Branch**: `009-osc8-hyperlinks`
**Date**: 2026-05-21
**Purpose**: Per-user-story manual verification scenarios that satisfy each
story's "Independent Test" requirement from `spec.md`. Constitution II
permits this manual path because the feature has no automated harness that
already pins this behavior and no new automated tests are requested by the
spec.

**Prereqs**:

- A Scribe build with the OSC 8 implementation present.
- A shell with `printf` (any POSIX shell).
- A directory of test files including names with spaces and meta
  characters (`mkdir -p /tmp/osc8-test && touch '/tmp/osc8-test/with space'
  '/tmp/osc8-test/regex.*'`).
- `coreutils` ≥ 8.30 for `ls --hyperlink=auto` (any modern Linux distro
  qualifies).

**Convention**: `printf '\e]8;%s;%s\e\\%s\e]8;;\e\\'` is the canonical OSC 8
emit. Read it as `OSC 8 ; <params> ; <URI> ST <text> OSC 8 ; ; ST`.

---

## US1 — Tool-emitted hyperlinks reach their real destination

### Scenario 1 — `ls --hyperlink=auto` activation

```bash
cd /tmp/osc8-test
ls --hyperlink=auto
```

**Expect**: Each filename is rendered like a regular `ls` line. Ctrl+click
on `with space` → the OS file handler opens (or selects) the file at
`/tmp/osc8-test/with space`, NOT a `with` URL guess or nothing.

**Pass criterion**: All listed filenames open the correct `file://` URI on
Ctrl+click. Includes the space-containing and regex-meta-containing names.

### Scenario 2 — Precedence over heuristic URL detection

```bash
printf 'See \e]8;;https://anthropic.com\e\\example.com\e]8;;\e\\ for details\n'
```

**Expect**: The cell containing `example.com` (text) is hyperlinked to
`https://anthropic.com` (URI). Ctrl+click opens `anthropic.com`, not
`example.com`.

**Pass criterion**: The OS browser receives `anthropic.com`. Verify in
the browser address bar.

### Scenario 3 — Close clears the URI

```bash
printf '\e]8;;https://example.com\e\\linked\e]8;;\e\\ then plain\n'
```

**Expect**: Hovering "linked" shows the tooltip with `https://example.com`;
hovering "plain" shows no tooltip and no hover affordance. Ctrl+click on
"plain" does nothing OSC-8-related (heuristic may still match the bare
word, but no OSC 8 link is involved).

**Pass criterion**: Two distinct hover behaviors on the same line.

### Scenario 4 — `id=` reconnects across a wrap

Resize the window narrow enough that a long-line URI wraps. Emit:

```bash
printf '\e]8;id=demo;https://example.com/very/long\e\\This is a very long label that will wrap across visual rows\e]8;;\e\\\n'
```

**Expect**: Hover/Ctrl+click any cell of the wrapped label resolves to
the same URI.

**Pass criterion**: Tooltip shows `https://example.com/very/long` from any
cell in the span; Ctrl+click anywhere fires the same destination.

### Scenario 5 — Disallowed scheme prompts the confirmation dialog

```bash
printf '\e]8;;javascript:alert(1)\e\\click-me\e]8;;\e\\\n'
```

**Expect**: Hover shows the URI in the tooltip
(`javascript:alert(1)`). Ctrl+click opens the new
DisallowedSchemeDialog with: the full URI, a "scheme `javascript:` is
normally blocked" warning, **Cancel** focused, and an **Open Anyway**
button.

**Pass criterion**:
- **Cancel** dismisses and no browser opens.
- **Open Anyway** routes through the existing `open_url` path (which the
  OS may or may not honour — that's the OS handler's call; the test is
  that Scribe attempts it).
- Esc dismisses (same as Cancel).
- The same dialog also appears for `data:` and any other non-allowlisted
  scheme.

### Scenario 6 — Multi-pane isolation

Open two panes. Emit OSC 8 streams in each independently. Confirm the
hyperlink state in pane A does not affect pane B (no cross-pane leakage
of open URI state per FR-011).

### Scenario 7 — Malformed / oversized OSC 8 (FR-013, FR-010)

Emit a series of malformed OSC 8 sequences and confirm the parser does
not crash and does not leave a dangling hyperlink:

```bash
# (a) missing URI (no scheme bytes between the two semicolons)
printf '\e]8;;\e\\after-empty-open\n'

# (b) missing closing ST — emit open then a normal newline, then plain text
printf '\e]8;;https://example.com\e\\open-no-close'
printf '\nplain-text-next-line\n'

# (c) garbage params with no semicolon
printf '\e]8;garbage_no_semis\e\\garbage-no-uri\e]8;;\e\\\n'

# (d) oversized URI — emit a URI well over the 2 KiB FR-010 cap
python3 -c 'import sys; sys.stdout.write("\x1b]8;;https://example.com/" + "x"*3000 + "\x1b\\over-cap\x1b]8;;\x1b\\\n")'
```

**Pass criterion**:
- Scribe does not crash or hang on any of (a)-(d).
- After (a) and (c), the "open-no-close"/"garbage-no-uri" cells are
  either treated as plain text (no hyperlink) or activate to whatever
  the parser interpreted — but no dangling URI bleeds into subsequent
  cells.
- After (b) the "plain-text-next-line" cells MUST NOT carry the
  previous URI (whatever the upstream "unclosed at boundary" policy is,
  it MUST be deterministic; record what you observe).
- (d) MUST NOT activate to the oversized URI per FR-010 — either
  upstream rejects it before reaching the cell, or the OSC 8 cell-walk
  pass treats it as absent. The cell behaves as plain text.

---

## US2 — Real destination visible before activation

### Scenario 1 — Tooltip on dwell

Emit any OSC 8 hyperlink (see US1 scenarios). Park the cursor over a
hyperlinked cell without moving for ~300 ms.

**Expect**: The existing tooltip overlay appears above or below the cell,
showing the verbatim URI (truncated to the pane width for display).

**Pass criterion**: Tooltip visible, full URI legible (or truncated with
a clear cap if it exceeds pane width). Moving the cursor to a non-OSC 8
cell hides the tooltip.

### Scenario 2 — Context-menu "Open URL" shows the real URI

Emit a hyperlink whose displayed text differs from the URI (US1 #2 form).
Right-click the cell.

**Expect**: The context menu's "Open URL" item references the OSC 8 URI
(visible in the menu hover state and consistent with what would be
opened on click), not the displayed label.

**Pass criterion**: User can read the real destination in the menu before
clicking. Differentiation between displayed text and URI is observable.

### Scenario 3 — "Copy hyperlink address" item

Right-click an OSC 8 hyperlink. Pick "Copy hyperlink address".

**Expect**: The OSC 8 URI verbatim is on the system clipboard.

**Pass criterion**: Paste into another app yields the URI. The existing
"Copy" path on a *text selection* spanning the hyperlink still copies
the displayed text (unchanged behavior — FR-007).

### Scenario 4 — Selection copy semantics unchanged

Click-drag-select text *across* an OSC 8 hyperlink span. Press the copy
shortcut (Ctrl+C or platform equivalent) or use context-menu "Copy".

**Expect**: The displayed text is on the clipboard (no change in
behavior; this is *not* the URI).

**Pass criterion**: Pasted text equals the on-screen selection.

### Scenario 5 — Disallowed-scheme hover still shows URI

Repeat US1 scenario 5 but hover (do not click). The tooltip MUST still
show the full URI even though activation is gated by the dialog.

**Pass criterion**: User sees `javascript:alert(1)` in the tooltip before
deciding whether to click. (Trust signal independent of the activation
gate.)

---

## US3 — Hyperlinks survive scrollback, wrapping, and reattach

### Scenario 1 — Wrapped span end-to-end

Emit US1 scenario 4 (wrapped span). Hover both ends of the wrap; Ctrl+click
the first cell, then the last cell.

**Pass criterion**: Both ends show the same URI in the tooltip; both
Ctrl+clicks fire the same destination.

### Scenario 2 — Scrollback survival

Emit a hyperlink, then scroll-spam (`yes` for a couple of seconds and
Ctrl+C) until the hyperlink is in history but still inside the scrollback
cap. Scroll back to it.

**Pass criterion**: Hover and Ctrl+click both still work. Span boundaries
unchanged.

### Scenario 3 — Live post-reattach hyperlinks

Force a server hot reattach (zero-downtime upgrade flow per
`lat.md/server.md`). After reattach, emit a *new* OSC 8 hyperlink in any
session.

**Pass criterion**: The newly-emitted hyperlink works exactly as US1 #1.
This is the FR-012 firm MUST.

**Known limitation (per `research.md` decision 3)**: Hyperlinks that were
present in scrollback *before* reattach do NOT survive replay — those
cells appear as plain text after reattach. This is the documented
limitation; do not treat it as a defect.

### Scenario 4 — Scrollback trim consistency

Lower the scrollback cap in settings (or emit far more lines than the cap)
until a hyperlinked cell is trimmed. Continue using the pane.

**Pass criterion**: No crash. No hover misfires on trimmed-line indices.
Memory does not grow monotonically across many trim cycles (observable as
stable RSS in a `top`/`htop` watch).

### Scenario 5 — Cross-pane isolation under load

In two split panes, emit OSC 8 streams emitting many distinct URIs at the
same time. Close one pane.

**Pass criterion**: The other pane's hyperlinks continue working
unchanged. No memory growth in the surviving pane attributable to the
closed pane's URIs.

### Scenario 6 — `id=` anti-merge across separate open/close runs (FR-005)

Emit two separate OSC 8 spans that reuse the same `id` value but with
different URIs:

```bash
printf '\e]8;id=demo;https://example.com/AAA\e\\span-A\e]8;;\e\\\n'
printf '\e]8;id=demo;https://example.com/BBB\e\\span-B\e]8;;\e\\\n'
```

Hover both cells and Ctrl+click each.

**Pass criterion**:
- Hovering "span-A" shows `https://example.com/AAA`; Ctrl+click opens
  AAA.
- Hovering "span-B" shows `https://example.com/BBB`; Ctrl+click opens
  BBB.
- The two spans MUST NOT merge into a single span (per FR-005
  per-open-sequence scope clarified in spec.md Clarifications session
  2026-05-21). The later `id=demo` does not retroactively redirect the
  earlier span and is not retroactively redirected by it.

---

## Performance spot-check (PR-001, SC-005)

After US1+US2 pass, time the Ctrl+click activation manually for an
allowed-scheme OSC 8 hyperlink versus a plain `https://` heuristic URL.
There should be no perceptible difference. If the dwell timer makes
hovering feel laggy, note the value (300 ms default) and consider
tuning. No formal benchmark is required by the spec.

## Replay-scrollback limitation messaging

When the implementation lands, `lat.md/client.md` MUST gain a short note
under URL detection (or a new "Hyperlinks" subsection) calling out the
replay-scrollback limitation so future readers know it is intentional and
where to find the follow-up improvement path (extending
`snapshot_to_ansi` to emit OSC 8 open/close around hyperlinked runs).
