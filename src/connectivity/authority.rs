//! Pairing and credential rules independent of HTTP and file persistence.
//!
//! The inbound adapter extracts a bearer token and maps failures to HTTP. This
//! module decides what that credential represents, consumes one-time pairing
//! codes, and maintains the bounded paired-device set.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const PAIRING_CODE_LENGTH: usize = 9;
const PAIRING_CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPairing {
    pub request_id: String,
    pub device_name: String,
    #[serde(default)]
    pub install_id: Option<String>,
    pub code: String,
    pub code_hash: String,
    pub created_unix_ms: u128,
    #[serde(default)]
    pub failed_attempts: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingCodeError {
    Missing,
    Expired,
    Invalid,
}

/// One paired device. The raw bearer token is returned only once during claim;
/// persistent state contains this record and therefore only its hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    /// Independent AEAD key. A leaked bearer token alone cannot construct an
    /// encrypted request for this device.
    #[serde(default)]
    pub transport_key: Option<String>,
    pub paired_unix_ms: u128,
    #[serde(default)]
    pub last_seen_unix_ms: u128,
    #[serde(default)]
    pub install_id: Option<String>,
}

pub fn hash_token(token: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// Match every record without short-circuiting. This keeps the matching
/// record's position from becoming an observable timing signal.
pub fn identify_device(devices: &[DeviceRecord], token: &str) -> Option<String> {
    if token.len() > 256 {
        return None;
    }
    let presented = hash_token(token);
    let mut matched = None;
    for device in devices {
        if constant_time_eq(presented.as_bytes(), device.token_hash.as_bytes()) {
            matched = Some(device.id.clone());
        }
    }
    matched
}

pub fn authenticates_admin(admin_token_hash: &str, token: &str) -> bool {
    token.len() <= 256
        && constant_time_eq(hash_token(token).as_bytes(), admin_token_hash.as_bytes())
}

/// Update activity and report whether persistence is due.
pub fn touch_device(
    devices: &mut [DeviceRecord],
    device_id: &str,
    now_unix_ms: u128,
    flush_after_ms: u128,
) -> bool {
    let Some(device) = devices.iter_mut().find(|device| device.id == device_id) else {
        return false;
    };
    let stale = now_unix_ms.saturating_sub(device.last_seen_unix_ms) >= flush_after_ms;
    device.last_seen_unix_ms = now_unix_ms;
    stale
}

/// Replace an existing install, retain insertion order, and enforce the device
/// ceiling. Persistence remains the caller's responsibility.
pub fn enroll_device(devices: &mut Vec<DeviceRecord>, record: DeviceRecord, maximum: usize) {
    if let Some(install_id) = record.install_id.as_deref() {
        devices.retain(|device| device.install_id.as_deref() != Some(install_id));
    }
    devices.push(record);
    devices.sort_by_key(|item| item.paired_unix_ms);
    if devices.len() > maximum {
        let excess = devices.len() - maximum;
        devices.drain(..excess);
    }
}

pub fn consume_pairing_code(
    pending: &mut Option<PendingPairing>,
    request_id: &str,
    code: &str,
    now_unix_ms: u128,
    ttl_ms: u128,
    maximum_attempts: u8,
) -> Result<(), PairingCodeError> {
    let Some(current) = pending.as_mut() else {
        return Err(PairingCodeError::Missing);
    };
    if pairing_code_expired(current, now_unix_ms, ttl_ms) {
        *pending = None;
        return Err(PairingCodeError::Expired);
    }
    let request_matches = constant_time_eq(request_id.as_bytes(), current.request_id.as_bytes());
    let valid = valid_pairing_code(code)
        && request_matches
        && constant_time_eq(hash_token(code).as_bytes(), current.code_hash.as_bytes());
    if !valid {
        current.failed_attempts = current.failed_attempts.saturating_add(1);
        if current.failed_attempts >= maximum_attempts {
            *pending = None;
        }
        return Err(PairingCodeError::Invalid);
    }
    *pending = None;
    Ok(())
}

pub fn pairing_code_expired(pending: &PendingPairing, now_unix_ms: u128, ttl_ms: u128) -> bool {
    now_unix_ms.saturating_sub(pending.created_unix_ms) >= ttl_ms
}

pub fn valid_pairing_code(value: &str) -> bool {
    value.len() == PAIRING_CODE_LENGTH
        && value.as_bytes()[4] == b'-'
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 4 || PAIRING_CODE_ALPHABET.contains(&byte))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (left, right) in left.iter().zip(right) {
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    fn device(id: &str, token: &str, install_id: Option<&str>) -> DeviceRecord {
        DeviceRecord {
            id: id.into(),
            name: id.into(),
            token_hash: hash_token(token),
            transport_key: None,
            paired_unix_ms: 1,
            last_seen_unix_ms: 1,
            install_id: install_id.map(str::to_owned),
        }
    }

    #[test]
    fn admin_and_device_credentials_are_distinct_authorities() {
        let devices = vec![device("phone", "device-token", None)];
        assert_eq!(
            identify_device(&devices, "device-token"),
            Some("phone".into())
        );
        assert_eq!(identify_device(&devices, "admin-token"), None);
        assert!(authenticates_admin(
            &hash_token("admin-token"),
            "admin-token"
        ));
        assert!(!authenticates_admin(
            &hash_token("admin-token"),
            "device-token"
        ));
    }

    #[test]
    fn enrolling_the_same_install_replaces_its_old_credential() {
        let mut devices = vec![device("old", "old-token", Some("install"))];
        enroll_device(
            &mut devices,
            device("new", "new-token", Some("install")),
            16,
        );
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "new");
        assert_eq!(identify_device(&devices, "old-token"), None);
    }
    #[test]
    fn token_hash_is_stable_and_not_plaintext() {
        let hash = hash_token("secret");
        assert_eq!(hash, hash_token("secret"));
        assert_ne!(hash, "secret");
    }

    #[test]
    fn pairing_code_uses_unambiguous_characters() {
        let code = generate_pairing_code();
        assert_eq!(code.len(), PAIRING_CODE_LENGTH);
        assert_eq!(code.as_bytes()[4], b'-');
        assert!(valid_pairing_code(&code));
        assert!(!valid_pairing_code("ABCD2345"));
        assert!(!valid_pairing_code("abcD-2345"));
        assert!(!code.contains('0'));
        assert!(!code.contains('O'));
        assert!(!code.contains('1'));
        assert!(!code.contains('I'));
        assert!(!code.contains('L'));
    }

    /// What the code is worth is what every position can hold and how evenly it
    /// holds it. Both halves are asserted, because both were wrong: one
    /// position could only reach sixteen of the thirty-one glyphs because it
    /// was reading a UUID's version nibble, and every position leaned on the
    /// first eight because a byte was folded with `%`.
    ///
    /// The bands are wide on purpose. Twenty thousand draws puts about 645 of
    /// each glyph in each position and about 5161 overall; a tenth of that is
    /// seven standard deviations, so the old bias (a ninth over, five of them)
    /// fails and a fair generator does not flake.
    #[test]
    fn every_glyph_can_land_in_every_position_and_none_is_favoured() {
        const DRAWS: usize = 20_000;
        let mut counts =
            vec![vec![0_usize; super::PAIRING_CODE_ALPHABET.len()]; PAIRING_CODE_CHARACTER_COUNT];
        for _ in 0..DRAWS {
            let code = generate_pairing_code();
            assert!(valid_pairing_code(&code), "{code} is not a pairing code");
            let glyphs: Vec<u8> = code.bytes().filter(|byte| *byte != b'-').collect();
            for (position, glyph) in glyphs.iter().enumerate() {
                let index = super::PAIRING_CODE_ALPHABET
                    .iter()
                    .position(|candidate| candidate == glyph)
                    .expect("a code is drawn from the alphabet");
                counts[position][index] += 1;
            }
        }

        for (position, row) in counts.iter().enumerate() {
            for (index, count) in row.iter().enumerate() {
                assert!(
                    *count > 0,
                    "position {position} never produced {}",
                    super::PAIRING_CODE_ALPHABET[index] as char
                );
            }
        }

        let total = DRAWS * PAIRING_CODE_CHARACTER_COUNT;
        let expected = total / super::PAIRING_CODE_ALPHABET.len();
        for index in 0..super::PAIRING_CODE_ALPHABET.len() {
            let seen: usize = counts.iter().map(|row| row[index]).sum();
            assert!(
                seen * 10 > expected * 9 && seen * 10 < expected * 11,
                "{} came up {seen} times against {expected} expected",
                super::PAIRING_CODE_ALPHABET[index] as char
            );
        }
    }

    #[test]
    fn request_id_validation_is_restrictive() {
        assert!(valid_request_id("iphone-15.req_1"));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id("has space"));
        assert!(!valid_request_id(&"x".repeat(81)));
    }

    #[test]
    fn pairing_code_is_consumed_after_one_successful_claim() {
        let mut pending = test_pending_pairing(1_000);
        assert_eq!(
            consume_test_pairing_code(&mut pending, "request-1", "2345-6789", 1_001),
            Ok(())
        );
        assert!(pending.is_none());
        assert_eq!(
            consume_test_pairing_code(&mut pending, "request-1", "2345-6789", 1_002),
            Err(PairingCodeError::Missing)
        );
    }

    #[test]
    fn expired_pairing_code_is_rejected_and_cleared() {
        let mut pending = test_pending_pairing(1_000);
        assert_eq!(
            consume_test_pairing_code(
                &mut pending,
                "request-1",
                "2345-6789",
                1_000 + PAIRING_CODE_TTL_MS
            ),
            Err(PairingCodeError::Expired)
        );
        assert!(pending.is_none());
    }

    #[test]
    fn repeated_invalid_pairing_attempts_invalidate_code() {
        let mut pending = test_pending_pairing(1_000);
        for _ in 0..MAX_PAIRING_CODE_ATTEMPTS {
            assert_eq!(
                consume_test_pairing_code(&mut pending, "request-1", "AAAA-AAAA", 1_001),
                Err(PairingCodeError::Invalid)
            );
        }
        assert!(pending.is_none());
    }
}
