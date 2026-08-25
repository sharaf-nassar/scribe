---
title: Agent consent opens in another Scribe window
date: 2026-08-25
component: scribe-server agent API prompt routing
tags: [agent-api, consent, prompt, multi-window, routing, regression-test]
problem_type: bug
---

## Problem

An agent running in one Scribe window can raise its capability consent dialog
in another connected Scribe window. The action still waits for approval, but
the user must find and answer a modal outside the agent's working context.

## Root cause

`handle_transient_agent_request` selects the prompt writer before dispatching
the request (`crates/scribe-server/src/ipc_server.rs:11045-11050`).
`first_local_agent_api_writer` ignores `origin_session_id` and chooses the
capable client with the lexicographically smallest `WindowId`
(`crates/scribe-server/src/ipc_server.rs:11025-11037`).

The dispatcher sends `AgentPromptRequest` through that preselected writer
(`crates/scribe-server/src/agent_api/mod.rs:415-445`). The receiving client's
reader parks the request for its own foreground view
(`crates/scribe-client/src/main.rs:15105-15107,16244-16263`), so no later layer
can move the modal back to the caller's window.

During this investigation, the caller belonged to window `b0d774d0...` while
another connected window `a6d0ece5...` sorted first. A clean-worktree unit
reproduction with two capable writers failed because the selector returned the
first writer rather than the origin writer.

## What didn't work

Stable global ordering looked like a safe way to avoid broadcasting one prompt
to several windows. It solved duplicate delivery but discarded request
locality. The single-window consent visual test in
`tests/e2e/visual/agent-consent-dialog.sh:9-10,20-39` cannot distinguish global
selection from caller-window selection, so it remained green.

The later helper simplification in commit `4f64d047` preserved this behavior;
it did not introduce the bug. No earlier bead, session-history result, or
learning documented caller-window prompt routing.

## Fix

Fix filed as `scribe-ag3v`, unlanded as of this writing. Make prompt selection
request-aware: prefer the window owning a valid `origin_session_id`; if that
known caller window cannot render prompts, deny instead of falling back to a
different window. Keep deterministic fallback only for originless or stale
external-CLI requests, and keep protected target lookup after authorization.

## Prevention

Any server route that opens per-window UI needs a two-window test with both
clients capable and the caller placed in the window that loses global sort
order. Assert that only the caller's writer receives the frame. A one-window
visual test proves rendering, not routing locality.
