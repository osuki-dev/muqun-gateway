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

/// Per-stream key for an encrypted server-sent event stream.
///
/// One whole-response envelope cannot authenticate a stream that never ends,
/// so each event is sealed on its own under a key derived per connection. The
/// info string carries the stream id AND the request envelope's nonce: the
/// nonce binds the stream to the one request that opened it, so a captured
/// stream replayed into a later connection derives a different key and never
/// opens. Must match the app's `sse-record` derivation byte for byte.
pub fn derive_stream_key(
    material: &[u8],
    stream_id: &str,
    request_nonce: &str,
) -> anyhow::Result<[u8; 32]> {
    let hkdf = Hkdf::<Sha256>::new(Some(b"muqun-transport-v1"), material);
    let info = format!("muqun-transport-v1/sse/{stream_id}/{request_nonce}");
    let mut key = [0_u8; 32];
    hkdf.expand(info.as_bytes(), &mut key)
        .map_err(|_| anyhow!("failed to derive stream key"))?;
    Ok(key)
}

/// The nonce for one stream event: the sequence number, big-endian, in the low
/// bytes. Unique under the per-stream key by construction, which is what lets
/// the stream skip per-event random nonces and their collision arithmetic.
fn stream_nonce(seq: u64) -> [u8; 12] {
    let mut nonce = [0_u8; 12];
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    nonce
}

/// Seal one server-sent event for an encrypted stream. Returns the base64url
/// ciphertext (tag appended); `seq` is bound through both the nonce and the
/// caller's AAD, so reordering, splicing and replay all fail authentication.
pub fn seal_stream_event(
    key: &[u8; 32],
    seq: u64,
    aad: &[u8],
    plaintext: &[u8],
) -> anyhow::Result<String> {
    let cipher = Aes256Gcm::new_from_slice(key).expect("AES-256 key has fixed length");
    let nonce = stream_nonce(seq);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| anyhow!("stream encryption failed"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(ciphertext))
}

/// The inverse of `seal_stream_event`, used by tests to prove the wire format
/// a client has to implement.
#[cfg(test)]
pub fn open_stream_event(
    key: &[u8; 32],
    seq: u64,
    aad: &[u8],
    ciphertext: &str,
) -> anyhow::Result<Vec<u8>> {
    let sealed = decode_key(ciphertext)?;
    let cipher = Aes256Gcm::new_from_slice(key).expect("AES-256 key has fixed length");
    let nonce = stream_nonce(seq);
    cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &sealed,
                aad,
            },
        )
        .map_err(|_| anyhow!("stream authentication failed"))
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

    /// The wire fixture the app's `sse-record.test.ts` opens too. If either
    /// side drifts -- salt, info shape, nonce layout, AAD -- this is the test
    /// that names the disagreement instead of a phone that silently reconnects
    /// forever.
    #[test]
    fn stream_event_fixture_matches_the_app_side() {
        let material: Vec<u8> = (1..=32).collect();
        let key = derive_stream_key(&material, "stream-fixture", "req-nonce-fixture").unwrap();
        assert_eq!(
            key,
            [
                0xa5, 0x1b, 0x22, 0x38, 0xd4, 0xdd, 0xd0, 0xf6, 0x66, 0xa9, 0xbe, 0x3b, 0x93,
                0x31, 0x69, 0x1a, 0xb5, 0x42, 0xfd, 0xee, 0xb2, 0x4a, 0x4c, 0xa2, 0xab, 0x7c,
                0x1a, 0xda, 0xb6, 0xdc, 0x7c, 0x63,
            ]
        );
        let aad = b"GET /api/sessions/main/events?types=pane_updated\nstream-fixture\n7";
        let sealed = seal_stream_event(&key, 7, aad, b"{\"event\":\"herdr\",\"data\":\"hello\"}").unwrap();
        assert_eq!(
            sealed,
            "0afg46aA-yj6Uts5xXCCNP5EbDcFekXZzFWOTuUoD8vWBTvoYRZrlSBcnLS4VK9g"
        );
        assert_eq!(
            open_stream_event(&key, 7, aad, &sealed).unwrap(),
            b"{\"event\":\"herdr\",\"data\":\"hello\"}"
        );
    }

    #[test]
    fn stream_events_refuse_reorder_replay_and_foreign_streams() {
        let material = [3_u8; 32];
        let key = derive_stream_key(&material, "sid-a", "nonce-a").unwrap();
        let sealed = seal_stream_event(&key, 0, b"aad\nsid-a\n0", b"first").unwrap();
        assert_eq!(open_stream_event(&key, 0, b"aad\nsid-a\n0", &sealed).unwrap(), b"first");
        // A different seq is a different nonce and a different AAD: both fail.
        assert!(open_stream_event(&key, 1, b"aad\nsid-a\n0", &sealed).is_err());
        assert!(open_stream_event(&key, 0, b"aad\nsid-a\n1", &sealed).is_err());
        // The same event under another connection's key never opens: replaying
        // a captured stream into a new request is the attack this closes.
        let other = derive_stream_key(&material, "sid-a", "nonce-b").unwrap();
        assert!(open_stream_event(&other, 0, b"aad\nsid-a\n0", &sealed).is_err());
        let foreign = derive_stream_key(&material, "sid-b", "nonce-a").unwrap();
        assert!(open_stream_event(&foreign, 0, b"aad\nsid-a\n0", &sealed).is_err());
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
