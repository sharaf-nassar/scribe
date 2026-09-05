---
title: A local-only fixture task triggered an embedding request
date: 2026-09-05
component: implementation-workflow
tags: [subagents, lat, offline, context, discovery]
problem_type: workflow
---

# A local-only fixture task triggered an embedding request

## Problem

The `scribe-nq92` followup only needed a shared test fixture and one spec
sentence. Its fresh worker had no dedicated `lat_search` tool and tried CLI
`lat search` despite the local-only task boundary. Per its recorded result,
an embedding API request failed with HTTP 400: 395373 requested tokens exceeded
the 300000-token limit. A failed request is still a network attempt.

## Root cause

The handoff did not explicitly carry forward that intent discovery was already
complete or name the parent's local section exports. The worker treated an
available CLI command as a local substitute for an unavailable extension tool.
That substitution changed the task's side effects without being necessary.

The actual code change was inside the test-only module and one fixture
initializer (`crates/scribe-client/src/main.rs:18904-18905,19060-19066`), with
its existing specification at `lat.md/test.md:3006`. No new architecture or
external API decision needed investigation.

## What didn't work

- Assuming a command named search is local or harmless when it fails.
- Relying on parent-session research to survive a fresh child context without
  an explicit handoff.
- Using Cargo's offline mode as evidence that unrelated tools did not use
  the network.

## Fix

The parent stopped further semantic-search attempts, supplied already-exported
local sections, and directed local source reading, `lat section` and
`lat check`. The cleanup then passed its checks and landed as
`70736731f620180e89c7a5c94b2ae64523b1ede2`, task `scribe-nq92`.
No lat or subagent tooling was changed; the failed request remains recorded
separately from successful offline test results.

## Prevention

- Give fresh workers the exact local evidence paths and state which discovery
  is complete. Explicitly skip redundant semantic search for such followups.
- Check a substitute tool's side effects before using it under a restricted
  task. Ask the parent when required capabilities are unavailable.
- Use the full local heading chain for `lat section`; missing intermediate
  headings are lookup errors, not a reason to retry networked discovery.
- Report accidental requests honestly rather than describing the entire run
  as offline because later tests used `--offline`.
