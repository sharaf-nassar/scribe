//! Feature 013/014 remote dial + handshake preamble, ported into the GPUI
//! rebuild.
//!
//! Holds the transport-free pieces of the winit client's dial path
//! ([`ipc_client.rs`](../../scribe-client/src/ipc_client.rs)): the
//! `SCRIBE_REMOTE_DIAL` / `SCRIBE_LAN_DIAL` plumbing-hook env parser and the
//! [`RemoteHandshake`](scribe_common::protocol::ClientMessage::RemoteHandshake)
//! preamble exchange. The connect picker ([`crate::remote`]) spawns a fresh
//! client process per remote-control window and passes the dial target through
//! these env vars; a process launched by the raw plumbing hook keeps its own
//! `--window-id` and never takes over.
//!
//! [`perform_remote_handshake`] runs the preamble over any framed async stream
//! (a real `TcpStream` in the shipped client, an in-memory duplex in the scripted
//! handshake test): it sends the preamble as the first frame, reads the mandatory
//! [`RemoteHandshakeReply`](scribe_common::protocol::ServerMessage::RemoteHandshakeReply),
//! and maps every terminal condition to a [`RemoteConnectOutcome`].

use std::time::Duration;

use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::WindowId;
use scribe_common::protocol::{ClientMessage, REMOTE_PROTOCOL_VERSION, ServerMessage};
use scribe_common::socket::server_socket_path;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};

use crate::remote::RemoteConnectOutcome;

/// How long a TCP connect to a tailnet peer may take before it is abandoned.
/// A `MagicDNS` name either resolves and answers promptly or the peer is not
/// reachable; anything slower is indistinguishable from unreachable to a user
/// waiting on a window.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a transient local-socket probe (`GetRemoteEnv`) waits for its single
/// reply. The server answers from its own `LocalAPI` view with its own fail-closed
/// timeout, so anything slower is a wedged server rather than a slow tailnet.
const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The tailnet dial hook env var: `host` or `host:port`, a `MagicDNS` name or IP.
pub const REMOTE_DIAL_ENV: &str = "SCRIBE_REMOTE_DIAL";
/// The LAN dial hook env var: `host` or `host:port` (resolved subnet address).
pub const LAN_DIAL_ENV: &str = "SCRIBE_LAN_DIAL";
/// The claim-target env var set by the picker when spawning a remote-control
/// process: names an existing window to claim (absent ⇒ a fresh window).
pub const REMOTE_WINDOW_ENV: &str = "SCRIBE_REMOTE_WINDOW";
/// The explicit-attach marker env var: marks the explicit-attach path that may
/// displace a connected controller.
pub const REMOTE_TAKEOVER_ENV: &str = "SCRIBE_REMOTE_TAKEOVER";

/// Bundled parameters for a remote/LAN dial: the address to reach and the window
/// claim to make once the preamble is accepted. Grouped so the dial target and
/// claim travel as one unit past the argument-count budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDial {
    pub host: String,
    pub port: u16,
    /// `None` creates a fresh window on the peer; `Some` claims an existing one.
    pub window_id: Option<WindowId>,
    /// Set only for explicit picker attach / lost-control reclaim, never on the
    /// auto-reconnect path (FR-011).
    pub takeover: bool,
}

/// Parse a `host` / `host:port` dial-target string shared by the plumbing hooks
/// (`SCRIBE_REMOTE_DIAL`, feature 013; `SCRIBE_LAN_DIAL`, feature 014). Returns
/// `None` when empty so the caller keeps the default local Unix-socket path.
/// Splits on the FINAL colon only when the host carries no colon of its own, so a
/// bare IPv6 literal falls through to `default_port` and is dialed verbatim.
#[must_use]
pub fn parse_dial_target(raw: &str, default_port: u16) -> Option<(String, u16)> {
    let target = raw.trim();
    if target.is_empty() {
        return None;
    }
    let (host, port) = match target.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(':') => {
            (host, port.parse::<u16>().unwrap_or(default_port))
        }
        _ => (target, default_port),
    };
    Some((host.to_owned(), port))
}

/// The `host:port` of the tailnet peer to dial, from the `SCRIBE_REMOTE_DIAL`
/// plumbing hook. `None` — the normal case — keeps the client on its local Unix
/// socket.
///
/// Named to match [`crate::lan_dial::target_from_env`] so the shell's transport
/// selection reads as one table of two identically-shaped hooks.
#[must_use]
pub fn target_from_env() -> Option<(String, u16)> {
    remote_dial_target_from_env()
}

/// Parse the optional `SCRIBE_REMOTE_DIAL` plumbing hook. `None` when unset or
/// empty so the default local Unix-socket path runs.
#[must_use]
pub fn remote_dial_target_from_env() -> Option<(String, u16)> {
    let raw = std::env::var(REMOTE_DIAL_ENV).ok()?;
    let default_port = scribe_common::config::RemoteConfig::default().port;
    parse_dial_target(&raw, default_port)
}

/// Parse the optional `SCRIBE_LAN_DIAL` plumbing hook. The default port is the LAN
/// listener port (46062), distinct from the tailnet 46061.
#[must_use]
pub fn lan_dial_target_from_env() -> Option<(String, u16)> {
    let raw = std::env::var(LAN_DIAL_ENV).ok()?;
    let default_port = scribe_common::config::LanRemoteConfig::default().port;
    parse_dial_target(&raw, default_port)
}

/// Parse the optional `SCRIBE_REMOTE_WINDOW` claim target set by the connect
/// picker. `None` (unset, empty, or unparsable) creates a fresh window on the
/// peer; `Some` claims that existing window.
#[must_use]
pub fn remote_dial_window_from_env() -> Option<WindowId> {
    let raw = std::env::var(REMOTE_WINDOW_ENV).ok()?;
    parse_window_target(&raw)
}

/// Parse a `SCRIBE_REMOTE_WINDOW` value into a [`WindowId`]. Split out from the
/// env read so it is testable without mutating process env (which the workspace
/// lints ban). `None` for an empty or unparsable value.
#[must_use]
pub fn parse_window_target(raw: &str) -> Option<WindowId> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<WindowId>() {
        Ok(window_id) => Some(window_id),
        Err(error) => {
            tracing::warn!(%error, value = %trimmed, "invalid SCRIBE_REMOTE_WINDOW; ignoring");
            None
        }
    }
}

/// Whether `SCRIBE_REMOTE_TAKEOVER` marks this as the explicit-attach path that
/// may displace a connected controller. Only ever set by the picker's attach
/// action, never on the auto-reconnect path (FR-011).
#[must_use]
pub fn remote_dial_takeover_from_env() -> bool {
    std::env::var(REMOTE_TAKEOVER_ENV).is_ok_and(|value| flag_is_set(&value))
}

/// Parse a boolean env flag with the `1` / `true` spelling shared by the
/// feature-013 spawn markers. Split from the env read to stay testable.
#[must_use]
pub fn flag_is_set(value: &str) -> bool {
    let value = value.trim();
    value == "1" || value.eq_ignore_ascii_case("true")
}

/// Run the feature-013 remote preamble on a freshly connected framed stream: send
/// [`ClientMessage::RemoteHandshake`] as the first frame, then read the mandatory
/// [`ServerMessage::RemoteHandshakeReply`]. Every terminal condition maps to a
/// [`RemoteConnectOutcome`]: an accepted reply, a typed refusal, or — for an EOF,
/// an I/O error, or any frame other than the reply — the merged
/// [`RemoteConnectOutcome::ConnectionFailure`] (FR-004).
pub async fn perform_remote_handshake<S>(
    stream: &mut S,
    device_name: String,
) -> RemoteConnectOutcome
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let preamble = ClientMessage::RemoteHandshake {
        remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        scribe_version: env!("CARGO_PKG_VERSION").to_owned(),
        device_name,
    };
    if let Err(error) = write_message(stream, &preamble).await {
        tracing::warn!(%error, "failed to send remote handshake preamble");
        return RemoteConnectOutcome::ConnectionFailure;
    }

    match read_message::<ServerMessage, _>(stream).await {
        Ok(ServerMessage::RemoteHandshakeReply {
            accepted,
            refusal,
            server_remote_protocol_version,
            server_scribe_version,
            version_mismatch: _,
        }) => map_reply(accepted, refusal, server_remote_protocol_version, &server_scribe_version),
        Ok(other) => {
            tracing::warn!(
                ?other,
                "unexpected first frame from remote server; expected RemoteHandshakeReply"
            );
            RemoteConnectOutcome::ConnectionFailure
        }
        Err(error) => {
            tracing::warn!(%error, "remote server closed before handshake reply");
            RemoteConnectOutcome::ConnectionFailure
        }
    }
}

/// Map a decoded [`RemoteHandshakeReply`](ServerMessage::RemoteHandshakeReply)
/// into a [`RemoteConnectOutcome`]. A refusal with no reason is a protocol
/// violation, treated as a generic connection failure rather than inventing a
/// cause.
fn map_reply(
    accepted: bool,
    refusal: Option<scribe_common::protocol::RemoteRefusal>,
    server_remote_protocol_version: u32,
    server_scribe_version: &str,
) -> RemoteConnectOutcome {
    match (accepted, refusal) {
        (true, _) => {
            tracing::info!(
                server_remote_protocol_version,
                %server_scribe_version,
                "remote handshake accepted"
            );
            RemoteConnectOutcome::Accepted
        }
        (false, Some(reason)) => {
            tracing::info!(?reason, "remote handshake refused");
            RemoteConnectOutcome::Refused(reason)
        }
        (false, None) => {
            tracing::warn!("remote handshake refused without a reason");
            RemoteConnectOutcome::ConnectionFailure
        }
    }
}

/// Why a tailnet dial could not be started or completed, in the shape the shell
/// surfaces on the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDialError(String);

impl std::fmt::Display for RemoteDialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A ready-to-use tailnet dialer: the peer's `MagicDNS` name or IP plus its
/// control port. Unlike the LAN dialer there is no identity to build — the
/// tailnet transport is plain TCP and identity is `tailscaled`'s `WhoIs` on the
/// far side (FR-003), never a certificate this process holds.
pub struct RemoteDialer {
    host: String,
    port: u16,
}

impl RemoteDialer {
    /// A dialer for `host:port`.
    #[must_use]
    pub fn new(host: String, port: u16) -> Self {
        Self { host, port }
    }

    /// The peer this dialer targets, for logging and status copy.
    #[must_use]
    pub fn target(&self) -> (&str, u16) {
        (&self.host, self.port)
    }

    /// Dial the peer over plain TCP.
    ///
    /// # Errors
    /// Returns [`RemoteDialError`] on a connect timeout or a refused/unresolvable
    /// address. Both are the connecting side's single
    /// [`RemoteConnectOutcome::ConnectionFailure`] (FR-004); the message
    /// distinguishes them for the log without inventing new UX states.
    pub async fn connect(&self) -> Result<TcpStream, RemoteDialError> {
        let address = format!("{}:{}", self.host, self.port);
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address))
            .await
            .map_err(|_| RemoteDialError(format!("tailnet dial to {address} timed out")))?
            .map_err(|error| {
                RemoteDialError(format!("tailnet dial to {address} failed: {error}"))
            })?;
        // Nagle would coalesce keystroke frames into the previous packet, which
        // is exactly the latency the local socket never has.
        if let Err(error) = tcp.set_nodelay(true) {
            tracing::debug!(%error, "failed to set TCP_NODELAY on the tailnet connection");
        }
        tracing::info!(host = %self.host, port = self.port, "tailnet TCP connection established");
        Ok(tcp)
    }
}

/// This machine's advertised short device name in the `RemoteHandshake`
/// preamble.
///
/// Display only — never a trust key. The owning side resolves the real identity
/// from `tailscaled`'s `WhoIs` on the connection's source address, so this is
/// only what the peer's banner and audit log show.
#[must_use]
pub fn local_device_name() -> String {
    nix::unistd::gethostname()
        .map_or_else(|_| String::from("localhost"), |name| name.to_string_lossy().into_owned())
}

/// Ask this machine's own server, on a transient local socket, for its tailnet
/// environment ([`ClientMessage::GetRemoteEnv`]) and return the raw reply.
///
/// The raw [`ServerMessage`] is returned rather than a parsed summary so the
/// caller folds it through the SAME handler the live reader uses — the reply is
/// a `RemoteEnv` either way, and there is exactly one place that knows what to
/// do with one.
///
/// # Errors
/// Returns [`RemoteDialError`] when the local server is unreachable, silent, or
/// closes before answering.
pub async fn probe_remote_env() -> Result<ServerMessage, RemoteDialError> {
    transient_local_request(ClientMessage::GetRemoteEnv).await.map_err(RemoteDialError)
}

/// Send one local-only first frame on a fresh Unix socket and read its single
/// reply, then drop the connection.
///
/// The server serves the local-only helper queries (`GetRemoteEnv`, and
/// feature 014's `GetLanEnv` / `GetLanDialIdentity`) as pre-`Hello` first frames
/// and closes afterwards, so a fresh socket per call is the protocol, not a
/// convenience — and it keeps these off the session connection whose ordering
/// keystrokes depend on. Shared with [`crate::lan_dial`] so both features open
/// their transient sockets exactly the same way.
///
/// # Errors
/// Returns the human-readable reason the round trip failed: the connect timed
/// out or was refused, the write failed, or the server did not answer in time.
pub(crate) async fn transient_local_request(
    request: ClientMessage,
) -> Result<ServerMessage, String> {
    let path = server_socket_path();
    let mut stream = tokio::time::timeout(LOCAL_PROBE_TIMEOUT, UnixStream::connect(&path))
        .await
        .map_err(|_| format!("connecting to {} timed out", path.display()))?
        .map_err(|error| format!("connecting to {} failed: {error}", path.display()))?;
    write_message(&mut stream, &request)
        .await
        .map_err(|error| format!("sending the local request failed: {error}"))?;
    tokio::time::timeout(LOCAL_PROBE_TIMEOUT, read_message::<ServerMessage, _>(&mut stream))
        .await
        .map_err(|_| String::from("the local server did not answer in time"))?
        .map_err(|error| format!("the local server closed before answering: {error}"))
}

#[cfg(test)]
mod tests;
