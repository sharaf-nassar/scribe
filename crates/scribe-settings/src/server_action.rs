//! Synchronous one-shot client for `scribe-server` actions.
//!
//! The settings binary runs on a GTK or tao event loop and has no Tokio
//! runtime. To trigger a manual update check we open a blocking
//! `std::os::unix::net::UnixStream` to `server.sock`, hand-frame a single
//! `ClientMessage`, read back a single `ServerMessage`, and close. The wire
//! format matches `scribe_common::framing` (4-byte big-endian length prefix
//! followed by msgpack), implemented synchronously here so we do not need to
//! pull a runtime into the settings process.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use scribe_common::framing::MAX_MESSAGE_SIZE;
use scribe_common::protocol::{ClientMessage, ServerMessage, UpdateCheckResultState};
use scribe_common::socket::server_socket_path;

/// Send `CheckForUpdates` to the server and wait for the matching response.
///
/// The returned state is the result of the check; a transport or protocol
/// error becomes `UpdateCheckResultState::Failed { reason }` so the UI always
/// has a single shape to render.
pub fn request_update_check(timeout: Duration) -> UpdateCheckResultState {
    match try_request_update_check(timeout) {
        Ok(state) => state,
        Err(reason) => {
            tracing::warn!("manual update check transport error: {reason}");
            UpdateCheckResultState::Failed { reason }
        }
    }
}

fn try_request_update_check(timeout: Duration) -> Result<UpdateCheckResultState, String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| format!("set_read_timeout: {e}"))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;

    write_frame(&mut stream, &ClientMessage::CheckForUpdates)?;

    match read_frame(&mut stream)? {
        ServerMessage::UpdateCheckResult { state } => Ok(state),
        other => Err(format!("unexpected server response: {other:?}")),
    }
}

fn write_frame<W: std::io::Write>(writer: &mut W, msg: &ClientMessage) -> Result<(), String> {
    let payload =
        rmp_serde::to_vec_named(msg).map_err(|e| format!("serialize ClientMessage failed: {e}"))?;
    let len: u32 = payload.len().try_into().map_err(|_| String::from("frame too large to send"))?;
    writer.write_all(&len.to_be_bytes()).map_err(|e| format!("write length prefix: {e}"))?;
    writer.write_all(&payload).map_err(|e| format!("write payload: {e}"))?;
    writer.flush().map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

fn read_frame<R: std::io::Read>(reader: &mut R) -> Result<ServerMessage, String> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(|e| format!("read length prefix: {e}"))?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_MESSAGE_SIZE {
        return Err(format!("frame size {len} exceeds limit {MAX_MESSAGE_SIZE}"));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).map_err(|e| format!("read payload: {e}"))?;
    rmp_serde::from_slice(&buf).map_err(|e| format!("deserialize ServerMessage failed: {e}"))
}
