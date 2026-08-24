---
title: Pi stays running while questionnaire waits
date: 2026-08-23
component: Pi lifecycle adapter
tags: [pi, ai-state, ask-user-question, tool-lifecycle, waiting-for-input]
problem_type: bug
---

# Pi stays running while questionnaire waits

## Problem

A Pi pane keeps Scribe's `Processing` indicator while `ask_user_question` is
open and the agent is waiting only for the user. The questionnaire visibly
asks for input, but the pane border and tab still say the model is running.
Fix filed as scribe-mh1z, unlanded as of this writing.

## Root cause

The Scribe Pi adapter models work at agent-run boundaries, but the
questionnaire waits inside a tool execution:

- `input` emits `state_changed { processing }` at
  `dist/pi-extension.ts:416-421`.
- The normal transition away from Processing occurs only in `agent_settled`
  at `dist/pi-extension.ts:445-450`.
- The adapter's only `tool_call` branch handles bash issue claims and returns
  for `ask_user_question` at `dist/pi-extension.ts:460-466`.

Pi cannot emit `agent_settled` while a tool is still executing. The installed
`@juicesharp/rpiv-ask-user-question` 2.7.0 package explicitly brackets its
actual TUI and RPC wait with the stable `rpiv:ask-user:blocked` event at
`/home/mamba/.pi/agent/npm/node_modules/@juicesharp/rpiv-ask-user-question/ask-user-question.ts:365-400`.
Scribe does not consume that signal, so Processing remains the last state.

## What didn't work

The existing question coverage looked relevant but tested a different phase.
`tests/e2e/func/pi-extension-harness.mjs:260-271` finalizes assistant text and
then calls `agent_settled`; `tests/e2e/func/pi-ai-lifecycle.sh:150-163` sends a
synthetic `session_stopped` event. Both prove the server classifies a settled
question as `WaitingForInput`. Neither leaves an interactive tool unresolved.

Mapping every `tool_call` named `ask_user_question` would be earlier than the
real wait. Validation failure, missing UI, or load failure can return without
showing a questionnaire. The package's blocked event is the exact boundary and
already clears itself on answer, cancel, or error.

## Fix

Per this investigation, fix filed as scribe-mh1z and unlanded: subscribe the
standalone Scribe adapter to `rpiv:ask-user:blocked` without importing the
optional package. Map boolean `active: true` to `WaitingForInput` and
`active: false` to `Processing` through the existing bounded serial queue.
Ignore malformed payloads and keep `PermissionPrompt` unsupported.

Add a harness event bus and leave a questionnaire open long enough to assert
that the last helper state changes from Processing to WaitingForInput, then
returns to Processing when the blocked event clears.

## Prevention

- State-integration tests must distinguish a settled question from a human wait
  inside an unresolved tool.
- Prefer an extension's explicit blocked/unblocked signal over inference from
  generic tool lifecycle events.
- Document mid-tool human waits separately from the stop classifier in
  `lat.md/server.md` and pin the open/close sequence in `lat.md/test.md`.
