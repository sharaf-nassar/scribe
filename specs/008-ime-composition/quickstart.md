# Quickstart: Manual Verification — IME Composition and Preedit Handling

> **Subagent verification (2026-05-21):** build / clippy / test gates green
> (`cargo build --workspace` clean, `cargo clippy --workspace --all-targets
> -- -D warnings` clean, `cargo test -p scribe-client` 127/127). The
> OS-IME-dependent flows below still require human verification per the
> sign-off checklist at the end of this file.

**Phase 1 output for `008-ime-composition`.** One verification path per
user story, ordered by priority. Each is self-contained and exercises the
minimum surface to confirm the story works end-to-end.

These flows substitute for automated tests (Constitution §II permits this
when documented; rationale in `plan.md` Constitution Check).

---

## Prerequisites

- Local Scribe checkout on `008-ime-composition`.
- Built client: `cargo build -p scribe-client --release`.
- An OS IME or composing input configured:
  - **macOS**: System Settings → Keyboard → Input Sources → add Pinyin
    Simplified (or any CJK source). Press-and-Hold is enabled by default.
  - **Linux X11**: Install + start `ibus` or `fcitx5` with a Chinese
    engine (e.g., `ibus-libpinyin`, `fcitx5-chinese-addons`). Export
    `GTK_IM_MODULE=ibus`, `QT_IM_MODULE=ibus`, `XMODIFIERS=@im=ibus`
    before launching the client.
  - **Linux Wayland**: A compositor implementing `zwp_text_input_v3` (GNOME
    44+, KDE Plasma 5.27+, Sway with `sway-input`). IME engine as above.
  - **Windows**: any installed IME (Microsoft Pinyin, Japanese IME, etc.).

> **Note**: On Linux X11 the `_NET_ACTIVE_WINDOW` focus guard already in
> `x11_focus.rs` is the IME activation gate. If IME does not appear to
> engage, check that the window is the active one and that the IME daemon
> is running (`ibus-daemon --xim` / `fcitx5`).

---

## P1 — Compose and commit text via the OS IME (MVP)

**Goal**: Confirm that committed IME text arrives at the shell.

1. Launch Scribe: `target/release/scribe-client` (or `just run-client`).
2. With a shell prompt visible in the focused pane, switch to a CJK IME.
3. Type `nihao` (or your IME's equivalent for 你好).
4. Select 你好 from the candidate popup and press space/enter to commit.
5. **Expect**:
   - 你好 appears at the prompt.
   - Pressing Enter executes the shell command (or shows
     `command not found: 你好` — either confirms the bytes arrived).
   - `echo 你好` round-trips correctly.
6. Repeat for: French dead keys (`Compose ' e` → `é`) on Linux with a
   Compose key configured, OR macOS Press-and-Hold for accented Latin
   characters.
7. With the pane **unfocused** (click another app), type via the IME:
   no characters appear in any Scribe pane.

**Pass criteria (SC-001, SC-002)**:

- Full 4-character CJK commit lands within typical typing rhythm.
- All standard accented Latin characters (á é í ó ú ñ ü ç ß) reproduce
  via the OS sequence.
- Unfocused pane receives nothing.

---

## P2 — In-line preedit at the cursor

**Goal**: Confirm preedit visual feedback during composition.

1. From the P1 setup, start typing a CJK sequence (e.g., `nihao`).
2. **Before committing**, observe the cursor cell.
3. **Expect**:
   - The composing characters render at the cursor position with an
     underline (or otherwise visually distinct treatment).
   - The cursor remains at the composition start.
   - The terminal grid content at the composition row is **not** mutated
     — pressing Escape clears preedit and the original grid content
     remains intact (run `clear` then immediately compose to verify the
     blank line stays blank after cancel).
4. Press Escape (or your IME's cancel sequence).
5. **Expect**: preedit cells clear in the same frame; no residue.
6. Commit a different composition.
7. **Expect**: preedit visually disappears the instant the commit is
   delivered (no flash of stale preedit + committed text simultaneously).

**Pass criteria (UX-002, PR-001, FR-006, FR-010)**:

- Preedit renders within one display frame of the first composing
  keystroke (visually instant on 60Hz+).
- Cancel and commit both clear preedit cleanly with no one-frame
  residue.
- Terminal grid contents are never altered by preedit.

---

## P3 — IME state survives workflow events

**Goal**: Confirm robustness across pane switches, focus loss, scroll,
resize.

### P3a — Pane switch cancels composition

1. Open two panes (split: Ctrl+Shift+D on Linux or platform shortcut).
2. In pane A, start composing a CJK sequence (do not commit).
3. Click pane B.
4. **Expect**:
   - Pane A's preedit cells clear immediately.
   - Pane B is IME-ready (try typing in pane B; composition starts
     cleanly from empty).
   - The candidate popup attaches to pane B's cursor cell.

### P3b — Window focus loss cancels composition

1. In a pane, start composing.
2. Alt-tab to another application (or trigger a compositor overlay).
3. **Expect**: preedit clears; IME deactivates.
4. Alt-tab back; start a new composition.
5. **Expect**: preedit starts cleanly from empty.

### P3c — Cursor-rect tracking under scroll / alt-screen

1. In a shell, run `seq 1 200` to fill scrollback.
2. Scroll the scrollback up so the live cursor is off-screen.
3. Trigger an IME composition (the candidate popup should appear).
4. **Expect**: popup anchored near the visible cursor's position; or, if
   the cursor is fully off-screen, the popup is positioned at the bottom
   of the visible area (winit's clamping) — not at (0,0) or off the
   window.
5. Run `vim` (or `htop`). With the alt-screen active, start composition
   somewhere in the buffer.
6. **Expect**: popup follows the cursor as you move it within the alt-
   screen.

### P3d — Resize / DPI change

1. Start composing.
2. Drag a pane divider to resize the focused pane.
3. **Expect**: preedit cells reposition to the new cursor cell on the
   next frame; popup follows.
4. Move the window to a screen with a different scale factor (if
   available).
5. **Expect**: preedit and popup reposition correctly post-DPI change.

**Pass criteria (SC-005, FR-003, FR-008)**:

- No orphan preedit cells anywhere after any of the above transitions.
- Popup follows the cursor on every cursor-cell movement within one
  frame.

---

## Regression checks (must pass alongside the above)

These verify the spec's no-change guarantees.

### R1 — ASCII typing unchanged

1. Disable the OS IME (switch to "ABC" / "US English" input source).
2. Type `echo hello && date`. Press Enter.
3. **Expect**: identical behavior to pre-change Scribe (no extra latency,
   no preedit indicator, no popup attempt).

### R2 — Kitty CSI-u keystrokes unchanged

1. With a TUI that negotiates the Kitty keyboard protocol (e.g.,
   `nvim` recent enough, or the Codex CLI), confirm Ctrl/Shift/Alt
   modifier disambiguation still works as it did after the Kitty CSI-u
   ship in May 2026.
2. **Expect**: zero behavioral change — Kitty CSI-u byte output is
   identical pre/post this feature.

### R3 — Input-pipeline test suite

```sh
cargo test -p scribe-client --lib input
```

**Expect**: all existing tests pass (target: 122/122 green per the
2026-05-18 audit's verification baseline). Zero regressions.

### R4 — Search overlay and dialog surfaces

1. Open the search overlay (Ctrl+F or platform shortcut).
2. With a CJK IME active, attempt to compose into the search bar.
3. **Expect (v1)**: no IME activity inside the search bar; raw key
   events continue. (CJK search support is explicitly out of scope.)
4. Open the update / close dialog. Confirm same behavior.

---

## Sign-off checklist (paste into the PR description)

- [ ] P1 (commit pipeline) verified on macOS / Linux X11 / Linux Wayland /
      Windows (whichever the contributor has access to; mark N/A
      otherwise).
- [ ] P2 (preedit rendering) verified on at least one platform.
- [ ] P3a–P3d (workflow robustness) verified on at least one platform.
- [ ] R1 (ASCII unchanged) verified.
- [ ] R2 (Kitty CSI-u unchanged) verified.
- [ ] R3 (`cargo test -p scribe-client --lib input` green).
- [ ] R4 (search/dialog IME-disabled) verified.
- [ ] `lat.md/client.md` updated; `lat check` passes.
- [ ] Audit doc `design/modern-terminal-audit-2026-05-18.md` annotated
      with "Update — Shipped" for the IME item (SC-006).
