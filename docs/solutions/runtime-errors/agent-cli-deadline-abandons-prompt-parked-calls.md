---
title: Agent CLI 3s deadline abandons prompt-parked calls
date: 2026-08-28
component: scribe-cli / scribe-server agent API prompt lifecycle
tags: [agent-api, consent, prompt, timeout, cli, pi-extension, layered-deadlines]
problem_type: bug
---

## Problem

When a gated agent capability raises a consent dialog, the calling LLM does
not wait for the user: the tool call fails after ~3 seconds with
`{"code":"unsupported","message":"The connected Scribe server does not
support the agent API."}` while the dialog is still on screen. Retries raise
fresh prompts and die the same way. Quill session history showed the retry
pattern live (write→write→action a minute apart, 2026-08-24) and no session
ever completing a prompt-mode approval round trip.

## Root cause

Three stacked deadlines disagree, and the shortest one wins:

- Server parks a prompt-mode request up to `prompt_timeout_ms` (60s default,
  300s ceiling — `crates/scribe-common/src/config.rs:1974`,
  `crates/scribe-server/src/agent_api/policy.rs:201`) and waits up to
  `AGENT_ACTION_COMPLETION_TIMEOUT` (1 min —
  `crates/scribe-server/src/ipc_server.rs:135`) for action completion; the
  single `AgentResponse` is sent only after the dispatch resolves
  (`ipc_server.rs:11095`).
- The CLI wraps the whole exchange in `AGENT_API_DEADLINE = 3s`
  (`crates/scribe-cli/src/main.rs:231,565`), which spec 027 introduced only
  to detect an old server that never replies to the undecodable first frame
  (`specs/027-llm-control-surface.md:719`).
- The pi extension kills the CLI child at `AGENT_CLI_TIMEOUT_MS = 10_000`
  (`dist/pi-extension.ts:116`), also far under the server budgets.

A second, separable half: nothing dismisses a dialog whose prompt stopped
mattering. `expire`, `refresh`, and `cancel_waiter` remove server state only
(`policy.rs:254-307`), and `resolve_at` returns `false` before
`apply_always_decision` for an unknown prompt id (`policy.rs:238-244`), so a
late "Always allow" — a durable user preference — is silently dropped.

## What didn't work

- First hypothesis: "the requester's death cancels the waiter via
  `PendingAuthorization::Drop`, so every click is a no-op." Wrong — the
  transient connection task awaits the dispatch without selecting against
  reader EOF, so it survives the CLI's death and a click *within* the 60s
  window still resolves (Always does persist there). The no-op path only
  opens after expiry, policy refresh, or waiter cancellation. Trace the task
  lifetime before reasoning from `Drop` impls.
- Naive fix "just lengthen the CLI deadline": breaks old-server detection —
  the response read IS the unsupported signal, since an old server never
  replies at all. The fix needs a liveness ack before the long wait.

## Fix

Filed, unlanded as of this writing:

- `scribe-n72m` (P1): server emits an immediate progress-ack for requests
  that can park or wait (opt-in `#[serde(default)]` request field; framing is
  `rmp_serde::to_vec_named`, so the compat pattern holds); the CLI keeps 3s
  only until the first frame, then waits out the server budgets; the pi
  extension timeout becomes a pure backstop above the server ceilings.
- `scribe-a796` (P2): `AgentPromptDismiss` closes stale dialogs on
  expiry/refresh/cancellation, plus a short prompt-key tombstone so a click
  racing the dismissal still applies "Always".

## Prevention

- When a protocol gains a human-in-the-loop wait, audit every consumer
  deadline up the stack in the same change: each caller's budget must exceed
  the server's, or the caller must get a liveness signal that switches it
  into patient mode.
- Every UI artifact backed by server state needs a cancellation message in
  the same design; state removed server-side with a still-visible dialog
  guarantees a dead interaction eventually.
- A timeout that exists to classify a peer (old vs new) must never bound a
  proven-live exchange; separate classification from completion.
