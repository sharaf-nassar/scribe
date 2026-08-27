---
title: "Pane split creates a strip tab: a reachable action can pass parity while its model deviates"
date: 2026-08-26
component: scribe-client
tags: [pane-split, tab-strip, parity, gpui-rebuild, pane-shell, tab-session]
problem_type: bug
---

# Pane split creates a strip tab: a reachable action can pass parity while its model deviates

## Problem

Splitting a pane (`ctrl+shift+\`) in the GPUI client splits the grid but
ALSO adds a new tab to the section's tab strip. The user's model — pane
splits stay inside the current tab; only workspace splits create tabs —
matches the legacy client and the wire format, not the running client.

## Root cause

The GPUI rebuild collapsed the legacy per-TAB pane-tree model into one
pane tree per workspace region, with every server session surfaced as a
strip tab:

- `TerminalView::split_pane` (crates/scribe-client/src/main.rs:6378) →
  `PaneShell::split_focused_pane` (crates/scribe-client/src/pane_shell.rs:709)
  splits the region's single tree (`trees` keyed by `WorkspaceId`) and
  requests a `CreateSession`.
- The reader's `open_created_tab` (crates/scribe-client/src/main.rs:16499)
  inserts EVERY genuine `SessionCreated` into the strip via
  `TabSessions::insert_active` (crates/scribe-client/src/tab_session.rs:278).
  `TabEntry` has no "pane, not tab" concept, so the split's session is
  both a pane and a tab.
- The deviation is self-documented at
  crates/scribe-client/src/pane_shell.rs:1372-1379: "Every session the
  split shows is also a tab".
- The legacy winit client
  (`git show 7f90edf^:crates/scribe-client-legacy/src/main.rs`,
  `prepare_split_pane`/`finish_split_pane`) split inside
  `active_tab.pane_layout` and never called `add_tab`; only
  `handle_workspace_split` added a tab. The wire format still encodes
  that model: `WorkspaceTreeNode::Leaf` carries per-tab `pane_trees`
  (lat.md/protocol.md:111) and the server round-trips it opaquely.

Introduced by commit cf78b22 (bead scribe-38e.58): "PaneShell owns …
one PaneTree per region". The 016 parity ratchet counted the split
action as WIRED — reachability rows verify the action fires, not that
the resulting model matches the product it replaced.

## What didn't work

- Log-based reproduction: the installed client's diagnostics are
  discarded — `init_tracing` (crates/scribe-client/src/main.rs:12086) is
  stderr-only and the desktop launch points stderr at `/dev/null`, while
  `~/.local/state/scribe/client.log` is a stale pre-rebuild file that
  looks current but never updates (filed as scribe-q0yi).
- Agent-API reproduction: `scribe agent` action dispatch is rejected by
  the running server ("does not support the agent API"), so the split
  could not be driven remotely; the chord path plus the unconditional
  code path served as the reproduction.

## Fix

Restore per-tab pane trees in the GPUI shell: trees keyed per tab, a
split's `CreateSession` answer matched by the existing FIFO
pending-create rule and never inserted into the strip, tab switch swaps
the whole tree, close-tab closes the tree's sessions, and
`region_tab_payload`/`adopt_server_tree` file split sessions only inside
`pane_trees`. Filed as scribe-uc1y (P1), unlanded as of this writing;
the diagnostics loss is scribe-q0yi (P2).

## Prevention

- When a rebuild replaces a data model, pin the MODEL invariants in the
  parity artifacts (e.g. "a pane split leaves the tab count unchanged"),
  not just action reachability — a wired action can still do the wrong
  thing with the right log lines.
- The wire format is the durable statement of intent here: per-tab
  `pane_trees` existed the whole time; a client whose in-memory shape
  cannot fill it faithfully is deviating even if every E2E passes.
- The deleted legacy client remains the behavioral oracle:
  `git show 7f90edf^:crates/scribe-client-legacy/src/main.rs`.
