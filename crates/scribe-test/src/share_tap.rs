//! Wire tap for the feature-015 sharing surface.
//!
//! Multi-machine sharing is the one client surface that cannot be exercised by
//! a single machine: the server only broadcasts `ShareRoster` /
//! `ControlRequested` / `ControlDenied` / `ShareEnded` once a *remote*
//! participant has joined a window's share, which needs a second machine with
//! tailnet or LAN identity. The tap stands in for that second machine without
//! faking the client or the server: it is a transparent relay on the real Unix
//! socket, so the client under test performs its normal `Hello` handshake with
//! the real `scribe-server` and every frame in both directions is the real
//! length-prefixed msgpack wire format.
//!
//! Two extra abilities make it a test oracle:
//!
//! * every frame in both directions is appended to a JSONL record as
//!   `{"dir": "client"|"server", "message": {…}}`, so a script can assert that
//!   `ControlClaim` / `ControlGrant` actually left the client — not that a unit
//!   test constructed one — and can read the server's `Welcome` to learn the
//!   window id the injected roster must name;
//! * a control socket injects a `ServerMessage` toward the client, which is how
//!   the four share notices a second machine would have caused are delivered.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use scribe_common::framing::{read_message, write_message};
use scribe_common::protocol::{ClientMessage, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// The injection target: the sender of the most recently accepted client
/// connection, which under the E2E entrypoint is the client under test.
type InjectTarget = Arc<Mutex<Option<UnboundedSender<ServerMessage>>>>;

/// Relay every connection that arrives on `listen` to the real server at
/// `upstream`, recording the client half and serving `control` for injection.
///
/// Runs until killed: an E2E entrypoint backgrounds it for the life of the
/// container. Several connections may be relayed at once (the harness daemon and
/// the client under test both speak to the same socket); an injected notice goes
/// to the most recent one, which the entrypoint arranges to be the client.
///
/// # Errors
/// Returns the underlying I/O error when a socket cannot be bound, accepted, or
/// dialed.
pub async fn run(
    listen: &Path,
    upstream: &Path,
    record: &Path,
    control: &Path,
) -> std::io::Result<()> {
    remove_stale_socket(listen);
    remove_stale_socket(control);
    let client_listener = UnixListener::bind(listen)?;
    let control_listener = UnixListener::bind(control)?;

    let target: InjectTarget = Arc::new(Mutex::new(None));
    let control_target = Arc::clone(&target);
    tokio::spawn(async move { serve_control(control_listener, control_target).await });

    loop {
        let (client, _) = client_listener.accept().await?;
        let server = UnixStream::connect(upstream).await?;
        // Everything bound for this client funnels through one channel so an
        // injected notice can never interleave inside a relayed frame.
        let (to_client, to_client_rx) = unbounded_channel::<ServerMessage>();
        if let Ok(mut slot) = target.lock() {
            *slot = Some(to_client.clone());
        }
        tokio::spawn(relay(client, server, record.to_path_buf(), to_client, to_client_rx));
    }
}

/// Pump one client connection against one upstream server connection until
/// either side closes.
async fn relay(
    client: UnixStream,
    server: UnixStream,
    record: PathBuf,
    to_client: UnboundedSender<ServerMessage>,
    mut to_client_rx: UnboundedReceiver<ServerMessage>,
) {
    let (mut client_reader, mut client_writer) = client.into_split();
    let (mut server_reader, mut server_writer) = server.into_split();
    let record_down = record.clone();

    let uplink = tokio::spawn(async move {
        loop {
            let Ok(message) = read_message::<ClientMessage, _>(&mut client_reader).await else {
                return;
            };
            record_frame(&record, "client", &message);
            if write_message(&mut server_writer, &message).await.is_err() {
                return;
            }
        }
    });

    let downlink_record = record_down;
    let downlink = tokio::spawn(async move {
        loop {
            let Ok(message) = read_message::<ServerMessage, _>(&mut server_reader).await else {
                return;
            };
            record_frame(&downlink_record, "server", &message);
            if to_client.send(message).is_err() {
                return;
            }
        }
    });

    while let Some(message) = to_client_rx.recv().await {
        if write_message(&mut client_writer, &message).await.is_err() {
            break;
        }
    }
    uplink.abort();
    downlink.abort();
}

/// Unlink a socket path so a rebind (or a clean exit) never trips over a stale
/// node. A missing path is the normal case and is not an error.
fn remove_stale_socket(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "share-tap could not unlink a socket path");
    }
}

/// Append one relayed frame to the JSONL record, tagged with its direction.
///
/// Serialization or write failures are reported through tracing rather than
/// propagated: losing a record line must not tear the relay down mid-test.
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
        tracing::warn!(%error, "share-tap could not append to the wire record");
    }
}

/// Serve the injection socket: each connection carries one JSON-encoded
/// `ServerMessage` per line, framed onward to the client.
async fn serve_control(listener: UnixListener, target: InjectTarget) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let decoded = match serde_json::from_str::<ServerMessage>(&line) {
                Ok(message) => message,
                Err(error) => {
                    tracing::warn!(%error, "share-tap could not decode an injection");
                    continue;
                }
            };
            let Ok(slot) = target.lock() else {
                return;
            };
            let sent = slot.as_ref().is_some_and(|to_client| to_client.send(decoded).is_ok());
            drop(slot);
            if !sent {
                tracing::warn!("share-tap has no live client to inject into");
            }
        }
    }
}

/// Hand one JSON-encoded `ServerMessage` to a running tap.
///
/// # Errors
/// Returns the underlying I/O error when the control socket cannot be reached
/// or written.
pub async fn inject(control: &Path, message: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(control).await?;
    stream.write_all(format!("{message}\n").as_bytes()).await?;
    stream.flush().await
}
