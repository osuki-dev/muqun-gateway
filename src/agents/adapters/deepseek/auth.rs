use base64::prelude::*;
use hmac::{Hmac, KeyInit as _, Mac};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// Generate an authority-bound signed session cookie for DeepSeek Harness / dsh-client-connection.
///
/// Cookie name: `dsh-auth-` + base64url(sha256(authority))
/// Payload: `{ "version": 1, "authority": authority, "issuedAt": now, "expiresAt": now + 30d }`
/// Cookie value: `v1.` + base64url(payload) + `.` + base64url(hmac_sha256(secret_bytes, body))
pub fn generate_dsh_cookie(
    authority: &str,
    secret_b64url: &str,
) -> Result<(String, String), String> {
    let secret_bytes = BASE64_URL_SAFE_NO_PAD
        .decode(secret_b64url.trim())
        .map_err(|e| format!("Invalid base64url secret: {e}"))?;

    let mut hasher = Sha256::new();
    hasher.update(authority.as_bytes());
    let authority_hash = hasher.finalize();
    let cookie_name = format!("dsh-auth-{}", BASE64_URL_SAFE_NO_PAD.encode(authority_hash));

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as u64;
    let expires_ms = now_ms + 30 * 86400 * 1000;

    let payload = serde_json::json!({
        "version": 1,
        "authority": authority,
        "issuedAt": now_ms,
        "expiresAt": expires_ms
    });

    let payload_bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    let body = BASE64_URL_SAFE_NO_PAD.encode(&payload_bytes);

    let mut mac = HmacSha256::new_from_slice(&secret_bytes)
        .map_err(|e| format!("HMAC key init failed: {e}"))?;
    mac.update(body.as_bytes());
    let sig = BASE64_URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());

    let cookie_value = format!("v1.{body}.{sig}");
    Ok((cookie_name, cookie_value))
}

/// Attempt to read the browser-session signing secret from `$DSH_HOME/.credentials.yaml`
/// or `$HOME/.dsh/.credentials.yaml`.
pub fn load_local_secret() -> Option<String> {
    let home = std::env::var("DSH_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.dsh")))
        .ok()?;
    let path = std::path::Path::new(&home).join(".credentials.yaml");
    let content = std::fs::read_to_string(path).ok()?;

    let mut in_browser_session = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("client-connection/browser-session:") {
            in_browser_session = true;
            continue;
        }
        if in_browser_session {
            if !line.starts_with(' ') && !line.starts_with('\t') {
                break;
            }
            if trimmed.starts_with("secret:") {
                let parts: Vec<&str> = trimmed.splitn(2, ':').collect();
                if parts.len() == 2 {
                    let secret = parts[1].trim();
                    if !secret.is_empty() {
                        return Some(secret.to_string());
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_cookie_structure() {
        let secret = "Qz1iQUfg3Hve5G6HgLLfie-xeSvi016I1X24SBL-WC8";
        let authority = "127.0.0.1:3080";
        let (name, value) = generate_dsh_cookie(authority, secret).unwrap();

        assert_eq!(name, "dsh-auth-VPhEEcLKeqRDBoBalzN2Nm7CnfxKhLE00pKIDWxt1sw");
        assert!(value.starts_with("v1."));

        let parts: Vec<&str> = value.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "v1");

        let payload_bytes = BASE64_URL_SAFE_NO_PAD.decode(parts[1]).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).unwrap();
        assert_eq!(payload["version"], 1);
        assert_eq!(payload["authority"], "127.0.0.1:3080");
        assert!(payload["issuedAt"].as_u64().unwrap() > 0);
        assert!(payload["expiresAt"].as_u64().unwrap() > payload["issuedAt"].as_u64().unwrap());
    }
}
