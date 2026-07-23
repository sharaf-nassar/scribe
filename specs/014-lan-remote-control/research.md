# Research: LAN Remote Window Control (Phase 0)

**Feature**: `014-lan-remote-control` | **Date**: 2026-07-08
**Question**: How to add a Tailscale-free LAN transport, peer discovery, and
device-approval authorization to Scribe's remote window control (013),
keeping it local-first, encrypted, and off-by-default.

Consolidates three web-research passes (mDNS discovery, device
identity/pairing/TLS, trusted-network identification). Each decision is
Decision / Rationale / Alternatives. Symbols and structure of the 013
feature this layers onto are known from `lat.md/server#Server#Remote
Control` and `lat.md/protocol#Protocol#Remote Protocol`.

## D1 — LAN peer discovery

- **Decision**: Use `mdns-sd` (0.20.x, pure Rust, no Avahi/Bonjour daemon)
  for both advertising and browsing. Service type `_scribe._tcp.local.`;
  the control port rides the SRV record; TXT carries `txtvers=1`,
  `id=<device-id>`, `protovers=<remote protocol version>`. Dedupe peers by
  TXT `id`, filter resolved addresses to the current LAN subnet, and
  `disable_interface` the tailnet/VPN interfaces so a peer never resolves
  to a `100.x` tailscale address.
- **Rationale**: Pure-Rust satisfies local-first (no extra system service);
  it is the only well-maintained crate doing advertise + browse + async
  (`recv_async().await`) cross-platform (Linux + macOS), and coexists with
  a host Avahi/Bonjour (mandatory on macOS where mDNSResponder owns 5353).
- **Alternatives**: `zeroconf`/`astro-dnssd` (require Avahi on Linux + C
  FFI — breaks no-extra-services); `libmdns` (responder-only, cannot
  browse); `searchlight` (unmaintained since 2023, drags in hickory-dns).
- **Pitfalls to honor**: multi-interface/VPN is the #1 gotcha (filter both
  sides); dedupe by `id` not socket address (IPv4/IPv6 double-listing);
  crashed peers linger to TTL — use `ServiceRemoved` + `verify()` to evict;
  macOS 14+ prompts for Local Network access on first multicast.
  Source: docs.rs/mdns-sd, RFC 6763 §6–7.

## D2 — Device identity

- **Decision**: Each Scribe install generates once a self-signed X.509
  certificate wrapping an **Ed25519** keypair (via `rcgen` 0.13,
  `PKCS_ED25519`). The **Device ID** is `SHA-256(SubjectPublicKeyInfo)` — a
  32-byte value that is the trust anchor and the mDNS TXT `id`. The private
  key is sealed in the OS keyring (already a workspace dependency).
- **Rationale**: Self-signed cert is wire-compatible with TLS unchanged and
  matches the accepted self-hosted-pairing standard (Syncthing, KDE
  Connect). Hashing the SPKI (not the whole cert) lets the cert be re-minted
  (validity/CN) without changing identity or forcing re-approval.
- **Alternatives**: RFC 7250 raw public keys (identity == raw key, no X.509
  parsing; supported in rustls 0.23 but thinner interop — defer to v2);
  whole-cert-DER hash à la Syncthing (simpler but couples ID to the exact
  cert); Noise protocol (cleanest P2P but a new crypto stack + bespoke
  framing when rustls is already present).

## D3 — Transport encryption (mutual TLS)

- **Decision**: Wrap the LAN TCP stream in **TLS 1.3, mutual** (both peers
  present and verify certs) via `rustls` 0.23 + `tokio-rustls` for async
  accept/connect. Both roles install a custom verifier
  (`ServerCertVerifier` on the dialer, `ClientCertVerifier` on the
  listener) plus their own cert (`with_client_auth_cert` /
  `with_client_cert_verifier` — NOT `with_no_client_auth`). The TLS 1.3
  AEAD is the link encryption (FR-007); no hand-rolled crypto on top.
- **Rationale**: `rustls` is already in the tree (via reqwest rustls-tls);
  mutual TLS reuses it with a ~150-line verifier and gives both encryption
  and per-connection identity proof. The `danger` modules are always
  available in 0.23 (the `dangerous_configuration` feature was removed in
  0.22). Verified verbatim against docs.rs/rustls 0.23.40.
- **Critical correctness**: a TOFU verifier skips CA-chain validation but
  MUST still verify the handshake signature (proves the peer holds the
  private key) — delegate `verify_tls12_signature`/`verify_tls13_signature`
  to `rustls::crypto::verify_tls1{2,3}_signature`; never stub them `Ok`.
  Connecting by IP, pass a fixed placeholder `ServerName` ("scribe");
  identity comes from the pinned key, not the name.
- **Alternatives**: reqwest (HTTP-client only, cannot accept inbound);
  Noise/`snow` (new dep + framing); raw rustls without tokio-rustls
  (manual async plumbing).

## D4 — Trust-on-first-use + approval flow

- **Decision**: The custom verifier accepts an unknown peer's handshake
  (records the presented identity as "pending"), the handshake completes,
  and the **app layer** (not the verifier, which cannot await user input)
  gates on approval: a matched pinned identity proceeds; a pending identity
  shows the owning-side approval prompt and exchanges no window/session data
  until approved; on approve the Device ID + cert + label + approval time +
  network context are persisted, on decline the connection is torn down.
  Later connections are enforced strictly by the verifier against the pinned
  `device_id`; a peer that regenerated its key presents a NEW `device_id` and
  is simply a new unknown device requiring fresh approval (never silently
  trusted), with a name-collision hint on the prompt — not a distinct
  "identity changed" state (analysis I1).
- **Rationale**: This is the KDE Connect / Syncthing model — explicit
  approve + pin-on-accept + strict pinning thereafter. The approval prompt
  displays the peer's fingerprint so the user can recognize it.
- **First-connect MITM**: TOFU's one hole is an on-path attacker during the
  very first connection. v1 mitigates it primarily with the trusted-network
  gate (D5) — pairing only happens on a network the user vouched for — plus
  showing the fingerprint on both the prompt and each machine's Settings for
  an OPTIONAL out-of-band compare. The residual case (an attacker already on
  a trusted LAN impersonating a device on its first pairing) is **explicitly
  accepted for v1** (analysis C2). The design MUST NOT preclude two later
  hardenings that close it: a mandatory out-of-band short-authentication-string
  cross-check (same fingerprint on both machines, user confirms match) and an
  optional SPAKE2 one-time-code "secure pairing" mode.
- **Alternatives**: SAS-mandatory or SPAKE2-mandatory first pair (stronger
  but more friction; the user chose approve-on-sight gated by trusted
  networks); static shared passphrase (no per-device identity/revocation).

## D5 — Trusted-network activation gate

- **Decision**: LAN discovery + listening are active only while the machine
  is on a network the user has explicitly marked trusted. The network
  fingerprint is the **default-gateway MAC address** (primary anchor) plus
  the **local subnet/CIDR** (corroborator), read via `netdev` 0.45 (netlink
  on Linux, routing table + SystemConfiguration on macOS — permissionless,
  no root). A match requires equal gateway MAC (non-zero) and subnet. **Fail
  closed** on a zero/unresolved gateway MAC, no default route, or when the
  default route is a VPN/tunnel: fingerprint the **physical LAN interface**,
  not the tunnel, so LAN + tailnet coexist (FR-008/009). SSID is stored as a
  display-only label (unreadable to a background app on macOS 14.5+).
- **Rationale**: Gateway-MAC + subnet is the pragmatic sweet spot — the most
  robust permissionless, transport-agnostic signal, stable across Wi-Fi
  roaming (unlike BSSID), present on wired/VPN (unlike SSID). It is a
  **defense-in-depth activation gate, not authentication**: its job is to
  stop accidental café-Wi-Fi exposure. Real security is the layered device
  approval (D4) + TLS pinning (D2/D3) + fail-closed. This mirrors Syncthing,
  whose docs state local discovery is not crypto-protected precisely because
  trust is anchored in pinned certs.
- **Honest residual risk**: an attacker already on the trusted LAN can
  ARP-spoof the gateway MAC and re-open the listener — but they still cannot
  connect without being an approved, identity-pinned device over pinned TLS.
- **Alternatives**: SSID-only (absent on wired/VPN, unreadable on modern
  macOS, trivially spoofed); "any RFC-1918 net" (too coarse — café is also
  192.168.x); single opaque hash of all signals (brittle to benign IP
  changes — store structured fields instead).

## D6 — How it slots into feature 013

- **Decision**: Add a **LAN transport path alongside** the existing tailnet
  path, reusing 013's machinery. The `RemoteControl` supervisor
  (`ipc_server.rs`) generalizes to manage a LAN listener (gated on the
  trusted-network check + the separate LAN opt-in) in addition to the
  tailnet listener. A LAN connection: TCP accept → TLS handshake (mutual,
  pinning verifier) → device-trust/approval gate → the SAME
  `serve_connection` dispatch, with a `ClientSink::Remote` bounded queue and
  a controller identity of `Remote(device name)` instead of a tailnet login.
  Discovery, device-trust store, network-trust store, and TLS live in new
  modules; the wire protocol and takeover model are unchanged.
- **Rationale**: Maximum reuse — takeover, flow control, replay resync,
  host-action gates, and audit are transport-agnostic once a connection
  reaches `serve_connection`. The only genuinely new surfaces are: get on
  the wire (mDNS + TLS), decide who is allowed (device approval + network
  gate), and pick a path (D7).
- **Refactor required (analysis C4/S1)**: 013's `RemoteControl` is
  single-transport — one `enabled`, shared connection + pending-handshake
  caps, one conn-id-keyed sever registry. Hosting LAN "alongside" it demands
  splitting that state per-transport (each transport its own enable, listener,
  caps, and sever registry) plus a `device_id → connection-id` index so a
  per-device revoke (FR-010) and a per-transport disable (FR-012) target only
  the right connections, and LAN caps/pending-holds cannot starve tailnet
  admission. This is a clean generalization, done in Phase 2 before either
  transport's story work.
- **Compatibility**: LAN is a separate opt-in from tailnet (FR-012); the
  local Unix-socket and tailnet paths are unchanged. The remote protocol
  version applies unchanged (exact match). New `[remote.lan]` config.

## D7 — Transport selection (prefer LAN, fall back to tailnet)

- **Decision**: On the connecting side, a peer discovered via mDNS on the
  LAN and the same peer known via the tailnet picker are deduped by **machine
  name/hostname** (mDNS TXT `host` vs the tailnet MagicDNS name) — a UX
  convenience, NOT cryptographic identity, because the LAN device id
  (SHA-256 SPKI) and the tailnet identity are different namespaces and
  013's `RemotePeerInfo` carries no Scribe device id (analysis C3). A
  confidently name-matched dual-reachable peer uses the direct LAN path and
  appears once (FR-008); when names don't confidently match, it may list once
  per transport with clear labels. Automatic with a manual override.
- **Rationale**: Direct encoding of "only need Tailscale when not home." LAN
  is preferred when present because it is the direct path and needs no
  account. Dedup by identity avoids a double-listed peer.
- **Alternatives**: user-chosen transport per connection (more friction);
  always-tailnet-when-available (defeats the local-first goal).

## D8 — Fingerprint presentation

- **Decision**: Store the full 256-bit Device ID; in the approval prompt and
  trusted-devices list show a short authentication string as a **word list**
  (BIP39/PGP-style, read-aloud friendly) alongside grouped hex/base32 for
  exact paste/compare. A future SAS/QR compare (D4) reuses this rendering.
- **Rationale**: Word lists are the least error-prone spoken; grouped
  base32 with check digits (Syncthing style) is the compact canonical form.
- **Alternatives**: emoji SAS (ambiguous rendering); raw hex only (error-
  prone to compare aloud).

## D9 — New dependencies & compatibility

- **Decision**: Add `mdns-sd` (discovery), `netdev` (network fingerprint),
  `rcgen` (self-signed Ed25519 cert), and `tokio-rustls` (async mutual TLS);
  promote `rustls` to a direct workspace dependency (already transitively
  present via reqwest). All are the standard, actively-maintained choice for
  their purpose with no existing in-tree abstraction to reuse.
- **Compatibility**: additive only. New `[remote.lan]` config table (off by
  default), new persisted trust stores (trusted devices, trusted networks,
  device keypair). The remote wire protocol gains device-approval framing
  under the same exact-match version policy as 013; the local and tailnet
  paths are unchanged. The private key + trust stores are new on-disk state
  needing a documented location and a first-run generation step.
- **cargo-machete / deny**: each new dep is actually used; run the same
  license/security gate (cargo-deny) 013 passed.

## D10 — Testing & verification strategy

- **Decision**: Pure logic is unit-test-friendly and worth cheap coverage
  where it exists already or is naturally seamed: the pinning verifier's
  match/mismatch/pending decisions, Device-ID derivation, network
  fingerprint match/fail-closed rules, and mDNS TXT dedupe. End-to-end
  validation is manual on two machines on a real LAN (quickstart), since
  discovery, TLS, and network identity need real interfaces. No automated
  tests are added unless requested (project rule); every user story carries
  a manual quickstart scenario.
- **Rationale**: Constitution II and the repo testing rule; the security-
  critical seams (verifier, network gate) are pure functions that are the
  highest-ROI to unit test if the user opts in.

## Flagged uncertainties (carry into implementation)

- `mdns-sd` `SO_REUSEADDR` flag not read verbatim (coexistence confirmed
  indirectly); verify on macOS where 5353 reuse is mandatory.
- `netdev` macOS gateway-MAC may be the zero address if the neighbor cache
  is cold — add a probe/retry; verify across macOS 13/14/15.
- Confirm `netdev`'s gateway/interface reads trip no macOS Location/TCC
  prompt (it links `objc2-core-wlan` but we never call SSID APIs).
- RFC 7250 raw-public-key path deferred; if ever adopted, prototype the
  handshake first (thinner interop).
- rustls builder generic parameters re-checked at code time (trait/method
  names confirmed current; exact `CertificateDer<'static>` generics from
  standard usage).
