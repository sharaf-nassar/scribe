use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::ScribeError;

/// Maximum size of a single length-prefixed protocol frame, in bytes.
///
/// 256 MiB accommodates reattach payloads that batch many session snapshots
/// at once. Exposed publicly so synchronous callers (e.g. the settings
/// binary's transient action client) can enforce the same upper bound.
pub const MAX_MESSAGE_SIZE: u32 = 256 * 1024 * 1024;

/// Read a single length-prefixed msgpack frame from an async reader.
///
/// # Errors
///
/// Returns `ScribeError::Io` on read failure, `ScribeError::Deserialization`
/// on decode failure, or `ScribeError::ProtocolError` if the message
/// exceeds the size limit.
pub async fn read_message<T, R>(reader: &mut R) -> Result<T, ScribeError>
where
    T: for<'de> Deserialize<'de>,
    R: AsyncReadExt + Unpin,
{
    let len = reader.read_u32().await.map_err(|e| ScribeError::Io { source: e })?;

    if len > MAX_MESSAGE_SIZE {
        return Err(ScribeError::ProtocolError {
            reason: format!("message size {len} exceeds limit {MAX_MESSAGE_SIZE}"),
        });
    }

    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await.map_err(|e| ScribeError::Io { source: e })?;

    rmp_serde::from_slice(&buf).map_err(Into::into)
}

/// Write a serializable message as a length-prefixed msgpack frame.
///
/// # Errors
///
/// Returns `ScribeError::Serialization` on encode failure or
/// `ScribeError::Io` on write failure.
pub async fn write_message<T, W>(writer: &mut W, msg: &T) -> Result<(), ScribeError>
where
    T: Serialize,
    W: AsyncWriteExt + Unpin,
{
    let payload = rmp_serde::to_vec_named(msg)?;

    if payload.len() > MAX_MESSAGE_SIZE as usize {
        return Err(ScribeError::ProtocolError {
            reason: format!(
                "outgoing message size {} exceeds limit {}",
                payload.len(),
                MAX_MESSAGE_SIZE
            ),
        });
    }

    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| ScribeError::ProtocolError { reason: "message too large".to_owned() })?;

    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&payload);

    writer.write_all(&frame).await.map_err(|e| ScribeError::Io { source: e })?;
    Ok(())
}
