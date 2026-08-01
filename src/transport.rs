//! Versioned application-layer encryption for untrusted HTTP transports.
//!
//! HTTP still exposes routing and traffic-shape metadata. This module protects
//! credentials and payload bytes with independent request/response keys.

use aes_gcm::aead::{Aead as _, AeadCore as _, OsRng, Payload};
use aes_gcm::{Aes256Gcm, KeyInit as _, Nonce};
use anyhow::{anyhow, Context as _};
use base64::Engine as _;
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

pub const VERSION: u8 = 1;
pub const MAX_CLOCK_SKEW_MS: u128 = 5 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u8,
    pub timestamp_ms: u128,
    pub nonce: String,
    pub ciphertext: String,
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Request,
    Response,
    PairingRequest,
    PairingResponse,
}

impl Direction {
    fn info(self) -> &'static [u8] {
        match self {
            Self::Request => b"muqun-transport-v1/request",
            Self::Response => b"muqun-transport-v1/response",
            Self::PairingRequest => b"muqun-transport-v1/pairing-request",
            Self::PairingResponse => b"muqun-transport-v1/pairing-response",
        }
    }
}

pub fn decode_key(value: &str) -> anyhow::Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(value))
        .context("invalid transport key encoding")
}

fn derive_key(material: &[u8], direction: Direction) -> anyhow::Result<[u8; 32]> {
    let hkdf = Hkdf::<Sha256>::new(Some(b"muqun-transport-v1"), material);
    let mut key = [0_u8; 32];
    hkdf.expand(direction.info(), &mut key)
        .map_err(|_| anyhow!("failed to derive transport key"))?;
    Ok(key)
}

pub fn seal(
    material: &[u8],
    direction: Direction,
    aad: &[u8],
    plaintext: &[u8],
    timestamp_ms: u128,
) -> anyhow::Result<Envelope> {
    let key = derive_key(material, direction)?;
    let cipher = Aes256Gcm::new_from_slice(&key).expect("AES-256 key has fixed length");
    let nonce_bytes = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(
            &nonce_bytes,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| anyhow!("transport encryption failed"))?;
    Ok(Envelope {
        version: VERSION,
        timestamp_ms,
        nonce: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce_bytes),
        ciphertext: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(ciphertext),
    })
}

pub fn open(
    material: &[u8],
    direction: Direction,
    aad: &[u8],
    envelope: &Envelope,
    now_ms: u128,
) -> anyhow::Result<Vec<u8>> {
    if envelope.version != VERSION {
        return Err(anyhow!("unsupported transport version"));
    }
    if now_ms.abs_diff(envelope.timestamp_ms) > MAX_CLOCK_SKEW_MS {
        return Err(anyhow!("transport envelope expired"));
    }
    let nonce = decode_key(&envelope.nonce)?;
    if nonce.len() != 12 {
        return Err(anyhow!("invalid transport nonce"));
    }
    let ciphertext = decode_key(&envelope.ciphertext)?;
    let key = derive_key(material, direction)?;
    let cipher = Aes256Gcm::new_from_slice(&key).expect("AES-256 key has fixed length");
    cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad,
            },
        )
        .map_err(|_| anyhow!("transport authentication failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_binds_direction_and_aad() {
        let key = [7_u8; 32];
        let envelope = seal(
            &key,
            Direction::Request,
            b"POST /api/test",
            b"secret",
            1_000,
        )
        .unwrap();
        assert_eq!(
            open(
                &key,
                Direction::Request,
                b"POST /api/test",
                &envelope,
                1_001
            )
            .unwrap(),
            b"secret"
        );
        assert!(open(
            &key,
            Direction::Response,
            b"POST /api/test",
            &envelope,
            1_001
        )
        .is_err());
        assert!(open(&key, Direction::Request, b"GET /api/test", &envelope, 1_001).is_err());
    }

    #[test]
    fn rejects_expired_and_tampered_envelopes() {
        let key = [9_u8; 32];
        let mut envelope = seal(&key, Direction::Request, b"aad", b"secret", 1_000).unwrap();
        assert!(open(
            &key,
            Direction::Request,
            b"aad",
            &envelope,
            1_000 + MAX_CLOCK_SKEW_MS + 1
        )
        .is_err());
        envelope.ciphertext.push('A');
        assert!(open(&key, Direction::Request, b"aad", &envelope, 1_000).is_err());
    }
}
