//! Minimal Tailscale `LocalAPI` client and identity policy for feature 013
//! (remote window control over Tailscale).
//!
//! This module hand-rolls the one or two `LocalAPI` endpoints the remote-control
//! listener needs, avoiding a third-party dependency on a thin wrapper crate
//! (research D2). It talks to the local `tailscaled` daemon:
//!
//! - Linux: a plain HTTP/1.1 request over the daemon's Unix socket
//!   (`/var/run/tailscale/tailscaled.sock`).
//! - macOS: the sandboxed daemon exposes a localhost TCP `LocalAPI` whose port
//!   and auth token are discovered through the `sameuserproof` mechanism the
//!   Tailscale CLI uses; requests carry HTTP Basic auth.
//!
//! Two endpoints are consumed:
//!
//! - `GET /localapi/v0/status` — this machine's own identity (tailnet user id,
//!   tailnet IPs, device name) plus the same-account peer list for the connect
//!   picker.
//! - `GET /localapi/v0/whois?addr=ip:port` — the tailnet identity behind an
//!   accepted connection.
//!
//! On top of the transport, [`check_policy`] and [`authorize_peer`] implement
//! the same-account authorization rule: a peer is authorized iff it carries a
//! concrete tailnet user id equal to this machine's own and is not tagged.
//! Tagged / identity-less peers are refused (carrying a `tagged` flag for audit
//! detail), and ANY `LocalAPI` failure fails closed — the accept path maps that
//! to an `IdentityUnavailable` refusal (research D2, data-model `TailnetIdentity`).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tracing::debug;

/// Linux path to the `tailscaled` `LocalAPI` Unix socket.
#[cfg(not(target_os = "macos"))]
const TAILSCALED_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

/// `Host` header the `LocalAPI` expects. tailscaled rejects any other host value
/// (and any request carrying `Origin`/`Referer`) as an anti-DNS-rebind guard.
const LOCALAPI_HOST: &str = "local-tailscaled.sock";

/// `LocalAPI` status endpoint (includes the peer list by default).
const STATUS_PATH: &str = "/localapi/v0/status";

/// Overall deadline for a single `LocalAPI` round-trip; a wedged daemon must not
/// stall the accept path (fail closed instead).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on a `LocalAPI` response body, so a misbehaving daemon cannot make
/// us allocate without limit.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// A stable tailnet account id, as reported by the `LocalAPI`. Authorization
/// compares this (never display names). Mirrors Tailscale's `UserID` (`int64`);
/// `0` is the "no user" sentinel, treated as identity-less.
pub type UserId = i64;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failure talking to the Tailscale `LocalAPI`. Every variant is a fail-closed
/// condition: the accept path maps all of them to an `IdentityUnavailable`
/// refusal.
#[derive(Debug, thiserror::Error)]
pub enum TailnetError {
    /// The daemon could not be reached (socket missing, connection refused,
    /// discovery failed, request timed out, or an I/O error mid-request).
    #[error("tailscale daemon unreachable: {reason}")]
    DaemonUnreachable { reason: String },

    /// The `LocalAPI` answered with a non-200 HTTP status.
    #[error("tailscale localapi returned HTTP status {status}")]
    Http { status: u16 },

    /// The `LocalAPI` response could not be parsed into the expected shape.
    #[error("failed to parse tailscale localapi response: {reason}")]
    Parse { reason: String },
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// This machine's own tailnet identity, derived from `LocalAPI` `status`.
#[derive(Debug, Clone)]
pub struct SelfIdentity {
    /// Stable tailnet account id used as the authorization anchor.
    pub user_id: UserId,
    /// Short `MagicDNS` device name, for status/banner display.
    pub device_name: String,
    /// The signed-in tailnet account's login name (e.g. `user@example.com`),
    /// resolved from the `LocalAPI` `User` map via this node's `UserID`. Empty
    /// when the daemon omits the profile. Display-only — authorization always
    /// keys off `user_id`, never this. Feeds the Settings → Remote
    /// "signed in as <account>" statement (UX-003).
    pub login_name: String,
    /// The machine's tailnet IP addresses — the ONLY addresses the remote
    /// listener may bind to (never `0.0.0.0`).
    pub tailnet_ips: Vec<IpAddr>,
}

/// A same-tailnet peer, as surfaced to the connect picker (research D7).
#[derive(Debug, Clone)]
pub struct TailnetPeer {
    /// Short `MagicDNS` name; also usable as a manual-entry target.
    pub name: String,
    /// A tailnet address to dial (IPv4 preferred).
    pub addr: IpAddr,
    /// Whether the peer is currently connected to the control plane.
    pub online: bool,
    /// The peer's tailnet account id.
    pub user_id: UserId,
    /// Whether the peer belongs to the same account as this machine and is not
    /// tagged (the picker lists only these).
    pub same_account: bool,
}

/// This machine's status: own identity plus the same-tailnet peer list.
#[derive(Debug, Clone)]
pub struct TailnetStatus {
    pub self_identity: SelfIdentity,
    pub peers: Vec<TailnetPeer>,
}

/// The tailnet identity behind a single accepted connection, from `whois`.
#[derive(Debug, Clone)]
pub struct TailnetIdentity {
    /// Short `MagicDNS` name, shown in banners/indicators.
    pub node_name: String,
    /// The peer's tailnet account id (`0` if the peer is identity-less).
    pub user_id: UserId,
    /// Account login name, for display and audit only.
    pub login_name: String,
    /// Whether the peer is a tagged node (no user identity).
    pub is_tagged: bool,
}

/// Outcome of the pure same-account authorization policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Peer is the same account and not tagged.
    Authorized,
    /// Peer is refused. `tagged` marks a tagged/identity-less node, carried as
    /// audit `detail=tagged`.
    Refused { tagged: bool },
}

/// Why [`authorize_peer`] refused a connecting peer. Maps 1:1 onto the wire
/// refusal taxonomy at the accept path.
#[derive(Debug, thiserror::Error)]
pub enum PeerAuthError {
    /// Same-account policy refused the peer (wrong account, tagged, or
    /// identity-less). Maps to `RemoteRefusal::Unauthorized`; `tagged` becomes
    /// the audit `detail=tagged` qualifier.
    #[error("remote peer refused by same-account policy (tagged={tagged})")]
    Unauthorized { identity: TailnetIdentity, tagged: bool },

    /// A `LocalAPI` failure — fail closed. Maps to
    /// `RemoteRefusal::IdentityUnavailable`.
    #[error("tailnet identity unavailable: {0}")]
    IdentityUnavailable(#[from] TailnetError),
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Fetch this machine's tailnet status: own identity plus the same-account,
/// online peer list. Fails closed on any `LocalAPI` error.
pub async fn fetch_status() -> Result<TailnetStatus, TailnetError> {
    let dto: StatusDto = localapi_json(STATUS_PATH).await?;

    let self_dto = dto.self_node.ok_or_else(|| TailnetError::Parse {
        reason: "tailscale status is missing the Self identity".to_string(),
    })?;
    let self_user_id =
        self_dto.user_id.filter(|id| *id != 0).ok_or_else(|| TailnetError::Parse {
            reason: "tailscale status Self carries no tailnet user id".to_string(),
        })?;

    // The node's own tailnet IPs live at the top level; fall back to the Self
    // entry if the top-level list is absent.
    let top_ips = dto.tailscale_ips.unwrap_or_default();
    let self_ips = self_dto.tailscale_ips.clone().unwrap_or_default();
    let ip_source = if top_ips.is_empty() { self_ips } else { top_ips };
    let tailnet_ips = parse_ips(&ip_source);

    let device_name = short_name(self_dto.host_name.as_deref(), self_dto.dns_name.as_deref());
    let login_name = self_login_name(dto.users.as_ref(), self_user_id);
    let self_identity =
        SelfIdentity { user_id: self_user_id, device_name, login_name, tailnet_ips };

    let peers: Vec<TailnetPeer> = dto
        .peers
        .unwrap_or_default()
        .into_values()
        .filter_map(|peer| map_peer(peer, self_user_id))
        .collect();

    debug!(
        peers = peers.len(),
        tailnet_ips = self_identity.tailnet_ips.len(),
        "fetched tailnet status"
    );
    Ok(TailnetStatus { self_identity, peers })
}

/// Enumerate the machine's tailnet IP addresses for the remote listener to bind
/// to. Fails closed on any `LocalAPI` error (research D2; the listener must never
/// fall back to a wildcard bind).
pub async fn bind_addresses() -> Result<Vec<IpAddr>, TailnetError> {
    Ok(fetch_status().await?.self_identity.tailnet_ips)
}

/// Resolve the tailnet identity behind `peer_addr` via `LocalAPI` `whois`.
pub async fn whois(peer_addr: SocketAddr) -> Result<TailnetIdentity, TailnetError> {
    let mut path = String::from("/localapi/v0/whois?addr=");
    path.push_str(&percent_encode(&peer_addr.to_string()));
    let dto: WhoIsDto = localapi_json(&path).await?;
    Ok(map_identity(dto))
}

/// The pure same-account authorization policy: authorize iff the peer carries a
/// concrete tailnet user id equal to `self_user_id` and is not tagged.
#[must_use]
pub fn check_policy(peer: &TailnetIdentity, self_user_id: UserId) -> PolicyDecision {
    if peer.is_tagged {
        return PolicyDecision::Refused { tagged: true };
    }
    if peer.user_id != 0 && peer.user_id == self_user_id {
        PolicyDecision::Authorized
    } else {
        PolicyDecision::Refused { tagged: false }
    }
}

/// Resolve and authorize a connecting peer end-to-end: read this machine's own
/// identity and the peer's `whois`, then apply [`check_policy`]. Any `LocalAPI`
/// failure fails closed as [`PeerAuthError::IdentityUnavailable`]; a policy
/// refusal is [`PeerAuthError::Unauthorized`] carrying the resolved identity and
/// the tagged qualifier for audit.
pub async fn authorize_peer(peer_addr: SocketAddr) -> Result<TailnetIdentity, PeerAuthError> {
    let status = fetch_status().await?;
    let identity = whois(peer_addr).await?;
    match check_policy(&identity, status.self_identity.user_id) {
        PolicyDecision::Authorized => Ok(identity),
        PolicyDecision::Refused { tagged } => Err(PeerAuthError::Unauthorized { identity, tagged }),
    }
}

// ---------------------------------------------------------------------------
// `LocalAPI` response DTOs (narrow — only the fields we consume)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StatusDto {
    #[serde(rename = "Self")]
    self_node: Option<PeerStatusDto>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "Peer")]
    peers: Option<HashMap<String, PeerStatusDto>>,
    /// `LocalAPI` `User` map (tailnet `UserID` → profile). Used to resolve THIS
    /// node's own signed-in login name via `Self.UserID` for the Settings →
    /// Remote statement; the ids are stringified in Tailscale's JSON.
    #[serde(rename = "User")]
    users: Option<HashMap<String, UserProfileDto>>,
}

#[derive(Debug, Deserialize)]
struct PeerStatusDto {
    #[serde(rename = "HostName")]
    host_name: Option<String>,
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
    #[serde(rename = "UserID")]
    user_id: Option<i64>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "Tags")]
    tags: Option<Vec<String>>,
    #[serde(rename = "Online")]
    online: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct WhoIsDto {
    #[serde(rename = "Node")]
    node: Option<NodeDto>,
    #[serde(rename = "UserProfile")]
    user_profile: Option<UserProfileDto>,
}

#[derive(Debug, Default, Deserialize)]
struct NodeDto {
    #[serde(rename = "User")]
    user: Option<i64>,
    #[serde(rename = "Tags")]
    tags: Option<Vec<String>>,
    #[serde(rename = "ComputedName")]
    computed_name: Option<String>,
    #[serde(rename = "Name")]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserProfileDto {
    #[serde(rename = "LoginName")]
    login_name: Option<String>,
}

// ---------------------------------------------------------------------------
// DTO → domain mapping
// ---------------------------------------------------------------------------

fn map_peer(dto: PeerStatusDto, self_user_id: UserId) -> Option<TailnetPeer> {
    let ips = parse_ips(&dto.tailscale_ips.unwrap_or_default());
    let addr = preferred_addr(&ips)?;
    let user_id = dto.user_id.unwrap_or(0);
    let tagged = dto.tags.as_ref().is_some_and(|tags| !tags.is_empty());
    let name = short_name(dto.host_name.as_deref(), dto.dns_name.as_deref());
    let online = dto.online.unwrap_or(false);
    let same_account = !tagged && user_id != 0 && user_id == self_user_id;
    Some(TailnetPeer { name, addr, online, user_id, same_account })
}

fn map_identity(dto: WhoIsDto) -> TailnetIdentity {
    let node = dto.node.unwrap_or_default();
    let user_id = node.user.unwrap_or(0);
    let is_tagged = node.tags.as_ref().is_some_and(|tags| !tags.is_empty());
    let node_name = short_name(node.computed_name.as_deref(), node.name.as_deref());
    let login_name = dto.user_profile.and_then(|profile| profile.login_name).unwrap_or_default();
    TailnetIdentity { node_name, user_id, login_name, is_tagged }
}

/// Resolve THIS node's own account login name from the `LocalAPI` `User` map by
/// its `UserID`. Empty when the map is absent or omits the profile — the
/// Settings → Remote statement then keeps its generic account placeholder.
fn self_login_name(users: Option<&HashMap<String, UserProfileDto>>, user_id: UserId) -> String {
    users
        .and_then(|users| users.get(&user_id.to_string()))
        .and_then(|profile| profile.login_name.clone())
        .unwrap_or_default()
}

/// Prefer the short host name; otherwise the first label of the `MagicDNS` name.
fn short_name(host_name: Option<&str>, dns_name: Option<&str>) -> String {
    host_name
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
        .or_else(|| dns_name.map(first_label))
        .unwrap_or_default()
}

fn first_label(dns: &str) -> String {
    dns.trim_end_matches('.').split('.').next().unwrap_or(dns).to_string()
}

fn parse_ips(raw: &[String]) -> Vec<IpAddr> {
    raw.iter().filter_map(|value| value.parse::<IpAddr>().ok()).collect()
}

fn preferred_addr(ips: &[IpAddr]) -> Option<IpAddr> {
    ips.iter().find(|ip| ip.is_ipv4()).copied().or_else(|| ips.first().copied())
}

// ---------------------------------------------------------------------------
// Hand-rolled HTTP/1.1 `LocalAPI` transport
// ---------------------------------------------------------------------------

/// Perform a `LocalAPI` GET and deserialize the JSON body, under a hard deadline.
async fn localapi_json<T: DeserializeOwned>(path: &str) -> Result<T, TailnetError> {
    let body = match tokio::time::timeout(REQUEST_TIMEOUT, localapi_get(path)).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(TailnetError::DaemonUnreachable {
                reason: "tailscale localapi request timed out".to_string(),
            });
        }
    };
    serde_json::from_slice(&body).map_err(|err| TailnetError::Parse { reason: err.to_string() })
}

/// Linux (and other non-macOS unix): dial the daemon's Unix socket.
#[cfg(not(target_os = "macos"))]
async fn localapi_get(path: &str) -> Result<Vec<u8>, TailnetError> {
    let stream = tokio::net::UnixStream::connect(TAILSCALED_SOCKET)
        .await
        .map_err(|err| TailnetError::DaemonUnreachable { reason: err.to_string() })?;
    send_request(stream, path, None).await
}

/// Build the request line + headers for a bodyless GET. Never sends
/// `Origin`/`Referer` (tailscaled rejects those); `Host` is always the `LocalAPI`
/// sentinel host even when dialing a TCP port.
fn build_request(path: &str, auth: Option<&str>) -> String {
    let mut request = String::new();
    request.push_str("GET ");
    request.push_str(path);
    request.push_str(" HTTP/1.1\r\n");
    request.push_str("Host: ");
    request.push_str(LOCALAPI_HOST);
    request.push_str("\r\n");
    if let Some(auth) = auth {
        request.push_str("Authorization: ");
        request.push_str(auth);
        request.push_str("\r\n");
    }
    request.push_str("Connection: close\r\n\r\n");
    request
}

/// Write the request and read the whole (bounded) response over an already
/// connected stream, returning the JSON body bytes.
async fn send_request<S>(
    mut stream: S,
    path: &str,
    auth: Option<&str>,
) -> Result<Vec<u8>, TailnetError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request = build_request(path, auth);
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|err| TailnetError::DaemonUnreachable { reason: err.to_string() })?;
    stream
        .flush()
        .await
        .map_err(|err| TailnetError::DaemonUnreachable { reason: err.to_string() })?;

    // `Connection: close` ⇒ the server writes the response and closes; read to
    // EOF under a size cap. `take` bounds the read without holding a large
    // buffer across the await point. Any chunked framing is decoded in
    // `extract_body`.
    let mut raw = Vec::new();
    let read = stream
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .await
        .map_err(|err| TailnetError::DaemonUnreachable { reason: err.to_string() })?;
    if read > MAX_RESPONSE_BYTES {
        return Err(TailnetError::Parse {
            reason: "tailscale localapi response exceeded size cap".to_string(),
        });
    }

    extract_body(&raw)
}

/// Split an HTTP response into head + body, verify a 200, and return the entity
/// body — de-framing chunked transfer-encoding when the daemon uses it (Go's
/// `net/http` chunks larger `LocalAPI` responses under HTTP/1.1).
fn extract_body(raw: &[u8]) -> Result<Vec<u8>, TailnetError> {
    let header_end = find_subslice(raw, b"\r\n\r\n").ok_or_else(|| TailnetError::Parse {
        reason: "tailscale localapi response has no header terminator".to_string(),
    })?;
    let head = raw.get(..header_end).unwrap_or_default();
    let body = raw.get(header_end + 4..).unwrap_or_default();

    let head_text = std::str::from_utf8(head).map_err(|_| TailnetError::Parse {
        reason: "tailscale localapi response header is not valid UTF-8".to_string(),
    })?;

    let status = parse_status_code(head_text)?;
    if status != 200 {
        return Err(TailnetError::Http { status });
    }

    if header_is_chunked(head_text) {
        dechunk(body)
    } else {
        // With `Connection: close` the whole entity body was already read to EOF.
        Ok(body.to_vec())
    }
}

fn parse_status_code(head: &str) -> Result<u16, TailnetError> {
    let status_line = head.lines().next().unwrap_or_default();
    // e.g. "HTTP/1.1 200 OK" → the second whitespace-separated token.
    let code = status_line.split_whitespace().nth(1).ok_or_else(|| TailnetError::Parse {
        reason: "tailscale localapi response has a malformed status line".to_string(),
    })?;
    code.parse::<u16>().map_err(|_| TailnetError::Parse {
        reason: "tailscale localapi response has an unparseable status code".to_string(),
    })
}

/// Whether the response headers declare `Transfer-Encoding: chunked`.
fn header_is_chunked(head: &str) -> bool {
    head.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
        })
    })
}

/// Decode an HTTP/1.1 chunked-transfer body into the raw entity bytes.
fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, TailnetError> {
    let mut out = Vec::new();
    loop {
        let line_end = find_subslice(body, b"\r\n").ok_or_else(|| TailnetError::Parse {
            reason: "tailscale localapi chunk size line is missing its terminator".to_string(),
        })?;
        let size = parse_chunk_size(body.get(..line_end).unwrap_or_default())?;
        body = body.get(line_end + 2..).unwrap_or_default();
        if size == 0 {
            break;
        }
        let chunk = body.get(..size).ok_or_else(|| TailnetError::Parse {
            reason: "tailscale localapi chunk data is truncated".to_string(),
        })?;
        out.extend_from_slice(chunk);
        if out.len() > MAX_RESPONSE_BYTES {
            return Err(TailnetError::Parse {
                reason: "tailscale localapi chunked response exceeded size cap".to_string(),
            });
        }
        // Skip the chunk data and its trailing CRLF.
        body = body.get(size + 2..).unwrap_or_default();
    }
    Ok(out)
}

fn parse_chunk_size(line: &[u8]) -> Result<usize, TailnetError> {
    let text = std::str::from_utf8(line).map_err(|_| TailnetError::Parse {
        reason: "tailscale localapi chunk size is not valid UTF-8".to_string(),
    })?;
    // Ignore any chunk extension after a ';'.
    let hex = text.split(';').next().unwrap_or(text).trim();
    usize::from_str_radix(hex, 16).map_err(|_| TailnetError::Parse {
        reason: "tailscale localapi chunk size is unparseable".to_string(),
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Percent-encode a string for use as a URL query value (RFC 3986 unreserved
/// set kept verbatim; everything else `%`-escaped). Keeps `ip:port` — including
/// bracketed IPv6 — valid in the `whois?addr=` query.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

// ---------------------------------------------------------------------------
// macOS: sandboxed daemon discovery + TCP `LocalAPI` with Basic auth
// ---------------------------------------------------------------------------

/// macOS: dial the sandboxed daemon's localhost TCP `LocalAPI`, authenticating
/// with the `sameuserproof` token as an HTTP Basic-auth password.
#[cfg(target_os = "macos")]
async fn localapi_get(path: &str) -> Result<Vec<u8>, TailnetError> {
    // Discovery reads files / runs `lsof`, so keep it off the async reactor.
    let creds = tokio::task::spawn_blocking(discover_localapi).await.map_err(|err| {
        TailnetError::DaemonUnreachable { reason: format!("localapi discovery task failed: {err}") }
    })?;
    let (port, token) = creds?;

    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|err| TailnetError::DaemonUnreachable { reason: err.to_string() })?;
    let auth = basic_auth_header(&token);
    send_request(stream, path, Some(auth.as_str())).await
}

/// Discover the sandboxed daemon's `LocalAPI` `(port, token)` the way the
/// Tailscale CLI does: the App Store variant first (open `sameuserproof` file
/// belonging to the `IPNExtension` process, found via `lsof`), then the
/// standalone "macsys" variant (files under `/Library/Tailscale`).
#[cfg(target_os = "macos")]
fn discover_localapi() -> Result<(u16, String), TailnetError> {
    read_appstore_proof().or_else(read_macsys_proof).ok_or_else(|| {
        TailnetError::DaemonUnreachable {
            reason: "tailscale localapi credentials not found (sameuserproof)".to_string(),
        }
    })
}

/// App Store variant: `lsof` the current user's `IPNExtension` open files and
/// pull `<port>-<token>` out of the `sameuserproof-<port>-<token>` filename.
#[cfg(target_os = "macos")]
fn read_appstore_proof() -> Option<(u16, String)> {
    // Homogeneous `&str` args: bind the formatted uid selector first.
    let user_arg = format!("-u{}", scribe_common::socket::current_uid());
    let output = std::process::Command::new("lsof")
        .args(["-n", "-a", user_arg.as_str(), "-c", "IPNExtension", "-F"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let needle = ".tailscale.ipn.macos/sameuserproof-";
    for line in text.lines() {
        let Some(index) = line.find(needle) else {
            continue;
        };
        let Some(rest) = line.get(index + needle.len()..) else {
            continue;
        };
        let mut parts = rest.splitn(2, '-');
        let port_str = parts.next()?;
        let token = parts.next()?;
        if let Ok(port) = port_str.parse::<u16>() {
            return Some((port, token.to_string()));
        }
    }
    None
}

/// Standalone "macsys" variant: `/Library/Tailscale/ipnport` is a symlink whose
/// target is the port; `/Library/Tailscale/sameuserproof-<port>` holds the
/// token as a hex string.
#[cfg(target_os = "macos")]
fn read_macsys_proof() -> Option<(u16, String)> {
    let shared_dir = std::path::Path::new("/Library/Tailscale");
    let port_target = std::fs::read_link(shared_dir.join("ipnport")).ok()?;
    let port_str = port_target.to_str()?;
    let port = port_str.parse::<u16>().ok()?;
    let raw = std::fs::read_to_string(shared_dir.join(format!("sameuserproof-{port_str}"))).ok()?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        return None;
    }
    Some((port, token))
}

/// Build the HTTP Basic-auth header value. tailscaled compares only the
/// password field against its required token, so the username is empty
/// (matching the Tailscale client's `SetBasicAuth("", token)`).
#[cfg(target_os = "macos")]
fn basic_auth_header(token: &str) -> String {
    format!("Basic {}", base64_encode(format!(":{token}").as_bytes()))
}

/// Standard (padded) base64 encoding — a tiny hand-rolled encoder so macOS
/// Basic auth needs no extra dependency. Works entirely in the `u8` domain to
/// keep every 6-bit index in range without lossy casts.
#[cfg(target_os = "macos")]
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let encode = |sextet: u8| -> char {
        TABLE.get(usize::from(sextet & 0x3f)).map_or('=', |byte| char::from(*byte))
    };
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(encode(b0 >> 2));
        out.push(encode(((b0 & 0x03) << 4) | (b1 >> 4)));
        out.push(if chunk.len() > 1 { encode(((b1 & 0x0f) << 2) | (b2 >> 6)) } else { '=' });
        out.push(if chunk.len() > 2 { encode(b2 & 0x3f) } else { '=' });
    }
    out
}
