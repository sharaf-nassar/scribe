---
title: Agent world routing crosses Scribe's library and binary module boundary
date: 2026-08-20
component: scribe-server agent_api, ipc_server, workspace_manager
tags: [rust, nominal-types, binary, library, agent-api, worktrees]
problem_type: convention
---

## Problem

The first `World`/`Siblings` implementation compiled only as an unreferenced `world.rs`. Wiring it into the dispatcher failed when the server binary called the library-owned agent module: the apparently identical `WindowShare` and `WorkspaceManager` types did not match.

## Root cause

`scribe-server` exports `agent_api` from the library, while the binary also compiles `ipc_server.rs` and `workspace_manager.rs` in its own module tree. Rust types are nominal. The library's `ipc_server::WindowShare` and the binary's recompiled `ipc_server::WindowShare` are different types even when they come from the same source file.

A concrete `world::capture` signature therefore bound the library copy and rejected the binary copy. Merely adding `pub mod world` exposed latent privacy and lint failures that the unreferenced file had never compiled.

## Fix

`crates/scribe-server/src/agent_api/world.rs:29-49` defines the read-only `ShareView` and `WorkspaceView` traits. Both module-tree copies implement those one library-owned traits at `crates/scribe-server/src/ipc_server.rs:1179-1191` and `crates/scribe-server/src/workspace_manager.rs:718-733`.

The dispatcher receives transport-owned operations through `DispatchSources` at `crates/scribe-server/src/agent_api/mod.rs:322-332`. Registry capture, session lookup, and action execution remain behind policy authorization without naming binary-owned concrete types.

Fix record: bead `scribe-8uuf.9`, squash commit `4649a3769d5fdd988566a1e8097e12ab756ce140`.

## Prevention

- Before passing a server-internal type into a library-exported module, check whether the binary recompiles the source module.
- Use one library-owned narrow trait or DTO at the boundary; do not duplicate the whole module in the binary to make names line up.
- New Rust files are not verified until declared by a compiled module. Add the module declaration early enough that privacy and lint checks run before handler wiring.
- A bead that says only `world.rs` is underscoped when acceptance requires a reachable dispatch path; declare the router and transport call site too.
