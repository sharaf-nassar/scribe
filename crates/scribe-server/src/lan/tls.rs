//! Async mutual TLS for the LAN transport (feature 014, research D3/D4).
//!
//! The LAN TCP stream is wrapped in **TLS 1.3, mutual** (both peers present and
//! verify a certificate) via `rustls` 0.23 + `tokio-rustls`. Each peer presents
//! this machine's device certificate (from [`super::identity`]) and installs a
//! custom pinning verifier — [`ClientCertVerifier`] on the listener,
//! [`ServerCertVerifier`] on the dialer — that anchors trust in the peer's
//! **Device ID** (`SHA-256`(`SubjectPublicKeyInfo`)), never a CA chain or a
//! hostname.
//!
//! ## Trust-on-first-use, gated by the app layer
//!
//! The verifier deliberately does NOT reject an unknown peer. Per handshake it:
//!
//! 1. parses the presented end-entity certificate and derives its Device ID
//!    ([`device_id_from_cert`]); a certificate that cannot be parsed is a hard
//!    handshake failure ([`rustls::CertificateError::BadEncoding`]);
//! 2. classifies the Device ID against the pinned trusted-device set
//!    ([`DevicePins`]) as [`PinDecision::Known`] or [`PinDecision::Pending`];
//! 3. records the observed [`PeerIdentity`] so the caller can read it after the
//!    handshake completes; and
//! 4. returns *verified* either way (trust-on-first-use) — so the TLS handshake
//!    completes for a first-seen peer too.
//!
//! The known/pending split is a classification, not an authorization: no
//! window or session data flows for a `Pending` peer until the **app layer**
//! (not this verifier, which cannot await a human decision) gates it behind the
//! owning-side approval prompt. There is intentionally **no** "identity
//! changed" state — trust is keyed by Device ID, so a re-keyed peer simply
//! presents a new, unpinned Device ID and is a normal `Pending` device.
//!
//! ## Proving key possession (never stub the signature check)
//!
//! Skipping CA-chain validation is safe here only because the handshake
//! signature is still verified: it proves the peer holds the private key behind
//! the certificate it presented. Both roles therefore delegate
//! `verify_tls12_signature` / `verify_tls13_signature` to the crypto provider's
//! [`rustls::crypto::verify_tls12_signature`] /
//! [`rustls::crypto::verify_tls13_signature`] — these are never stubbed `Ok`.
//!
//! ## Per-connection configuration
//!
//! A fresh verifier (and its recording slot) is built per [`LanTls::accept`] /
//! [`LanTls::connect`] so each handshake surfaces exactly one peer identity
//! with no cross-connection sharing. Rebuilding the `rustls` config per
//! connection re-parses this machine's own Ed25519 key — a microsecond-scale
//! cost that is irrelevant at the LAN's handful-of-connections rate.

use std::fmt;
use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms,
    verify_tls12_signature as crypto_verify_tls12_signature,
    verify_tls13_signature as crypto_verify_tls13_signature,
};
use rustls::pki_types::{CertificateDer, InvalidDnsNameError, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error as TlsError,
    ServerConfig, SignatureScheme,
};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use x509_parser::parse_x509_certificate;

use super::identity::DeviceIdentity;

/// A pinned LAN device identity: `SHA-256` of a certificate's
/// `SubjectPublicKeyInfo`. Matches [`DeviceIdentity::device_id`], so a peer's
/// own advertised `id` and the value derived from its presented certificate are
/// the same 32 bytes.
pub type DeviceId = [u8; 32];

/// Fixed placeholder SNI presented by the dialer. LAN peers are dialed by IP
/// and their identity is the pinned key, not the name (research D3), so the
/// [`ServerCertVerifier`] ignores it entirely; it only has to be a
/// syntactically valid DNS name. Mirrors the certificate's placeholder SAN.
const PLACEHOLDER_SERVER_NAME: &str = "scribe";

/// Read-only view of the trusted-device pin set the pinning verifier consults
/// during a handshake to classify a peer as already-known vs. first-seen.
///
/// Implemented by the LAN trusted-device store (task T008); kept as a trait so
/// this TLS layer stays independent of the concrete store. [`is_pinned`] runs
/// synchronously inside the TLS handshake, so implementations MUST NOT block on
/// async work or acquire a lock the caller already holds across the handshake.
///
/// [`is_pinned`]: DevicePins::is_pinned
pub trait DevicePins: Send + Sync {
    /// Returns `true` when `device_id` matches an approved, currently-pinned
    /// trusted device.
    fn is_pinned(&self, device_id: &DeviceId) -> bool;
}

/// How the pinning verifier classified a peer during the handshake. There is no
/// "identity changed" variant: trust is keyed by Device ID, so a re-keyed peer
/// is simply [`Pending`](PinDecision::Pending).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinDecision {
    /// The presented Device ID matches an already-pinned trusted device.
    Known,
    /// The presented Device ID is not pinned. The handshake still completes
    /// (trust-on-first-use); the app layer must gate this peer behind explicit
    /// owning-side approval before any window/session data flows.
    Pending,
}

/// The peer identity observed during a completed mutual-TLS handshake, surfaced
/// to the caller so the app layer can gate approval and persist the pin.
#[derive(Clone, Debug)]
pub struct PeerIdentity {
    /// The peer's pinned Device ID, derived from its presented certificate.
    pub device_id: DeviceId,
    /// The peer's full presented certificate (public identity) for re-verify,
    /// display, and persistence on approval.
    pub cert_der: CertificateDer<'static>,
    /// The verifier's known/pending classification of this peer.
    pub decision: PinDecision,
}

/// Failure to derive a Device ID from a presented certificate.
#[derive(Debug, thiserror::Error)]
#[error("could not parse the peer certificate to derive its LAN device id")]
pub struct CertParseError;

/// Errors establishing a LAN mutual-TLS connection.
#[derive(Debug, thiserror::Error)]
pub enum LanTlsError {
    /// Building the `rustls` server/client configuration failed (e.g. the
    /// device key could not be loaded by the crypto provider). Boxed to keep the
    /// enum small — `rustls::Error` is large.
    #[error("LAN TLS configuration failed: {0}")]
    Config(#[source] Box<TlsError>),
    /// The TLS handshake itself failed — connection reset, an untrusted or
    /// unparseable peer certificate, or a bad handshake signature.
    #[error("LAN TLS handshake failed: {0}")]
    Handshake(#[source] std::io::Error),
    /// The fixed placeholder server name was rejected by `rustls` (should never
    /// happen for the compile-time-constant [`PLACEHOLDER_SERVER_NAME`]).
    #[error("the placeholder LAN server name was rejected by rustls")]
    ServerName(#[source] InvalidDnsNameError),
    /// The handshake completed without the verifier recording a peer identity.
    /// Unreachable under mandatory mutual auth; treated as a hard failure.
    #[error("mutual TLS completed but no peer certificate was recorded")]
    MissingPeerCert,
}

impl LanTlsError {
    /// Box a `rustls` configuration error into [`LanTlsError::Config`], keeping
    /// the error enum small (`rustls::Error` is large).
    fn config(error: TlsError) -> Self {
        Self::Config(Box::new(error))
    }
}

/// Derive a peer's Device ID from its presented certificate:
/// `SHA-256`(`SubjectPublicKeyInfo`). Byte-for-byte identical to
/// [`DeviceIdentity::device_id`], which hashes the same SPKI DER produced by
/// `rcgen`, so a pin written from one side verifies from the other.
pub fn device_id_from_cert(cert: &CertificateDer<'_>) -> Result<DeviceId, CertParseError> {
    let (_, parsed) = parse_x509_certificate(cert.as_ref()).map_err(|_| CertParseError)?;
    Ok(Sha256::digest(parsed.public_key().raw).into())
}

/// The custom pinning verifier installed on both TLS roles. It enforces valid
/// certificate encoding and (via the trait defaults' companion signature
/// methods) proof of key possession, records the observed peer, and accepts
/// first-seen peers so the app layer can gate approval (see the module docs).
struct PinningVerifier {
    /// The trusted-device set consulted to classify known vs. pending.
    pins: Arc<dyn DevicePins>,
    /// The crypto provider's signature-verification algorithms, used both to
    /// delegate the handshake-signature check and to advertise supported
    /// schemes. `Copy`, so cheap to hold per connection.
    algs: WebPkiSupportedAlgorithms,
    /// Per-connection slot the verifier records the observed peer into; read by
    /// [`LanTls::accept`] / [`LanTls::connect`] once the handshake completes.
    observed: Arc<Mutex<Option<PeerIdentity>>>,
}

impl PinningVerifier {
    fn new(
        pins: Arc<dyn DevicePins>,
        algs: WebPkiSupportedAlgorithms,
        observed: Arc<Mutex<Option<PeerIdentity>>>,
    ) -> Self {
        Self { pins, algs, observed }
    }

    /// Derive the peer's Device ID, classify it against the pin set, and record
    /// the observed identity. Fails only when the certificate cannot be parsed;
    /// each trait method maps that to a [`CertificateError::BadEncoding`]
    /// handshake failure. Any well-formed certificate is accepted
    /// (trust-on-first-use) with its known/pending classification.
    fn record_peer(&self, end_entity: &CertificateDer<'_>) -> Result<(), CertParseError> {
        let device_id = device_id_from_cert(end_entity)?;
        let decision =
            if self.pins.is_pinned(&device_id) { PinDecision::Known } else { PinDecision::Pending };
        let peer = PeerIdentity { device_id, cert_der: end_entity.clone().into_owned(), decision };
        if let Ok(mut slot) = self.observed.lock() {
            *slot = Some(peer);
        }
        Ok(())
    }
}

impl fmt::Debug for PinningVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render the pin set or key material; the trait only needs Debug.
        f.debug_struct("PinningVerifier").finish_non_exhaustive()
    }
}

/// Dialer side: verify the listener's certificate. Names and CA chains are
/// ignored; identity is the pinned Device ID and the delegated signature check.
impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        self.record_peer(end_entity)
            .map_err(|_| TlsError::InvalidCertificate(CertificateError::BadEncoding))?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        crypto_verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        crypto_verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Listener side: require and verify the dialer's certificate. Mutual auth is
/// mandatory — a peer that presents no certificate is rejected by `rustls`
/// before this verifier records anything.
impl ClientCertVerifier for PinningVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // No CA hints: the client is authenticated by its pinned key, not by a
        // trust anchor the server advertises.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        self.record_peer(end_entity)
            .map_err(|_| TlsError::InvalidCertificate(CertificateError::BadEncoding))?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        crypto_verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        crypto_verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Builds LAN mutual-TLS connections that present this machine's device
/// certificate and pin peers by Device ID. Cheap to clone/share (all fields are
/// `Arc` or `Copy`); one instance serves every LAN connection.
pub struct LanTls {
    identity: Arc<DeviceIdentity>,
    pins: Arc<dyn DevicePins>,
    provider: Arc<CryptoProvider>,
    algs: WebPkiSupportedAlgorithms,
}

impl LanTls {
    /// Create the TLS builder from this machine's device identity and its
    /// trusted-device pin set, using the aws-lc-rs crypto provider (shared with
    /// `rcgen` and `tokio-rustls`). The provider is passed explicitly to every
    /// config, so no process-wide default provider need be installed.
    #[must_use]
    pub fn new(identity: Arc<DeviceIdentity>, pins: Arc<dyn DevicePins>) -> Self {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let algs = provider.signature_verification_algorithms;
        Self { identity, pins, provider, algs }
    }

    /// Accept a mutual-TLS handshake on an already-accepted TCP stream (listener
    /// role). On success returns the encrypted stream and the peer identity the
    /// pinning verifier observed, for the app-layer approval gate.
    pub async fn accept<IO>(
        &self,
        stream: IO,
    ) -> Result<(tokio_rustls::server::TlsStream<IO>, PeerIdentity), LanTlsError>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        let observed: Arc<Mutex<Option<PeerIdentity>>> = Arc::new(Mutex::new(None));
        let verifier = self.build_verifier(Arc::clone(&observed));
        let acceptor = TlsAcceptor::from(Arc::new(self.server_config(verifier)?));
        let stream = acceptor.accept(stream).await.map_err(LanTlsError::Handshake)?;
        let peer = take_peer(&observed)?;
        Ok((stream, peer))
    }

    /// Perform a mutual-TLS handshake as the dialer on a connected TCP stream.
    /// On success returns the encrypted stream and the peer identity the pinning
    /// verifier observed, for the app-layer approval gate.
    pub async fn connect<IO>(
        &self,
        stream: IO,
    ) -> Result<(tokio_rustls::client::TlsStream<IO>, PeerIdentity), LanTlsError>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        let server_name =
            ServerName::try_from(PLACEHOLDER_SERVER_NAME).map_err(LanTlsError::ServerName)?;
        let observed: Arc<Mutex<Option<PeerIdentity>>> = Arc::new(Mutex::new(None));
        let verifier = self.build_verifier(Arc::clone(&observed));
        let connector = TlsConnector::from(Arc::new(self.client_config(verifier)?));
        let stream =
            connector.connect(server_name, stream).await.map_err(LanTlsError::Handshake)?;
        let peer = take_peer(&observed)?;
        Ok((stream, peer))
    }

    /// Build a fresh pinning verifier bound to a per-connection recording slot.
    fn build_verifier(&self, observed: Arc<Mutex<Option<PeerIdentity>>>) -> Arc<PinningVerifier> {
        Arc::new(PinningVerifier::new(Arc::clone(&self.pins), self.algs, observed))
    }

    /// Listener config: present this device's certificate, require + pin the
    /// client certificate, TLS 1.3 only (research D3).
    fn server_config(&self, verifier: Arc<PinningVerifier>) -> Result<ServerConfig, LanTlsError> {
        ServerConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(LanTlsError::config)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(self.identity.cert_chain(), self.identity.signing_key())
            .map_err(LanTlsError::config)
    }

    /// Dialer config: present this device's certificate for client auth and pin
    /// the server certificate via the custom verifier, TLS 1.3 only (research
    /// D3).
    fn client_config(&self, verifier: Arc<PinningVerifier>) -> Result<ClientConfig, LanTlsError> {
        ClientConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(LanTlsError::config)?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(self.identity.cert_chain(), self.identity.signing_key())
            .map_err(LanTlsError::config)
    }
}

/// Take the peer identity the verifier recorded during the handshake. A missing
/// record (poisoned lock or, impossibly, no verification) is a hard failure.
fn take_peer(slot: &Mutex<Option<PeerIdentity>>) -> Result<PeerIdentity, LanTlsError> {
    slot.lock().ok().and_then(|mut guard| guard.take()).ok_or(LanTlsError::MissingPeerCert)
}
