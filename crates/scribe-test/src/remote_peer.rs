//! Tailnet peer stand-in for the feature-013 remote dial.
//!
//! The connecting half of tailnet remote control cannot be exercised by one
//! machine: `RemoteHandshake` and `RemoteHandshakeReply` only exist once a
//! client has opened a TCP connection to a *second* machine's remote listener.
//! This is the tailnet analogue of [`crate::lan_peer`] — and a much smaller one,
//! because the tailnet transport is plain TCP: identity is `tailscaled`'s `WhoIs`
//! on the owning side, never a certificate the dialer holds, so there is nothing
//! to borrow and nothing to pin.
//!
//! It stands in for the second machine without faking the client or the
//! protocol:
//!
//! * it refuses anything but a `RemoteHandshake` as the first frame, exactly as
//!   the shipped listener does, so the preamble the client sends is the one the
//!   real server would have had to accept;
//! * every framed message in both directions is appended to a JSONL record in
//!   the same `{"dir": …, "message": …}` shape the share tap uses, so a script
//!   can assert that `RemoteHandshake` really left the client and that the
//!   client acted on the reply;
//! * it answers the mandatory reply — accepted, or a typed
//!   [`RemoteRefusal`] — and, on acceptance, splices the connection to the real
//!   local `scribe-server` so the admitted client behaves exactly like any other
//!   one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use scribe_common::framing::{read_message, write_message};
use scribe_common::protocol::{
    ClientMessage, REMOTE_PROTOCOL_VERSION, RemoteRefusal, ServerMessage,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};

/// How the stand-in answers the mandatory handshake preamble.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Accept the dialer and splice the connection to the local server.
    Accept,
    /// Refuse with a typed [`RemoteRefusal`] and close.
    Refuse(RemoteRefusal),
}

/// One run of the stand-in peer.
pub struct RemotePeerConfig {
    /// `host:port` to bind the tailnet listener on.
    pub listen: String,
    /// Local server socket an accepted connection is spliced to.
    pub upstream: PathBuf,
    /// JSONL file every framed message in both directions is appended to.
    pub record: PathBuf,
    /// How the handshake gate answers.
    pub verdict: Verdict,
    /// How long to hold before the reply, so a script can screenshot the
    /// connecting side mid-dial.
    pub hold: Duration,
}

/// Serve tailnet connections until killed, running the handshake gate on each.
///
/// # Errors
/// Returns the underlying I/O error when the listener cannot be bound. A failure
/// on one accepted connection is logged and the loop continues, so a script can
/// drive several dials against one peer.
pub async fn run(config: RemotePeerConfig) -> std::io::Result<()> {
    let listener = TcpListener::bind(&config.listen).await?;
    tracing::info!(listen = %config.listen, "remote-peer listening");
    let config = Arc::new(config);

    loop {
        let (tcp, from) = listener.accept().await?;
        tracing::info!(%from, "remote-peer accepted a TCP connection");
        let config = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(error) = serve(tcp, &config).await {
                tracing::warn!(%error, "remote-peer connection ended");
            }
        });
    }
}

/// Read the preamble, answer the gate, and splice on acceptance.
async fn serve(mut stream: TcpStream, config: &RemotePeerConfig) -> std::io::Result<()> {
    let record = config.record.as_path();

    // The preamble is mandatory and must be the FIRST frame: anything else is
    // refused exactly as the shipped listener refuses it.
    let preamble = read_message::<ClientMessage, _>(&mut stream)
        .await
        .map_err(|error| std::io::Error::other(format!("no remote preamble: {error}")))?;
    record_frame(record, "client", &preamble);
    let ClientMessage::RemoteHandshake { device_name, remote_protocol_version, scribe_version } =
        &preamble
    else {
        return Err(std::io::Error::other("first remote frame was not RemoteHandshake"));
    };
    tracing::info!(
        %device_name,
        remote_protocol_version,
        %scribe_version,
        "remote-peer received RemoteHandshake"
    );

    tokio::time::sleep(config.hold).await;

    let (accepted, refusal) = match config.verdict {
        Verdict::Accept => (true, None),
        Verdict::Refuse(reason) => (false, Some(reason)),
    };
    send(
        &mut stream,
        record,
        ServerMessage::RemoteHandshakeReply {
            accepted,
            refusal,
            server_remote_protocol_version: REMOTE_PROTOCOL_VERSION,
            server_scribe_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
    )
    .await?;
    if !accepted {
        // The shipped server sends the best-effort `RemoteDisconnect` before it
        // closes a link for a policy reason, so the stand-in does too — that is
        // the frame the displaced/refused client renders its reason from.
        send(
            &mut stream,
            record,
            ServerMessage::RemoteDisconnect { reason: refusal_or_disabled(refusal) },
        )
        .await?;
        stream.shutdown().await.ok();
        return Ok(());
    }

    // Accepted: from here the connection is an ordinary session, so hand it to
    // the real server rather than simulating one.
    let server = UnixStream::connect(&config.upstream).await?;
    splice(stream, server, record).await;
    Ok(())
}

/// The reason a refusal's trailing `RemoteDisconnect` carries. A refusal always
/// has one; the fallback only exists so this stays total.
fn refusal_or_disabled(refusal: Option<RemoteRefusal>) -> RemoteRefusal {
    refusal.unwrap_or(RemoteRefusal::Disabled)
}

/// Relay an accepted tailnet stream to the real local server, recording both
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
        tracing::warn!(%error, "remote-peer uplink task ended abnormally");
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
        .map_err(|error| std::io::Error::other(format!("remote gate write failed: {error}")))
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
        tracing::warn!(%error, "remote-peer could not append to the wire record");
    }
}
