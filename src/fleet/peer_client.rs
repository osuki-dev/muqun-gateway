//! The owner's other gateway, spoken to exactly as the phone speaks to it:
//! its device API, with the device token it issued this gateway, sealed with
//! the transport key it issued when it runs `transport_encryption: required`.
//! One-shot JSON calls; the task socket comes with Task G2.

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use reqwest::Method;
use serde_json::{json, Value};

use super::peers::Peer;
use crate::connectivity::routes::code_pairing_material;
use crate::platform::server::{
    EncryptedRequestPayload, TRANSPORT_DEVICE_HEADER, TRANSPORT_ENVELOPE_HEADER, TRANSPORT_HEADER,
};
use crate::{now_unix_ms, transport};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CLAIM_AAD: &[u8] = b"POST /api/pair/claim";

/// Why a call to a peer did not answer what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerError {
    /// Nothing (or not a gateway) answered; a short reason for the log.
    Unreachable(String),
    /// The peer no longer knows this gateway's token: the link is gone.
    Unauthorized,
    /// It answered with a refusal.
    Refused { status: u16, code: String },
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(reason) => write!(f, "unreachable: {reason}"),
            Self::Unauthorized => f.write_str("unauthorized"),
            Self::Refused { status, code } => write!(f, "refused {status} {code}"),
        }
    }
}

/// What `/api/pair/claim` hands a newly paired device.
pub(crate) struct Claimed {
    pub server_id: String,
    #[allow(dead_code)] // the name a link defaults to once the reconciler omits one
    pub label: String,
    pub token: String,
    pub device_id: String,
    pub transport_key: Option<String>,
}

/// The most a peer's answer may weigh. A sealed page of 200 task views is
/// well under it; anything larger is not a gateway answering.
const MAX_ANSWER_BYTES: usize = 4 << 20;
/// The longest error code kept from a peer's refusal.
const MAX_CODE_LEN: usize = 64;

/// A gateway never redirects, and the sealed envelope header would follow
/// a redirect anywhere, so none is followed.
fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a plain reqwest client builds")
}

fn unreachable(error: reqwest::Error) -> PeerError {
    PeerError::Unreachable(error.without_url().to_string())
}

fn string(value: &Value) -> Result<String, PeerError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| PeerError::Unreachable("not a gateway".into()))
}

/// The JSON of a response, `null` for an empty one (`204`); at most
/// [`MAX_ANSWER_BYTES`] of it is read.
async fn body_of(mut response: reqwest::Response) -> Result<Value, PeerError> {
    let too_large = || PeerError::Unreachable("answer too large".into());
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ANSWER_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(unreachable)? {
        if bytes.len() + chunk.len() > MAX_ANSWER_BYTES {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|_| PeerError::Unreachable("not a gateway".into()))
}

/// `POST {url}/api/pair/request` as a gateway device: B shows the code its
/// owner then reads off `muqun-gateway manage`. Answers B's label.
pub(crate) async fn request_pairing(
    url: &str,
    request_id: &str,
    device_name: &str,
) -> Result<String, PeerError> {
    let response = http()
        .post(format!("{url}/api/pair/request"))
        .json(&json!({
            "request_id": request_id,
            "device_name": device_name,
            "kind": "gateway",
        }))
        .send()
        .await
        .map_err(unreachable)?;
    let status = response.status();
    let body = body_of(response).await?;
    if !status.is_success() {
        return Err(refused(status.as_u16(), &body));
    }
    string(&body["server_label"])
}

/// `POST {url}/api/pair/claim`. A gateway in `required` mode seals a claim it
/// was sent in the clear with key material from the code itself
/// (`code_pairing_material`), which is how a phone that typed the code
/// opens it too.
pub(crate) async fn claim(url: &str, request_id: &str, code: &str) -> Result<Claimed, PeerError> {
    let response = http()
        .post(format!("{url}/api/pair/claim"))
        .json(&json!({ "request_id": request_id, "code": code }))
        .send()
        .await
        .map_err(unreachable)?;
    let status = response.status();
    let body = body_of(response).await?;
    if !status.is_success() {
        return Err(refused(status.as_u16(), &body));
    }
    let body = if body.get("ciphertext").is_some() {
        let envelope: transport::Envelope = serde_json::from_value(body)
            .map_err(|_| PeerError::Unreachable("not a gateway".into()))?;
        let (code, request_id) = (code.trim().to_ascii_uppercase(), request_id.to_owned());
        let material =
            tokio::task::spawn_blocking(move || code_pairing_material(&code, &request_id))
                .await
                .map_err(|_| PeerError::Unreachable("claim".into()))?
                .map_err(|_| PeerError::Unreachable("claim".into()))?;
        let aad = [CLAIM_AAD, b"\ncode-pairing\n"].concat();
        let plaintext = transport::open(
            &material,
            transport::Direction::PairingResponse,
            &aad,
            &envelope,
            now_unix_ms(),
        )
        .map_err(|_| PeerError::Unreachable("claim answer did not open".into()))?;
        serde_json::from_slice(&plaintext).map_err(|_| PeerError::Unreachable("claim".into()))?
    } else {
        body
    };
    Ok(Claimed {
        server_id: string(&body["server_id"])?,
        label: string(&body["label"]).unwrap_or_default(),
        token: string(&body["token"])?,
        device_id: string(&body["device_id"])?,
        transport_key: body["transport_key"].as_str().map(str::to_owned),
    })
}

/// One linked peer's device API.
pub(crate) struct PeerClient {
    http: reqwest::Client,
    base: String,
    token: String,
    device_id: String,
    /// The decoded transport key; `None` speaks plain bearer.
    material: Option<Vec<u8>>,
}

impl PeerClient {
    pub(crate) fn for_peer(peer: &Peer) -> anyhow::Result<Self> {
        Ok(Self {
            http: http(),
            base: peer.url.trim_end_matches('/').to_owned(),
            token: peer.token.clone(),
            device_id: peer.device_id.clone(),
            material: peer
                .transport_key
                .as_deref()
                .map(transport::decode_key)
                .transpose()?,
        })
    }

    /// `method path` with `body`, sealed when the peer issued a transport
    /// key. A 2xx answers its JSON (`null` when empty).
    pub(crate) async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, PeerError> {
        if let Some(material) = &self.material {
            return self.sealed(material, method, path, body).await;
        }
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(unreachable)?;
        let status = response.status().as_u16();
        let body = body_of(response).await?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(refused(status, &body))
        }
    }

    async fn sealed(
        &self,
        material: &[u8],
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, PeerError> {
        let aad = format!("{method} {path}");
        let payload = EncryptedRequestPayload {
            token: self.token.clone(),
            content_type: body.map(|_| "application/json".to_owned()),
            body: URL_SAFE_NO_PAD.encode(body.map(Value::to_string).unwrap_or_default()),
        };
        let envelope = transport::seal(
            material,
            transport::Direction::Request,
            aad.as_bytes(),
            &serde_json::to_vec(&payload).expect("payload serializes"),
            now_unix_ms(),
        )
        .map_err(|_| PeerError::Unreachable("seal".into()))?;
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .header(TRANSPORT_HEADER, "1")
            .header(TRANSPORT_DEVICE_HEADER, &self.device_id);
        // A GET carries its envelope in a header, as the app's sealed GETs do.
        request = if method == Method::GET {
            request.header(
                TRANSPORT_ENVELOPE_HEADER,
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&envelope).expect("envelope")),
            )
        } else {
            request.json(&envelope)
        };
        let response = request.send().await.map_err(unreachable)?;
        let outer_status = response.status().as_u16();
        let outer = body_of(response).await?;
        let Ok(sealed) = serde_json::from_value::<transport::Envelope>(outer.clone()) else {
            // A refusal before the device was known (401/403) comes back unsealed.
            return Err(refused(outer_status, &outer));
        };
        let plaintext = transport::open(
            material,
            transport::Direction::Response,
            format!("{aad}\n{}", envelope.nonce).as_bytes(),
            &sealed,
            now_unix_ms(),
        )
        .map_err(|_| PeerError::Unreachable("answer did not open".into()))?;
        let answer: Value = serde_json::from_slice(&plaintext)
            .map_err(|_| PeerError::Unreachable("answer".into()))?;
        // No `x-muqun-envelope-accept` is sent, so a compressed body is not ours.
        if answer
            .get("content_encoding")
            .is_some_and(|value| !value.is_null())
        {
            return Err(PeerError::Unreachable("compressed".into()));
        }
        let status = answer["status"].as_u64().unwrap_or(500) as u16;
        let bytes = URL_SAFE_NO_PAD
            .decode(answer["body"].as_str().unwrap_or_default())
            .unwrap_or_default();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(refused(status, &body))
        }
    }

    /// An authenticated `GET /api/tasks?limit=1`: the link still works.
    pub(crate) async fn ping(&self) -> Result<(), PeerError> {
        self.json(Method::GET, "/api/tasks?limit=1", None)
            .await
            .map(|_| ())
    }

    /// The public `GET /api/capabilities` list.
    pub(crate) async fn capabilities(&self) -> Result<Vec<String>, PeerError> {
        let response = self
            .http
            .get(format!("{}/api/capabilities", self.base))
            .send()
            .await
            .map_err(unreachable)?;
        let status = response.status().as_u16();
        let body = body_of(response).await?;
        if !(200..300).contains(&status) {
            return Err(refused(status, &body));
        }
        Ok(body["capabilities"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// 401, or 403 for a token or proof this peer no longer knows, is the link
/// being gone; any other refusal names its code, kept only when it looks
/// like one (`[a-z0-9_]`, at most 64) since whatever answered at the URL
/// chose it.
fn refused(status: u16, body: &Value) -> PeerError {
    let code = body["error"]["code"]
        .as_str()
        .filter(|code| {
            !code.is_empty()
                && code.len() <= MAX_CODE_LEN
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
        .unwrap_or("refused")
        .to_owned();
    match (status, code.as_str()) {
        (401, _)
        | (403, "invalid_token" | "device_proof_required")
        | (426, "device_repair_required") => PeerError::Unauthorized,
        _ => PeerError::Refused { status, code },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{refused, PeerError};

    #[test]
    fn a_refusal_keeps_only_a_code_shaped_code() {
        let long = "a".repeat(65);
        for (status, code, expected) in [
            (401, json!("anything"), PeerError::Unauthorized),
            (403, json!("invalid_token"), PeerError::Unauthorized),
            (
                426,
                json!("device_repair_required"),
                PeerError::Unauthorized,
            ),
            (
                410,
                json!("pairing_code_expired"),
                PeerError::Refused {
                    status: 410,
                    code: "pairing_code_expired".into(),
                },
            ),
            (
                409,
                json!("<b>hi</b> there"),
                PeerError::Refused {
                    status: 409,
                    code: "refused".into(),
                },
            ),
            (
                409,
                json!(long),
                PeerError::Refused {
                    status: 409,
                    code: "refused".into(),
                },
            ),
            (
                500,
                json!(null),
                PeerError::Refused {
                    status: 500,
                    code: "refused".into(),
                },
            ),
        ] {
            assert_eq!(
                refused(status, &json!({ "error": { "code": code } })),
                expected,
                "{status} {code}"
            );
        }
    }
}
