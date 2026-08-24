---
title: Exited background tab paints its sibling over the focused workspace's pane
date: 2026-02-14
component: scribe-client
tags: [active-session, session-exit, workspace-regions, pane-shell, focus, input-routing]
problem_type: bug
---

# Exited background tab paints its sibling over the focused workspace's pane

## Problem

Exiting a terminal whose workspace region does not hold window focus (agent
shell ending on its own, titlebar ✕ on another region's tab, `exit` then
switching away) makes a terminal from that workspace appear in the focused
workspace, painted on top of the pane the user is looking at. The displaced
pane's tab stays highlighted and its scrollbar/prompt chrome still render —
a chimera pane. Keystrokes silently go to the other workspace's session.
Switching tabs back and forth repairs it. Filed as scribe-txyk.

## Root cause

`Shared::active_session` is a single window-global pointer that two things
follow: the focused placement's paint and keystroke routing. The invariant
(lat.md/client.md, "Per-Pane Grids And Sizing") is that it is re-pointed
only on focus moves. The session-exit path violates it:

- `on_session_exited` (crates/scribe-client/src/main.rs:13704) takes the
  region-local refocus target from `TabSessions::remove` and calls
  `attach_session` → `adopt_attached_session` (main.rs:15416), which sets
  `active_session` unconditionally — the reader thread has no idea which
  region holds window focus.
- `render_panes` paints the placement with `placement.focused == true` from
  `active_session`'s grid, ignoring that placement's own `session_id`
  (main.rs:7007, content built by `sync_split_scroll` main.rs:8325).
- `send_key_bytes` (main.rs:9163) routes typing to `active_session`.

So after a background-region exit, the focused pane in another workspace
paints (and types into) the exited tab's sibling until the next
`attach()` re-points the pointer.

Variant: exiting a region's LAST tab while attached leaves `active_session`
pointing at the dead session (nothing clears it). `reconcile_panes`
(main.rs:6628) then adopts it through `tab_adoption_pane`'s focused-pane
fallback (main.rs:5795) — a dead session has no tab, so `workspace_of()`
returns `None` and the fallback fires — displacing the surviving
workspace's terminal with a blank grid and re-adopting every frame against
`retain_sessions` (log signature: repeated `pane adopted a session` with
one id). `follow_session_to_region` is no-op-guarded by
`tabs.set_workspace`, so the server is not corrupted; the damage is
client-side only.

## What didn't work

scribe-xpn (closed P1) fixed the same family by making exit refocus
region-scoped and adoption prefer the session's own region. That held for
the *model* (no more cross-workspace MoveSession), but three couplings
survived it: the reader's exit-refocus still steals the window-global
`active_session`, the renderer still paints the focused placement from
`active_session` instead of the placement's own session, and the
focused-pane adoption fallback is still reachable for tab-less (dead)
sessions. The per-region strip refactor (commit bb19b26) explicitly kept
`active_session` window-global. Do not re-fix this by tightening
`TabSessions::remove` — the refocus choice is already region-correct; the
divergence is downstream of it.

## Fix

The scribe-txyk fix re-points `active_session` in `on_session_exited` only
when the exited session *was* the attached one; background regions are
already repopulated by `fill_empty_region_panes` via `stream_session`, which
deliberately never touches `active_session`. It clears `active_session` when
the attached session exits with no refocus target and re-points it from the
shell's focused pane after the region collapse. Reconciliation never adopts
an active session that has no tab in `TabSessions`.

## Prevention

- Anything on the IPC reader thread must not mutate focus-derived state
  (`active_session`) — the reader cannot see GPUI focus. Reader-side
  "attach" and GPUI-side "make active" are different operations; the GPUI
  side already has the split (`attach` vs `stream_session`, main.rs:5843).
- When a pane paints or routes input, key it off the pane's own
  `placement.session_id` wherever possible; every read of the window-global
  pointer is a latent cross-workspace bleed.
- Diagnostic signatures: content of one workspace rendered under another
  workspace's tab chrome; per-frame `pane adopted a session` log spam;
  keystrokes landing in a different region's shell.
