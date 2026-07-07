# Contract: Settings, Config & UI Surfaces

**Feature**: `013-remote-window-control` | **Date**: 2026-07-03
**Scope**: TOML schema, settings-webview section, client UI states, log
surface. Grounded in research.md D7–D9.

## TOML schema (`[remote]` table)

```toml
[remote]
enabled = false   # default; FR-001 — listener exists only while true
port = 46061      # default; bind on tailnet addresses only (FR-002)
```

- Types live beside the existing config structs in
  `scribe-common/src/config.rs`; server-side application in
  `scribe-server/src/config.rs`.
- Missing table ⇒ defaults (feature fully off). Unknown keys tolerated per
  existing config conventions.
- Live apply: settings write → TOML → client file watcher →
  `ClientMessage::ConfigReloaded` → server starts/stops/rebinds the
  listener and severs remote connections on disable. NO server restart.

## Settings webview — "Remote" section

| Control | Key path | Behavior |
|---|---|---|
| Toggle "Allow remote control from my devices" | `remote.enabled` | Default off. Beneath the toggle, permanent plain-language copy (UX-003): "When on, your own devices on your Tailscale network (signed in as <account>) can view and control this machine's terminal windows." |
| Port (advanced) | `remote.port` | Numeric field, validated 1024–65535; helper text notes default 46061 |

- Applied through `apply.rs` key-path writes like every existing setting
  (Constitution III). No network calls from the webview (CSP unchanged).
- When Tailscale is not detected on the machine, the section shows a
  passive note ("Tailscale not detected — remote access stays off") and the
  toggle remains usable but the server will fail closed (FR-015).

## Client UI states (scribe-client)

| State | Surface | Contract |
|---|---|---|
| Remote access enabled (owning machine) | Status bar segment | Persistent subtle indicator while `remote.enabled` (FR-009a) |
| Window remote-controlled, local client displaced (owning machine) | Window content + banner | Last frame dimmed & frozen; banner: "Controlled by <device> (<account>) — Take back control" (FR-007, FR-009b); Enter/click reclaims |
| Any window remote-controlled, displaced or not (owning machine) | Status bar segment + window list | The remote segment names the controller(s) while any window is remote-controlled (e.g. "laptop-2 controls 1 window"), and window-listing UI marks controlled windows with device + account — covers remotely created windows that never had a local client (FR-009b, SC-006) |
| Connect flow (connecting machine) | Command palette action "Connect to remote machine…" + GPU overlay picker | Lists same-account online peers by short name; manual entry field; then window list (workspace names, session counts, in-use marker) + "New window" (FR-004, FR-005) |
| Reconnecting (connecting machine) | Overlay on affected window | "Reconnecting to <peer>… (attempt n)" + Cancel (FR-011) |
| Refusals | Dialog/toast copy | Distinct copy per `RemoteRefusal` variant + unreachable + taken-over (UX-002); each names the remedy |

Failure copy table (UX-002 → `RemoteRefusal` mapping):

| Outcome | Copy sketch |
|---|---|
| Connection failure (combined; a disabled peer has no listener) | "Can't reach <peer> — it may be offline, Scribe may not be running, or remote access may be turned off there." |
| `Disabled` (typed: handshake race or delivered `RemoteDisconnect` sever notice) | "Remote access is turned off on <peer>. Enable it in Scribe Settings on that machine." / "Remote access was turned off on <peer> — connection closed." |
| `Unauthorized` | "<peer> refused: this device isn't signed in as the same Tailscale account." |
| `IdentityUnavailable` | "<peer> can't verify device identity right now (Tailscale unavailable there)." |
| `IncompatibleVersion` | "Scribe versions don't match: this machine <x>, <peer> <y>. Update the older one." |
| `Busy` | "<peer> has too many remote connections right now." |
| Taken over | "<device> (<account>) took control of this window — Take back control" |

## Audit log surface (owning machine)

Structured server-log lines (research D9), one per lifecycle event:

```text
remote: accepted   peer=<node> user=<login> window=<id>
remote: refused    peer=<node?> reason=<disabled|unauthorized|identity-unavailable|version|busy> [detail=tagged]
remote: disconnect peer=<node> window=<id?>
remote: severed    reason=disabled  (bulk, on FR-016 disable)
```

`reason` mirrors the wire `RemoteRefusal` enum exactly (one canonical
taxonomy); `detail=tagged` is an optional qualifier when an `unauthorized`
refusal was specifically a tagged/identity-less node.

Documented location: the existing server log file. No dedicated UI in v1.
