//! Per-install device identity for LAN remote control (research D2).
//!
//! Each Scribe install mints ONCE a self-signed `Ed25519` `X.509` certificate
//! (`rcgen`, `PKCS_ED25519`). The stable **Device ID** is
//! `SHA-256`(`SubjectPublicKeyInfo`) — a 32-byte value that is the trust anchor
//! and the `mDNS` TXT `id`. Hashing the SPKI (not the whole cert) lets the
//! certificate be re-minted (validity/subject) without changing identity or
//! forcing peers to re-approve.
//!
//! The private key is sealed in the OS keyring, reusing the feature 006
//! keystore wrapper (its [`KeystoreError`] already classifies a locked
//! Keychain / unavailable Secret Service / denied access). The public
//! certificate DER is cached on disk under the server's per-user state
//! directory; the private key is NEVER written to disk in the clear.
//!
//! First-run generation therefore requires an interactive session with an
//! available, unlocked keyring. If the keyring is unavailable the owning side
//! FAILS CLOSED with [`IdentityError::KeyringUnavailable`] rather than fall
//! back to a plaintext on-disk key (analysis I2; a headless machine cannot be
//! an owning-side LAN host in this first version).
//!
//! For display, [`DeviceIdentity::fingerprint_words`] renders a short,
//! read-aloud-friendly word-list authentication string (`BIP39`/`PGP`-style,
//! research D8) derived from the Device ID; the full 32-byte Device ID stays
//! the only trust anchor. [`DeviceIdentity::cert_chain`] /
//! [`DeviceIdentity::signing_key`] are the handles a `rustls` config builder
//! consumes (`with_single_cert` / `with_client_auth_cert`).

use std::fmt;
use std::path::{Path, PathBuf};

use rcgen::{CertificateParams, KeyPair, PKCS_ED25519, PublicKeyData as _};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest as _, Sha256};

use scribe_common::app::current_state_dir;

use crate::env_store::keystore::{self, KeystoreError};

/// Keyring account holding the sealed `Ed25519` private key (`PKCS#8` DER).
/// Scoped by install flavor through [`keystore::service_identifier`], so stable
/// and dev installs never collide. A singleton per install (unlike the
/// per-envelope DEK accounts), so a fixed account name is correct.
const DEVICE_KEY_ACCOUNT: &str = "lan-device-key";

/// Cached public device certificate (DER), stored flat under the per-user state
/// directory with the shared `lan_` prefix (matching the trusted-networks
/// store). The private key is NEVER written here — only in the keyring.
const DEVICE_CERT_FILE: &str = "lan_device_cert.der";

/// Fixed placeholder subject alternative name on the self-signed cert. Identity
/// is the pinned key, not the name (research D3 connects by IP with a fixed
/// placeholder `ServerName` of "scribe"); this only keeps the cert well-formed.
const CERT_PLACEHOLDER_SAN: &str = "scribe";

/// Device-ID width in bytes: the `SHA-256` output length. Public so the
/// trusted-device store ([`crate::lan::trust`]) can name the same `[u8; 32]`
/// Device-ID shape without re-deriving the width.
pub const DEVICE_ID_LEN: usize = 32;

/// Number of leading Device-ID bytes rendered as fingerprint words. Six words
/// (~48 bits) is a comfortable read-aloud SAS length; the full 256-bit Device
/// ID remains the trust anchor.
const FINGERPRINT_WORD_COUNT: usize = 6;

/// Errors loading or generating the device identity. The owning-side LAN
/// surface fails closed on any of these.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The per-user state directory could not be resolved, so the device
    /// certificate cannot be cached.
    #[error("could not determine the state directory for the device identity")]
    StateDirUnavailable,
    /// The OS keyring is unavailable (locked, no Secret Service, or access
    /// denied), so the private key cannot be sealed. LAN remote control needs
    /// an interactive session with an unlocked keyring and never degrades to a
    /// plaintext on-disk key (analysis I2).
    #[error(
        "the OS keyring is unavailable, so the device key cannot be sealed; \
         LAN remote control needs an interactive session with an unlocked \
         keyring and never stores the key in plaintext: {0}"
    )]
    KeyringUnavailable(#[source] KeystoreError),
    /// Generating or parsing the device keypair / certificate failed.
    #[error("device identity certificate error: {0}")]
    Crypto(#[from] rcgen::Error),
    /// Reading or writing the cached device certificate failed.
    #[error("device certificate I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A loaded per-install device identity: the pinned Device ID, the public
/// certificate presented on the LAN, the sealed signing key held in memory for
/// building `rustls` configs, and the display fingerprint words.
pub struct DeviceIdentity {
    device_id: [u8; DEVICE_ID_LEN],
    cert_der: CertificateDer<'static>,
    key_der: PrivatePkcs8KeyDer<'static>,
    fingerprint_words: String,
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately never render the private key; show the public identity.
        f.debug_struct("DeviceIdentity")
            .field("device_id", &render_hex(&self.device_id))
            .field("fingerprint_words", &self.fingerprint_words)
            .field("cert_len", &self.cert_der.as_ref().len())
            .finish_non_exhaustive()
    }
}

impl DeviceIdentity {
    /// Assemble an identity from a keypair plus its sealed key and public cert,
    /// deriving the Device ID and fingerprint from the keypair's SPKI.
    fn assemble(
        key_pair: &KeyPair,
        key_der: PrivatePkcs8KeyDer<'static>,
        cert_der: CertificateDer<'static>,
    ) -> Self {
        let device_id = derive_device_id(key_pair);
        let fingerprint_words = fingerprint_words_for(&device_id);
        Self { device_id, cert_der, key_der, fingerprint_words }
    }

    /// The 32-byte pinned Device ID (`SHA-256`(`SubjectPublicKeyInfo`)) — the
    /// LAN trust anchor and `mDNS` TXT `id`.
    #[must_use]
    pub fn device_id(&self) -> [u8; DEVICE_ID_LEN] {
        self.device_id
    }

    /// The Device ID as lowercase hex, e.g. for the `mDNS` TXT `id` value and
    /// exact copy/compare display.
    #[must_use]
    pub fn device_id_hex(&self) -> String {
        render_hex(&self.device_id)
    }

    /// The self-signed certificate presented on the LAN link (public material).
    #[must_use]
    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    /// A fresh single-element certificate chain for a `rustls` config builder.
    #[must_use]
    pub fn cert_chain(&self) -> Vec<CertificateDer<'static>> {
        vec![self.cert_der.clone()]
    }

    /// The `rustls` signing handle: a `'static` clone of the private key, ready
    /// for `with_single_cert` / `with_client_auth_cert`.
    #[must_use]
    pub fn signing_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::from(self.key_der.clone_key())
    }

    /// The read-aloud fingerprint words shown on the approval prompt and in the
    /// trusted-devices list (research D8).
    #[must_use]
    pub fn fingerprint_words(&self) -> &str {
        &self.fingerprint_words
    }
}

/// Load the persisted device identity, generating and sealing a fresh one on
/// first run. Fails closed if the state directory cannot be resolved or the OS
/// keyring is unavailable — the private key is never written to disk in the
/// clear.
pub async fn load_or_generate() -> Result<DeviceIdentity, IdentityError> {
    let cert_path = device_cert_path()?;
    match keyring_get_device_key().await {
        Ok(Some(key_der)) => {
            let key_pair = KeyPair::from_pkcs8_der_and_sign_algo(&key_der, &PKCS_ED25519)?;
            let cert_der = load_or_remint_cert(&key_pair, &cert_path).await?;
            Ok(DeviceIdentity::assemble(&key_pair, key_der, cert_der))
        }
        Ok(None) => generate_and_seal(&cert_path).await,
        Err(err) => {
            tracing::warn!(%err, "LAN device-key keyring lookup failed; failing closed");
            Err(IdentityError::KeyringUnavailable(err))
        }
    }
}

/// Read this machine's OWN identity fingerprint WITHOUT generating one, for the
/// read-only Settings "This device's fingerprint" surface (`GetLanEnv`). Returns
/// `Ok(None)` when the LAN identity has not been generated yet (first LAN enable
/// has not happened), so merely viewing Settings never mints a key or writes to
/// disk — both the Device ID and the fingerprint words are derived directly from
/// the keyring-sealed keypair's SPKI, touching no certificate. Fails closed on a
/// keyring error exactly as [`load_or_generate`], letting the caller fold it into
/// the fail-closed `LanEnv` (identity absent).
pub async fn own_fingerprint() -> Result<Option<OwnFingerprint>, IdentityError> {
    match keyring_get_device_key().await {
        Ok(Some(key_der)) => {
            let key_pair = KeyPair::from_pkcs8_der_and_sign_algo(&key_der, &PKCS_ED25519)?;
            let device_id = derive_device_id(&key_pair);
            Ok(Some(OwnFingerprint {
                device_id_hex: render_hex(&device_id),
                fingerprint_words: fingerprint_words_for(&device_id),
            }))
        }
        Ok(None) => Ok(None),
        Err(err) => {
            tracing::warn!(%err, "LAN device-key keyring lookup failed; failing closed");
            Err(IdentityError::KeyringUnavailable(err))
        }
    }
}

/// This machine's own public LAN identity fingerprint (Device ID hex + read-aloud
/// words), returned by [`own_fingerprint`] for the Settings out-of-band compare
/// (FR-006). Carries no private material.
pub struct OwnFingerprint {
    /// Lowercase hex of the 32-byte `device_id = SHA-256(SPKI)`.
    pub device_id_hex: String,
    /// The read-aloud fingerprint words (research D8).
    pub fingerprint_words: String,
}

/// Path to the cached public device certificate under the per-user state dir.
fn device_cert_path() -> Result<PathBuf, IdentityError> {
    current_state_dir()
        .map(|dir| dir.join(DEVICE_CERT_FILE))
        .ok_or(IdentityError::StateDirUnavailable)
}

/// First-run path: generate a fresh keypair, seal the private key in the OS
/// keyring BEFORE touching disk (fail closed on any keyring error, leaving
/// nothing persisted), then mint and cache the public certificate.
async fn generate_and_seal(cert_path: &Path) -> Result<DeviceIdentity, IdentityError> {
    let key_pair = KeyPair::generate_for(&PKCS_ED25519)?;
    let key_pkcs8 = key_pair.serialize_der();

    keyring_set_device_key(key_pkcs8.clone()).await.map_err(IdentityError::KeyringUnavailable)?;

    let key_der = PrivatePkcs8KeyDer::from(key_pkcs8);
    let cert_der = self_signed_cert(&key_pair)?;
    write_cert(cert_path, &cert_der).await?;

    let identity = DeviceIdentity::assemble(&key_pair, key_der, cert_der);
    tracing::info!(
        device_id = %identity.device_id_hex(),
        fingerprint = %identity.fingerprint_words,
        "generated new LAN device identity"
    );
    Ok(identity)
}

/// Load the cached certificate, or re-mint it from the (already-sealed) key if
/// the cache is missing. The Device ID (SPKI hash) is unchanged by a re-mint,
/// so peers stay pinned and no re-approval is forced.
async fn load_or_remint_cert(
    key_pair: &KeyPair,
    cert_path: &Path,
) -> Result<CertificateDer<'static>, IdentityError> {
    match tokio::fs::read(cert_path).await {
        Ok(bytes) => Ok(CertificateDer::from(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cert_der = self_signed_cert(key_pair)?;
            write_cert(cert_path, &cert_der).await?;
            Ok(cert_der)
        }
        Err(e) => Err(IdentityError::Io(e)),
    }
}

/// Mint a self-signed certificate over the keypair and return its DER.
fn self_signed_cert(key_pair: &KeyPair) -> Result<CertificateDer<'static>, IdentityError> {
    let params = CertificateParams::new(vec![CERT_PLACEHOLDER_SAN.to_owned()])?;
    let cert = params.self_signed(key_pair)?;
    Ok(cert.der().clone())
}

/// Persist the (public) certificate DER, creating the state dir if needed.
async fn write_cert(
    cert_path: &Path,
    cert_der: &CertificateDer<'static>,
) -> Result<(), IdentityError> {
    if let Some(parent) = cert_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(cert_path, cert_der.as_ref()).await?;
    Ok(())
}

/// Device ID = `SHA-256` of the keypair's `SubjectPublicKeyInfo` DER. Derived
/// from the key (not the cert), so it is stable across certificate re-mints.
fn derive_device_id(key_pair: &KeyPair) -> [u8; DEVICE_ID_LEN] {
    Sha256::digest(key_pair.subject_public_key_info()).into()
}

/// Fetch the sealed private key from the OS keyring. A missing entry is
/// `Ok(None)` (first run); any other keyring condition is a real error the
/// caller maps to a fail-closed [`IdentityError::KeyringUnavailable`].
async fn keyring_get_device_key() -> Result<Option<PrivatePkcs8KeyDer<'static>>, KeystoreError> {
    let stored = tokio::task::spawn_blocking(|| -> Result<Option<Vec<u8>>, KeystoreError> {
        let entry = keyring::Entry::new(keystore::service_identifier(), DEVICE_KEY_ACCOUNT)?;
        match entry.get_secret() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(other) => Err(KeystoreError::from(other)),
        }
    })
    .await
    .map_err(|e| KeystoreError::Other(format!("blocking task panicked: {e}")))?;
    stored.map(|opt| opt.map(PrivatePkcs8KeyDer::from))
}

/// Seal the private key (`PKCS#8` DER) in the OS keyring, overwriting any prior
/// value at the same identifier.
async fn keyring_set_device_key(bytes: Vec<u8>) -> Result<(), KeystoreError> {
    tokio::task::spawn_blocking(move || -> Result<(), KeystoreError> {
        let entry = keyring::Entry::new(keystore::service_identifier(), DEVICE_KEY_ACCOUNT)?;
        entry.set_secret(&bytes)?;
        Ok(())
    })
    .await
    .map_err(|e| KeystoreError::Other(format!("blocking task panicked: {e}")))?
}

/// Lowercase hex of the Device ID, no separators.
fn render_hex(bytes: &[u8; DEVICE_ID_LEN]) -> String {
    bytes.iter().flat_map(|&b| [hex_digit(b >> 4), hex_digit(b & 0x0f)]).collect()
}

/// Map a 4-bit nibble to its lowercase hex digit.
fn hex_digit(nibble: u8) -> char {
    let n = nibble & 0x0f;
    char::from(if n < 10 { b'0' + n } else { b'a' + n - 10 })
}

/// Map the leading Device-ID bytes to space-separated fingerprint words
/// (research D8). Public and pure so the trusted-device store
/// ([`crate::lan::trust`]) renders the SAME read-aloud fingerprint for a peer's
/// pinned `device_id` as the owning machine shows for its own identity — the
/// word list is one shared table, never duplicated.
pub fn fingerprint_words_for(device_id: &[u8; DEVICE_ID_LEN]) -> String {
    device_id
        .iter()
        .take(FINGERPRINT_WORD_COUNT)
        .map(|&b| WORDS.get(usize::from(b)).copied().unwrap_or(CERT_PLACEHOLDER_SAN))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read-aloud fingerprint word list (research D8): exactly 256 deduplicated,
/// phonetically-distinct words, one per possible Device-ID byte value.
#[rustfmt::skip]
const WORDS: [&str; 256] = [
    "acid", "agent", "alien", "angle", "april", "aspen", "atom", "axis",
    "badge", "bamboo", "barley", "beach", "beam", "beaver", "bird", "blade",
    "board", "bonus", "boss", "brain", "bread", "brief", "brush", "buffalo",
    "bunny", "cactus", "canary", "canvas", "carpet", "cello", "charm", "chili",
    "cider", "city", "clever", "clock", "coach", "coffee", "copper", "cotton",
    "cover", "crane", "credit", "crimson", "cube", "dandy", "denim", "diamond",
    "dingo", "diver", "domino", "dragon", "dress", "duck", "earth", "echo",
    "egg", "elf", "emerald", "envoy", "ether", "exit", "fancy", "fence",
    "fiction", "finch", "flame", "flint", "flute", "foggy", "fork", "frost",
    "fungus", "galaxy", "gate", "gem", "giant", "glass", "globe", "goat",
    "gorilla", "grape", "gravel", "grill", "guitar", "guru", "hammer", "harp",
    "heart", "helm", "hex", "hippo", "hood", "hornet", "hound", "humble",
    "hydra", "igloo", "ingot", "iron", "ivy", "jazz", "jet", "jordan",
    "jumbo", "kayak", "keyhole", "kilt", "kite", "koala", "lake", "lance",
    "lark", "laurel", "ledge", "lentil", "lilac", "linen", "llama", "locust",
    "logic", "lucky", "lynx", "magnet", "manor", "market", "mason", "melody",
    "mercury", "meteor", "mimic", "mist", "moose", "mound", "muse", "nacho",
    "nectar", "nest", "nickel", "nomad", "nova", "nylon", "oats", "office",
    "onion", "orbit", "orion", "owl", "ozone", "palm", "pansy", "parade",
    "parsley", "patch", "peacock", "pebble", "penny", "petunia", "phoenix", "picnic",
    "pine", "pixel", "plasma", "plum", "poet", "poppy", "pottery", "prince",
    "proton", "puffin", "pumpkin", "puzzle", "quail", "quilt", "radar", "ragged",
    "ranch", "raven", "redwood", "relic", "rhino", "ridge", "river", "robot",
    "rope", "royal", "ruler", "rusty", "saffron", "salmon", "sand", "scene",
    "scone", "sculpt", "sedan", "shadow", "shear", "shine", "shrimp", "signal",
    "sketch", "sky", "smoke", "snow", "solid", "sorrel", "spark", "sphere",
    "spiral", "sprite", "squid", "statue", "stellar", "stork", "strand", "studio",
    "summit", "sweater", "syrup", "tapir", "teapot", "tent", "thunder", "timber",
    "tomato", "tornado", "track", "tribe", "tulip", "tunnel", "turtle", "tweed",
    "umbra", "upland", "urchin", "vanilla", "velvet", "vertex", "video", "violet",
    "vivid", "wagon", "wander", "wattle", "weasel", "wedge", "whisk", "window",
    "wolf", "wood", "wren", "yarn", "yield", "yogurt", "zephyr", "zinc",
];
