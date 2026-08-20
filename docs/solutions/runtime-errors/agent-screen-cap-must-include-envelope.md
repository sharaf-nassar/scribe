---
title: Agent screen response caps must include the serialized envelope
date: 2026-08-20
component: scribe-server agent_api
tags: [agent-api, messagepack, response-cap, utf-8, benchmark]
problem_type: bug
---

## Problem

The agent API benchmark produced a `ReadScreen` response of 262,318 bytes even though `max_response_bytes` was clamped to 256 KiB. The extractor correctly limited terminal text, but the final reply still exceeded the public response ceiling.

## Root cause

The limit was applied while formatting terminal rows, before `AgentScreenText` was wrapped in `AgentPayload`, `AgentResponse`, and `ServerMessage::AgentResponse`. Session id, title, CWD, line count, timestamps, snapshot id, enum tags, map keys, and MessagePack string framing were added after the text had already consumed the complete budget.

A payload cap and a wire-response cap are different contracts. The response ceiling belongs at the last serialization boundary.

## Fix

`crates/scribe-server/src/agent_api/mod.rs:391-394` now constructs the full reply, calls `enforce_serialized_response_ceiling`, and only then computes the audit byte count. The helper at `crates/scribe-server/src/agent_api/mod.rs:636-667` measures the exact `ServerMessage::AgentResponse`, trims only successful screen text on UTF-8 boundaries, updates `lines`, and sets `truncated`. If metadata alone cannot fit, it returns `TooLarge` rather than emitting an oversized reply.

`crates/scribe-server/benches/agent_api.rs:156-164` keeps a deliberately saturated response case. The fixed benchmark reports exactly 262,144 bytes, alongside the three latency budgets and default-Deny no-touch check.

Fix record: bead `scribe-8uuf.23`, squash commit `cc77c446b644d5b9b13a3edb1df5bd6c87ce2821`.

## Prevention

- Enforce byte ceilings at the serialized transport envelope, not an inner field.
- Benchmark the same wrapper the socket writer emits.
- Include a multibyte fixture so trimming proves UTF-8 boundary safety.
- Keep metadata in the measured response; subtracting a guessed fixed overhead will drift when DTO fields change.
