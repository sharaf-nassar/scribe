//! AEAD seal/open for the on-disk env-delta envelope. ChaCha20-Poly1305 with
//! a per-write random nonce; data-encryption key is stored in the OS
//! keystore (see `super::keystore`). On-disk binary layout:
//!
//! ```text
//! offset 0:   version: u8 = 1
//! offset 1:   reserved: [u8; 7] = [0; 7]
//! offset 8:   nonce: [u8; 12]
//! offset 20:  ciphertext: Vec<u8>  (rmp_serde::to_vec_named of TerminalEnvDelta)
//! tail:       tag: [u8; 16]        (Poly1305 — appended by AEAD seal in-place)
//! ```

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};

use super::delta::TerminalEnvDelta;
use super::keystore::Dek;

/// Current on-disk envelope format version. Bump if the header layout or
/// AEAD primitive changes.
pub const ENVELOPE_VERSION: u8 = 1;

/// Header size in bytes (version + reserved + nonce). The ciphertext +
/// 16-byte AEAD tag follow.
pub const HEADER_LEN: usize = 1 + 7 + 12;

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("envelope shorter than minimum header + tag")]
    Truncated,
    #[error("unsupported envelope version: {0}")]
    UnsupportedVersion(u8),
    #[error("aead seal/open failed")]
    Aead,
    #[error("msgpack encode failed: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode failed: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

/// Seal a `TerminalEnvDelta` into the on-disk envelope binary format.
/// Returns the full envelope bytes ready to write to disk.
pub fn seal(delta: &TerminalEnvDelta, dek: &Dek) -> Result<Vec<u8>, EnvelopeError> {
    let plaintext = rmp_serde::to_vec_named(delta)?;
    let key = Key::from_slice(dek.as_slice());
    let cipher = ChaCha20Poly1305::new(key);
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher.encrypt(&nonce, plaintext.as_ref()).map_err(|_| EnvelopeError::Aead)?;

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.push(ENVELOPE_VERSION);
    out.extend_from_slice(&[0u8; 7]);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Open an on-disk envelope back into a `TerminalEnvDelta`. Verifies the
/// version byte, reads the nonce, AEAD-opens the ciphertext (which authenticates
/// via the appended Poly1305 tag), and `rmp_serde`-deserializes the plaintext.
pub fn open(envelope: &[u8], dek: &Dek) -> Result<TerminalEnvDelta, EnvelopeError> {
    // 16 bytes for the AEAD tag are part of the ciphertext slice we pass to
    // `decrypt`, so the minimum total length is HEADER_LEN + 16 (a degenerate
    // empty plaintext).
    if envelope.len() < HEADER_LEN + 16 {
        return Err(EnvelopeError::Truncated);
    }
    let Some(&version) = envelope.first() else {
        return Err(EnvelopeError::Truncated);
    };
    if version != ENVELOPE_VERSION {
        return Err(EnvelopeError::UnsupportedVersion(version));
    }
    let nonce_bytes = envelope.get(8..20).ok_or(EnvelopeError::Truncated)?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let ciphertext = envelope.get(HEADER_LEN..).ok_or(EnvelopeError::Truncated)?;

    let key = Key::from_slice(dek.as_slice());
    let cipher = ChaCha20Poly1305::new(key);
    let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|_| EnvelopeError::Aead)?;

    let delta: TerminalEnvDelta = rmp_serde::from_slice(&plaintext)?;
    Ok(delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn sample_dek() -> Dek {
        // Fixed bytes — never use in production.
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap_or(u8::MAX);
        }
        k
    }

    fn sample_delta() -> TerminalEnvDelta {
        let mut added = BTreeMap::new();
        added.insert("FOO".to_owned(), "bar".to_owned());
        added.insert("API_KEY".to_owned(), "sk-test-1234".to_owned());
        let mut removed = BTreeSet::new();
        removed.insert("STALE".to_owned());
        TerminalEnvDelta { added, removed }
    }

    #[test]
    fn round_trip_preserves_delta() {
        let dek = sample_dek();
        let original = sample_delta();
        let envelope = seal(&original, &dek).expect("seal");
        assert!(envelope.len() >= HEADER_LEN + 16);
        assert_eq!(envelope[0], ENVELOPE_VERSION);
        assert_eq!(&envelope[1..8], &[0u8; 7]);
        let recovered = open(&envelope, &dek).expect("open");
        assert_eq!(recovered, original);
    }

    #[test]
    fn rejects_unsupported_version() {
        let dek = sample_dek();
        let mut envelope = seal(&sample_delta(), &dek).expect("seal");
        envelope[0] = 99;
        let err = open(&envelope, &dek).unwrap_err();
        assert!(matches!(err, EnvelopeError::UnsupportedVersion(99)));
    }

    #[test]
    fn rejects_truncated_envelope() {
        let dek = sample_dek();
        let envelope = seal(&sample_delta(), &dek).expect("seal");
        let truncated = &envelope[..HEADER_LEN + 10]; // not enough for tag
        let err = open(truncated, &dek).unwrap_err();
        assert!(matches!(err, EnvelopeError::Truncated));
    }

    #[test]
    fn detects_ciphertext_tamper() {
        let dek = sample_dek();
        let mut envelope = seal(&sample_delta(), &dek).expect("seal");
        // Flip a byte in the ciphertext — Poly1305 must reject.
        let last = envelope.len() - 1;
        envelope[last] ^= 0xff;
        let err = open(&envelope, &dek).unwrap_err();
        assert!(matches!(err, EnvelopeError::Aead));
    }

    #[test]
    fn rejects_wrong_key() {
        let mut wrong_dek = sample_dek();
        wrong_dek[0] ^= 1;
        let envelope = seal(&sample_delta(), &sample_dek()).expect("seal");
        let err = open(&envelope, &wrong_dek).unwrap_err();
        assert!(matches!(err, EnvelopeError::Aead));
    }
}
