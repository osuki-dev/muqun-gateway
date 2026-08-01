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

    fn device(id: &str, token: &str, install_id: Option<&str>) -> DeviceRecord {
        DeviceRecord {
            id: id.into(),
            name: id.into(),
            token_hash: hash_token(token),
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
}
