---
title: A rejected AI create makes a later shell ignore Ctrl+Z
date: 2026-09-05
component: scribe-client
tags: [ai-tabs, keyboard, ipc, fifo, reconnect, launch-origin]
problem_type: bug
---

# A rejected AI create makes a later shell ignore Ctrl+Z

## Problem

Premerge validation of `scribe-mguo` found that a refused AI creation could
make a later plain shell inherit AI launch origin on an older server. The
client then swallowed that shell's Ctrl+Z. Reversing request order assigned
false origin to an AI session; a lost acknowledgement caused the same problem.

## Root cause

FIFO ordering does not prove that every request received an acknowledgement.
A server creation failure sends an error without `SessionCreated`
(`crates/scribe-server/src/ipc_server.rs:8620-8624`). A completed write can
also lose its reply before disconnection. Per this run's red regressions,
using the oldest remaining launch boolean as authority shifted provenance
onto an unrelated session.

The delivery queue and trustworthy origin are separate concerns. Current
`PendingCreates` records both under one shared mutex
(`crates/scribe-client/src/ipc_bridge.rs:1131-1148`); preserving a queued
completion is not evidence that its origin remains correlated.

## What didn't work

- Local enqueue-refusal coverage missed server-side refusal and lost replies.
- Successful creation tests could not expose a queue whose alignment was lost.
- Rejected alternative: clearing only current origin values, then trusting
  later entries while the same FIFO remains misaligned.

## Fix

Landed as `eb74e7df21ada3f1098317e526bda307857faaf3`, task `scribe-mguo`.

`claim_pending_create` still pops the original completion, but returns no
origin after uncertainty is latched. Only a new sink starts with trusted
fallback (`crates/scribe-client/src/ipc_bridge.rs:1277-1293`). Generic errors
and termination of a served reader/writer invalidate it before redial
(`crates/scribe-client/src/main.rs:15792-15799,17781-17784`). Server metadata
and existing session-keyed origin remain independent
(`crates/scribe-client/src/main.rs:18698-18710`).

Three production regressions failed before the repair and passed afterward:
both rejection orders with successive requests, and a fully written create
whose acknowledgement is lost across an actual framed reconnect. Run:

```bash
cargo test -p scribe-client --bin scribe-client rejected_
cargo test -p scribe-client --bin scribe-client serve_connection_lost_create_ack
```

The design rule is recorded in `lat.md/client.md:246-257`; this incident does
not claim to repair the existing pane/tab completion correlation protocol.

## Prevention

- Test refusals and lost acknowledgements through production dispatch, not
  only queue helpers.
- Do not restore provenance confidence merely because a queue drains or a
  connection returns.
- Preserve known session/server facts while degrading ambiguous fallback to
  unknown. For old peers, unknown must retain native shell input.
