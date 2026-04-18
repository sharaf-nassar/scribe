# Scribe hot-reload handoff redesign (v5)

**Date:** 2026-04-17
**Status:** Design — approved direction, no implementation yet
**Scope:** `crates/scribe-server/src/handoff.rs`, `crates/scribe-server/src/session_manager.rs`, `crates/scribe-server/src/ipc_server.rs`, `crates/scribe-server/src/attach_flow.rs`, `crates/scribe-server/src/main.rs`, `crates/scribe-common/src/screen.rs`, `crates/scribe-client/src/main.rs` (replay helper relocation), `dist/debian/postinst`, `lat.md/server.md`

## Problem

The current hot-reload handoff serializes each live session's full `ScreenSnapshot` (flat `Vec<ScreenCell>` for visible grid plus a separate `Vec<ScreenCell>` for scrollback) as MessagePack, then sends it to the new server as a single length-prefixed blob over a Unix socket. On 2026-04-17 at 13:36 PDT, an upgrade payload was observed at `state_len = 440_618_573` bytes across 10 sessions with default `scrollback_lines = 10_000`. The Debian maintainer script's watchdog timed out at 5 seconds, the new receiver was killed, and `postinst` cold-restarted the server — terminating every live terminal.

Raising the watchdog to 30 seconds (committed locally at the time of this spec) masks the immediate failure but leaves the underlying problem: the handoff size grows linearly with total cells across all sessions, the receiver blocks on full deserialize + `Term` allocation before binding the IPC socket, and the restored `Term` is not durably populated with pre-upgrade scrollback.

## Non-goals

- **No capping, dropping, or degrading scrollback.** Full per-cell fidelity must be preserved for every live terminal, across the configured `scrollback_lines` range (default 10_000, max 100_000).
- **No protocol changes to the client-facing IPC wire.** `ServerMessage::ScreenSnapshot` and the `ScreenSnapshot` type used on reconnect stay as-is. This spec targets only the server-to-server handoff channel.
- **No redesign of the PTY fd passing.** `SCM_RIGHTS` transfer of master fds is unchanged.

## Root cause

The bottleneck is **not** the physical transport — it's the combination of a verbose encoding applied to very large structures and a restore path that serializes every step behind the IPC socket bind.

1. **Wire encoding overhead.** MessagePack encodes each `ScreenCell` as a named-field map (`c`, `fg`, `bg`, `flags`), and `CellFlags` as a deep nested struct of eight named booleans. Typical cost per cell: 30–50 bytes, for content that's usually a single ASCII byte. A 10-session, 10 000-line scrollback payload at 80–200 cols produces hundreds of megabytes of mostly-whitespace cell maps.
2. **Sequential serialization under term locks.** `serialize_live_for_handoff` (`ipc_server.rs:2483`) iterates live sessions one at a time, locking each `live.term`, calling `snapshot_term` (`session_manager.rs:645`) to clone every cell into fresh `Vec<ScreenCell>` allocations, then unlocking. PTY readers block for the duration of each session's snapshot.
3. **Single-blob send and decode.** `prepare_handoff_payload` (`handoff.rs:255`) calls `rmp_serde::to_vec(&state)` → one contiguous ~440 MB `Vec<u8>` → one `sendmsg` with a 440 MB `IoSlice`. The receiver does `rmp_serde::from_slice` into millions of `ScreenCell` structs, each a small heap allocation.
4. **Double allocation on restore.** `restore_from_handoff` (`session_manager.rs:376`) stores the received state's snapshot twice — once in the decoded `HandoffState.sessions[i].snapshot` and again via `handoff_session.snapshot.clone()` into `ManagedSession.handoff_snapshot` (line 450). Peak memory briefly doubles.
5. **Socket bind after restore.** `run_upgrade_receiver` (`main.rs:72`) calls `receive_handoff()` → `restore_from_handoff()` → `run_server_loop()` → `acquire_server_socket()`. The log line `"IPC server listening"` is emitted only at step 3 (`ipc_server.rs:348`), so the maintainer-script watchdog can't see liveness until every upstream step is complete. This exposes the full decode + allocation cost on the critical path.

## Approach

Adopt three orthogonal changes in one release:

- **Format.** Replace per-cell MessagePack with a per-session ANSI replay byte stream, zstd-compressed. Reuse the existing `snapshot_to_ansi` logic already proven on the client reconnect path.
- **Restore.** Feed each replay through an `AnsiProcessor` into a fresh `Term` so the server-side Term is durably populated. Eliminate the `handoff_snapshot` field and the first-attach special case.
- **Socket ordering.** Bind the IPC socket on the new server before per-session restore runs. Per-session restore becomes a background task; clients can attach before their session's restore completes and receive a `Restoring` marker until the Term is ready.

Expected impact on the observed 440 MB payload:

- ANSI replay of typical scrollback (ASCII content, long runs of spaces, SGR-diffed) is 3–10× smaller than per-cell MessagePack.
- zstd over ANSI (highly repetitive — lots of spaces, repeated prompt strings, runs of `ESC[0m`) typically delivers another 5–10× on top.
- Realistic reduction: 440 MB → ~5–30 MB. Watchdog passes well inside 1 second for any plausible session count.

## Representation

New wire type in `scribe-common/src/screen_replay.rs` (or reused from existing `snapshot_to_ansi` location):

```rust
pub struct SessionReplay {
    pub cols: u16,
    pub rows: u16,
    pub scrollback_rows: u32,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cursor_style: CursorStyle,
    pub cursor_visible: bool,
    pub alt_screen: bool,
    /// zstd-compressed ANSI byte stream. When decompressed and fed to a
    /// fresh Term via AnsiProcessor, reconstructs the full scrollback
    /// plus visible grid, SGR state, and wrap markers.
    pub replay_zstd: Vec<u8>,
}
```

The ANSI replay body (before zstd) is what `snapshot_to_ansi` already produces for client reconnect:

- `\x1b[?1049h` if alt-screen.
- Home + clear + SGR reset.
- Scrollback rows oldest-first, with `\r\n` separators except between soft-wrap runs (preserves `WRAPLINE` via no CRLF between wrapped rows).
- Visible grid rows, same separator rule.
- Final SGR reset, cursor position (`\x1b[R;CH`), DECSCUSR for cursor style, `\x1b[?25h` if cursor visible.
- SGR state diff across cells within a row and across rows (current implementation already does this).

The replay output is identical bytes to what the client already consumes on reconnect — we're reusing a tested primitive, not inventing a new format.

`HandoffSession` gets one new field:

```rust
pub struct HandoffSession {
    // ... existing fields ...
    /// v5+ replay. Mutually exclusive with `snapshot`: senders at v5 write
    /// this; senders at v4 write `snapshot`.
    #[serde(default)]
    pub session_replay: Option<SessionReplay>,
    /// v4 legacy snapshot. Retained so v5 receivers can accept v4 payloads
    /// during forward upgrades. v5+ senders leave this None.
    #[serde(default)]
    pub snapshot: Option<ScreenSnapshot>,
}
```

## Transfer

The handoff wire protocol is unchanged:

```
magic "SCRIBE_UPGRADE" → length-prefixed msgpack HandoffState → SCM_RIGHTS PTY fds → "ACK"
```

What changes is the body of `HandoffState`: `HandoffSession.snapshot` is no longer populated by v5 senders; `HandoffSession.session_replay` carries the compressed ANSI. The total msgpack encode size drops dramatically (the big `Vec<ScreenCell>` is replaced by a `Vec<u8>` holding compressed bytes). The receiver decompresses the replay per-session in a streaming loop, so there's never a single 440 MB `String` or `Vec<u8>` materialized on either side.

No memfd transport in v5. The post-compression payload is small enough (typically tens of megabytes) that `sendmsg` is no longer a bottleneck. memfd remains available as a follow-up transport optimization if future profiling warrants it.

## Restore

`run_upgrade_receiver` (`main.rs:72`) is reordered:

1. Load config.
2. `receive_handoff()` — pulls the (now small) state blob and the PTY fds. No per-session decompression or Term construction here.
3. Build a `SessionManager` with each session pre-registered in a `Restoring` state. Each entry owns the PTY fd, the `SessionReplay` blob (or v4 `ScreenSnapshot` in the compat path), and an empty `Term`.
4. `acquire_server_socket()` + `start_ipc_server()` — socket binds, `"IPC server listening"` is logged. Watchdog passes here.
5. Spawn per-session restore tasks concurrently (one per session). Each task:
   - Construct a `Term` with `scrolling_history = cfg.scrollback_lines`.
   - Streaming-decompress the zstd replay.
   - Feed the decompressed ANSI into a local `AnsiProcessor` bound to the Term.
   - On completion, mark the session `Ready` and signal a per-session `tokio::sync::Notify`.
6. PTY read loop for each session starts as soon as the session is `Ready` (not before). Until then, bytes arriving on the PTY fd are buffered in a small bounded channel and drained into the Term after restore completes so no output is lost.

Client-visible state during restore:

- `AttachSessions` for a session still in `Restoring` receives a new `ServerMessage::SessionRestoring { session_id }` or an equivalent `SessionState` field on `SessionInfo` that the client renders as "restoring scrollback…".
- `ListSessions` returns the sessions with their current state; the client can show the workspace layout immediately even before individual sessions are ready.
- Once each session's restore completes, the server sends the usual initial `ScreenSnapshot` as it would for any attach. Because the Term is durably populated, `take_session_snapshot` calls `snapshot_term(&term)` directly — no first-attach special case.

The `ManagedSession.handoff_snapshot: Option<ScreenSnapshot>` field is removed. The helper `take_handoff_snapshot` on `LiveSession` is removed. `attach_flow::take_session_snapshot` loses its handoff branch and calls `snapshot_term(&term)` unconditionally.

## Compatibility and versioning

Bump `HANDOFF_VERSION` from 4 to 5.

**Invariant (new, to be documented in `lat.md/server.md` under `Handoff`):**

> Every `HANDOFF_VERSION` bump MUST ship a receiver capable of decoding the immediately previous version (N-1). The sender writes only the current format. Cold-restart is permitted only when hot-reload is genuinely impossible: missing decoder for the peer's version (off by more than one normal release step), operational failure (OOM, fd/size limits, socket or zstd decode error, corrupted payload), or downgrade. A normal forward upgrade through any two consecutive releases must hot-reload without terminating sessions.

Concrete v4→v5 behavior:

| Scenario | Old sender version | New receiver version | Receiver path | Result |
|---|---|---|---|---|
| Normal forward upgrade | 4 | 5 | Compat decoder reads `snapshot: Some(...)` (v4 legacy) and restores through the pre-existing `handoff_snapshot` path | Hot-reload succeeds |
| Re-upgrade on v5 (dev or same-version reload) | 5 | 5 | Primary decoder reads `session_replay: Some(...)`, feeds through AnsiProcessor into Term | Hot-reload succeeds |
| Downgrade (user installs an older version on top of v5) | 5 | 4 | v4 receiver has no v5 decoder; its strict version check fails | Cold-restart (correct for downgrade) |
| Skip-a-release (auto-update off for a long time) | 4 | 6 (hypothetical) | If v6 only carries v5 decoder, version gap > 1 | Cold-restart, clear log message |
| Pre-v4 sender | ≤3 | 5 | No decoder for v3 or older | Cold-restart (pre-v4 already cold-restarts today) |

Code change to the receiver in `handoff.rs::receive_handoff`: replace the strict `state.version != HANDOFF_VERSION` gate with a branch that accepts `HANDOFF_VERSION` and `HANDOFF_VERSION - 1`, routing each to the appropriate decoder. Any other version → same error path as today (receiver exits, old server loops, postinst cold-restarts).

Sender changes in `serialize_live_for_handoff`:

- Produce `session_replay` for each session: call `snapshot_term` (for the grid + scrollback data), convert via `snapshot_to_ansi` (relocated to `scribe-common`), compress with zstd.
- Leave `snapshot` at `None`.
- Set `HandoffState.version = 5`.

## Failure handling

- **Per-session replay decode failure** (zstd error, AnsiProcessor panic captured by a `std::panic::catch_unwind` in the restore task, or the replay is truncated): mark the session `RestoreFailed`, keep the PTY fd live so new output still flows to the client, emit a client-visible notice (`ServerMessage::SessionRestoreFailed { session_id, reason }`). The session continues working from the point of restore forward; only pre-upgrade scrollback is lost for that one session. Do **not** abort the whole handoff.
- **Whole-handoff failure** (socket error mid-transfer, `state_len > MAX_STATE_SIZE`, fd count mismatch, version gap > 1): new server exits cleanly, old server loops back to accept, postinst cold-restarts. Same as today.
- **Decoder panic during restore task**: catch, log, mark the session `RestoreFailed`, keep the rest of the handoff healthy.
- **PTY byte buffering overflow during restore**: if the bounded channel for post-bind PTY bytes fills before restore completes, drop the oldest buffered bytes and log. This is a degenerate case (shell printing megabytes during the few-hundred-ms restore window). The alternative — unbounded buffering — risks OOM.
- **`MAX_STATE_SIZE`**: keep at 1 GiB for v5. In practice the new format makes 100 MiB a realistic worst case; consider lowering to 256 MiB in a later release once telemetry shows typical sizes.

## Rollout (single release)

One `scribe` release ships the full change. Implementation order within that release:

1. **Move `snapshot_to_ansi` + supporting helpers** (`write_sgr`, `SgrState`, `write_snapshot_row`, `row_wraps`, `write_color_sgr`) from `crates/scribe-client/src/main.rs` to `crates/scribe-common/src/screen_replay.rs`. Client consumes it from there. No behavior change. This is a standalone refactor that can merge independently.
2. **Round-trip test harness.** `Term` → `snapshot_to_ansi` → `zstd` → `zstd-decode` → `AnsiProcessor` on a fresh `Term` → assert grid + scrollback match byte-for-byte. Corpus includes ASCII text, SGR-rich output, wide chars (CJK, emoji), soft-wrapped long lines, cursor in various positions, alt-screen apps. Gate merging the restore-path change on this test.
3. **Add `SessionReplay` + `session_replay` field** to `HandoffState` as additive. Receiver accepts both v4 snapshot and v5 replay. Sender still writes v4 snapshot. No version bump yet. Shippable: no user-visible change.
4. **Switch sender to v5.** `serialize_live_for_handoff` writes `session_replay`, leaves `snapshot` None. Bump `HANDOFF_VERSION` to 5. v4→v5 upgrade now uses the compat decoder path.
5. **Replace restore path.** In `restore_from_handoff`, when a v5 payload is present, feed the replay into a fresh Term via AnsiProcessor during per-session restore. Remove `handoff_snapshot` field from `ManagedSession`, remove `take_handoff_snapshot`, remove the handoff branch in `attach_flow::take_session_snapshot`.
6. **Socket-bind-first ordering.** Reorder `run_upgrade_receiver` so that `acquire_server_socket` + `start_ipc_server` run before per-session restore; per-session restore becomes spawned tasks; introduce `SessionState::Restoring` on the client protocol; buffer late-arriving PTY bytes into the Term post-restore.

Steps 1 and 2 are no-op refactors; 3 is additive; 4, 5, 6 land together in the version-bump release. The invariant in the compat section ensures step 4's version bump does not cold-restart anyone.

Later release (not this spec, but anticipated): drop the v4 `snapshot` field and the compat decoder branch once telemetry or elapsed time confirms no v4 senders remain in the wild. That later release carries a v5 + v6 decoder, per the invariant.

`lat.md/server.md` updates in this release:

- `Handoff#State Transfer`: describe `SessionReplay` instead of per-cell snapshot.
- `Handoff#Protocol`: add the socket-bind-first ordering detail.
- `Handoff#Version Bumps`: replace the current strict-match wording with the "receiver supports N-1" invariant.
- `Handoff#Size Limits`: note that typical compressed payloads are tens of megabytes, not hundreds.
- New child section `Handoff#Restore States` documenting `Restoring` / `Ready` / `RestoreFailed` and what clients do with them.

## Latent correctness bugs called out

Fixed by this spec:

1. **`Term` not rebuilt after handoff.** `restore_from_handoff` (`session_manager.rs:376`) constructs an empty Term and stashes `ScreenSnapshot` in `ManagedSession.handoff_snapshot`. `attach_flow::take_session_snapshot` (`attach_flow.rs:259`) consumes it on first attach. After that, the Term is empty of pre-upgrade history — a client disconnect/reconnect, a second window attaching, or any server-side grid read (future search features, screen capture tests) silently loses scrollback. **Fix:** step 5 of rollout populates the Term durably via AnsiProcessor; no more `handoff_snapshot` field.
2. **Double allocation on restore.** `handoff_session.snapshot.clone()` at `session_manager.rs:450` briefly doubles peak memory. **Fix:** step 5 removes the clone; replay bytes are consumed once and dropped.
3. **`MAX_STATE_SIZE = 1 GiB` is load-bearing.** With today's format at 100 000 scrollback × 200 cols × 10 sessions × ~40 bytes/cell ≈ 8 GB, the receiver silently refuses the upgrade and cold-restarts. **Fix:** v5 format reduces the realistic ceiling by two orders of magnitude.
4. **Watchdog tied to full restore.** Debian watchdog grep's for `"IPC server listening"`, which is emitted only after restore completes (`ipc_server.rs:348`). **Fix:** step 6 moves the bind before restore.

Documented, not fixed:

5. **Alt-screen scrollback policy.** `snapshot_term` deliberately drops scrollback for alt-screen sessions. This is intentional (alt grid history is resize artifact, not user content) and survives v5 unchanged. Document in `lat.md/server.md#Handoff` so future contributors don't reintroduce alt-screen scrollback accidentally.
6. **Serial per-session snapshot under Term lock.** `serialize_live_for_handoff` locks each term in turn. Cloning cells under the lock blocks PTY readers. v5 still holds the lock during `snapshot_term` (to get a consistent grid view), but the subsequent ANSI encoding and zstd compression happen outside the lock. Optional follow-up: parallelize per-session serialization (needs care re. ordering of fds vs. sessions).

## Testing

Minimum test coverage before merging step 5 or 6:

- Round-trip test harness (step 2 of rollout).
- Handoff integration test with 10 sessions, each at 10 000-line scrollback full of mixed content. Assert: post-handoff `snapshot_term` on each session produces the same cells as the original. Measure wire size.
- Handoff size regression bench gating CI: fail if uncompressed cell-volume × compression ratio goes above a threshold (e.g. 512 KiB per 10 000 lines × 80 cols for typical ASCII output).
- v4 sender → v5 receiver compat test. Construct a v4 payload (legacy format), feed to the v5 receiver, assert restored sessions work.
- Downgrade test: v5 sender → v4 receiver. Assert clean cold-restart with a version mismatch error. This test runs manually or in a dedicated compat job, since production downgrades are rare.
- Watchdog timing test on Linux: spawn a v5 server with 10 large-scrollback sessions, run through postinst emulation, assert `"IPC server listening"` appears within 5 seconds. Confirms the bind-first ordering works under realistic load.

## Open questions

- **N-2 decoder coverage.** The invariant requires N-1; is N-2 worth carrying so that users who skip a release (disabled auto-update for months) still hot-reload? The cost is one extra decoder branch per bump. Recommend: N-1 only for now, document the >1-release-gap cold-restart, revisit if telemetry shows it hurts.
- **Should `MAX_STATE_SIZE` be reduced?** Current 1 GiB is enough headroom for degenerate Unicode scrollback. Lowering to 256 MiB catches runaway bugs earlier. Recommend: defer until v5 has a quarter of telemetry.
- **Do we need per-session restore progress?** For 10 sessions at 10 000 lines the whole restore should complete in well under a second. If it doesn't, surfacing per-session progress to the client is easy (one `ServerMessage::RestoreProgress` per session). Punt until measured.
