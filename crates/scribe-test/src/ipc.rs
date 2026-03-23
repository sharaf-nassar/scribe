//! Thin async IPC client for communicating with scribe-server.
//!
//! Connects over a Unix domain socket using length-prefixed msgpack framing
//! from `scribe_common::framing`.

use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::protocol::{ClientMessage, ServerMessage};
use scribe_common::socket::server_socket_path;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// Connect to the scribe-server Unix domain socket.
///
/// # Errors
///
/// Returns `ScribeError::Io` if the connection cannot be established.
pub async fn connect() -> Result<UnixStream, ScribeError> {
    let path = server_socket_path();
    UnixStream::connect(&path).await.map_err(|source| ScribeError::Io { source })
}

/// Send a `ClientMessage` to the server over the given write half.
///
/// # Errors
///
/// Returns `ScribeError::Serialization` on encode failure or
/// `ScribeError::Io` on write failure.
pub async fn send(writer: &mut OwnedWriteHalf, msg: &ClientMessage) -> Result<(), ScribeError> {
    write_message(writer, msg).await
}

/// Receive a `ServerMessage` from the server over the given read half.
///
/// # Errors
///
/// Returns `ScribeError::Io` on read failure, `ScribeError::Deserialization`
/// on decode failure, or `ScribeError::ProtocolError` if the message exceeds
/// the size limit.
pub async fn recv(reader: &mut OwnedReadHalf) -> Result<ServerMessage, ScribeError> {
    read_message(reader).await
}
