# Research: Remote Window Control over Tailscale

**Feature**: `013-remote-window-control` | **Date**: 2026-07-03
**Question**: What is the standard and best approach for connecting to and
controlling a Scribe window from another machine on the same Tailscale
network, given both machines run Scribe?

This document records pre-specification research (user-requested). It surveys
prior art, Tailscale-native integration patterns, and the accepted security
posture, then ranks candidate architectures. The `/speckit-plan` phase should
treat the recommendation here as a researched default, not a final commitment.

## Prior art — how existing tools do remote terminal control

| Tool | Transport | Auth | Sync model |
|---|---|---|---|
| tmux -CC | unix socket; ssh for remote | fs perms + `server-access` | raw pane bytes (`%output`) + structured layout events; client emulates |
| GNU screen | unix socket; ssh | `multiuser` + `acladd` bit ACLs | server IS the emulator; rendered redraw stream |
| mosh | UDP (SSP) | ssh bootstraps AES-OCB key | server-authoritative screen-state sync, frame skipping + predictive echo |
| Eternal Terminal | TCP :2022, protobuf | ssh bootstraps secret | sequence-numbered raw byte stream + replay buffer |
| zellij web (2025+) | HTTP(S); HTTPS mandatory off-localhost | hashed login tokens | server-side state rendered to browser or native attach |
| sshx | gRPC via relay | capability URL + E2EE | sequenced encrypted byte stream + predictive echo |
| tty-share | WebSocket via TLS relay | secret URL (no E2EE) | raw PTY bytes → xterm.js |
| upterm | reverse SSH tunnel | real SSH auth | literally SSH; raw bytes |

Key observations:

- tmux control mode forwards "exactly what the application sent" and grafted a
  state-resync escape hatch on later (`pause-after` → `%pause` → client
  re-syncs via `capture-pane`) for slow consumers.
  <https://github.com/tmux/tmux/wiki/Control-Mode>
- mosh optimizes for ~500 ms lossy links by syncing screen *state* and
  skipping frames; the cost is fidelity (no scrollback, breaks tmux -CC).
  <https://mosh.org/> ·
  <https://www.usenix.org/conference/atc12/technical-sessions/presentation/winstein>
- Eternal Terminal is the counter-pole: transparent sequence-numbered byte
  pipe with reconnect replay, preserving native scrollback.
  <https://eternalterminal.dev/howitworks/>
- Auth is almost never home-grown: either SSH bootstraps a per-session key
  (mosh, ET, WezTerm TLS) or bearer tokens/capability URLs gate a relay
  (sshx, zellij, tty-share).

## WezTerm — closest architectural analog (GPU client + mux server)

- Three remote domain types: **unix** (socket), **ssh** (bridges the remote
  mux's unix socket over an ssh channel via a `wezterm cli proxy` stdio
  bridge), and **TLS** (mux protocol over TCP).
  <https://wezterm.org/multiplexing.html>
- Its wire protocol is server-parsed: clients receive structured line deltas
  (`GetPaneRenderChangesResponse`), leb128-framed, zstd-compressed, with an
  explicit `CODEC_VERSION` handshake (currently 45).
  <https://github.com/wezterm/wezterm> (`codec/src/lib.rs`)
- TLS auth bootstraps a mini-PKI over ssh (server issues CA + client certs),
  then mutual TLS with hostname verification.
- Known pain points that become Scribe requirements: output floods stalling
  the whole domain (#2048), mux slower than plain ssh before rate-limit
  tuning (#1872), attach hangs under load (#7692). Mitigations: rate-limited
  prefetch, bounded buffers, RTT-gated predictive echo
  (`local_echo_threshold_ms`).
  <https://github.com/wezterm/wezterm/issues/2048> ·
  <https://github.com/wezterm/wezterm/issues/1872>

## Tailscale-native integration patterns

- **LocalAPI `WhoIs` is the standard identity primitive.**
  `GET /localapi/v0/whois?addr=IP:PORT` on the tailscaled socket returns
  `{ Node, UserProfile, CapMap }` for the peer behind a connection. This is
  how Tailscale SSH, golink, and tclip authenticate. LocalAPI is formally
  "internal" but pragmatically stable, and the grants documentation lists it
  as the sanctioned way to read app capabilities.
  <https://tailscale.com/docs/features/access-control/grants/grants-app-capabilities>
- **Tailscale SSH is the reference in-daemon authenticator**: WireGuard node
  keys authenticate and encrypt; the inner SSH layer "will not be further
  authenticated"; authorization defaults to `dst: autogroup:self` (users
  reach only their own devices). <https://tailscale.com/kb/1193/tailscale-ssh>
- **Rust story**: no official Rust client. `tailscale-localapi` crate
  (jtdowney) is thin but functional; hand-rolling the 1–2 needed endpoints is
  common. Embedding a tailnet node (tsnet) is Go-only; libtailscale C
  bindings have no Rust binding and low maturity.
  <https://github.com/jtdowney/tailscale-localapi> ·
  <https://github.com/tailscale/libtailscale>
- **`tailscale serve --tcp`** forwards raw TCP but identity headers exist
  only for HTTP; you would still call WhoIs yourself — an extra hop for
  nothing. <https://tailscale.com/kb/1242/tailscale-serve>
- **Tailscale Services (`svc:`)** require tag-based (non-user) host identity
  and target HA logical services — wrong fit for a per-user desktop daemon.
  <https://tailscale.com/blog/services-ga>
- **ACL app-capability grants** (read via WhoIs `CapMap`) are the sanctioned
  per-user permission channel if cross-user sharing is ever added (golink
  precedent: `tailscale.com/cap/golink`).
  <https://github.com/tailscale/golink>
- Discovery: MagicDNS `device-name.tailnet.ts.net`; peer enumeration via
  `tailscale status --json` / LocalAPI status.

## Accepted security posture

**"Tailnet as trusted encrypted transport + WhoIs identity per connection" is
the accepted standard.** Tailscale's own apps ship plain HTTP over the
tailnet (golink, tclip, the tsnet example); Tailscale SSH itself declines to
re-authenticate above WireGuard. TLS is layered only for browser-facing
concerns or Funnel. <https://tailscale.com/blog/tsnet-virtual-private-services>

Design rules that fall out of the caveats:

1. **Bind only to the Tailscale interface/IP** — never `0.0.0.0` — or LAN
   callers bypass identity entirely.
2. **Tagged nodes have no user identity** — deny by default.
3. **WhoIs identifies node + tailnet account, not the OS user** on a
   multi-user machine. Mitigation is Tailscale's own default: same-account
   policy (mirror `autogroup:self`).
4. Peer 100.x addresses cannot be spoofed through the tunnel (identity is
   bound to WireGuard session state). Residual trust: the coordination
   server / tailnet admin.
5. Fail closed: if tailscaled/WhoIs is unavailable, refuse remote
   connections; local operation is unaffected.

## State-sync model for the remote link

Two proven poles: (a) **thin proxy** — sequence-numbered raw PTY bytes,
client parses (ET, sshx, tmux -CC) — dominant on LAN-class links; (b)
**server-parsed state deltas** (mosh, WezTerm) — wins on ~500 ms lossy links
at the cost of fidelity or protocol complexity.

**For Scribe: (a), emphatically.** The existing protocol already *is* a thin
proxy (raw `PtyOutput`, client-side VTE), and the server already keeps an
authoritative terminal per session powering `SessionReplay` (zstd-compressed
ANSI: modes + scrollback + grid + cursor). That combination yields ET-style
fidelity plus tmux-`%pause`-style catch-up for free: on reconnect or
backpressure, drop the output backlog and send a fresh replay. Typical
tailnet RTTs are LAN-class when direct; DERP-relayed paths (~100 ms+) can
later be served by optional RTT-gated predictive echo (WezTerm model) without
redesign.

## Ranked architecture candidates

### 1. RECOMMENDED — listener on the Tailscale IP + WhoIs identity per connection

Run the existing framed protocol over a network listener bound strictly to
the machine's tailnet address; on every accept, resolve the peer's tailnet
identity and enforce: same tailnet account as the host's owner (the
`autogroup:self` analog), deny tagged/unknown peers, optionally honor an app
capability grant later. Discovery via MagicDNS names. No extra TLS
on-tailnet, matching Tailscale SSH / golink / tclip.

- **Why standard**: exactly Tailscale SSH's own architecture and the pattern
  of every Tailscale reference app; WhoIs replaces `SO_PEERCRED` one-for-one
  as the per-connection identity check.
- **Pros**: reuses nearly all of the existing protocol (replay already solves
  attach catch-up; window-claim scoping carries over); no new key material or
  PKI; admin-controllable via tailnet ACLs; no extra processes.
- **Cons / effort (moderate)**: hand-roll the WhoIs LocalAPI call; add an
  explicit protocol version handshake (WezTerm lesson); add flow control for
  output floods (bounded queue + replay resync); LocalAPI is pragmatically
  stable but formally internal — degrade gracefully; macOS sandboxed
  tailscaled uses a TCP+password LocalAPI variant.

### 2. Strong fallback / possible v0 — tunnel the existing socket over SSH

A `scribe proxy` stdio bridge to the local server socket, dialed via
`ssh host scribe proxy` (WezTerm ssh-domain pattern; pairs with Tailscale
SSH for zero-config keys).

- **Pros**: smallest change; no new listener or auth surface (existing
  same-UID check still holds under the ssh'd account).
- **Cons**: requires sshd/Tailscale SSH + matching account; GUI must manage
  an ssh child process; double encryption; clunkier connect UX. Worth keeping
  forever as an escape hatch even if #1 ships.

### 3. NOT recommended now — embedded tsnet/libtailscale node

In-process tailnet identity (golink-style). No official Rust bindings;
community crate dormant; drags the Go runtime into a Rust process; second
device identity to enroll per machine. Revisit only if Tailscale ships
supported Rust bindings.

Also considered and rejected: `tailscale serve --tcp` fronting (identity is
HTTP-only; extra hop) and Tailscale Services `svc:` (tag identity + HA
semantics don't fit a per-user desktop daemon).

## Cross-cutting design notes (from verified pain points)

- Version handshake first — both machines run Scribe but versions will skew
  (WezTerm `CODEC_VERSION` precedent; today the wire protocol has no version
  field and assumes client/server ship together).
- Flood control: bound the per-connection output queue; on overflow, reset +
  replay resync rather than unbounded buffering (tmux pause / WezTerm #2048).
- Keep existing input chunking and frame caps; replay compression already
  handles the big-payload path.
- Later, optional: RTT-gated predictive local echo for relayed paths.

## Plan-phase decisions (Phase 0)

Added by `/speckit-plan`. Each unknown from the plan's Technical Context is
resolved here in Decision / Rationale / Alternatives form. D1 formalizes
the pre-spec recommendation above.

### D1 — Transport architecture

- **Decision**: A TCP listener inside `scribe-server`, bound strictly to
  the machine's Tailscale addresses, speaking the existing 4-byte-BE +
  named-MessagePack framing and sharing the existing message dispatch. The
  listener exists only while `remote.enabled = true`.
- **Rationale**: Matches the industry-standard pattern (Tailscale SSH,
  golink, tclip); reuses framing, dispatch, replay, and window-claim logic;
  no extra processes; `framing.rs` is already stream-generic.
- **Alternatives considered**: SSH tunnel of the unix socket (kept as a
  documented manual escape hatch, not built); embedded tsnet (rejected: no
  viable Rust bindings); `tailscale serve --tcp` (rejected: identity is
  HTTP-only, extra hop).

### D2 — Peer authentication and authorization

- **Decision**: On every accept, resolve the peer via Tailscale LocalAPI
  `whois?addr=ip:port` and require the peer's tailnet user ID to equal the
  host node's own user ID (from LocalAPI `status`). Tagged/identity-less
  nodes and other accounts are refused. Implemented in a new
  `scribe-server/src/tailnet.rs` module as a minimal hand-rolled HTTP/1.1
  client over the tailscaled Unix socket (Linux:
  `/var/run/tailscale/tailscaled.sock`); on macOS, discover the sandboxed
  daemon's TCP port + password via the `sameuserproof` mechanism the
  Tailscale CLI uses. Fail closed on any LocalAPI error.
- **Rationale**: WhoIs replaces `SO_PEERCRED` one-for-one as a
  per-connection identity check; comparing stable user IDs (not display
  names) avoids rename pitfalls; hand-rolling two GET endpoints avoids a
  dependency on a thin third-party crate.
- **Alternatives considered**: `tailscale-localapi` crate (low activity,
  easy to outgrow); shelling out to `tailscale whois` (process-per-connect
  latency, parsing fragility); app-capability grants via ACL `CapMap`
  (deferred until cross-user sharing is in scope).

### D3 — Protocol versioning & compatibility policy

- **Decision**: A remote-only preamble exchanged before anything else on
  TCP connections: client sends `RemoteHandshake { remote_protocol_version,
  scribe_version }`, server replies `RemoteHandshakeReply` with accept or a
  typed refusal (incompatible / disabled / unauthorized). v1 policy is
  **exact match** on a new `REMOTE_PROTOCOL_VERSION: u32` constant;
  mismatch refuses with both versions named (FR-012). The local Unix-socket
  flow is completely unchanged; within an accepted remote session, message
  evolution keeps relying on named-msgpack additive tolerance.
- **Rationale**: Resolves the clarify-phase deferred item. Exact match is
  the simplest safe policy for one user's own machines; the preamble gives
  a clean refusal path before any window data flows, and old servers simply
  have no listener (connection refused → distinct "unreachable/disabled"
  UX). WezTerm's `CODEC_VERSION` handshake is precedent.
- **Alternatives considered**: semver compatibility window (more states to
  test, no v1 benefit); extending `Hello` with a version field (would run
  after accept on the shared dispatch and complicate local-path
  compatibility guarantees).

### D4 — Takeover mechanics

- **Decision**: Add `takeover: bool` (serde-default `false`) to `Hello`.
  When a claim targets a connected window with `takeover = true`, the
  server atomically swaps the client writer under the existing
  `connected_clients` lock, sends the displaced client a new
  `ServerMessage::WindowTakenOver { device, account }`, and proceeds with
  the normal attach/replay flow for the new controller. The displaced
  client stops sending input, renders its last frame dimmed under the
  takeover banner, and offers one-action reclaim (a fresh connection with
  `takeover = true` — identical mechanism in both directions). Local
  same-machine claims keep today's refuse-and-assign-different-window
  behavior unless takeover is explicitly set.
- **Rationale**: Reuses the existing single-writer ownership invariant
  (`Arc::ptr_eq` release guard) instead of introducing multi-writer state;
  symmetric reclaim satisfies FR-007 with one code path; serde default
  keeps old local clients wire-compatible.
- **Alternatives considered**: separate `ClaimWindow` message (more surface
  for the same semantics); server-initiated push of window to a named peer
  (inverts the trust model; rejected).

### D5 — Flow control for slow remote links

- **Decision**: Per-remote-connection bounded output queue. On overflow,
  drop that connection's queued `PtyOutput` backlog, mark affected sessions
  replay-dirty, and send a fresh `SessionReplay` when the link drains
  (catch-up-to-current semantics). Local Unix-socket clients keep today's
  behavior.
- **Rationale**: Satisfies FR-013/PR-004 with a primitive Scribe already
  has; mirrors tmux's pause→capture-pane recovery and avoids WezTerm's
  flood-stall failure mode (#2048).
- **Alternatives considered**: unbounded buffering (unbounded host memory —
  rejected); TCP backpressure propagated to the PTY reader (would stall the
  server's authoritative Term and other clients — rejected).

### D6 — Client reconnect loop

- **Decision**: The remote client auto-reconnects with exponential backoff
  (capped), surfacing a cancelable "reconnecting to <peer>…" state; on
  success it re-runs the preamble and claims its window with
  `Hello { takeover: false }`. Unconnected window (common case) ⇒ normal
  resume with fresh replay. Held by another controller (someone reclaimed
  mid-outage) ⇒ the server answers with the lost-control outcome and the
  client renders the displaced state — automatic reconnection never seizes
  control; `takeover: true` is reserved for explicit user actions (picker
  attach, banner reclaim). Cancel settles into a disconnected state with
  one-action reconnect.
- **Rationale**: Direct encoding of the clarify answer (automatic w/
  status) without violating FR-007's never-silent rule from the other
  side's perspective; replay-on-reattach already guarantees FR-011
  convergence.
- **Alternatives considered**: manual-only (clarify answer rejected it);
  reconnect with forced takeover (rejected: a network blip on B would
  silently re-seize a window A deliberately reclaimed); transparent
  connection migration à la mosh (unnecessary — tailnet IPs are stable
  across path changes; TCP reconnect + replay suffices).

### D7 — Peer discovery & connect UX

- **Decision**: The connect flow lists the user's own online peers from
  LocalAPI `status` (same user ID, filtered to reachable), shown by MagicDNS
  short name; manual host entry remains available. Surfaced as a command
  palette action plus a GPU-overlay picker dialog consistent with existing
  dialog chrome. Each failure class (unreachable / disabled / unauthorized /
  version mismatch / taken over) maps to distinct copy per UX-002.
- **Rationale**: `status` is already required for self-identity (D2), so the
  picker costs nothing extra; palette + overlay matches Constitution III.
- **Alternatives considered**: typing names only (worse discoverability);
  a settings-webview picker (wrong surface for a per-window action).

### D8 — Configuration & live apply

- **Decision**: New `[remote]` TOML table: `enabled: bool = false`,
  `port: u16 = 46061` (configurable; documented). Settings webview gains a
  "Remote" section with the plain-language who-can-connect statement
  (UX-003). Changes propagate through the existing watcher →
  `ConfigReloaded` path; the server starts/stops/rebinds the listener live
  and severs active remote connections on disable (FR-016). **No server
  restart is involved anywhere in this feature.**
- **Rationale**: Identical shape to every existing setting (Constitution
  III); live apply is mandatory because restarting the server is
  prohibited.
- **Alternatives considered**: CLI-only toggle (hidden, inconsistent);
  separate remote-config file (needless second config path).

### D9 — Audit surface

- **Decision**: Resolves the clarify-phase deferred item. v1 records
  accepted connections (device + account), refusals (typed reason), and
  disconnects as structured server-log events; the status bar shows
  remote-enabled and per-window controlled-by state. No dedicated history
  UI in v1.
- **Rationale**: FR-017 requires an auditable record, not a browser; the
  log is the established server-side record with the least new surface.
- **Alternatives considered**: settings-webview audit page (new surface,
  deferred until demand); OS notifications per connect (noisy; the banner
  already provides in-context visibility).

### D10 — Testing & verification strategy

- **Decision**: Manual quickstart scenarios per user story (two tailnet
  machines; dev-flavor builds so the production server is never touched),
  plus replay-fidelity comparison reusing existing snapshot tooling
  (`RequestSnapshot` via `scribe-cli`). No new automated test code unless
  explicitly requested.
- **Rationale**: Constitution II and the repo testing rule; the risky
  surfaces (auth policy, takeover, resync) are exercised directly by the
  quickstart's negative scenarios.
- **Alternatives considered**: new integration harness spinning fake
  tailscaled (valuable later; not requested, and hand-rolled LocalAPI
  parsing is small enough to verify manually).

## Flagged uncertainties

- LocalAPI has no formal stability promise (source comments call it internal
  but "pretty stable in practice").
- `tailscale-localapi` crate maintenance freshness unverified.
- DERP relay latency figures (~100 ms+) from prior knowledge, not
  re-measured.
- WezTerm mutual-TLS client-cert enforcement inferred from codec/config
  rather than listener code.
