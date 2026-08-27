---
title: A test helper that writes, chmods, then execs races cargo's forks into ETXTBSY
date: 2026-08-27
component: scribe-client url_detect, scribe-server session_manager/shell_integration
tags: [tests, fixtures, etxtbsy, exec, race, cargo, fork, scaffolding]
problem_type: runtime-error
---

## Problem

Five `#[cfg(test)]` helpers across two crates followed the same shape: write a
small program to a temp dir, `chmod` it `0o755`, then have a spawned child
`execve` that exact path. Under `cargo test --workspace` these fail
intermittently with `ETXTBSY` (`Text file busy`).

Fixed by `a26a5f7` (bead `scribe-gqr3`) and `1a52155` (bead `scribe-c3rk.1`).

## Root cause

The hazard needs three conditions at once:

1. a file is written,
2. that same file is later `execve`'d,
3. other threads fork while the write is still in flight.

`cargo test` runs test threads concurrently and forks freely, so condition 3 is
always true in this repo. `fs::write` closes its own descriptor, so this is
**not** a leaked-descriptor bug in the helper — the race is that a concurrent
fork elsewhere in the process inherits a still-writable descriptor to the file,
and `execve` refuses to run a file any process holds open for writing.

Because it depends on unrelated sibling tests forking at the wrong moment, it
reproduces rarely, and it looks like flake rather than a defect.

### What is *not* a hazard

A file that is only **read** is safe. Dot-sourcing it, or passing it as an
argument to an interpreter (`sh driver.sh`, `fish --no-config driver.fish`),
never calls `execve` on that path.

Probing is also safe: `resolve_bd_executable_from`
(`crates/scribe-server/src/beads_board.rs:696`) does `fs::metadata` plus
`nix::unistd::access(candidate, AccessFlags::X_OK)` at
`crates/scribe-server/src/beads_board.rs:709` and never constructs a `Command`,
so its written stand-ins carry no race.

That distinction is why an audit that greps for `set_permissions` or an
executable `mode()` will over-report. Grep for the exec, not the chmod: only
sites where the **written path itself** becomes the program are hazards.

## Fix

Two remedies, chosen by what the stand-in has to do.

**Needs only an exit status → use a system binary.** No file is written, so
there is no window at all. `crates/scribe-client/src/url_detect.rs:1665` passes
`/bin/false` as the program and a unique nonexistent path as the fallback,
proving a nonzero exit is final without any temp executable.

**Must produce output the test parses → check the program in.** The program
becomes a read-only tracked file and only per-run *data* stays in a scratch
dir, so nothing is written-then-executed. Resolve it the way this repo already
does:

```rust
Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/restore-env-recorder.sh")
```

(`crates/scribe-server/src/session_manager.rs:2505`; the pre-existing `bd`
fixtures at `crates/scribe-server/src/beads_board.rs:1786` and `:2242` use the
identical pattern.) Pass the per-run output path in out-of-band through an
environment variable instead of interpolating it into a generated script body,
and have the fixture fail loudly when that variable is unset.

Fixtures must be committed at mode `100755` — verify with
`git ls-files -s crates/scribe-server/tests/fixtures`.

## Prevention

Writing an executable at test time is the smell. Before adding one, ask whether
the test needs a *behaviour* (check in a fixture) or merely an *exit code*
(use `/bin/false`, `/bin/true`). Neither answer requires writing a program.

Note the residual exposure the fixture remedy carries: it depends on git
preserving the exec bit. A `core.fileMode=false` clone or an archive export
breaks these fixtures — loudly, with `EACCES`/`ENOENT`, not silently.
