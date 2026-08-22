---
title: Verifying the installed scribe-dev flavor against a stale server, a stale
  binary, and a lying pointer
date: 2026-08-22
component: tests/install/dev-package-smoke.sh, scribe-dev packaging, X11 capture
tags: [packaging, scribe-dev, install, stale-binary, xdotool, gpui, evidence]
problem_type: environment
---

## Problem

Proving that a packaged app matches an approved design needs the *installed*
binary under a *real* desktop session. Three separate things silently answer for
the wrong binary or the wrong pixel, and each one produces confident, wrong
evidence.

## 1. A running dev server can be a deleted binary

`dpkg -i` replaces `/usr/bin/scribe-dev-server` on disk, but a server started
before the install keeps running the old inode. It looks healthy in `ps`:

```bash
ls -l /proc/<pid>/exe    # -> /usr/bin/scribe-dev-server (deleted)
```

`(deleted)` is the tell. A client that connects to it exercises the previous
build's protocol while every on-disk check passes. Before capturing installed
evidence, confirm the running server's `exe` link has no `(deleted)` suffix, and
restart it if it does. Verify the pid's `cmdline` starts with the dev path before
sending any signal — a machine running a stable Scribe with live user sessions is
one `pkill scribe-server` away from losing real work.

## 2. The installed-payload check is build-path sensitive

`tests/install/dev-package-smoke.sh` compares three things: repo release binaries,
the `.deb` payload, and the installed files. `scribe-server` and `scribe-cli`
build reproducibly, so a worktree build and a main-checkout build are
byte-identical. `scribe-client` is not: the same source produces binaries that
differ in 116 bytes — the 20-byte `.note.gnu.build-id`, plus 96 bytes of
compile-time hash-seed immediates inside two SipHash routines in `.text`.

So the smoke fails on `installed asset usr/bin/scribe-dev` whenever the installed
`.deb` was built from a different checkout than the one running the test, even at
an identical commit. That is not a packaging defect. Run the installed leg against
the `.deb` that was actually installed, and confirm identity independently:

```bash
grep usr/bin/scribe-dev /var/lib/dpkg/info/scribe-dev.md5sums
md5sum /usr/bin/scribe-dev
```

Matching dpkg md5sums prove the installed bytes are the package's bytes. Use
`git diff <installed-commit> <worktree-commit> -- crates dist` to prove the
sources agree.

## 3. `xdotool` window coordinates are not client coordinates

Under a reparenting WM, `xdotool getwindowgeometry` reports the *frame* origin,
and `xdotool mousemove --window` is relative to that frame. GPUI hit-tests in
*client* coordinates. The two differed by 14x49 px here, which is enough to miss
a small control entirely while still landing inside a tall lane tab — so hover
appeared to work, clicks appeared to be ignored, and the app looked broken.

Always take the origin from `xwininfo`, which reports the client area:

```bash
OX=$(xwininfo -id $WID | awk '/Absolute upper-left X/{print $4}')
OY=$(xwininfo -id $WID | awk '/Absolute upper-left Y/{print $4}')
xdotool mousemove --sync $((OX+rel_x)) $((OY+rel_y))
```

Re-read the origin after any `windowmove`/`windowsize`, and before clicking,
assert the pointer is over the intended window:

```bash
w=$(xdotool getmouselocation --shell | grep '^WINDOW=' | cut -d= -f2)
[ "$(xdotool getwindowpid $w)" = "$DEVPID" ] || abort
```

That guard matters on a shared desktop: the other Scribe window on this machine
was stacked above the dev window and covered the whole left monitor, so
unguarded clicks would have gone into the user's live terminal. Moving the dev
window to unoccupied screen area is more reliable than fighting the WM for stack
order.

## Do not park the pointer inside the window under test

With `focus_follows_mouse`, leaving the pointer parked in the window under test
steals keyboard focus from whatever the human is doing; their keystrokes end up
in your test window. Park outside the window instead — a pinned board stays
pinned without hover.
