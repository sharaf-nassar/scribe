use std::str::FromStr as _;

use scribe_common::ids::SessionId;

use crate::TestError;
use crate::cmd_socket::{DaemonRequest, DaemonResponse, send_request};

/// Convert escape sequences in the input string to raw bytes.
///
/// Supported escapes: `\n` (newline), `\t` (tab), `\\` (backslash),
/// `\xNN` (hex byte). Other `\X` sequences pass through literally.
/// Non-escape characters are encoded as UTF-8 bytes.
pub fn parse_escapes(input: &str) -> Vec<u8> {
    let mut result = Vec::with_capacity(input.len());
    let mut chars = input.chars();

    loop {
        let Some(c) = chars.next() else {
            break;
        };
        if c != '\\' {
            let mut buf = [0u8; 4];
            result.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }

        // Backslash — look at the next character.
        let Some(escaped) = chars.next() else {
            // Trailing backslash — emit it literally.
            result.push(b'\\');
            break;
        };

        match escaped {
            'n' => result.push(b'\n'),
            't' => result.push(b'\t'),
            '\\' => result.push(b'\\'),
            'x' => {
                if let Some(byte) = parse_hex_byte(&mut chars) {
                    result.push(byte);
                } else {
                    // Not a valid hex sequence — emit `\x` literally.
                    result.push(b'\\');
                    result.push(b'x');
                }
            }
            other => {
                // Unknown escape — emit backslash + character literally.
                result.push(b'\\');
                let mut buf = [0u8; 4];
                result.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }

    result
}

/// Try to parse two hex digits from the character iterator.
fn parse_hex_byte(chars: &mut std::str::Chars<'_>) -> Option<u8> {
    let hi = chars.next()?.to_digit(16)?;
    let lo = chars.next()?.to_digit(16)?;
    u8::try_from(hi * 16 + lo).ok()
}

/// Send data (keystrokes) to a session.
pub fn send(session_id: &str, data: &str) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let bytes = parse_escapes(data);

    let response = send_request(&DaemonRequest::Send { session_id: id, data: bytes })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Resize a session's terminal dimensions.
pub fn resize(session_id: &str, cols: u16, rows: u16) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let response = send_request(&DaemonRequest::Resize { session_id: id, cols, rows })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}
