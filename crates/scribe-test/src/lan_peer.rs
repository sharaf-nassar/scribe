//! LAN peer stand-in for the feature-014 mutual-TLS dial.
//!
//! The connecting half of LAN remote control cannot be exercised by one machine:
//! `LanHello`, `LanApprovalPending`, and `LanApprovalResult` only exist once a
//! client has completed a pinned mutual-TLS handshake with a *second* machine's
//! LAN listener. This is the analogue of [`crate::share_tap`] for that gap — it
//! stands in for the second machine without faking either the client or the
//! protocol:
//!
//! * it terminates a REAL mutual-TLS handshake using the server-owned
//!   `scribe_server::lan::tls::LanTls`, the same builder the shipped listener
//!   uses, so the client under test presents its real device certificate and the
//!   real SPKI-pinning verifier runs on both ends;
//! * it borrows this machine's own device identity from the running
//!   `scribe-server` over the local-socket-only `GetLanDialIdentity`, so no
//!   second keyring and no hand-minted certificate is involved;
//! * every framed message in both directions is appended to a JSONL record in
//!   the same `{"dir": …, "message": …}` shape the share tap uses, so a script
//!   can assert that `LanHello` really left the client;
//! * it runs the app-layer approval gate the owning side would run — optionally
//!   answering `LanApprovalPending` first, then the terminal
//!   `LanApprovalResult` — and, on approval, splices the encrypted stream to the
//!   real local `scribe-server` so the accepted connection behaves exactly like
//!   any other one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use scribe_common::framing::{read_message, write_message};
use scribe_common::protocol::{ClientMessage, LanRefusal, ServerMessage};
use scribe_server::lan::identity::DeviceIdentity;
use scribe_server::lan::tls::{DeviceId, DevicePins, LanTls};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};

/// How the stand-in answers the device-approval gate.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Approve the device and splice the connection to the local server.
    Approve,
    /// Refuse with [`LanRefusal::Declined`] and close.
    Decline,
}

/// One run of the stand-in peer.
pub struct LanPeerConfig {
    /// `host:port` to bind the LAN listener on.
    pub listen: String,
    /// Local server socket the identity is borrowed from and, after approval,
    /// the accepted connection is spliced to.
    pub upstream: PathBuf,
    /// JSONL file every framed message in both directions is appended to.
    pub record: PathBuf,
    /// How the approval gate answers.
    pub verdict: Verdict,
    /// Whether to answer `LanApprovalPending` first, which is what an *unknown*
    /// device sees. `false` mimics an already-trusted device.
    pub pending: bool,
    /// How long to hold before the terminal result, so a script can screenshot
    /// the connecting side's waiting state.
    pub hold: Duration,
}

/// The stand-in pins nothing: it is a test peer, and the pin set only decides
/// the classification the verifier records, never whether the handshake
/// succeeds.
struct NoPins;

impl DevicePins for NoPins {
    fn is_pinned(&self, _device_id: &DeviceId) -> bool {
        false
    }
}

/// Serve LAN connections until killed, running the approval gate on each.
///
/// # Errors
/// Returns the underlying I/O error when the identity cannot be borrowed or the
/// listener cannot be bound. A failure on one accepted connection is logged and
/// the loop continues, so a script can drive several dials against one peer.
pub async fn run(config: LanPeerConfig) -> std::io::Result<()> {
    let identity = borrow_identity(&config.upstream).await?;
    let tls = Arc::new(LanTls::new(Arc::new(identity), Arc::new(NoPins)));
    let listener = TcpListener::bind(&config.listen).await?;
    tracing::info!(listen = %config.listen, "lan-peer listening");
    let config = Arc::new(config);

    loop {
        let (tcp, from) = listener.accept().await?;
        tracing::info!(%from, "lan-peer accepted a TCP connection");
        let tls = Arc::clone(&tls);
        let config = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(error) = serve(&tls, tcp, &config).await {
                tracing::warn!(%error, "lan-peer connection ended");
            }
        });
    }
}

/// Complete the handshake, run the gate, and splice on approval.
async fn serve(
    tls: &LanTls,
    tcp: tokio::net::TcpStream,
    config: &LanPeerConfig,
) -> std::io::Result<()> {
    let record = config.record.as_path();
    let (mut stream, peer) = tls
        .accept(tcp)
        .await
        .map_err(|error| std::io::Error::other(format!("LAN TLS accept failed: {error}")))?;
    tracing::info!(device = %hex(&peer.device_id), "lan-peer completed mutual TLS");

    // The preamble is mandatory and must be the FIRST frame: anything else is
    // refused exactly as the shipped listener refuses it.
    let hello = read_message::<ClientMessage, _>(&mut stream)
        .await
        .map_err(|error| std::io::Error::other(format!("no LAN preamble: {error}")))?;
    record_frame(record, "client", &hello);
    let ClientMessage::LanHello { device_name, remote_protocol_version } = &hello else {
        return Err(std::io::Error::other("first LAN frame was not LanHello"));
    };
    tracing::info!(%device_name, remote_protocol_version, "lan-peer received LanHello");

    if config.pending {
        send(&mut stream, record, ServerMessage::LanApprovalPending).await?;
    }
    tokio::time::sleep(config.hold).await;

    let (approved, refusal) = match config.verdict {
        Verdict::Approve => (true, None),
        Verdict::Decline => (false, Some(LanRefusal::Declined)),
    };
    send(&mut stream, record, ServerMessage::LanApprovalResult { approved, refusal }).await?;
    if !approved {
        stream.shutdown().await.ok();
        return Ok(());
    }

    // Approved: from here the connection is an ordinary session, so hand it to
    // the real server rather than simulating one.
    let server = UnixStream::connect(&config.upstream).await?;
    splice(stream, server, record).await;
    Ok(())
}

/// Relay an accepted LAN stream to the real local server, recording both
/// directions, until either side closes.
async fn splice<S>(peer: S, server: UnixStream, record: &Path)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut peer_reader, mut peer_writer) = tokio::io::split(peer);
    let (mut server_reader, mut server_writer) = server.into_split();
    let up_record = record.to_path_buf();
    let down_record = record.to_path_buf();

    let uplink = tokio::spawn(async move {
        while let Ok(message) = read_message::<ClientMessage, _>(&mut peer_reader).await {
            record_frame(&up_record, "client", &message);
            if write_message(&mut server_writer, &message).await.is_err() {
                return;
            }
        }
    });
    let downlink = tokio::spawn(async move {
        while let Ok(message) = read_message::<ServerMessage, _>(&mut server_reader).await {
            record_frame(&down_record, "server", &message);
            if write_message(&mut peer_writer, &message).await.is_err() {
                return;
            }
        }
    });
    if let Err(error) = uplink.await {
        tracing::warn!(%error, "lan-peer uplink task ended abnormally");
    }
    downlink.abort();
}

/// Frame one gate message to the client and record it.
async fn send<S>(stream: &mut S, record: &Path, message: ServerMessage) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    record_frame(record, "server", &message);
    write_message(stream, &message)
        .await
        .map_err(|error| std::io::Error::other(format!("LAN gate write failed: {error}")))
}

/// Borrow this machine's own device identity from the running `scribe-server`.
///
/// Uses the same local-socket-only `GetLanDialIdentity` first frame the shipped
/// dialer uses, so the stand-in never touches the OS keyring and never mints a
/// certificate of its own.
async fn borrow_identity(upstream: &Path) -> std::io::Result<DeviceIdentity> {
    let mut stream = UnixStream::connect(upstream).await?;
    write_message(&mut stream, &ClientMessage::GetLanDialIdentity)
        .await
        .map_err(|error| std::io::Error::other(format!("GetLanDialIdentity failed: {error}")))?;
    match read_message::<ServerMessage, _>(&mut stream).await {
        Ok(ServerMessage::LanDialIdentity { available: true, cert_der, private_key_pkcs8_der }) => {
            DeviceIdentity::from_der(cert_der, private_key_pkcs8_der).map_err(|error| {
                std::io::Error::other(format!("borrowed identity is unusable: {error}"))
            })
        }
        Ok(_) => Err(std::io::Error::other("the local server reports no LAN device identity")),
        Err(error) => Err(std::io::Error::other(format!("identity read failed: {error}"))),
    }
}

/// Lowercase hex of a device id, for the peer's own log lines.
fn hex(device_id: &DeviceId) -> String {
    use std::fmt::Write as _;
    device_id.iter().fold(String::with_capacity(64), |mut out, byte| {
        if write!(out, "{byte:02x}").is_err() {
            out.push_str("??");
        }
        out
    })
}

/// Append one framed message to the JSONL record, tagged with its direction.
///
/// Failures are reported on stderr rather than propagated: losing a record line
/// must not tear the peer down mid-test.
fn record_frame<T: serde::Serialize>(path: &Path, direction: &str, message: &T) {
    let Ok(line) = serde_json::to_string(&serde_json::json!({
        "dir": direction,
        "message": message,
    })) else {
        return;
    };
    let appended =
        std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut file| {
            std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes())
        });
    if let Err(error) = appended {
        tracing::warn!(%error, "lan-peer could not append to the wire record");
    }
}
