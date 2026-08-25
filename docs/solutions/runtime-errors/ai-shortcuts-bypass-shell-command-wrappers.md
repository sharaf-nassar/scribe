---
title: AI shortcuts ignore shell command wrappers
date: 2026-08-24
component: server session launch, AI shortcuts
tags: [ai-launch, shell-functions, aliases, environment, pi, claude, codex]
problem_type: bug
---

## Problem

Shortcut-launched Pi did not receive environment or arguments applied by a
same-named shell function, even though the configured shell rc file and its
exported `PATH` loaded.

The same mechanism affects Claude and Codex shortcuts.

## Root cause

The client sends typed launch intent. Ctrl+Alt+Z reaches `create_pi_tab`, which
selects structured Pi intent or the legacy shell-tool shape
(`crates/scribe-client/src/main.rs:5960-5977`). Claude and Codex use
`create_ai_tab` (`crates/scribe-client/src/main.rs:5931-5957`).

Both server paths converge in `launch_exec_command`
(`crates/scribe-server/src/session_manager.rs:1134-1142`). For every
non-PowerShell provider, `exec_prefixed` makes the command `exec <binary>`
(`crates/scribe-server/src/session_manager.rs:1186-1193`). In Bash, the
provider name is an argument to the `exec` builtin, not a command-position word.
Bash therefore resolves the external executable and bypasses same-named aliases
and functions loaded from the rc file.

The exported startup environment was not the failing boundary. A fresh plain
Scribe shell and shortcut Pi had equal exported values after expected
per-session/runtime differences were excluded. The missing values were scoped
inside the bypassed wrapper.

## What didn't work

The earlier parity work treated "same shell, rc file, and PATH" as equivalent
to "same launch as typing the command." That is false when startup files define
an alias or function rather than only exports.

The visual regression checks rc exports, `PATH`, `TERM_PROGRAM`, CWD, and empty
Pi argv (`tests/e2e/visual/tab-window-chords.sh:301-375`). Server unit tests
also require the literal `exec pi` shape
(`crates/scribe-server/src/session_manager.rs:1760-1800`). Both stay green while
wrapper-scoped environment is lost.

`exec` was chosen so provider exit ends the shell and closes the tab. Removing
it without an explicit shell exit would fix wrapper resolution but leave a
stray prompt, regressing the behavior documented at `lat.md/test.md:2920-2931`.

## Fix

Invoke each provider in normal shell command position, then exit the shell with
the provider status. Keep shell-specific restore-delta ordering and resume-arg
quoting intact. This approach honors aliases/functions and preserves tab closure.

The reported Pi fix is filed as `scribe-t9on.1`. Claude/Codex coverage is filed
as dependent bug `scribe-t9on.2`. Both are unlanded as of this writing.

## Prevention

Shell-launch parity tests must cover command resolution, not only exported
startup values. Define a same-named wrapper in the fixture rc file, set a marker
inside it, forward to the stub, and require the stub to record the marker.

Document startup parity and command-resolution parity as separate invariants.
A direct-child process shape is an implementation choice, not proof that typing
the same command in a fresh terminal behaves identically.
