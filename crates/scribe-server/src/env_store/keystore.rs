//! OS secret-store wrapper around the `keyring` crate.
//!
//! Provides binary DEK (data-encryption key) get/set/delete keyed by the
//! install flavor (via [`scribe_common::app::current_identity`]) and the
//! `(window_id, launch_id)` pair, plus the [`KeystoreError`] → wire-level
//! `PreflightError` mapping consumed by the T013 `preflight()` function.
//!
//! All keyring operations wrap synchronous `keyring` calls in
//! `tokio::task::spawn_blocking` so blocking I/O does not stall the async
//! runtime. The `preflight()` low-cost-sentinel probe lives in T013 on top
//! of these helpers.
//!
//! See specs/006-persist-terminal-env/research.md sections R2.1, R2.3, R2.4
//! and `specs/006-persist-terminal-env/data-model.md::KeystorePreflight`.

use chacha20poly1305::aead::OsRng;
use chacha20poly1305::aead::rand_core::RngCore;
use thiserror::Error;

use scribe_common::app::current_identity;
use scribe_common::ids::WindowId;

/// Stable service identifier scoped to the install flavor. Used as the
/// macOS Keychain service name and as a Linux Secret Service attribute so
/// stable and `scribe-dev` installs cannot collide on the same keystore
/// items.
///
/// Derived from [`scribe_common::app::AppIdentity::launchd_label`], which
/// returns `"com.scribe.server"` for stable and `"com.scribe.dev.server"`
/// for the dev flavor.
#[must_use]
pub fn service_identifier() -> &'static str {
    current_identity().launchd_label()
}

/// Per-envelope account name: stable across the envelope's lifetime so
/// the DEK can be fetched at restore time and deleted on clean close.
///
/// Matches the identifier scheme in research.md R2.3: every key is
/// namespaced by the install flavor (via [`service_identifier`]) and the
/// `(window_id, launch_id)` pair.
#[must_use]
pub fn account_for(window_id: WindowId, launch_id: &str) -> String {
    format!("env-key-{}-{launch_id}", window_id.to_full_string())
}

/// AEAD data-encryption key. 256-bit / 32 bytes, suitable for
/// `chacha20poly1305::ChaCha20Poly1305::new`.
pub type Dek = [u8; 32];

/// Internal keystore error. Maps cleanly onto the wire-level
/// [`scribe_common::protocol::PreflightError`] via [`to_preflight_error`].
///
/// `keyring::Error::PlatformFailure` and `NoStorageAccess` carry boxed
/// platform-specific errors with no machine-readable kind, so the inner
/// `Display` text is inspected for a small set of keywords ("locked",
/// "dbus" / "secret service", "access" / "denied") to classify them. This
/// is a deliberate trade-off — the alternative (target-specific downcasts
/// into `security-framework::Error` / `secret-service::Error`) would
/// double the surface for marginal precision.
#[derive(Debug, Error)]
pub enum KeystoreError {
    /// macOS: the login keychain is locked.
    #[error("login keychain is locked")]
    KeychainLocked,
    /// Linux: D-Bus session bus or Secret Service backend is unavailable.
    #[error("secret service not available")]
    SecretServiceUnavailable,
    /// Either platform: keystore access denied for our identifier.
    #[error("access denied")]
    AccessDenied,
    /// The credential entry does not exist (e.g. DEK never written, or
    /// already deleted on clean close).
    #[error("dek not found")]
    NotFound,
    /// Any other underlying error; inner string is preserved for
    /// diagnostics and surfaced into `PreflightError::Unknown`.
    #[error("keyring error: {0}")]
    Other(String),
}

impl From<keyring::Error> for KeystoreError {
    fn from(e: keyring::Error) -> Self {
        use keyring::Error as K;
        match e {
            K::NoEntry => Self::NotFound,
            K::Ambiguous(_) => Self::Other("ambiguous keyring entry".to_owned()),
            K::PlatformFailure(inner) => classify_platform_failure(&inner.to_string()),
            K::NoStorageAccess(inner) => classify_storage_access(&inner.to_string()),
            other => Self::Other(other.to_string()),
        }
    }
}

/// Classify a `PlatformFailure` message based on substring patterns.
/// Pulled out into its own function so the `From` impl stays scannable.
fn classify_platform_failure(msg: &str) -> KeystoreError {
    let lower = msg.to_lowercase();
    if lower.contains("locked") {
        KeystoreError::KeychainLocked
    } else if lower.contains("no such") || lower.contains("service") {
        KeystoreError::SecretServiceUnavailable
    } else if lower.contains("access") || lower.contains("denied") {
        KeystoreError::AccessDenied
    } else {
        KeystoreError::Other(msg.to_owned())
    }
}

/// Classify a `NoStorageAccess` message; semantics differ from
/// `PlatformFailure` (this variant indicates the store could not be
/// reached at all, so the default leans `AccessDenied`).
fn classify_storage_access(msg: &str) -> KeystoreError {
    let lower = msg.to_lowercase();
    if lower.contains("locked") {
        KeystoreError::KeychainLocked
    } else if lower.contains("dbus") || lower.contains("secret service") {
        KeystoreError::SecretServiceUnavailable
    } else {
        KeystoreError::AccessDenied
    }
}

/// Convert an internal [`KeystoreError`] to the wire-level
/// `PreflightError` reported back to the settings UI inside
/// `ServerMessage::EnvPreflightResult`.
///
/// `NotFound` is mapped to `Unknown` (rather than its own variant) because
/// it is an internal lookup-failure signal — surfacing it as a distinct
/// preflight outcome would not give the user any actionable next step.
#[must_use]
pub fn to_preflight_error(e: &KeystoreError) -> scribe_common::protocol::PreflightError {
    use scribe_common::protocol::PreflightError as P;
    match e {
        KeystoreError::KeychainLocked => P::KeychainLocked,
        KeystoreError::SecretServiceUnavailable => P::SecretServiceUnavailable,
        KeystoreError::AccessDenied => P::KeystoreAccessDenied,
        KeystoreError::NotFound => P::Unknown("dek not found".to_owned()),
        KeystoreError::Other(s) => P::Unknown(s.clone()),
    }
}

/// Fetch the 32-byte DEK for `(window_id, launch_id)` from the OS secret
/// store. Returns [`KeystoreError::NotFound`] if no entry exists.
///
/// Wraps the synchronous `keyring::Entry::get_secret` call in
/// `spawn_blocking` so the async runtime is not held by D-Bus / Keychain
/// I/O.
pub async fn get_dek(window_id: WindowId, launch_id: &str) -> Result<Dek, KeystoreError> {
    let account = account_for(window_id, launch_id);
    let launch_for_log = launch_id.to_owned();
    let result = tokio::task::spawn_blocking(move || -> Result<Dek, KeystoreError> {
        let entry = keyring::Entry::new(service_identifier(), &account)?;
        let bytes = entry.get_secret()?;
        let len = bytes.len();
        <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| KeystoreError::Other(format!("dek length mismatch: {len} bytes")))
    })
    .await
    .map_err(|e| KeystoreError::Other(format!("blocking task panicked: {e}")))?;
    if let Err(ref err) = result {
        tracing::debug!(
            target: "scribe_server::env_store::keystore",
            window_id = ?window_id,
            launch_id = %launch_for_log,
            error = %err,
            "get_dek failed"
        );
    }
    result
}

/// Store the 32-byte DEK for `(window_id, launch_id)` in the OS secret
/// store. Overwrites any prior value at the same identifier.
pub async fn set_dek(window_id: WindowId, launch_id: &str, dek: &Dek) -> Result<(), KeystoreError> {
    let account = account_for(window_id, launch_id);
    let bytes = dek.to_vec();
    let launch_for_log = launch_id.to_owned();
    let result = tokio::task::spawn_blocking(move || -> Result<(), KeystoreError> {
        let entry = keyring::Entry::new(service_identifier(), &account)?;
        entry.set_secret(&bytes)?;
        Ok(())
    })
    .await
    .map_err(|e| KeystoreError::Other(format!("blocking task panicked: {e}")))?;
    match &result {
        Ok(()) => tracing::debug!(
            target: "scribe_server::env_store::keystore",
            window_id = ?window_id,
            launch_id = %launch_for_log,
            "set_dek ok"
        ),
        Err(err) => tracing::warn!(
            target: "scribe_server::env_store::keystore",
            window_id = ?window_id,
            launch_id = %launch_for_log,
            error = %err,
            "set_dek failed"
        ),
    }
    result
}

/// Delete the DEK for `(window_id, launch_id)` from the OS secret store.
/// Idempotent in spirit but returns [`KeystoreError::NotFound`] if the
/// entry was already gone; callers can choose to ignore that variant
/// during cleanup sweeps.
pub async fn delete_dek(window_id: WindowId, launch_id: &str) -> Result<(), KeystoreError> {
    let account = account_for(window_id, launch_id);
    let launch_for_log = launch_id.to_owned();
    let result = tokio::task::spawn_blocking(move || -> Result<(), KeystoreError> {
        let entry = keyring::Entry::new(service_identifier(), &account)?;
        entry.delete_credential()?;
        Ok(())
    })
    .await
    .map_err(|e| KeystoreError::Other(format!("blocking task panicked: {e}")))?;
    if let Err(ref err) = result {
        tracing::debug!(
            target: "scribe_server::env_store::keystore",
            window_id = ?window_id,
            launch_id = %launch_for_log,
            error = %err,
            "delete_dek failed"
        );
    }
    result
}

/// Generate a fresh random 32-byte DEK for a new envelope.
///
/// Uses `chacha20poly1305::aead::OsRng` (re-exported from `crypto_common`'s
/// `rand_core` feature) so we don't pull in a separate `rand_core` dep
/// just for entropy.
#[must_use]
pub fn generate_dek() -> Dek {
    let mut dek = [0u8; 32];
    OsRng.fill_bytes(&mut dek);
    dek
}

/// Sentinel account name used by `preflight` to probe keystore reachability
/// without touching any real envelope DEK entries.
const PREFLIGHT_ACCOUNT: &str = "preflight";

/// Probe the OS secret store for reachability and write/delete permission.
///
/// Implementation: attempt a `set_secret` then `delete_credential` of a
/// sentinel item under our service identifier. If both succeed, the keystore
/// is available, unlocked, and we can read+write secrets. Any failure is
/// classified into `KeystoreError` (via the existing `From<keyring::Error>`
/// impl) and forwarded.
///
/// Called when:
/// 1. The user toggles `terminal.env_persistence.enabled` ON in Settings →
///    Terminal → General (`ClientMessage::EnvPreflight` request).
/// 2. The runtime fail-safe path re-tries after a `Degraded` transition,
///    once the user re-enables the setting.
///
/// Idempotent: each call leaves the keystore state unchanged on success
/// (we delete what we wrote). On failure no partial state is left if the
/// initial set succeeded — we still issue the delete defensively, ignoring
/// its result (the set was the gating success).
pub async fn preflight() -> Result<(), KeystoreError> {
    tokio::task::spawn_blocking(|| -> Result<(), KeystoreError> {
        let entry = keyring::Entry::new(service_identifier(), PREFLIGHT_ACCOUNT)?;

        // Probe write.
        entry.set_secret(b"preflight-ok")?;

        // Best-effort cleanup. Treat an error here as warn-and-continue: the
        // probe itself succeeded, which is what we promise the caller.
        if let Err(e) = entry.delete_credential() {
            tracing::warn!(
                target: "scribe_server::env_store::keystore",
                error = %e,
                "preflight delete-credential failed; sentinel item may remain in keystore"
            );
        }

        Ok(())
    })
    .await
    .map_err(|e| KeystoreError::Other(format!("blocking task panicked: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use scribe_common::protocol::PreflightError as P;

    // Note: We deliberately do NOT exercise `preflight()` here — it would
    // touch the OS keystore on the test runner and be flaky. The mapping
    // from `KeystoreError` to wire `PreflightError` IS tested below; T040
    // expands these mappings if needed.

    #[test]
    fn maps_keychain_locked() {
        let mapped = to_preflight_error(&KeystoreError::KeychainLocked);
        assert!(matches!(mapped, P::KeychainLocked));
    }

    #[test]
    fn maps_secret_service_unavailable() {
        let mapped = to_preflight_error(&KeystoreError::SecretServiceUnavailable);
        assert!(matches!(mapped, P::SecretServiceUnavailable));
    }

    #[test]
    fn maps_access_denied() {
        let mapped = to_preflight_error(&KeystoreError::AccessDenied);
        assert!(matches!(mapped, P::KeystoreAccessDenied));
    }

    #[test]
    fn maps_not_found_to_unknown() {
        let mapped = to_preflight_error(&KeystoreError::NotFound);
        assert!(matches!(mapped, P::Unknown(_)));
    }

    #[test]
    fn maps_other_to_unknown_with_message() {
        let mapped = to_preflight_error(&KeystoreError::Other("d-bus down".into()));
        let msg = match mapped {
            P::Unknown(s) => s,
            other => panic!("expected Unknown, got {other:?}"),
        };
        assert!(msg.contains("d-bus down"));
    }
}
