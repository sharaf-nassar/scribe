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

use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::WindowId;
use scribe_common::protocol::{ClientMessage, REMOTE_PROTOCOL_VERSION, ServerMessage};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::remote::RemoteConnectOutcome;

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

#[cfg(test)]
mod tests;
