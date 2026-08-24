---
title: Typed transactions need an outcome for every pre-commit failure
date: 2026-08-24
component: workspace transfer protocol, server transaction, env restore
tags: [protocol, transaction, typed-failure, workspace-transfer, env-store]
problem_type: architecture
---

# Typed transactions need an outcome for every pre-commit failure

## Problem

The workspace-transfer transaction promised one correlated typed result, but its
original refusal enum covered ownership, capability, collision, sole-workspace,
and handoff checks only. Environment DEK/envelope staging is fallible before the
commit point, so implementation could neither return success nor express the
failure without falling back to an unrelated generic error.

## Root cause

The protocol enumerated validation failures but not failures in transaction
preparation. `stage_envelope_transfer` deliberately propagates filesystem and
keystore errors so the server can abort before mutation
(`crates/scribe-server/src/env_store/store.rs:162-170`). Without a transfer
refusal for that path, the request's correlation contract was incomplete.

## Fix

Add `WorkspaceTransferRefusal::EnvironmentRebindFailed` with the invariant that
the source remains byte-identical and staged target copies are discarded
(`crates/scribe-common/src/protocol.rs:678-681`). The server returns it from the
staging loop before the state commit (`crates/scribe-server/src/ipc_server.rs:7828-7838`),
and fault injection proves failure mutates nothing before a later retry succeeds
(`crates/scribe-server/src/ipc_server.rs:17642-17652`).

Landed for bead `scribe-07xb.4` in squash commit
`d9566978dc039eda35be56f6b017e774c70daa0e`.

## Prevention

When designing a typed acknowledged transaction, enumerate the whole path to
its commit point: validation, lock acquisition, preparation, external stores,
and serialization. Every fallible pre-commit stage needs either a typed result
or an explicit documented transport-failure contract. Do not wait until
implementation to discover that the result type cannot represent an abort.
