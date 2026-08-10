# Scribe Beads board reader spike

This bounded spike measures a Scribe-owned board snapshot against three
installed-`bd` reads. The checked-in prototype is benchmark evidence only and
must not be wired into Scribe packaging or runtime as-is.

The helper pins Beads `5507f8466aa7` and deliberately uses its internal Go
packages. One embedded Dolt connector and one read transaction run Beads'
list, Ready, and Blocked logic, then partition the result with precedence
Done > Blocked > In Progress > Ready > Backlog. Output uses Scribe's
`format_version: 1` contract and returns exact counts plus at most `--limit`
items per queue.

Build with Go 1.26.5 and a native C compiler:

```bash
tools/scribe-beads-board/build.sh
target/release/scribe-beads-board --directory . --limit 8
```

The spike supports embedded Dolt only. It rejects missing databases,
server/proxied modes, maintenance-gate contention, and either main or
ignored/wisp schema cursors that differ from the pinned Beads source. Redirected
workspaces and source-database identity are unsupported: redirect discovery
does not preserve the source workspace's `dolt_database` selection. The fixed
list filter also omits custom or hooked statuses outside Beads' standard open,
in-progress, blocked, deferred, and closed set. Failures are plain stderr text,
not a versioned JSON error contract.

This program contains no migration, sync, push, or issue-mutation calls and
requests a read-only SQL transaction. That is intent, not a storage guarantee:
the internal `embeddeddolt.OpenSQL` connector is a raw read/write-capable API
and does not mechanically refuse writes. Safe fixture checks found no database
creation for missing storage and no version-control state change after a
snapshot, but the prototype is not an enforceably read-only reader.

Shipping would require adding Go/cgo builds to all four native jobs in
`.github/workflows/release.yml`, including the helper in the Debian assets and
macOS app bundle, and resolving its path from the client. Beads is MIT; bundled
Dolt components are Apache-2.0, so release packages must carry Beads' `LICENSE`
and `THIRD_PARTY_LICENSES` notices.

## Revised delivery direction

The speedup warrants a production reader, but not this raw prototype or a
mandatory base-package payload. The preferred design is a separately signed,
optional one-shot `scribe-beads` component. Scribe keeps a server-owned
memory/disk stale-while-revalidate cache, renders cached state immediately,
and falls back to installed `bd` when the component is absent, busy, timed out,
or schema-skewed. The helper is never persistent: one refresh runs per physical
database with a 500–750 ms deadline, then exits.

A disposable-fixture proof patched the pinned Dolt driver to enable its
engine-wide `IsReadOnly` guard and removed embedded telemetry. Existing queries
still completed in 0.15 s, while INSERT, CREATE TABLE, CREATE DATABASE, and
`DOLT_COMMIT` were rejected before execution. HEAD, staged/working roots,
`dolt_status`, and every existing file hash remained unchanged. Cooperative
gate creation and NBS timestamp-only metadata touches remain, as does the
roughly 150 ms exclusive embedded-Dolt lock window.

Production work must carry that minimal driver fork and still fix redirect
source-database identity, custom/hooked statuses, versioned errors, process
supervision, native artifacts, and license inventories. Deep Dolt size trimming
is not worthwhile: three bounded variants reduced the compressed artifact by
at most 1.7% because the storage engine dominates.

## Spike evidence

Measured 2026-08-09 on this 405-issue workspace. Each row is exactly 10 fresh
process samples; p95 uses nearest rank. Helper samples timed one
`target/release/scribe-beads-board --directory "$repo" --limit 8` process with
`/usr/bin/time -f '%e %M'`, writing JSON to a per-run temporary file. Each
installed-`bd` sample timed one fresh shell that sequentially ran this current
three-command fallback, with each JSON payload written to a separate temporary
file:

```bash
bd --readonly --json -C "$repo" list --all --limit 0 --skip-labels
bd --readonly --json -C "$repo" ready --limit 0
bd --readonly --json -C "$repo" blocked
```

| Reader | Median | p95 | Median peak RSS | Payload |
|---|---:|---:|---:|---:|
| `scribe-beads-board --limit 8` | 0.15 s | 0.18 s | 145,956 KiB | 2,896 B |
| Three installed `bd` reads | 1.46 s | 1.49 s | 238,142 KiB | 1,000,116 B |

The helper saves 1.31 s (89.73%) at median and 1.31 s (87.92%) at p95.
Median peak RSS falls 92,186 KiB (38.71%); bounded output is 99.71% smaller.
Both paths produced the same five queue counts.

Raw samples record wall seconds and peak RSS in KiB:

| Run | Helper seconds | Helper RSS | Three `bd` seconds | Three `bd` RSS |
|---:|---:|---:|---:|---:|
| 1 | 0.16 | 143,816 | 1.49 | 239,144 |
| 2 | 0.15 | 147,024 | 1.43 | 233,680 |
| 3 | 0.14 | 147,024 | 1.45 | 232,108 |
| 4 | 0.15 | 150,472 | 1.47 | 238,944 |
| 5 | 0.17 | 146,444 | 1.43 | 235,440 |
| 6 | 0.16 | 144,864 | 1.48 | 232,352 |
| 7 | 0.18 | 145,468 | 1.41 | 238,068 |
| 8 | 0.15 | 145,296 | 1.49 | 243,116 |
| 9 | 0.15 | 143,444 | 1.43 | 243,576 |
| 10 | 0.14 | 149,612 | 1.47 | 238,216 |

The measured stripped Linux x86_64 binary is 116,505,016 B; its zstd level 19
artifact is 23,138,436 B. That compressed helper alone is larger than Scribe's
current 19,096,700 B Debian package. Only Linux x86_64 was buildable locally.
Linux ARM64 lacks an ARM64 C compiler/sysroot; both macOS targets lack Apple
SDKs and osxcross. Existing native release runners could build those three
artifacts.

Embedded Dolt permits one process. A frozen helper holding its read transaction
made both an installed-`bd` read and a disposable-fixture write hit their
2-second timeout with “database is locked”; the write landed nothing. Normal
snapshots hold that lock for the measured ~150 ms, still creating a collision
window for unrelated `bd` work.

Recommendation: OPTIONAL COMPONENT CANDIDATE. Do not ship this prototype
unchanged or bundle it into base Scribe. Harden the small pinned fork, deliver
it separately, keep cache-first rendering, and retain installed `bd` as the
universal fallback.
