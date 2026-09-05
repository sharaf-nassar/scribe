---
title: Isolated Claude checks bypass the configured CPA route
date: 2026-09-05
component: ai-provider-validation
tags: [claude-code, cpa, bash, e2e, isolation, credentials]
problem_type: environment
---

# Isolated Claude checks bypass the configured CPA route

## Problem

During `scribe-mguo` verification, Claude Code 2.1.246 exited before keyboard
checks with `Unable to connect to Anthropic services`. An isolated OAuth
profile reported `loggedIn=true`, but the container had no network. The user
then identified the actual environment: Claude was configured through CPA in
`.bashrc`, not through the recreated OAuth setup.

## Root cause

An empty environment and replacement HOME changed the provider configuration
being tested. Scribe runs provider commands after interactive shell startup
(`crates/scribe-server/src/session_manager.rs:1152-1190`); the normal Linux
Bash integration sources `.bashrc`
(`dist/shell-integration/bash/scribe.bash:57-60`). Authentication status alone
did not prove that the test matched that launch path or could start its UI.

## What didn't work

- Treating an empty disposable profile as proof that no usable login existed.
- Copying OAuth state while omitting the configured CPA exports.
- Treating the Pi stand-in or a passing auth-status check as real-Claude
  keyboard acceptance.

## Fix

The feature landed in `eb74e7df21ada3f1098317e526bda307857faaf3` under
`scribe-mguo`. The verification environment was corrected separately; its
private relay was a run-local tool, not shipped application code.

With explicit user approval, the probe parsed only required literal routing
exports and reached the existing loopback CPA through a private Unix socket.
Docker remained `--network none`. An HTTP allowlist permitted only `GET /`
and `GET /v1/models`, rejected writes, inference routes, redirects and direct
upstream access, and forwarded only `GET /` in the successful run.

Per the archived run evidence, real Pi 0.85.0 and Claude Code 2.1.246 survived
Scribe-action Ctrl+Z press/hold/release and displayed subsequent unsent input.
Both also suspended and resumed with `fg` in ordinary disposable shells.
No prompt was submitted. Temporary credential copies stayed outside the
repository, used restrictive permissions, and were removed afterward.

The tested optional-traffic controls were checked against the current
[Claude environment-variable documentation](https://code.claude.com/docs/en/env-vars.md).
Do not assume those controls or private CLI flags are stable across versions.

## Prevention

- Identify effective shell exports, provider version and backend before
  building an isolated verification environment.
- Ask before using credentials or expanding connectivity. A local proxy is
  not permission for unrestricted egress or inference requests.
- Preserve startup diagnostics separately from keyboard results; do not mark
  a check passed when its process never reached the tested state.
- Log route/outcome/cleanup evidence, never credential values. Do not copy an
  entire personal profile or modify the live Scribe installation.
