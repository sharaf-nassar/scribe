//! Settings process singleton enforcement.
//!
//! Uses a Unix domain socket for singleton detection and a `flock` advisory
//! lock to prevent TOCTOU races during the bind-or-connect sequence.

use std::io::{BufRead as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use scribe_common::socket::{settings_lock_path, settings_socket_path};

/// Result of attempting to become the singleton settings process.
pub enum SingletonResult {
    /// We are the singleton. The listener is ready to accept focus commands.
    /// The caller must keep the `_lock_file` alive to hold the flock.
    Primary { listener: UnixListener, socket_path: PathBuf, _lock_file: std::fs::File },
    /// Another instance is already running and was told to focus.
    AlreadyRunning,
}

/// Attempt to become the singleton settings process.
///
/// Acquires an advisory flock, then tries to bind the socket. If another
/// instance holds the socket, sends it a focus command and returns
/// `AlreadyRunning`.
pub fn acquire() -> Result<SingletonResult, String> {
    let lock_path = settings_lock_path();
    let socket_path = settings_socket_path();

    // Ensure the parent directory exists with 0o700 permissions.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create socket dir: {e}"))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("failed to set socket dir permissions: {e}"))?;
    }

    // Acquire advisory flock to serialise the bind-or-connect sequence.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("failed to open lock file: {e}"))?;

    #[allow(
        deprecated,
        reason = "nix::fcntl::Flock requires OwnedFd which conflicts with our File ownership"
    )]
    nix::fcntl::flock(
        std::os::unix::io::AsRawFd::as_raw_fd(&lock_file),
        nix::fcntl::FlockArg::LockExclusive,
    )
    .map_err(|e| format!("flock failed: {e}"))?;

    // Try to bind the socket.
    match try_bind(&socket_path) {
        Ok(listener) => {
            Ok(SingletonResult::Primary { listener, socket_path, _lock_file: lock_file })
        }
        Err(_bind_err) => {
            // Socket exists — try to connect and send focus.
            if send_focus_to_existing(&socket_path) {
                Ok(SingletonResult::AlreadyRunning)
            } else {
                // Stale socket — remove and retry.
                drop(std::fs::remove_file(&socket_path));
                let listener = try_bind(&socket_path)
                    .map_err(|e| format!("failed to bind after stale removal: {e}"))?;
                Ok(SingletonResult::Primary { listener, socket_path, _lock_file: lock_file })
            }
        }
    }
}

/// Try to bind the Unix socket. Sets permissions to 0o600.
fn try_bind(socket_path: &std::path::Path) -> Result<UnixListener, std::io::Error> {
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;

    // Set socket file permissions to 0o600 (defense-in-depth).
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;

    Ok(listener)
}

/// Try to connect to an existing settings process and send focus command.
fn send_focus_to_existing(socket_path: &std::path::Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path) else {
        return false;
    };
    stream.write_all(b"{\"cmd\":\"focus\"}\n").is_ok()
}

/// Check if an incoming connection is from the same UID.
///
/// Uses `SO_PEERCRED` via nix. Returns `false` if credentials cannot be
/// retrieved or the UID does not match.
pub fn verify_peer_uid(stream: &UnixStream) -> bool {
    let cred =
        match nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("failed to get peer credentials: {e}");
                return false;
            }
        };
    let expected = scribe_common::socket::current_uid();
    if cred.uid() != expected {
        tracing::warn!(
            peer_uid = cred.uid(),
            expected,
            "rejected settings connection from different UID"
        );
        return false;
    }
    true
}

/// Parse a command from a connected client.
///
/// Reads a single newline-terminated JSON line and returns the `cmd` field.
pub fn read_command(stream: &UnixStream) -> Option<String> {
    // Set a short read timeout to avoid blocking the GTK loop.
    drop(stream.set_read_timeout(Some(std::time::Duration::from_millis(100))));

    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return None;
    }

    let parsed: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    parsed.get("cmd")?.as_str().map(String::from)
}

/// Cleanup: remove the socket file.
pub fn cleanup_socket(socket_path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %socket_path.display(), "failed to remove settings socket: {e}");
        }
    }
}
