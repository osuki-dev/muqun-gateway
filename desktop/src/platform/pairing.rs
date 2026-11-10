//! Client-side device pairing for Muqun Desktop connecting to remote gateways.

use anyhow::{anyhow, Context, Result};
use muqun_gateway::connectivity::routes::code_pairing_material;
use muqun_gateway::connectivity::transport::{self, Direction, Envelope};
use serde_json::{json, Value};
use std::time::Duration;

const CLAIM_AAD: &[u8] = b"POST /api/pair/claim";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn now_unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("failed to build http client")
}

#[derive(Debug, Clone)]
pub struct PairingOffer {
    pub request_id: String,
    #[allow(dead_code)]
    pub server_id: String,
    #[allow(dead_code)]
    pub server_label: String,
    #[allow(dead_code)]
    pub expires_in_ms: u64,
    pub transport_required: bool,
}

#[derive(Debug, Clone)]
pub struct ClaimedToken {
    pub token: String,
    #[allow(dead_code)]
    pub server_id: String,
    #[allow(dead_code)]
    pub server_label: String,
    #[allow(dead_code)]
    pub device_id: String,
}

/// Request pairing from a remote gateway (`POST /api/pair/request`).
pub async fn request_app_pairing(url: &str, device_name: &str) -> Result<PairingOffer> {
    let client = http_client()?;
    let request_id = format!("desktop-{}", uuid::Uuid::new_v4());
    let url_trimmed = url.trim_end_matches('/');

    let res = client
        .post(format!("{url_trimmed}/api/pair/request"))
        .json(&json!({
            "request_id": request_id,
            "device_name": device_name,
            "kind": "app"
        }))
        .send()
        .await
        .context("failed to send pairing request")?;

    let status = res.status();
    let body: Value = res
        .json()
        .await
        .context("failed to read pairing request response")?;
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("pairing request refused");
        return Err(anyhow!("{msg} (status: {status})"));
    }

    Ok(PairingOffer {
        request_id: body
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or(&request_id)
            .to_string(),
        server_id: body
            .get("server_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        server_label: body
            .get("server_label")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        expires_in_ms: body
            .get("expires_in_ms")
            .and_then(Value::as_u64)
            .unwrap_or(300_000),
        transport_required: body.get("transport_encryption").and_then(Value::as_str)
            == Some("required"),
    })
}

/// Normalize a pairing code to `XXXX-XXXX` format:
/// - Filters alphanumeric characters and uppercases them
/// - Automatically inserts '-' after the 4th character if omitted or typed
/// - Preserves trailing '-' if typed right after the 4th character
/// - Bounds to at most 9 characters (`XXXX-XXXX`)
pub fn normalize_pairing_code(input: &str) -> String {
    let alphanumeric: Vec<char> = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();

    if alphanumeric.is_empty() {
        return String::new();
    }

    if alphanumeric.len() < 4 {
        alphanumeric.into_iter().collect()
    } else if alphanumeric.len() == 4 {
        let s: String = alphanumeric.into_iter().collect();
        if input.ends_with('-') {
            format!("{s}-")
        } else {
            s
        }
    } else {
        let first_four: String = alphanumeric.iter().take(4).collect();
        let rest: String = alphanumeric.iter().skip(4).take(4).collect();
        format!("{first_four}-{rest}")
    }
}

/// Formats a code into spaced terminal display format: `V D K Q - 7 Q U F`.
pub fn format_spaced_code(code: &str) -> String {
    let clean = normalize_pairing_code(code);
    if clean.is_empty() {
        return String::new();
    }
    clean
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Claim an app token using a pairing code (`POST /api/pair/claim`).
pub async fn claim_app_pairing(
    url: &str,
    request_id: &str,
    code: &str,
    transport_required: bool,
) -> Result<ClaimedToken> {
    let client = http_client()?;
    let url_trimmed = url.trim_end_matches('/');
    let clean_code = normalize_pairing_code(code);

    let claim_body = json!({
        "request_id": request_id,
        "code": clean_code
    });

    let res = client
        .post(format!("{url_trimmed}/api/pair/claim"))
        .json(&claim_body)
        .send()
        .await
        .context("failed to send pairing claim")?;

    let status = res.status();
    let body: Value = res.json().await.context("failed to read claim response")?;
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("pairing claim refused");
        return Err(anyhow!("{msg} (status: {status})"));
    }

    let sealed = body.get("ciphertext").is_some();
    let final_body = if sealed {
        let envelope: Envelope =
            serde_json::from_value(body).context("failed to deserialize response envelope")?;
        let (c, r) = (clean_code.clone(), request_id.to_string());
        let material = tokio::task::spawn_blocking(move || code_pairing_material(&c, &r))
            .await
            .context("task spawn error")?
            .context("failed to derive code key material")?;

        let aad = [CLAIM_AAD, b"\ncode-pairing\n"].concat();
        let plaintext = transport::open(
            &material,
            Direction::PairingResponse,
            &aad,
            &envelope,
            now_unix_ms(),
        )
        .map_err(|e| anyhow!("failed to decrypt claim response: {e:?}"))?;

        serde_json::from_slice(&plaintext).context("failed to deserialize decrypted payload")?
    } else if transport_required {
        return Err(anyhow!(
            "remote gateway required encrypted transport, but returned unsealed response"
        ));
    } else {
        body
    };

    let token = final_body
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing token in claim answer"))?
        .to_string();
    let server_id = final_body
        .get("server_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let server_label = final_body
        .get("label")
        .or_else(|| final_body.get("server_label"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let device_id = final_body
        .get("device_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    Ok(ClaimedToken {
        token,
        server_id,
        server_label,
        device_id,
    })
}

/// Verify that a token works against `{url}/api/meta`.
pub async fn verify_token(url: &str, token: &str) -> Result<bool> {
    let client = http_client()?;
    let url_trimmed = url.trim_end_matches('/');
    let res = client
        .get(format!("{url_trimmed}/api/meta"))
        .bearer_auth(token.trim())
        .send()
        .await
        .context("connection failed")?;
    Ok(res.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_pairing_code() {
        assert_eq!(normalize_pairing_code(""), "");
        assert_eq!(normalize_pairing_code("v"), "V");
        assert_eq!(normalize_pairing_code("vdk"), "VDK");
        assert_eq!(normalize_pairing_code("vdkq"), "VDKQ");
        assert_eq!(normalize_pairing_code("vdkq-"), "VDKQ-");
        assert_eq!(normalize_pairing_code("vdkq7"), "VDKQ-7");
        assert_eq!(normalize_pairing_code("vdkq7quf"), "VDKQ-7QUF");
        assert_eq!(normalize_pairing_code("vdkq-7quf"), "VDKQ-7QUF");
        assert_eq!(normalize_pairing_code("V D K Q - 7 Q U F"), "VDKQ-7QUF");
        assert_eq!(normalize_pairing_code("vdkq7qufextra"), "VDKQ-7QUF");
    }

    #[test]
    fn test_format_spaced_code() {
        assert_eq!(format_spaced_code(""), "");
        assert_eq!(format_spaced_code("vdkq"), "V D K Q");
        assert_eq!(format_spaced_code("vdkq7quf"), "V D K Q - 7 Q U F");
        assert_eq!(format_spaced_code("VDKQ-7QUF"), "V D K Q - 7 Q U F");
    }
}
