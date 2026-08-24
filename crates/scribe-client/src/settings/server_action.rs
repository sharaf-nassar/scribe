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
use scribe_common::protocol::{
    ClientMessage, PreflightError, ReleaseListResultState, ServerMessage, TrustedDeviceInfo,
    TrustedNetworkInfo, UpdateCheckResultState,
};
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

/// Send `TriggerUpdate` to the server. Fire-and-forget: the server has no
/// reply for this message, so we just write the frame and close.
///
/// The settings webview kicks the install off this way instead of opening a
/// registered window connection. Install progress is broadcast only to
/// registered clients, so the in-client overlay (not the settings window)
/// owns the user-facing progress and restart-required prompt. The settings
/// UI just disables its button and waits for the next manual re-check.
pub fn request_trigger_update(timeout: Duration) -> Result<(), String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;
    write_frame(&mut stream, &ClientMessage::TriggerUpdate)
}

/// Send `ListReleases` to the server and wait for the matching response.
///
/// Mirrors [`request_update_check`]: any transport or protocol error becomes
/// `ReleaseListResultState::Failed { reason }` so the UI always has a single
/// shape to render. A fresh connection is opened per call so this never reuses
/// the update-check socket.
pub fn request_release_list(timeout: Duration) -> ReleaseListResultState {
    match try_request_release_list(timeout) {
        Ok(state) => state,
        Err(reason) => {
            tracing::warn!("manual release list transport error: {reason}");
            ReleaseListResultState::Failed { reason }
        }
    }
}

fn try_request_release_list(timeout: Duration) -> Result<ReleaseListResultState, String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| format!("set_read_timeout: {e}"))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;

    write_frame(&mut stream, &ClientMessage::ListReleases)?;

    parse_release_list_response(read_frame(&mut stream)?)
}

/// Pure helper that maps an arbitrary `ServerMessage` into the state expected
/// by the release-list code path. Anything other than `ReleaseList { state }`
/// — including the wrong-variant case the server should never produce — is
/// surfaced as an `Err` so the public entry point can fold it into
/// `ReleaseListResultState::Failed { reason }`.
fn parse_release_list_response(msg: ServerMessage) -> Result<ReleaseListResultState, String> {
    match msg {
        ServerMessage::ReleaseList { state } => Ok(state),
        other => Err(format!("unexpected server response: {other:?}")),
    }
}

/// Result of an env-persistence preflight request. `Ok` means the server's
/// keystore probe succeeded; `Err(PreflightError)` is the structured reason
/// the toggle should refuse to commit. Transport / protocol errors map to
/// `Err(PreflightError::Unknown { reason })` so the UI always renders a single
/// shape — same pattern as `UpdateCheckResultState::Failed` and
/// `ReleaseListResultState::Failed`.
#[derive(Debug, Clone)]
pub enum EnvPreflightOutcome {
    Ok,
    Err(PreflightError),
}

/// Send `EnvPreflight` to the server and wait for the matching response.
///
/// Any transport or protocol error becomes
/// `EnvPreflightOutcome::Err(PreflightError::Unknown { reason })` so the UI
/// always has a single shape to render. A fresh connection is opened per call
/// so this never reuses sockets from other server actions.
pub fn request_env_preflight(timeout: Duration) -> EnvPreflightOutcome {
    match try_request_env_preflight(timeout) {
        Ok(outcome) => outcome,
        Err(reason) => {
            tracing::warn!("env preflight transport error: {reason}");
            EnvPreflightOutcome::Err(PreflightError::Unknown { reason })
        }
    }
}

fn try_request_env_preflight(timeout: Duration) -> Result<EnvPreflightOutcome, String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| format!("set_read_timeout: {e}"))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;

    write_frame(&mut stream, &ClientMessage::EnvPreflight)?;

    parse_env_preflight_response(read_frame(&mut stream)?)
}

/// Pure helper that maps an arbitrary `ServerMessage` into the outcome
/// expected by the env-preflight code path. Anything other than
/// `EnvPreflightResult { .. }` — including the wrong-variant case the server
/// should never produce — is surfaced as an `Err` so the public entry point
/// can fold it into `EnvPreflightOutcome::Err(PreflightError::Unknown { .. })`.
fn parse_env_preflight_response(msg: ServerMessage) -> Result<EnvPreflightOutcome, String> {
    match msg {
        ServerMessage::EnvPreflightResult { ok: true, error: _ } => Ok(EnvPreflightOutcome::Ok),
        ServerMessage::EnvPreflightResult { ok: false, error: Some(e) } => {
            Ok(EnvPreflightOutcome::Err(e))
        }
        ServerMessage::EnvPreflightResult { ok: false, error: None } => {
            Ok(EnvPreflightOutcome::Err(PreflightError::Unknown {
                reason: String::from("server reported failure with no reason"),
            }))
        }
        other => Err(format!("unexpected server response: {other:?}")),
    }
}

/// Outcome of a `GetRemoteEnv` request: this machine's signed-in tailnet
/// account name (absent when unknown) and whether Tailscale was detected at
/// all. Any transport / protocol error — or a timeout — folds into the
/// fail-closed default `{ account: None, tailscale_detected: false }` so the
/// settings window always has a single shape to render, never blocks on
/// tailscaled, and simply shows the passive "Tailscale not detected" notice
/// (FR-015). `Default` yields exactly that fail-closed shape.
#[derive(Debug, Clone, Default)]
pub struct RemoteEnvOutcome {
    /// Signed-in tailnet login name for the UX-003 statement; `None` keeps the
    /// generic account placeholder in the webview.
    pub account: Option<String>,
    /// Whether the server reached its `LocalAPI` at all; `false` shows the
    /// passive "Tailscale not detected" notice.
    pub tailscale_detected: bool,
}

/// Send `GetRemoteEnv` to the server and wait for the matching response.
///
/// Any transport or protocol error — including a timeout — becomes the
/// fail-closed `RemoteEnvOutcome::default()` (`account: None,
/// tailscale_detected: false`) so the settings window always has a single shape
/// to render and never blocks on tailscaled. A fresh connection is opened per
/// call so this never reuses sockets from other server actions.
pub fn request_remote_env(timeout: Duration) -> RemoteEnvOutcome {
    match try_request_remote_env(timeout) {
        Ok(outcome) => outcome,
        Err(reason) => {
            tracing::warn!("remote env transport error: {reason}");
            RemoteEnvOutcome::default()
        }
    }
}

fn try_request_remote_env(timeout: Duration) -> Result<RemoteEnvOutcome, String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| format!("set_read_timeout: {e}"))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;

    write_frame(&mut stream, &ClientMessage::GetRemoteEnv)?;

    parse_remote_env_response(read_frame(&mut stream)?)
}

/// Pure helper that maps an arbitrary `ServerMessage` into the outcome expected
/// by the remote-env code path. Anything other than `RemoteEnv { .. }` —
/// including the wrong-variant case the server should never produce — is
/// surfaced as an `Err` so the public entry point can fold it into the
/// fail-closed default.
fn parse_remote_env_response(msg: ServerMessage) -> Result<RemoteEnvOutcome, String> {
    match msg {
        ServerMessage::RemoteEnv { account, tailscale_detected } => {
            Ok(RemoteEnvOutcome { account, tailscale_detected })
        }
        other => Err(format!("unexpected server response: {other:?}")),
    }
}

/// Outcome of a [`ClientMessage::GetLanEnv`] request (feature 014): this
/// machine's OWN LAN identity fingerprint (word list + hex, both absent until
/// the identity has been generated on first LAN enable) and whether the current
/// network can be fingerprinted as a trusted network (which drives the "Add
/// current network" control in the Settings "Local network" section). Any
/// transport or protocol error — including a timeout — folds into the fail-closed
/// `Default` (`device_id_hex: None`, `fingerprint_words: None`,
/// `current_network_addable: false`, `current_network_reason: None`) so the
/// settings window always renders one shape and never blocks — the same pattern
/// as [`RemoteEnvOutcome`]. `Default` yields exactly that fail-closed shape.
#[derive(Debug, Clone, Default)]
pub struct LanEnvOutcome {
    /// Lowercase hex of this machine's `device_id = SHA-256(SPKI)`; `None` until
    /// the LAN identity has been generated.
    pub device_id_hex: Option<String>,
    /// This machine's own word-list fingerprint; `None` until generated.
    pub fingerprint_words: Option<String>,
    /// Whether the current network can be fingerprinted as a trusted network.
    pub current_network_addable: bool,
    /// Short explanatory note shown when the "Add current network" action is
    /// unavailable; `None` otherwise or when the server sends none.
    pub current_network_reason: Option<String>,
}

/// Send [`ClientMessage::GetLanEnv`] to the server and wait for the matching
/// [`ServerMessage::LanEnv`].
///
/// Any transport or protocol error — including a timeout — becomes the
/// fail-closed [`LanEnvOutcome::default`] so the settings window always has a
/// single shape to render. A fresh connection is opened per call so this never
/// reuses sockets from other server actions.
pub fn request_lan_env(timeout: Duration) -> LanEnvOutcome {
    match try_request_lan_env(timeout) {
        Ok(outcome) => outcome,
        Err(reason) => {
            tracing::warn!("lan env transport error: {reason}");
            LanEnvOutcome::default()
        }
    }
}

pub(super) fn try_request_lan_env(timeout: Duration) -> Result<LanEnvOutcome, String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| format!("set_read_timeout: {e}"))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;

    write_frame(&mut stream, &ClientMessage::GetLanEnv)?;

    parse_lan_env_response(read_frame(&mut stream)?)
}

/// Pure helper mapping an arbitrary [`ServerMessage`] into a [`LanEnvOutcome`].
/// Anything other than [`ServerMessage::LanEnv`] is surfaced as an `Err` so the
/// public entry point can fold it into the fail-closed default.
fn parse_lan_env_response(msg: ServerMessage) -> Result<LanEnvOutcome, String> {
    match msg {
        ServerMessage::LanEnv {
            device_id_hex,
            fingerprint_words,
            current_network_addable,
            current_network_reason,
        } => Ok(LanEnvOutcome {
            device_id_hex,
            fingerprint_words,
            current_network_addable,
            current_network_reason,
        }),
        other => Err(format!("unexpected server response: {other:?}")),
    }
}

/// Outcome of a [`ClientMessage::ListTrustedNetworks`] request (feature 014):
/// the user's trusted networks and whether the network the machine is currently
/// on is one of them (`current_trusted` drives the active/dormant status line,
/// UX-004). Any transport or protocol error folds into the fail-closed `Default`
/// (an empty list, `current_trusted: false`) so the settings window always
/// renders one shape.
#[derive(Debug, Clone, Default)]
pub struct TrustedNetworksOutcome {
    /// The user's trusted networks, in the order the server returns them.
    pub networks: Vec<TrustedNetworkInfo>,
    /// Whether the network the machine is currently on is trusted.
    pub current_trusted: bool,
}

/// Send [`ClientMessage::ListTrustedNetworks`] and wait for the matching
/// [`ServerMessage::TrustedNetworkList`]. Any error folds into the fail-closed
/// [`TrustedNetworksOutcome::default`]. A fresh connection is opened per call.
pub fn request_trusted_networks(timeout: Duration) -> TrustedNetworksOutcome {
    match try_request_trusted_networks(timeout) {
        Ok(outcome) => outcome,
        Err(reason) => {
            tracing::warn!("trusted networks transport error: {reason}");
            TrustedNetworksOutcome::default()
        }
    }
}

pub(super) fn try_request_trusted_networks(
    timeout: Duration,
) -> Result<TrustedNetworksOutcome, String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| format!("set_read_timeout: {e}"))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;

    write_frame(&mut stream, &ClientMessage::ListTrustedNetworks)?;

    parse_trusted_networks_response(read_frame(&mut stream)?)
}

/// Pure helper mapping an arbitrary [`ServerMessage`] into a
/// [`TrustedNetworksOutcome`]. Anything other than
/// [`ServerMessage::TrustedNetworkList`] is surfaced as an `Err`.
fn parse_trusted_networks_response(msg: ServerMessage) -> Result<TrustedNetworksOutcome, String> {
    match msg {
        ServerMessage::TrustedNetworkList { networks, current_trusted } => {
            Ok(TrustedNetworksOutcome { networks, current_trusted })
        }
        other => Err(format!("unexpected server response: {other:?}")),
    }
}

/// Send [`ClientMessage::ListTrustedDevices`] and wait for the matching
/// [`ServerMessage::TrustedDeviceList`]. Any transport or protocol error folds
/// into a fail-closed empty list so the settings window always renders one
/// shape. A fresh connection is opened per call.
pub fn request_trusted_devices(timeout: Duration) -> Vec<TrustedDeviceInfo> {
    match try_request_trusted_devices(timeout) {
        Ok(devices) => devices,
        Err(reason) => {
            tracing::warn!("trusted devices transport error: {reason}");
            Vec::new()
        }
    }
}

fn try_request_trusted_devices(timeout: Duration) -> Result<Vec<TrustedDeviceInfo>, String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| format!("set_read_timeout: {e}"))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;

    write_frame(&mut stream, &ClientMessage::ListTrustedDevices)?;

    parse_trusted_devices_response(read_frame(&mut stream)?)
}

/// Pure helper mapping an arbitrary [`ServerMessage`] into the trusted-devices
/// list. Anything other than [`ServerMessage::TrustedDeviceList`] is surfaced as
/// an `Err`.
fn parse_trusted_devices_response(msg: ServerMessage) -> Result<Vec<TrustedDeviceInfo>, String> {
    match msg {
        ServerMessage::TrustedDeviceList { devices } => Ok(devices),
        other => Err(format!("unexpected server response: {other:?}")),
    }
}

/// Send [`ClientMessage::AddCurrentNetworkTrusted`] to the server (feature 014).
///
/// Fire-and-forget, exactly like [`request_trigger_update`]: the server has no
/// reply frame for this mutation, so we write the frame and close. The settings
/// UI re-issues the list/env queries afterward to render the updated trust
/// state; the conditional "Add current network" control already pre-empts the
/// unidentifiable-network case surfaced by [`request_lan_env`].
pub fn request_add_current_network(timeout: Duration) -> Result<(), String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;
    write_frame(&mut stream, &ClientMessage::AddCurrentNetworkTrusted)
}

/// Send [`ClientMessage::RemoveTrustedNetwork`] to the server (feature 014).
///
/// Fire-and-forget like [`request_add_current_network`]: the server has no reply
/// frame, so we write the frame and close. The settings UI refreshes the list
/// afterward.
pub fn request_remove_trusted_network(id: String, timeout: Duration) -> Result<(), String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;
    write_frame(&mut stream, &ClientMessage::RemoveTrustedNetwork { id })
}

/// Send [`ClientMessage::RevokeTrustedDevice`] to the server (feature 014).
///
/// Fire-and-forget like [`request_add_current_network`]: the server has no reply
/// frame (revoking removes the pin and severs the device's live connection), so
/// we write the frame and close. The settings UI refreshes the list afterward.
pub fn request_revoke_trusted_device(device_id: String, timeout: Duration) -> Result<(), String> {
    let path = server_socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("connect to {} failed: {e}", path.display()))?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| format!("set_write_timeout: {e}"))?;
    write_frame(&mut stream, &ClientMessage::RevokeTrustedDevice { device_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use scribe_common::protocol::UpdateCheckResultState;

    /// A non-`ReleaseList` server response must surface as a non-empty `Err`
    /// so the public entry point can fold it into
    /// `ReleaseListResultState::Failed { reason }`.
    ///
    /// This is the parser-side proxy for the transport-failure mapping:
    /// `request_release_list` cannot panic on an unexpected variant, and we
    /// avoid spinning up a real Unix socket (which would require overriding
    /// `server_socket_path` — there is no test hook today) by aiming the
    /// assertion at the parser the public entry point delegates to.
    #[test]
    fn parse_release_list_response_rejects_unexpected_variant() {
        let wrong_variant =
            ServerMessage::UpdateCheckResult { state: UpdateCheckResultState::NoUpdate };
        let err = parse_release_list_response(wrong_variant)
            .expect_err("unexpected variant must be reported as Err");
        assert!(!err.is_empty(), "error reason must not be empty");
        assert!(
            err.contains("unexpected server response"),
            "error message should describe the wrong-variant case: {err}"
        );
    }

    /// Sanity: a real `ReleaseList` payload round-trips through the parser
    /// untouched.
    #[test]
    fn parse_release_list_response_passes_through_release_list() {
        let state =
            ReleaseListResultState::Failed { reason: String::from("synthetic upstream failure") };
        let msg = ServerMessage::ReleaseList { state: state.clone() };
        let parsed =
            parse_release_list_response(msg).expect("ReleaseList variant must parse cleanly");
        assert_eq!(parsed, state);
    }

    /// A non-`EnvPreflightResult` server response must surface as a non-empty
    /// `Err` so the public entry point can fold it into
    /// `EnvPreflightOutcome::Err(PreflightError::Unknown { reason })`.
    ///
    /// This is the parser-side proxy for the transport-failure mapping:
    /// `request_env_preflight` cannot panic on an unexpected variant, and we
    /// avoid spinning up a real Unix socket (which would require overriding
    /// `server_socket_path` — there is no test hook today) by aiming the
    /// assertion at the parser the public entry point delegates to.
    #[test]
    fn parse_env_preflight_response_rejects_unexpected_variant() {
        let wrong_variant =
            ServerMessage::UpdateCheckResult { state: UpdateCheckResultState::NoUpdate };
        let err = parse_env_preflight_response(wrong_variant)
            .expect_err("unexpected variant must be reported as Err");
        assert!(!err.is_empty(), "error reason must not be empty");
        assert!(
            err.contains("unexpected server response"),
            "error message should describe the wrong-variant case: {err}"
        );
    }

    /// Sanity: real `EnvPreflightResult` payloads round-trip through the
    /// parser untouched. `ok=true` maps to `Ok` regardless of the (unused)
    /// `error` slot, and `ok=false` with a structured `error` maps to the
    /// matching `Err(PreflightError)` so the toggle gets the actionable
    /// reason it needs to surface inline.
    #[test]
    fn parse_env_preflight_response_passes_through_ok_and_error() {
        let ok_msg = ServerMessage::EnvPreflightResult { ok: true, error: None };
        match parse_env_preflight_response(ok_msg).expect("ok variant must parse cleanly") {
            EnvPreflightOutcome::Ok => {}
            unexpected @ EnvPreflightOutcome::Err(_) => panic!("expected Ok, got {unexpected:?}"),
        }

        let err_msg = ServerMessage::EnvPreflightResult {
            ok: false,
            error: Some(PreflightError::KeychainLocked),
        };
        match parse_env_preflight_response(err_msg).expect("err variant must parse cleanly") {
            EnvPreflightOutcome::Err(PreflightError::KeychainLocked) => {}
            unexpected => panic!("expected Err(KeychainLocked), got {unexpected:?}"),
        }
    }
}
