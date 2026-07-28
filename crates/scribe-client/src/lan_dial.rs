//! Feature 014 LAN (mutual-TLS) dial, ported into the GPUI rebuild.
//!
//! A LAN client reaches a peer over TCP → pinned mutual TLS →
//! [`LanHello`](scribe_common::protocol::ClientMessage::LanHello) preamble → the
//! owning side's device-approval gate, and only then sends the ordinary
//! `Hello`. Past that gate the link behaves byte-identically to the local Unix
//! socket, which is why the shell can hand the encrypted stream straight to the
//! same reader / writer pair.
//!
//! Two things make this different from the tailnet dial in
//! [`crate::remote_handshake`]:
//!
//! * **Identity comes from the local server, not the keyring.** The dialer must
//!   present this machine's persistent device certificate so an approval can be
//!   remembered (US2), but the sealed private key is granted only to the binary
//!   that created it — on macOS by a legacy `SecKeychain` per-item ACL that
//!   denies any other binary with no usable prompt. So the client asks its OWN
//!   co-located `scribe-server` over the local socket
//!   ([`ClientMessage::GetLanDialIdentity`]) and rebuilds a [`DeviceIdentity`]
//!   from the returned DER. Used on every platform for uniformity, and it fails
//!   closed: no identity means no dial.
//! * **The preamble can block on a human.** An unknown device is answered with
//!   [`LanApprovalPending`](ServerMessage::LanApprovalPending) and then held —
//!   no window or session data crosses — until the owning user decides or the
//!   peer's hold times out. [`handshake`] reports that interim state through a
//!   callback so the window can say so, and keeps reading for the terminal
//!   [`LanApprovalResult`](ServerMessage::LanApprovalResult).
//!
//! The security-critical verifier is NOT duplicated here: this reuses the
//! server-owned `scribe_server::lan::{identity, tls}` exactly as the winit
//! client does, so the SPKI pinning and the delegated handshake-signature check
//! live in one place.

use std::sync::Arc;
use std::time::Duration;

use scribe_common::framing::{read_message, write_message};
use scribe_common::protocol::{ClientMessage, REMOTE_PROTOCOL_VERSION, ServerMessage};
use scribe_server::lan::identity::{self, DeviceIdentity};
use scribe_server::lan::tls::{DeviceId, DevicePins, LanTls};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::remote::LanConnectOutcome;
use crate::remote_handshake::lan_dial_target_from_env;

/// How long a TCP connect to a LAN peer may take before it is abandoned.
/// Matches the tailnet dial's budget: a peer on the same subnet either answers
/// promptly or is not there.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The dialer-side encrypted LAN stream once the mutual-TLS handshake completes.
pub type LanStream = tokio_rustls::client::TlsStream<TcpStream>;

/// Why a LAN dial could not be started or completed, in the shape the shell
/// surfaces on the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanDialError(String);

impl std::fmt::Display for LanDialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `host:port` of the LAN peer to dial, from the `SCRIBE_LAN_DIAL` plumbing
/// hook. `None` — the normal case — keeps the client on its local Unix socket.
///
/// Re-exported through this module so the shell never has to name
/// [`crate::remote_handshake`], whose remaining surface is the *tailnet* dial.
#[must_use]
pub fn target_from_env() -> Option<(String, u16)> {
    lan_dial_target_from_env()
}

/// This machine's advertised display name in the `LanHello` preamble.
///
/// Display only — never a trust key. Identity is the pinned TLS client
/// certificate; this is what the owning user sees on the approval prompt.
#[must_use]
pub fn local_device_name() -> String {
    nix::unistd::gethostname()
        .map_or_else(|_| String::from("localhost"), |name| name.to_string_lossy().into_owned())
}

/// The connecting side keeps no trusted-*server* pin store: device approval is
/// owning-side (the LAN listener gates the dialer, not the reverse), so the
/// reused verifier only has to encrypt the link and prove the peer holds its
/// key. Every server is therefore classified as first-seen, and the
/// classification is recorded but unused on this side.
struct NoServerPins;

impl DevicePins for NoServerPins {
    fn is_pinned(&self, _device_id: &DeviceId) -> bool {
        false
    }
}

/// A ready-to-use LAN dialer: this machine's mutual-TLS identity plus the peer's
/// address. Built once per client process and reused for the life of the dial.
pub struct LanDialer {
    tls: LanTls,
    host: String,
    port: u16,
}

impl LanDialer {
    /// Fetch this machine's device identity from its co-located server and build
    /// the pinned mutual-TLS dialer for `host:port`.
    ///
    /// # Errors
    /// Returns [`LanDialError`] when the local server is unreachable, reports no
    /// identity, or returns DER the identity layer rejects — each of which must
    /// stop the dial rather than fall back to an anonymous connection.
    pub async fn build(host: String, port: u16) -> Result<Self, LanDialError> {
        let (cert_der, key_der) = fetch_dial_identity().await?;
        let identity = DeviceIdentity::from_der(cert_der, key_der)
            .map_err(|error| LanDialError(format!("LAN device identity is unusable: {error}")))?;
        tracing::info!(
            device_id = %identity.device_id_hex(),
            "built the LAN dial identity from the local server"
        );
        Ok(Self { tls: LanTls::new(Arc::new(identity), Arc::new(NoServerPins)), host, port })
    }

    /// The peer this dialer targets, for logging and status copy.
    #[must_use]
    pub fn target(&self) -> (&str, u16) {
        (&self.host, self.port)
    }

    /// Dial the peer over TCP and complete the pinned mutual-TLS handshake.
    ///
    /// # Errors
    /// Returns [`LanDialError`] on a connect timeout, a refused connection, or a
    /// failed handshake. All three are the connecting side's single
    /// `ConnectionFailure` outcome (FR-004); the message distinguishes them for
    /// the log without inventing new UX states.
    pub async fn connect(&self) -> Result<LanStream, LanDialError> {
        let address = format!("{}:{}", self.host, self.port);
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address))
            .await
            .map_err(|_| LanDialError(format!("LAN dial to {address} timed out")))?
            .map_err(|error| LanDialError(format!("LAN dial to {address} failed: {error}")))?;
        // Nagle would coalesce keystroke frames into the previous packet, which
        // is exactly the latency the local socket never has.
        if let Err(error) = tcp.set_nodelay(true) {
            tracing::debug!(%error, "failed to set TCP_NODELAY on the LAN connection");
        }
        let (stream, _peer) = self.tls.connect(tcp).await.map_err(|error| {
            LanDialError(format!("LAN mutual TLS handshake with {address} failed: {error}"))
        })?;
        tracing::info!(host = %self.host, port = self.port, "LAN mutual TLS established");
        Ok(stream)
    }
}

/// Run the LAN preamble on a freshly established mutual-TLS stream: send
/// [`ClientMessage::LanHello`], then read the owning side's approval-gate frames
/// until a terminal [`ServerMessage::LanApprovalResult`].
///
/// An unknown device is first told it is waiting, which is reported through
/// `on_pending` so the window can show it; the read then blocks with no timeout
/// of our own, because the owning user's decision legitimately takes as long as
/// it takes and the peer already bounds the hold. An already-trusted device is
/// admitted straight to `approved = true` with no pending frame.
///
/// Every terminal condition maps to a [`LanConnectOutcome`]: accepted, a typed
/// refusal, or — for an EOF, an I/O error, an unexpected frame, or a reason-less
/// refusal — the merged [`LanConnectOutcome::ConnectionFailure`].
pub async fn handshake<S>(
    stream: &mut S,
    device_name: String,
    mut on_pending: impl FnMut(),
) -> LanConnectOutcome
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let preamble =
        ClientMessage::LanHello { device_name, remote_protocol_version: REMOTE_PROTOCOL_VERSION };
    if let Err(error) = write_message(stream, &preamble).await {
        tracing::warn!(%error, "failed to send the LAN hello preamble");
        return LanConnectOutcome::ConnectionFailure;
    }

    loop {
        match read_message::<ServerMessage, _>(stream).await {
            Ok(ServerMessage::LanApprovalPending) => {
                tracing::info!("LAN connection held pending device approval on the peer");
                on_pending();
                // Keep reading: the terminal result follows the owning user's
                // decision or the peer's approval timeout.
            }
            Ok(ServerMessage::LanApprovalResult { approved: true, .. }) => {
                tracing::info!("LAN device approval accepted");
                return LanConnectOutcome::Accepted;
            }
            Ok(ServerMessage::LanApprovalResult { approved: false, refusal: Some(reason) }) => {
                tracing::info!(?reason, "LAN device approval refused");
                return LanConnectOutcome::Refused(reason);
            }
            Ok(ServerMessage::LanApprovalResult { approved: false, refusal: None }) => {
                // A refusal with no reason is a protocol violation; report a
                // generic failure rather than inventing a cause.
                tracing::warn!("LAN approval refused without a reason");
                return LanConnectOutcome::ConnectionFailure;
            }
            Ok(other) => {
                tracing::warn!(
                    variant = ?std::mem::discriminant(&other),
                    "unexpected frame during LAN approval; expected LanApprovalResult"
                );
                return LanConnectOutcome::ConnectionFailure;
            }
            Err(error) => {
                tracing::warn!(%error, "LAN peer closed before an approval result");
                return LanConnectOutcome::ConnectionFailure;
            }
        }
    }
}

/// Ask this machine's own server, on a transient local socket, for its LAN
/// environment ([`ClientMessage::GetLanEnv`]) and return the raw reply.
///
/// The raw [`ServerMessage`] is returned rather than a parsed summary so the
/// caller folds it through the SAME handler the live reader uses — the reply is
/// a `LanEnv` either way, and there is exactly one place that knows what to do
/// with one.
///
/// # Errors
/// Returns [`LanDialError`] when the local server is unreachable, silent, or
/// answers something other than a single frame.
pub async fn probe_lan_env() -> Result<ServerMessage, LanDialError> {
    transient_local_request(ClientMessage::GetLanEnv).await
}

/// Fetch this machine's OWN device identity (public certificate DER + sealed
/// `PKCS#8` private-key DER) from its co-located server.
///
/// Fails closed on any transport error or an `available = false` reply, so a
/// dial never proceeds without a valid identity. The returned key material is
/// handed straight to [`DeviceIdentity::from_der`] and is never logged, stored,
/// or placed in shared state.
async fn fetch_dial_identity() -> Result<(Vec<u8>, Vec<u8>), LanDialError> {
    match transient_local_request(ClientMessage::GetLanDialIdentity).await? {
        ServerMessage::LanDialIdentity { available, cert_der, private_key_pkcs8_der } => {
            if !available || cert_der.is_empty() || private_key_pkcs8_der.is_empty() {
                return Err(LanDialError(String::from(
                    "the local server reports no LAN device identity available",
                )));
            }
            Ok((cert_der, private_key_pkcs8_der))
        }
        other => Err(LanDialError(format!(
            "expected LanDialIdentity from the local server, got {}",
            server_message_name(&other)
        ))),
    }
}

/// Send one local-only first frame on a fresh Unix socket and read its single
/// reply, then drop the connection.
///
/// The server serves `GetLanEnv` / `GetLanDialIdentity` as pre-`Hello` first
/// frames and closes afterwards, so a fresh socket per call is the protocol, not
/// a convenience — and it keeps these off the session connection whose ordering
/// keystrokes depend on. The transport itself lives in
/// [`crate::remote_handshake::transient_local_request`], shared with feature
/// 013's `GetRemoteEnv` probe so both open their sockets identically; only the
/// error shape is this module's.
async fn transient_local_request(request: ClientMessage) -> Result<ServerMessage, LanDialError> {
    crate::remote_handshake::transient_local_request(request).await.map_err(LanDialError)
}

/// Name a [`ServerMessage`] for an error string without formatting its payload
/// — a `LanDialIdentity` carries private key material and must never be
/// `Debug`-printed into a log line.
fn server_message_name(message: &ServerMessage) -> &'static str {
    match message {
        ServerMessage::LanEnv { .. } => "LanEnv",
        ServerMessage::LanDialIdentity { .. } => "LanDialIdentity",
        ServerMessage::Error { .. } => "Error",
        _ => "another message",
    }
}

/// Adapt an [`identity::IdentityError`] into this module's error shape. Kept as
/// a named conversion so the identity layer's typed errors never leak into the
/// shell's status copy verbatim.
impl From<identity::IdentityError> for LanDialError {
    fn from(error: identity::IdentityError) -> Self {
        Self(format!("LAN device identity error: {error}"))
    }
}

#[cfg(test)]
mod tests;
