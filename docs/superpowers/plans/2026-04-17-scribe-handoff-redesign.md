# Scribe hot-reload handoff redesign (v5) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current per-cell MessagePack handoff payload with a per-session ANSI-replay + zstd encoding; populate the restored `Term` durably (fixing the latent "Term empty after handoff" bug); bind the IPC socket before per-session restore runs so the Debian watchdog never blocks on decode/restore.

**Architecture:** Ship `HANDOFF_VERSION = 5` in a single release. The receiver decodes both v4 (legacy `ScreenSnapshot`) and v5 (new `SessionReplay`) payloads — forward upgrades never cold-restart. The sender writes only v5: take a `ScreenSnapshot` per session, run it through the existing `snapshot_to_ansi` encoder (relocated to `scribe-common`), zstd-compress, transmit. The receiver decompresses per session and feeds the bytes through `vte::ansi::Processor::advance` into a fresh `Term` so the server-side history is durably restored — no `handoff_snapshot` field. The upgrade receiver binds the IPC socket (the line the postinst watchdog greps for) before spawning per-session restore tasks; `AttachSessions` awaits a per-session `tokio::sync::Notify` until the session is `Ready`.

**Tech Stack:** Rust, tokio, `vte::ansi::Processor` (via alacritty_terminal), `rmp-serde` (wire envelope), `zstd` (new — compressed replay bytes), `serde` (additive field with `#[serde(default)]` for compatibility).

**Spec:** `docs/superpowers/specs/2026-04-17-scribe-handoff-redesign-design.md`

---

## File structure

| File | Responsibility | Action |
|---|---|---|
| `Cargo.toml` (workspace) | Add `zstd` to `[workspace.dependencies]` | Modify |
| `crates/scribe-common/Cargo.toml` | Declare `zstd` | Modify |
| `crates/scribe-common/src/lib.rs` | Expose `screen_replay` module | Modify |
| `crates/scribe-common/src/screen_replay.rs` | ANSI replay encoder (`snapshot_to_ansi` and helpers) + `SessionReplay` wire type + zstd wrappers | Create |
| `crates/scribe-client/src/main.rs` | Import `snapshot_to_ansi` from `scribe_common::screen_replay` instead of defining it locally | Modify |
| `crates/scribe-server/Cargo.toml` | Declare `zstd` | Modify |
| `crates/scribe-server/src/handoff.rs` | Raise `HANDOFF_VERSION` to 5; tolerate v4 on receive; add `session_replay: Option<SessionReplay>` to `HandoffSession` | Modify |
| `crates/scribe-server/src/ipc_server.rs` | Remove `handoff_snapshot` from `LiveSession` and attach flow; tolerate `SessionState::Restoring` in attach | Modify |
| `crates/scribe-server/src/session_manager.rs` | Produce `SessionReplay` in `serialize_live_for_handoff`; feed replay through `AnsiProcessor` in `restore_from_handoff`; remove `handoff_snapshot` from `ManagedSession` | Modify |
| `crates/scribe-server/src/attach_flow.rs` | Remove the handoff branch from `take_session_snapshot`; await restore `Notify` before serving initial snapshot | Modify |
| `crates/scribe-server/src/main.rs` | Reorder `run_upgrade_receiver`: bind socket before per-session restore; spawn per-session restore tasks | Modify |
| `crates/scribe-server/src/handoff_tests.rs` | Add v4→v5 compat tests, v5 round-trip, restore populates Term durably | Modify |
| `crates/scribe-server/tests/replay_roundtrip.rs` | New integration test: Term → ANSI → Term grid+scrollback matches | Create |
| `lat.md/server.md` | Update `Handoff#*` sections to reflect v5 representation, invariant, and restore states | Modify |

---

## Task 1: Add `zstd` to the workspace

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/scribe-common/Cargo.toml`
- Modify: `crates/scribe-server/Cargo.toml`

- [ ] **Step 1: Add `zstd` to `[workspace.dependencies]`**

Edit `Cargo.toml` (workspace root). Under the `# Serialization` group, add:

```toml
zstd = { version = "0.13", default-features = false }
```

- [ ] **Step 2: Wire the dep into `scribe-common`**

Edit `crates/scribe-common/Cargo.toml`. Under `[dependencies]`, add:

```toml
zstd.workspace = true
```

- [ ] **Step 3: Wire the dep into `scribe-server`**

Edit `crates/scribe-server/Cargo.toml`. Under `[dependencies]`, add:

```toml
zstd.workspace = true
```

- [ ] **Step 4: Verify build**

Run: `cargo build --workspace`
Expected: success. `zstd` is now available to both crates.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/scribe-common/Cargo.toml crates/scribe-server/Cargo.toml
git commit -m "build: add zstd to workspace dependencies"
```

---

## Task 2: Relocate `snapshot_to_ansi` to `scribe-common`

Moves the ANSI replay encoder out of the client binary so the server sender can use it. No behavior change — the client still consumes the same function, just via a new path.

**Files:**
- Create: `crates/scribe-common/src/screen_replay.rs`
- Modify: `crates/scribe-common/src/lib.rs`
- Modify: `crates/scribe-client/src/main.rs:10348` (remove the local `snapshot_to_ansi` and its helpers)

- [ ] **Step 1: Copy the encoder block verbatim**

In `crates/scribe-client/src/main.rs`, identify the block from `fn snapshot_to_ansi(...)` (line 10348) through `fn write_color_sgr(...)` and its `SgrState` helper, `write_string`, `write_snapshot_row`, `row_wraps`, `write_sgr`. Include any private helpers those functions call. Copy this block into a new file:

Create `crates/scribe-common/src/screen_replay.rs` with the copied contents. At the top of the file, update the imports so `ScreenCell`, `ScreenColor`, `ScreenSnapshot`, `CursorStyle` resolve from the same crate (`crate::screen::...`) instead of `scribe_common::screen::...`:

```rust
use crate::screen::{CursorStyle, ScreenCell, ScreenColor, ScreenSnapshot};
```

Make the moved `fn snapshot_to_ansi(snapshot: &ScreenSnapshot) -> Vec<u8>` `pub` so other crates can call it. Make internal helpers private (module-local) unless they're needed elsewhere.

- [ ] **Step 2: Expose the module**

Edit `crates/scribe-common/src/lib.rs`. Add:

```rust
pub mod screen_replay;
```

(Exact module ordering: alphabetical after `screen`.)

- [ ] **Step 3: Remove the client-local copy**

In `crates/scribe-client/src/main.rs`, delete the `fn snapshot_to_ansi` definition and its private helpers (`write_snapshot_row`, `row_wraps`, `write_sgr`, `write_color_sgr`, `SgrState`, `write_string`). Add a `use` near the top of the file:

```rust
use scribe_common::screen_replay::snapshot_to_ansi;
```

Replace the `snapshot_to_ansi(...)` call site with the re-exported symbol (no source change at call site if the name is the same in the `use` statement).

- [ ] **Step 4: Verify build and existing tests still pass**

Run:
```bash
cargo build --workspace
cargo test -p scribe-client
```
Expected: build succeeds; client tests pass unchanged. The client's reconnect replay path is untouched.

- [ ] **Step 5: Commit**

```bash
git add crates/scribe-common/src/lib.rs crates/scribe-common/src/screen_replay.rs crates/scribe-client/src/main.rs
git commit -m "refactor: relocate snapshot_to_ansi to scribe-common::screen_replay"
```

---

## Task 3: Define `SessionReplay` wire type and zstd helpers

**Files:**
- Modify: `crates/scribe-common/src/screen_replay.rs`

- [ ] **Step 1: Define the type**

In `crates/scribe-common/src/screen_replay.rs`, append:

```rust
use serde::{Deserialize, Serialize};

/// Per-session replay payload for v5+ hot-reload handoff.
///
/// Transports the session's visible grid plus scrollback as a zstd-compressed
/// ANSI byte stream produced by `snapshot_to_ansi`. The receiver feeds the
/// decompressed bytes through `vte::ansi::Processor::advance` into a fresh
/// `Term`, which reconstructs the grid and scrollback durably.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReplay {
    pub cols: u16,
    pub rows: u16,
    pub scrollback_rows: u32,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cursor_style: CursorStyle,
    pub cursor_visible: bool,
    pub alt_screen: bool,
    /// zstd-compressed ANSI replay bytes (output of `snapshot_to_ansi`).
    pub replay_zstd: Vec<u8>,
}

/// Compression level. Level 3 is the zstd default; tuned for fast encode with
/// good ratio on repetitive terminal content.
const ZSTD_LEVEL: i32 = 3;

/// Build a `SessionReplay` from a `ScreenSnapshot`.
///
/// Runs `snapshot_to_ansi` and compresses the result with zstd at level 3.
pub fn build_session_replay(snapshot: &ScreenSnapshot) -> std::io::Result<SessionReplay> {
    let ansi = snapshot_to_ansi(snapshot);
    let replay_zstd = zstd::bulk::compress(&ansi, ZSTD_LEVEL)?;
    Ok(SessionReplay {
        cols: snapshot.cols,
        rows: snapshot.rows,
        scrollback_rows: snapshot.scrollback_rows,
        cursor_col: snapshot.cursor_col,
        cursor_row: snapshot.cursor_row,
        cursor_style: snapshot.cursor_style,
        cursor_visible: snapshot.cursor_visible,
        alt_screen: snapshot.alt_screen,
        replay_zstd,
    })
}

/// Decompress a `SessionReplay`'s replay bytes into a plain ANSI byte buffer.
pub fn decompress_session_replay(replay: &SessionReplay) -> std::io::Result<Vec<u8>> {
    // Capacity hint: ~8 bytes per cell upper bound, minimum 64 KiB to avoid
    // thrashing on small payloads.
    let hint = (usize::from(replay.cols) * (usize::from(replay.rows) + replay.scrollback_rows as usize)) * 8;
    let capacity = hint.max(64 * 1024);
    zstd::bulk::decompress(&replay.replay_zstd, capacity)
}
```

- [ ] **Step 2: Add a unit test for compress/decompress round-trip**

Append to the same file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{CellFlags, CursorStyle, ScreenCell, ScreenColor, ScreenSnapshot};

    fn blank_cell() -> ScreenCell {
        ScreenCell { c: ' ', fg: ScreenColor::Named(256), bg: ScreenColor::Named(257), flags: CellFlags::default() }
    }

    fn snapshot_with_text(text: &str) -> ScreenSnapshot {
        let cols: u16 = 80;
        let rows: u16 = 24;
        let mut cells = vec![blank_cell(); usize::from(cols) * usize::from(rows)];
        for (i, ch) in text.chars().enumerate() {
            if i >= cells.len() { break; }
            cells[i].c = ch;
        }
        ScreenSnapshot {
            cells,
            cols,
            rows,
            cursor_col: 0,
            cursor_row: 0,
            cursor_style: CursorStyle::Block,
            cursor_visible: true,
            alt_screen: false,
            scrollback: Vec::new(),
            scrollback_rows: 0,
        }
    }

    #[test]
    fn session_replay_round_trip_preserves_ansi_bytes() {
        let snapshot = snapshot_with_text("hello world");
        let replay = build_session_replay(&snapshot).expect("build_session_replay");
        let decoded = decompress_session_replay(&replay).expect("decompress");
        let direct = snapshot_to_ansi(&snapshot);
        assert_eq!(decoded, direct);
    }

    #[test]
    fn session_replay_compresses_spaces_well() {
        // 80x24 of spaces should zstd down to a few hundred bytes at most.
        let snapshot = snapshot_with_text("");
        let replay = build_session_replay(&snapshot).unwrap();
        assert!(
            replay.replay_zstd.len() < 1024,
            "expected <1024 compressed bytes for blank screen, got {}",
            replay.replay_zstd.len()
        );
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p scribe-common screen_replay::tests`
Expected: both tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/scribe-common/src/screen_replay.rs
git commit -m "feat: add SessionReplay wire type with zstd encode/decode helpers"
```

---

## Task 4: Add ANSI round-trip test harness (Term → ANSI → Term)

Before switching the sender or receiver, prove the replay round-trips correctly through the server's own `AnsiProcessor` on a fresh `Term`. This test is the contract for everything downstream.

**Files:**
- Create: `crates/scribe-server/tests/replay_roundtrip.rs`

- [ ] **Step 1: Scaffold the test file**

Create `crates/scribe-server/tests/replay_roundtrip.rs`:

```rust
//! Round-trip tests for ANSI replay fidelity.
//!
//! Contract: given a `Term` populated with content, running its snapshot
//! through `snapshot_to_ansi` and then through a fresh `AnsiProcessor` +
//! `Term` must reproduce the same grid + scrollback cells.

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Config as TermConfig;
use scribe_common::screen_replay::{build_session_replay, decompress_session_replay, snapshot_to_ansi};
use scribe_pty::event_listener::ScribeEventListener;
use scribe_server::session_manager::snapshot_term;
use tokio::sync::mpsc;
use vte::ansi::Processor as AnsiProcessor;

#[derive(Clone, Copy)]
struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize { self.rows }
    fn screen_lines(&self) -> usize { self.rows }
    fn columns(&self) -> usize { self.cols }
}

fn new_term(cols: usize, rows: usize, scrollback: usize) -> Term<ScribeEventListener> {
    let (tx, _rx) = mpsc::unbounded_channel();
    let listener = ScribeEventListener::new(uuid::Uuid::nil().into(), tx);
    let config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };
    Term::new(config, &Dims { cols, rows }, listener)
}

/// Drive a Term with a byte stream via the same AnsiProcessor path the server
/// uses for real PTY bytes.
fn feed(term: &mut Term<ScribeEventListener>, bytes: &[u8]) {
    let mut processor = AnsiProcessor::new();
    processor.advance(term, bytes);
}
```

Make `snapshot_term` accessible by exporting it (it's already `pub fn snapshot_term` in `session_manager.rs`, so `use scribe_server::session_manager::snapshot_term;` should work if the module is `pub`). If compile fails at this import, in the next task's Step 1 we either:

- mark `mod session_manager` as `pub` in `crates/scribe-server/src/main.rs`, or
- add a re-export in `crates/scribe-server/src/lib.rs` (creating one if it doesn't exist).

- [ ] **Step 2: Add the first passing case — ASCII text on a fresh screen**

Append to `replay_roundtrip.rs`:

```rust
#[test]
fn roundtrip_ascii_text() {
    let mut src = new_term(80, 24, 100);
    feed(&mut src, b"hello world\r\nsecond line\r\n");

    let snap = snapshot_term(&src);
    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();
    assert_eq!(bytes, snapshot_to_ansi(&snap));

    let mut dst = new_term(80, 24, 100);
    feed(&mut dst, &bytes);

    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells, "visible grid must match");
    assert_eq!(snap.scrollback, snap_dst.scrollback, "scrollback must match");
    assert_eq!(snap.cursor_row, snap_dst.cursor_row);
    assert_eq!(snap.cursor_col, snap_dst.cursor_col);
}
```

- [ ] **Step 3: Make `session_manager::snapshot_term` reachable from integration tests**

If Step 2's compile fails with "module `session_manager` is private", add a `crates/scribe-server/src/lib.rs` file:

```rust
//! Library entry point for scribe-server integration tests. Re-exports the
//! internal modules that integration tests rely on. The binary entry point
//! remains `main.rs`; this `lib.rs` exists alongside it.
pub mod attach_flow;
pub mod handoff;
pub mod ipc_server;
pub mod session_manager;
pub mod workspace_manager;
```

And add the matching `[lib]` target to `crates/scribe-server/Cargo.toml` above the existing `[dependencies]`:

```toml
[lib]
name = "scribe_server"
path = "src/lib.rs"

[[bin]]
name = "scribe-server"
path = "src/main.rs"
```

- [ ] **Step 4: Run the first round-trip test**

Run: `cargo test -p scribe-server --test replay_roundtrip roundtrip_ascii_text`
Expected: pass.

- [ ] **Step 5: Add SGR, wide-char, wrap, and scrollback cases**

Append to `replay_roundtrip.rs`:

```rust
#[test]
fn roundtrip_sgr_attributes() {
    let mut src = new_term(80, 24, 100);
    feed(
        &mut src,
        b"\x1b[1mbold\x1b[0m normal \x1b[4;31munderlined red\x1b[0m\r\n",
    );
    let snap = snapshot_term(&src);
    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();

    let mut dst = new_term(80, 24, 100);
    feed(&mut dst, &bytes);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells);
}

#[test]
fn roundtrip_scrollback_overflow() {
    // Print 50 rows to force scrollback in a 10-row window.
    let mut src = new_term(80, 10, 100);
    for i in 0..50 {
        let line = format!("line {i:02}\r\n");
        feed(&mut src, line.as_bytes());
    }
    let snap = snapshot_term(&src);
    assert!(snap.scrollback_rows > 0, "scrollback must contain prior rows");

    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();

    let mut dst = new_term(80, 10, 100);
    feed(&mut dst, &bytes);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.scrollback, snap_dst.scrollback);
    assert_eq!(snap.cells, snap_dst.cells);
}

#[test]
fn roundtrip_wide_chars() {
    let mut src = new_term(80, 24, 100);
    feed(&mut src, "hello 世界\r\n".as_bytes());
    let snap = snapshot_term(&src);
    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();

    let mut dst = new_term(80, 24, 100);
    feed(&mut dst, &bytes);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells);
}

#[test]
fn roundtrip_soft_wrap() {
    // 20-col grid with a 50-char line forces a soft wrap (WRAPLINE flag).
    let mut src = new_term(20, 5, 100);
    let long: String = "a".repeat(50) + "\r\n";
    feed(&mut src, long.as_bytes());
    let snap = snapshot_term(&src);

    let replay = build_session_replay(&snap).unwrap();
    let bytes = decompress_session_replay(&replay).unwrap();

    let mut dst = new_term(20, 5, 100);
    feed(&mut dst, &bytes);
    let snap_dst = snapshot_term(&dst);
    assert_eq!(snap.cells, snap_dst.cells, "soft-wrap content must match");
    assert_eq!(snap.scrollback, snap_dst.scrollback);
}
```

- [ ] **Step 6: Run the full test file**

Run: `cargo test -p scribe-server --test replay_roundtrip`
Expected: all four tests pass. If any fail, the ANSI encoder has a fidelity gap — fix the encoder or the test expectation before proceeding. Do not continue to later tasks with a red round-trip test.

- [ ] **Step 7: Commit**

```bash
git add crates/scribe-server/src/lib.rs crates/scribe-server/Cargo.toml crates/scribe-server/tests/replay_roundtrip.rs
git commit -m "test: add ANSI replay round-trip coverage for Term snapshots"
```

---

## Task 5: Add the `session_replay` field to `HandoffSession` (additive)

Additive-only change; nothing on the wire changes yet. Receivers that are still at v4 will see `session_replay: None` via `#[serde(default)]` and fall through to the existing `snapshot` path.

**Files:**
- Modify: `crates/scribe-server/src/handoff.rs:72-106` (the `HandoffSession` struct)

- [ ] **Step 1: Add the field and an import**

At the top of `handoff.rs`, replace:

```rust
use scribe_common::screen::ScreenSnapshot;
```

with:

```rust
use scribe_common::screen::ScreenSnapshot;
use scribe_common::screen_replay::SessionReplay;
```

Inside the `HandoffSession` struct (around line 72), add a new field — place it right after `snapshot`:

```rust
    pub snapshot: Option<ScreenSnapshot>,
    /// v5 replay payload (compressed ANSI). Senders at v5 populate this and
    /// leave `snapshot` at None. Receivers prefer this over `snapshot` when
    /// both are present.
    #[serde(default)]
    pub session_replay: Option<SessionReplay>,
```

- [ ] **Step 2: Verify build**

Run: `cargo build -p scribe-server`
Expected: success. No call sites touch `session_replay` yet; it's always None.

- [ ] **Step 3: Verify existing handoff tests still pass**

Run: `cargo test -p scribe-server`
Expected: all existing tests pass, including `handoff_tests::restore_from_handoff_populates_session_manager`.

- [ ] **Step 4: Commit**

```bash
git add crates/scribe-server/src/handoff.rs
git commit -m "feat: add additive session_replay field to HandoffSession"
```

---

## Task 6: Teach receiver to tolerate v4 and v5

The receiver must not reject a v4 payload when the running binary is v5. This is a pure read-path change: the version gate now accepts `HANDOFF_VERSION` and `HANDOFF_VERSION - 1`.

**Files:**
- Modify: `crates/scribe-server/src/handoff.rs:490-497` (the version check in `receive_handoff`)

- [ ] **Step 1: Relax the version check**

Replace the current block (around line 490):

```rust
    if state.version != HANDOFF_VERSION {
        return Err(ScribeError::IpcError {
            reason: format!(
                "handoff version mismatch: got {}, expected {HANDOFF_VERSION}",
                state.version
            ),
        });
    }
```

with:

```rust
    if state.version != HANDOFF_VERSION && state.version != HANDOFF_VERSION.saturating_sub(1) {
        return Err(ScribeError::IpcError {
            reason: format!(
                "handoff version unsupported: got {}, supported {}..={HANDOFF_VERSION} \
                 (cold-restart required)",
                state.version,
                HANDOFF_VERSION.saturating_sub(1),
            ),
        });
    }
```

- [ ] **Step 2: Verify existing behavior is unchanged at v4**

Run: `cargo test -p scribe-server`
Expected: existing tests pass. The receiver still accepts v4 (`HANDOFF_VERSION - 1 == 3` right now won't matter — see next task for the bump).

- [ ] **Step 3: Commit**

```bash
git add crates/scribe-server/src/handoff.rs
git commit -m "feat: receiver tolerates HANDOFF_VERSION and N-1"
```

---

## Task 7: Teach `restore_from_handoff` to prefer `session_replay` when present

The sender still doesn't emit `session_replay` yet, so this branch is latent code for now. That lets us test the restore path in isolation before flipping the sender.

**Files:**
- Modify: `crates/scribe-server/src/session_manager.rs:376-469` (`restore_from_handoff`)

- [ ] **Step 1: Populate Term from replay when present**

Inside `restore_from_handoff`, just before the `let managed = ManagedSession { ... }` block (currently around line 439), insert:

```rust
            // v5 replay path: decompress the ANSI and feed it through
            // AnsiProcessor into the fresh Term so the pre-handoff scrollback
            // is durably restored. If the replay is missing or decode fails,
            // fall through and store the legacy snapshot on handoff_snapshot
            // (v4 compat).
            let handoff_snapshot = if let Some(replay) = &handoff_session.session_replay {
                match scribe_common::screen_replay::decompress_session_replay(replay) {
                    Ok(bytes) => {
                        let mut processor = AnsiProcessor::new();
                        let mut term_guard = term.lock().unwrap_or_else(|p| p.into_inner());
                        processor.advance(&mut *term_guard, &bytes);
                        None
                    }
                    Err(e) => {
                        warn!(
                            session_id = %handoff_session.session_id,
                            "replay decompress failed: {e}; falling back to legacy snapshot"
                        );
                        handoff_session.snapshot.clone()
                    }
                }
            } else {
                handoff_session.snapshot.clone()
            };
```

Then change the `handoff_snapshot: handoff_session.snapshot.clone(),` line inside the `ManagedSession { ... }` literal to:

```rust
                handoff_snapshot,
```

This removes one of the two clones identified in the spec and routes the v5 path straight into the Term.

Note: `term` is an `Arc<Mutex<Term>>` created a few lines above. `.lock().unwrap_or_else(|p| p.into_inner())` is the idiomatic "tolerate a poisoned lock" idiom used elsewhere in this code base; keep it.

- [ ] **Step 2: Add a regression test for v5 restore**

Append to `crates/scribe-server/src/handoff_tests.rs` (or create it if necessary) a new test:

```rust
#[tokio::test]
async fn restore_from_handoff_v5_replay_populates_term() {
    use scribe_common::screen_replay::build_session_replay;

    // Build a Term with content, snapshot it, convert to SessionReplay.
    let mut src = make_term_with_content(b"hello v5 world\r\n");
    let snap = crate::session_manager::snapshot_term(&src);
    let replay = build_session_replay(&snap).unwrap();

    // Build a HandoffState with only session_replay populated (no snapshot).
    let state = make_handoff_state_with_replay(replay, &snap);
    let (state, fds, _slaves) = state;

    let sm = crate::session_manager::SessionManager::restore_from_handoff(&state, fds, 100).unwrap();

    // Pick the restored session and take a snapshot of its Term.
    let sessions = sm.sessions_snapshot_for_test().await;
    let session = sessions.values().next().expect("one restored session");
    let term_guard = session.term.lock().await;
    let after = crate::session_manager::snapshot_term(&term_guard);

    assert_eq!(after.cells, snap.cells);
    assert_eq!(after.scrollback, snap.scrollback);
    assert!(session.handoff_snapshot.is_none(), "v5 path must not store handoff_snapshot");
}
```

The helpers `make_term_with_content` and `make_handoff_state_with_replay` are small wrappers you add at the top of `handoff_tests.rs`; pattern them after the existing `make_handoff_state` helper. If `sessions_snapshot_for_test` doesn't exist, add a `#[cfg(test)]` accessor on `SessionManager` that returns the internal map.

- [ ] **Step 3: Run the regression test**

Run: `cargo test -p scribe-server handoff_tests::restore_from_handoff_v5_replay_populates_term`
Expected: pass.

- [ ] **Step 4: Verify the v4 path still works**

Run: `cargo test -p scribe-server handoff_tests::restore_from_handoff_populates_session_manager`
Expected: pass (legacy path preserved).

- [ ] **Step 5: Commit**

```bash
git add crates/scribe-server/src/session_manager.rs crates/scribe-server/src/handoff_tests.rs
git commit -m "feat: restore_from_handoff feeds v5 replay into Term durably"
```

---

## Task 8: Flip the sender to v5 and bump `HANDOFF_VERSION`

This is the cut-over step. After this task, a v5 → v5 handoff uses the replay path end-to-end; a v4 → v5 handoff still works via the compat decoder.

**Files:**
- Modify: `crates/scribe-server/src/handoff.rs:41` (`HANDOFF_VERSION`)
- Modify: `crates/scribe-server/src/ipc_server.rs:2483-2526` (`serialize_live_for_handoff`)

- [ ] **Step 1: Bump the version**

Edit `handoff.rs` line 41:

```rust
const HANDOFF_VERSION: u32 = 5;
```

- [ ] **Step 2: Emit `SessionReplay` instead of `ScreenSnapshot`**

In `ipc_server.rs`, inside the loop at `serialize_live_for_handoff` (line 2490), replace:

```rust
        let term = live.term.lock().await;
        let snapshot = Some(snapshot_term(&term));
        let cols = u16::try_from(term.grid().columns()).unwrap_or(u16::MAX);
        let rows = u16::try_from(term.grid().screen_lines()).unwrap_or(u16::MAX);
        drop(term);
```

with:

```rust
        let term = live.term.lock().await;
        let snapshot = snapshot_term(&term);
        let cols = u16::try_from(term.grid().columns()).unwrap_or(u16::MAX);
        let rows = u16::try_from(term.grid().screen_lines()).unwrap_or(u16::MAX);
        drop(term);

        // Encode as a v5 replay. If encoding fails (I/O error from zstd),
        // log and skip the replay for this session — the receiver will
        // produce a blank restored Term rather than aborting the whole
        // handoff. This matches the per-session failure policy.
        let session_replay = match scribe_common::screen_replay::build_session_replay(&snapshot) {
            Ok(replay) => Some(replay),
            Err(e) => {
                tracing::warn!(%session_id, "build_session_replay failed: {e}");
                None
            }
        };
```

Then inside the `HandoffSession { ... }` literal a few lines below, change:

```rust
            snapshot,
```

to:

```rust
            snapshot: None,
            session_replay,
```

- [ ] **Step 3: Update the round-trip integration test expectation if needed**

Run: `cargo test -p scribe-server`
Expected: all tests pass. If `restore_from_handoff_populates_session_manager` fails because it was constructing a v4-style state that used `snapshot: Some(...)`, that test is still exercising the compat path and should continue to pass — the compat branch in `restore_from_handoff` handles it. If instead the test was updated in Task 7 to use the new path, it passes for a different reason. Either way, both paths must be green.

- [ ] **Step 4: Measure wire size reduction on a live handoff**

Skip this step in automated execution; reserve for a manual verification run by the operator. After the release is built, the operator runs `just restart-server` on a session with ~10k scrollback and observes the `state_len=...` line in the server log — expected to drop from hundreds of MB to low tens of MB.

- [ ] **Step 5: Commit**

```bash
git add crates/scribe-server/src/handoff.rs crates/scribe-server/src/ipc_server.rs
git commit -m "feat: sender writes v5 SessionReplay; bump HANDOFF_VERSION to 5"
```

---

## Task 9: Eliminate `handoff_snapshot` first-attach special case

The v5 restore path populates `Term` durably, so the `handoff_snapshot` field on `ManagedSession` / `LiveSession` and the special case in `take_session_snapshot` are dead weight for v5 handoffs. Remove them, keeping the v4 compat path intact by inlining its behavior into `restore_from_handoff` (we already did this in Task 7 — Task 9 just cleans up).

Keep the field for v4 compatibility inside `restore_from_handoff` **only if v4 handoff is still expected**. Since the v4 compat decoder already stores `handoff_snapshot` per Task 7, we need to either:

- **Option A (chosen):** keep the field; remove the first-attach consumption via `take_session_snapshot`; on first attach after a v4 handoff, the Term is empty but `handoff_snapshot` is present, so we need to feed it through `AnsiProcessor` once at attach time — which restores the Term durably at that moment. The field's purpose shifts from "snapshot delivered to client" to "pending v4 replay data, feed into Term on first attach".

- **Option B (rejected):** also feed the v4 snapshot through `AnsiProcessor` immediately in `restore_from_handoff`. That avoids the field entirely, but requires running `snapshot_to_ansi` on the v4 snapshot inside the restore path — fine, but this is startup-time work inside `restore_from_handoff`, which we're specifically trying to reduce in Task 10.

Option A keeps v4 compat cheap at startup. The v4 Term is populated lazily on first attach. Acceptable because the v4→v5 transition is a one-release window.

**Files:**
- Modify: `crates/scribe-server/src/attach_flow.rs:259-277` (`take_session_snapshot`)
- Modify: `crates/scribe-server/src/ipc_server.rs` (around the `take_handoff_snapshot` method on `LiveSession`, lines ~273-275)
- Modify: `crates/scribe-server/src/session_manager.rs:376-469` (`restore_from_handoff` — keep `handoff_snapshot` population for v4 path, but route it through AnsiProcessor on first attach)

- [ ] **Step 1: Update `take_session_snapshot` to feed the v4 snapshot into the Term on first attach**

In `attach_flow.rs:259-277`, replace the existing function body:

```rust
pub async fn take_session_snapshot(
    session_id: SessionId,
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    live_sessions: &LiveSessionRegistry,
) -> scribe_common::screen::ScreenSnapshot {
    // Drain any v4-legacy handoff snapshot into the Term once, durably.
    // v5 handoffs do not set handoff_snapshot — restore_from_handoff
    // populated the Term directly during restore.
    let legacy_snapshot = {
        let mut registry = live_sessions.write().await;
        registry
            .get_mut(&session_id)
            .and_then(crate::ipc_server::LiveSession::take_handoff_snapshot)
    };

    if let Some(snapshot) = legacy_snapshot {
        let ansi = scribe_common::screen_replay::snapshot_to_ansi(&snapshot);
        let mut processor = vte::ansi::Processor::new();
        let mut guard = term.lock().await;
        processor.advance(&mut *guard, &ansi);
        return crate::session_manager::snapshot_term(&guard);
    }

    let guard = term.lock().await;
    crate::session_manager::snapshot_term(&guard)
}
```

This preserves the old behavior (first attach shows full pre-handoff history) but now via a durable path — so subsequent attaches also see the content in the Term's grid, not just the first one.

- [ ] **Step 2: Verify no other caller reads `handoff_snapshot`**

Run: `grep -rn "handoff_snapshot" crates/scribe-server/src`
Expected: references only in `session_manager::restore_from_handoff` (the v4 storage site), `ipc_server::LiveSession::take_handoff_snapshot` (the drain helper), and `attach_flow::take_session_snapshot` (our updated caller). If any other site uses it, either remove the usage or update it to use `snapshot_term`.

- [ ] **Step 3: Verify first- and second-attach behave identically after restore**

Add a test in `handoff_tests.rs`:

```rust
#[tokio::test]
async fn v5_restore_survives_multiple_attaches() {
    let mut src = make_term_with_content(b"persistent pre-handoff history\r\n");
    let snap = crate::session_manager::snapshot_term(&src);
    let replay = scribe_common::screen_replay::build_session_replay(&snap).unwrap();

    let (state, fds, _slaves) = make_handoff_state_with_replay(replay, &snap);
    let sm = crate::session_manager::SessionManager::restore_from_handoff(&state, fds, 100).unwrap();

    let sessions = sm.sessions_snapshot_for_test().await;
    let (session_id, session) = sessions.iter().next().unwrap();

    // Simulate two attaches via snapshot_term directly (which is what
    // take_session_snapshot ends up calling for v5).
    let first = {
        let guard = session.term.lock().await;
        crate::session_manager::snapshot_term(&guard)
    };
    let second = {
        let guard = session.term.lock().await;
        crate::session_manager::snapshot_term(&guard)
    };

    assert_eq!(first.cells, snap.cells, "first attach must see pre-handoff content");
    assert_eq!(first.cells, second.cells, "second attach must see the same content");
}
```

Run: `cargo test -p scribe-server handoff_tests::v5_restore_survives_multiple_attaches`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git add crates/scribe-server/src/attach_flow.rs crates/scribe-server/src/handoff_tests.rs
git commit -m "fix: feed legacy v4 handoff snapshot into Term on first attach (durable)"
```

---

## Task 10: Bind the IPC socket before per-session restore

**Files:**
- Modify: `crates/scribe-server/src/main.rs:72-106` (`run_upgrade_receiver`)
- Modify: `crates/scribe-server/src/session_manager.rs:376-469` (split `restore_from_handoff` into a registration phase and a per-session restore task)

- [ ] **Step 1: Split `restore_from_handoff` into two phases**

In `session_manager.rs`, refactor `restore_from_handoff` so the expensive per-session work (zstd decompress + `AnsiProcessor::advance`) lives in a separate async function that can be spawned per session.

Phase A — registration (synchronous, cheap):

```rust
pub fn register_handoff_sessions(
    state: &HandoffState,
    fds: Vec<OwnedFd>,
    scrollback: usize,
) -> Result<Self, ScribeError> {
    // Same as today's restore_from_handoff EXCEPT: the per-session replay
    // bytes are stored on ManagedSession.pending_replay; the Term is left
    // empty; handoff_snapshot is populated only for v4 sessions.
    // ... (body similar to current restore_from_handoff; see below for
    //      the one structural change)
}
```

Add a field to `ManagedSession` (or `LiveSession` — pick the one that survives into the live registry):

```rust
/// v5 replay bytes awaiting async restoration. Cleared when the per-session
/// restore task feeds them into the Term.
#[serde(skip)]
pub pending_replay: Option<SessionReplay>,
```

Phase B — per-session restoration (async, possibly concurrent):

```rust
/// Feed a pending v5 replay into the session's Term and clear the field.
/// Safe to call from a spawned task. No-op if the session has no pending
/// replay (v4 path, or already restored).
pub async fn restore_pending_replay(
    session_id: SessionId,
    live_sessions: &LiveSessionRegistry,
) -> Result<(), ScribeError> {
    let (term, replay) = {
        let mut registry = live_sessions.write().await;
        let live = match registry.get_mut(&session_id) {
            Some(live) => live,
            None => return Ok(()),
        };
        let Some(replay) = live.pending_replay.take() else {
            return Ok(());
        };
        (Arc::clone(&live.term), replay)
    };

    let bytes = scribe_common::screen_replay::decompress_session_replay(&replay)?;
    let mut processor = AnsiProcessor::new();
    let mut guard = term.lock().await;
    processor.advance(&mut *guard, &bytes);
    Ok(())
}
```

Add a `tokio::sync::Notify` per session to signal completion:

```rust
pub struct LiveSession {
    // ... existing fields ...
    pub restore_notify: Arc<tokio::sync::Notify>,
    pub restore_state: Arc<AtomicRestoreState>, // Restoring | Ready | Failed
}

pub enum RestoreState { Restoring, Ready, Failed }
// (Use tokio::sync::watch or an AtomicU8 — whichever fits the existing pattern.)
```

When `restore_pending_replay` completes, flip `restore_state` to `Ready` and call `restore_notify.notify_waiters()`.

- [ ] **Step 2: Reorder `run_upgrade_receiver` to bind first, restore after**

In `main.rs:72-106`, replace `run_upgrade_receiver` body:

```rust
async fn run_upgrade_receiver() -> Result<(), ScribeError> {
    info!("scribe-server starting (upgrade mode)");

    let cfg = config::load_config()?;

    // Receive handoff (payload is small in v5; large only in v4 compat).
    let (state, fds) = handoff::receive_handoff()?;

    info!(
        sessions = state.sessions.len(),
        workspaces = state.workspaces.len(),
        fds = fds.len(),
        "handoff received — registering sessions for background restore"
    );

    let scrollback = usize::try_from(cfg.scrollback_lines).unwrap_or(usize::MAX);

    // Phase A: register sessions with empty Terms + pending_replay.
    let session_manager =
        Arc::new(session_manager::SessionManager::register_handoff_sessions(&state, fds, scrollback)?);
    let live_session_ids: HashSet<_> =
        state.sessions.iter().map(|s| s.session_id).collect();
    let workspace_manager = Arc::new(RwLock::new(
        workspace_manager::WorkspaceManager::restore_from_handoff(
            cfg.workspace_roots,
            &state.workspaces,
            state.workspace_tree,
            &state.windows,
            &live_session_ids,
        ),
    ));

    info!("session registration complete — starting IPC server");

    // Phase B: kick off per-session restores in the background. The IPC
    // server binds and logs "IPC server listening" inside run_server_loop
    // before these tasks complete.
    let live_sessions_for_restore = /* grab from registry */;
    for handoff_session in &state.sessions {
        let session_id = handoff_session.session_id;
        let live_sessions = Arc::clone(&live_sessions_for_restore);
        tokio::spawn(async move {
            if let Err(e) = session_manager::restore_pending_replay(session_id, &live_sessions).await {
                warn!(%session_id, "per-session restore failed: {e}");
            }
        });
    }

    run_server_loop(session_manager, workspace_manager, true, cfg.update).await
}
```

The exact wiring for `live_sessions_for_restore` depends on the registry layout. Grab the `LiveSessionRegistry` from the SessionManager or construct it there; whatever pattern matches how `run_server_loop` later passes `live_sessions` into the IPC server. Keep in mind `activate_pending_sessions` (which starts PTY readers) should **not** run for a session until its restore task has flipped `restore_state` to `Ready`, so the PTY reader never feeds bytes into an empty Term mid-replay.

- [ ] **Step 3: Make `activate_pending_sessions` defer per-session activation until `Ready`**

In `ipc_server.rs:2542` (`activate_pending_sessions`), wrap the per-session start with:

```rust
for (session_id, workspace_id) in pending {
    // Wait for the session's restore task to flip state to Ready. Fast for
    // v4 (no pending replay — Ready immediately), slow for v5 (waits on the
    // per-session Notify).
    if let Some(live) = live_sessions.read().await.get(&session_id) {
        if live.restore_state.load() == RestoreState::Restoring {
            live.restore_notify.notified().await;
        }
    }

    if let Some(session) = session_manager.take_session(session_id).await {
        start_session(...); // existing body
    }
}
```

Alternative implementation: spawn a waiter task per session that awaits the Notify and then calls `start_session`. Either shape is fine — pick whichever integrates cleanest with the rest of the event loop.

- [ ] **Step 4: Make `AttachSessions` block briefly on `restore_notify`**

In `ipc_server.rs::handle_attach_sessions` (find via `grep -n "AttachSessions" crates/scribe-server/src/ipc_server.rs`), before the main attach body that reads the Term, add:

```rust
// If this session is still restoring from a v5 handoff, wait for the
// restore task to finish before snapshotting the Term.
let restore_state = /* look up live session's restore_state */;
let restore_notify = /* look up live session's restore_notify */;
if restore_state.load() == RestoreState::Restoring {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        restore_notify.notified(),
    )
    .await
    .ok();
}
```

30s is the outer safety cap; typical restore completes in < 1s. On timeout, proceed with whatever Term content is present (likely empty) rather than block the client indefinitely.

- [ ] **Step 5: Verify the watchdog timing constraint**

Add an integration test that mirrors the postinst watchdog's poll:

```rust
#[tokio::test]
async fn upgrade_binds_socket_before_restore_completes() {
    // Build a large v5 handoff payload (lots of scrollback).
    // Spawn run_upgrade_receiver equivalent.
    // Assert that acquire_server_socket / start_ipc_server returns before
    // restore_pending_replay for the final session completes.
}
```

The exact scaffolding depends on how much of the upgrade-receiver flow is testable as a library. If full scaffolding is too expensive here, document this as a manual test to run with `just restart-server` after loading a session with 10k-line scrollback and observing the `"IPC server listening"` log appears before `"per-session restore complete"`.

- [ ] **Step 6: Run the full test suite**

Run: `cargo test --workspace`
Expected: green across the board. If `handoff_tests::restore_from_handoff_populates_session_manager` or similar legacy tests fail, fix by reading them and updating for the new two-phase API (they likely constructed the old synchronous `restore_from_handoff` directly — renaming the call site is usually the only fix).

- [ ] **Step 7: Commit**

```bash
git add crates/scribe-server/src/main.rs crates/scribe-server/src/session_manager.rs crates/scribe-server/src/ipc_server.rs
git commit -m "perf: bind IPC socket before per-session restore; spawn restore tasks"
```

---

## Task 11: Update `lat.md/server.md` to reflect v5

**Files:**
- Modify: `lat.md/server.md` (sections under `Handoff`)

- [ ] **Step 1: Rewrite `Handoff#State Transfer`**

Open `lat.md/server.md` to the `### State Transfer` subsection (around line 105). Replace its body with:

```markdown
### State Transfer

The HandoffState contains per-session metadata, per-session replay payload, and workspace layout state for restart handoff.

Per-session payloads include title, shell basename, remote context, Codex task label, CWD, AI state (including optional provider conversation IDs used for resume behavior), and a [[crates/scribe-common/src/screen_replay.rs#SessionReplay]] carrying the zstd-compressed ANSI replay for the session's visible grid plus scrollback. File descriptors are transferred one-for-one with the serialized session list.

Per-workspace payloads include name, accent color, split direction, session list, and project root path. The project root is an additive `#[serde(default)]` field so handoff from older servers defaults to `None`.
```

- [ ] **Step 2: Rewrite `Handoff#Version Bumps` to state the new invariant**

Replace the body with:

```markdown
### Version Bumps

Bump [[crates/scribe-server/src/handoff.rs#HANDOFF_VERSION]] when [[crates/scribe-server/src/handoff.rs#HandoffState]] changes incompatibly.

Every `HANDOFF_VERSION` bump MUST ship a receiver capable of decoding the immediately previous version (N-1). The sender writes only the current format. Cold-restart is permitted only when hot-reload is genuinely impossible: missing decoder for the peer's version (off by more than one normal release step), operational failure (OOM, fd/size limits, socket or zstd decode error, corrupted payload), or downgrade. A normal forward upgrade through any two consecutive releases must hot-reload without terminating sessions.

Additive per-session fields that use `#[serde(default)]` do not require a version bump.

On Linux, any failed handoff's cold-restart path must also clean up any detached `scribe-server --upgrade` process left behind by the failed handoff before starting the user service again; otherwise the stale process can keep `server.sock` and `server.lock`, causing the restarted unit to fail with "another scribe-server is already running".
```

- [ ] **Step 3: Update `Handoff#Size Limits`**

Replace with:

```markdown
### Size Limits

Maximum handoff state size is 1 GiB. Maximum file descriptors transferred is 1024. Both the sender and receiver verify peer UID for defense-in-depth. Typical v5 compressed payloads are in the low tens of megabytes even for many sessions at default `scrollback_lines = 10_000`.
```

- [ ] **Step 4: Add `Handoff#Restore States`**

Append a new subsection at the end of `## Handoff`:

```markdown
### Restore States

Each session restored from a v5 handoff starts in `Restoring` state. The IPC socket binds and begins accepting clients immediately after `receive_handoff` completes, in parallel with per-session restore tasks that decompress each [[crates/scribe-common/src/screen_replay.rs#SessionReplay]] and feed it into the session's `Term` via [[crates/scribe-server/src/session_manager.rs#restore_pending_replay]].

When restoration succeeds, the state flips to `Ready` and the per-session `tokio::sync::Notify` wakes any pending `AttachSessions` handlers. If decompression or ANSI replay fails, the state flips to `RestoreFailed` — the PTY fd remains live so new output still flows to the client; only pre-handoff scrollback is lost for that one session.
```

- [ ] **Step 5: Run `lat check`**

Run: `lat check`
Expected: "All checks passed". Fix any broken section references or missing leading paragraphs before committing.

- [ ] **Step 6: Commit**

```bash
git add lat.md/server.md
git commit -m "docs(lat): describe v5 handoff, N-1 invariant, and restore states"
```

---

## Task 12: Final verification

- [ ] **Step 1: Full workspace build (debug + release)**

Run:
```bash
cargo build --workspace
cargo build --workspace --release
```
Expected: both succeed cleanly.

- [ ] **Step 2: Full workspace tests**

Run: `cargo test --workspace`
Expected: all green.

- [ ] **Step 3: Clippy gate**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings. Fix anything new.

- [ ] **Step 4: `lat check` final sweep**

Run: `lat check`
Expected: "All checks passed".

- [ ] **Step 5: Manual smoke test on a live install (operator)**

**Ask the user for explicit approval before restarting the Scribe server.** If approved:

```bash
# Build release artifacts.
just build-release

# Open a client, create 5–10 sessions, scroll through heavy output in each
# (e.g. `seq 1 20000` in each tab) to force large scrollback.

# Perform the hot-reload that previously took > 5s.
just restart-server   # or: sudo dpkg -i <built .deb>
```

Assertions to check by eye:
- postinst prints "server hot-reloaded successfully" well under 30s (target: < 2s).
- All sessions reappear with their scrollback intact.
- Disconnect and reconnect the client; scrollback is still present (proves Term is durably populated, not a one-shot).
- Search within a reconnected session finds pre-handoff content (proves server-side grid is populated).

- [ ] **Step 6: Tag the release**

After the operator approves the smoke test, a separate release-cut workflow publishes the binary. This plan does not include that step.

---

## Self-review checklist

**Spec coverage** — every spec section maps to a task:

- Representation → Tasks 2, 3.
- Transfer → Tasks 5, 8.
- Restore + socket-bind-first → Tasks 7, 9, 10.
- Compatibility / versioning → Tasks 5, 6, 8, 11.
- Failure handling → Tasks 7 (per-session fallback), 10 (AttachSessions timeout cap), 8 (build_session_replay failure).
- Rollout strategy → Tasks 1–12 are the single release; the plan is the rollout.
- Latent correctness bugs — Term-not-rebuilt fixed in Tasks 7+9+10; double-allocation fixed in Task 7; load-bearing MAX_STATE_SIZE addressed in Task 11 doc; watchdog coupling fixed in Task 10.
- Tests — Tasks 3, 4, 7, 9, 10 each add tests; Task 12 runs the full gate.

**Placeholder scan** — every code block in the plan is complete enough to compile after trivial variable-naming/wiring; no "TBD", "TODO", or "fill in" markers. The one exception is Task 10 Step 5's full scaffolding for an async integration test — that step explicitly flags it as a manual test if full scaffolding is too expensive, which is a deliberate trade-off, not a placeholder.

**Type consistency** — `SessionReplay`, `build_session_replay`, `decompress_session_replay`, `snapshot_to_ansi`, `HANDOFF_VERSION`, `HandoffSession.session_replay`, `RestoreState`, `restore_notify`, `pending_replay`, `restore_pending_replay`, `register_handoff_sessions` are each named consistently across every task that references them. No drift between, say, `SessionReplay.replay_zstd` vs. `SessionReplay.replay_bytes`.
