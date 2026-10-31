//! The owner's other gateway, spoken to exactly as the phone speaks to it:
//! its device API, with the device token it issued this gateway, sealed with
//! the transport key it issued when it runs `transport_encryption: required`.
//! One-shot JSON calls for its tasks, and the `subscribe_tasks` socket that
//! follows them (`TaskFeed`), framed exactly as the phone's.

use std::time::Duration;

use anyhow::Context as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};

use super::peers::Peer;
use crate::connectivity::routes::code_pairing_material;
use crate::platform::server::{
    EncryptedRequestPayload, TRANSPORT_DEVICE_HEADER, TRANSPORT_ENVELOPE_HEADER, TRANSPORT_HEADER,
};
use crate::tasks::domain::TaskState;
use crate::{now_unix_ms, transport};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CLAIM_AAD: &[u8] = b"POST /api/pair/claim";

/// Why a call to a peer did not answer what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerError {
    /// Nothing (or not a gateway) answered; a short reason for the log.
    Unreachable(String),
    /// The request went out but no answer came (a timeout, a cut body):
    /// whether it took effect over there is unknown.
    NoAnswer(String),
    /// The peer no longer knows this gateway's token: the link is gone.
    Unauthorized,
    /// It answered with a refusal. Also a claim answer this gateway refuses
    /// to take: `claim_unsealed` (in the clear where it had to be sealed)
    /// and `claim_not_opened` (sealed, but not under the pairing key), with
    /// the HTTP status the answer came with.
    Refused { status: u16, code: String },
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(reason) => write!(f, "unreachable: {reason}"),
            Self::NoAnswer(reason) => write!(f, "no answer: {reason}"),
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

impl std::fmt::Debug for Claimed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Claimed")
            .field("server_id", &self.server_id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// Secrets may cross a network only inside the existing authenticated
/// envelope or certificate-validated TLS. Literal loopback is the sole
/// plaintext exception for local development; names, private addresses and
/// tailnet addresses do not prove an encrypted route. Never resolve a name
/// here and then trust a second DNS resolution made by the HTTP client.
fn require_protected_transport(base: &str, sealed: bool) -> Result<(), PeerError> {
    let url = reqwest::Url::parse(base)
        .ok()
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .ok_or_else(|| PeerError::Unreachable("invalid peer URL".into()))?;
    let loopback = url
        .host_str()
        .and_then(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|ip| match ip {
            std::net::IpAddr::V4(ip) => ip.is_loopback(),
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
            }
        });
    if sealed || url.scheme() == "https" || loopback {
        Ok(())
    } else {
        Err(PeerError::Refused {
            status: 426,
            code: "transport_encryption_required".into(),
        })
    }
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

/// A 2xx whose answer could not be read: the request was taken, so what it
/// did is unknown.
fn unread(status: u16, error: PeerError) -> PeerError {
    match error {
        PeerError::Unreachable(reason) if (200..300).contains(&status) => {
            PeerError::NoAnswer(reason)
        }
        error => error,
    }
}

/// Not even connected: nothing reached the peer. Anything later (a
/// timeout, a cut body) may have.
fn unreachable(error: reqwest::Error) -> PeerError {
    let reason = error.is_connect();
    let text = error.without_url().to_string();
    if reason {
        PeerError::Unreachable(text)
    } else {
        PeerError::NoAnswer(text)
    }
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

/// What `POST /api/pair/request` answered: B's label, and whether B runs
/// `transport_encryption: required` -- then its claim answer is sealed, and
/// one in the clear is not B's (`claim` refuses it).
pub(crate) struct PairingOffer {
    pub label: String,
    pub transport_required: bool,
}

/// `POST {url}/api/pair/request` as a gateway device: B shows the code its
/// owner then reads off `muqun-gateway manage`. Answers B's label and mode.
pub(crate) async fn request_pairing(
    url: &str,
    request_id: &str,
    device_name: &str,
    server_id: &str,
) -> Result<PairingOffer, PeerError> {
    let response = http()
        .post(format!("{url}/api/pair/request"))
        .json(&json!({
            "request_id": request_id,
            "device_name": device_name,
            "kind": "gateway",
            "server_id": server_id,
        }))
        .send()
        .await
        .map_err(unreachable)?;
    let status = response.status();
    let body = body_of(response).await?;
    if !status.is_success() {
        return Err(refused(status.as_u16(), &body));
    }
    Ok(PairingOffer {
        label: string(&body["server_label"])?,
        transport_required: body["transport_encryption"].as_str() == Some("required"),
    })
}

/// `POST {url}/api/pair/claim`.
///
/// With `pairing_key` -- B's current pairing transport key, which its owner
/// read off B's `POST /api/pair/pending` answer and handed over with the code
/// -- the claim is sealed exactly as a phone that scanned B's QR seals it
/// (`Direction::PairingRequest`, AAD `POST /api/pair/claim`, envelope
/// version 1), and the answer has to be sealed under the same key and bound
/// to the request nonce. Neither the code nor the credentials cross the
/// wire in the clear, and nothing B could answer unsealed is taken.
///
/// Without it (an older app, or `muqun-gateway peers link`), a gateway in
/// `required` mode seals the answer with key material from the code itself
/// (`code_pairing_material`), which is how a phone that typed the code opens
/// it too. `transport_required` is what B's `POST /api/pair/request` said
/// of itself: then an answer in the clear is refused as `claim_unsealed`.
pub(crate) async fn claim(
    url: &str,
    request_id: &str,
    code: &str,
    pairing_key: Option<&str>,
    transport_required: bool,
) -> Result<Claimed, PeerError> {
    // A response sealed with material derived from a code sent in the clear
    // does not protect credentials from an observer who captured that code.
    require_protected_transport(url, pairing_key.is_some())?;
    let claim = json!({ "request_id": request_id, "code": code });
    let pairing_material = pairing_key
        .map(|key| {
            transport::decode_key(key)
                .ok()
                .filter(|material| material.len() == 32)
                .ok_or_else(|| PeerError::Unreachable("invalid pairing key".into()))
        })
        .transpose()?;
    let wire = match &pairing_material {
        Some(material) => serde_json::to_value(
            transport::seal(
                material,
                transport::Direction::PairingRequest,
                CLAIM_AAD,
                claim.to_string().as_bytes(),
                now_unix_ms(),
            )
            .map_err(|_| PeerError::Unreachable("claim could not be sealed".into()))?,
        )
        .map_err(|_| PeerError::Unreachable("claim".into()))?,
        None => claim,
    };
    let request_nonce = wire["nonce"].as_str().map(str::to_owned);
    let response = http()
        .post(format!("{url}/api/pair/claim"))
        .json(&wire)
        .send()
        .await
        .map_err(unreachable)?;
    let status = response.status();
    let body = body_of(response).await?;
    if !status.is_success() {
        return Err(refused(status.as_u16(), &body));
    }
    let unsealed = |code: &str| PeerError::Refused {
        status: status.as_u16(),
        code: code.into(),
    };
    let sealed = body.get("ciphertext").is_some();
    let body = match (pairing_material, request_nonce) {
        (Some(material), Some(request_nonce)) => {
            if !sealed {
                return Err(unsealed("claim_unsealed"));
            }
            let envelope: transport::Envelope = serde_json::from_value(body)
                .map_err(|_| PeerError::Unreachable("not a gateway".into()))?;
            let aad = [CLAIM_AAD, b"\n", request_nonce.as_bytes()].concat();
            let plaintext = transport::open(
                &material,
                transport::Direction::PairingResponse,
                &aad,
                &envelope,
                now_unix_ms(),
            )
            .map_err(|_| unsealed("claim_not_opened"))?;
            serde_json::from_slice(&plaintext)
                .map_err(|_| PeerError::Unreachable("claim".into()))?
        }
        _ if sealed => {
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
            .map_err(|_| unsealed("claim_not_opened"))?;
            serde_json::from_slice(&plaintext)
                .map_err(|_| PeerError::Unreachable("claim".into()))?
        }
        _ if transport_required => return Err(unsealed("claim_unsealed")),
        _ => body,
    };
    Ok(Claimed {
        server_id: string(&body["server_id"])?,
        label: string(&body["label"]).unwrap_or_default(),
        token: string(&body["token"])?,
        device_id: string(&body["device_id"])?,
        transport_key: body["transport_key"].as_str().map(str::to_owned),
    })
}

/// Authenticated metadata and the minimum transport this peer can speak.
pub(crate) struct PeerCapabilities {
    pub names: Vec<String>,
    pub transport_version: u8,
}

/// One linked peer's device API.
pub(crate) struct PeerClient {
    http: reqwest::Client,
    base: String,
    token: String,
    device_id: String,
    /// The decoded transport key; `None` speaks plain bearer.
    material: Option<Vec<u8>>,
    transport_version: u8,
}

impl PeerClient {
    pub(crate) fn for_peer(peer: &Peer) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=2).contains(&peer.transport_min_version),
            "unsupported peer transport version"
        );
        anyhow::ensure!(
            peer.transport_min_version == 1 || peer.transport_key.is_some(),
            "pinned peer transport requires a key"
        );
        Ok(Self {
            http: http(),
            base: peer.url.trim_end_matches('/').to_owned(),
            token: peer.token.clone(),
            device_id: peer.device_id.clone(),
            transport_version: peer.transport_min_version,
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
        require_protected_transport(&self.base, self.material.is_some())?;
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
        let body = body_of(response)
            .await
            .map_err(|error| unread(status, error))?;
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
        let envelope = transport::seal_version(
            self.transport_version,
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
        let unknown = |error| unread(outer_status, error);
        let outer = body_of(response).await.map_err(unknown)?;
        let Ok(sealed) = serde_json::from_value::<transport::Envelope>(outer.clone()) else {
            // A refusal before the device was known (401/403) comes back unsealed.
            // An unsealed success cannot establish whether a mutation ran.
            if (200..300).contains(&outer_status) {
                return Err(PeerError::NoAnswer("unsealed answer".into()));
            }
            return Err(refused(outer_status, &outer));
        };
        if sealed.version != envelope.version {
            return Err(unknown(PeerError::Unreachable(
                "answer transport version changed".into(),
            )));
        }
        let plaintext = transport::open(
            material,
            transport::Direction::Response,
            format!("{aad}\n{}", envelope.nonce).as_bytes(),
            &sealed,
            now_unix_ms(),
        )
        .map_err(|_| unknown(PeerError::Unreachable("answer did not open".into())))?;
        let answer: Value = serde_json::from_slice(&plaintext)
            .map_err(|_| unknown(PeerError::Unreachable("answer".into())))?;
        // No `x-muqun-envelope-accept` is sent, so a compressed body is not ours.
        if answer
            .get("content_encoding")
            .is_some_and(|value| !value.is_null())
        {
            return Err(unknown(PeerError::Unreachable("compressed".into())));
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

    /// Discover through an authenticated envelope whenever the peer issued a
    /// key. An unsigned capability list must never decide an encrypted upgrade.
    pub(crate) async fn capabilities(&self) -> Result<PeerCapabilities, PeerError> {
        let body = self.json(Method::GET, "/api/capabilities", None).await?;
        let names = body["capabilities"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let supports_v2 = self.material.is_some()
            && body["transport_versions"]
                .as_array()
                .is_some_and(|versions| versions.iter().any(|version| version.as_u64() == Some(2)));
        Ok(PeerCapabilities {
            names,
            transport_version: if supports_v2 {
                2
            } else {
                self.transport_version
            },
        })
    }

    /// Confirm v2 with a read-only v2 exchange before callers persist a pin.
    /// An unsealed 426 is only a hint to probe, never proof of support. This
    /// also recovers a lost probe response: the peer may already require v2.
    pub(crate) async fn negotiate(&self) -> Result<PeerCapabilities, PeerError> {
        match self.capabilities().await {
            Ok(capabilities)
                if self.material.is_none()
                    || self.transport_version == 2
                    || capabilities.transport_version == 1 =>
            {
                Ok(capabilities)
            }
            Ok(_) => self.probe_v2().await,
            Err(PeerError::Refused { status: 426, code })
                if self.material.is_some()
                    && self.transport_version == 1
                    && code == "transport_version_required" =>
            {
                self.probe_v2().await
            }
            Err(error) => Err(error),
        }
    }

    async fn probe_v2(&self) -> Result<PeerCapabilities, PeerError> {
        Self {
            http: self.http.clone(),
            base: self.base.clone(),
            token: self.token.clone(),
            device_id: self.device_id.clone(),
            material: self.material.clone(),
            transport_version: 2,
        }
        .capabilities()
        .await
    }

    /// `POST /api/tasks`: B opens `body` as its own task.
    /// An answer that is not a task counts as no answer: the peer took the
    /// request, and may have started it.
    /// `POST /api/tasks` for a command: the task, and the whole answer (its
    /// exit code and output tail when it ended within the wait).
    pub(crate) async fn create_command(
        &self,
        body: &Value,
    ) -> Result<(RemoteTask, Value), PeerError> {
        let answer = self.json(Method::POST, "/api/tasks", Some(body)).await?;
        let task = remote(answer.clone()).map_err(|_| PeerError::NoAnswer("not a task".into()))?;
        Ok((task, answer))
    }

    pub(crate) async fn create_task(&self, body: &Value) -> Result<RemoteTask, PeerError> {
        let answer = self.json(Method::POST, "/api/tasks", Some(body)).await?;
        remote(answer).map_err(|_| PeerError::NoAnswer("not a task".into()))
    }

    /// `GET /api/tasks`: the tasks this gateway opened there, newest first
    /// (a peer sees only its own).
    pub(crate) async fn own_tasks(&self) -> Result<Vec<RemoteTask>, PeerError> {
        let answer = self.json(Method::GET, "/api/tasks?limit=200", None).await?;
        Ok(answer["tasks"]
            .as_array()
            .map(|tasks| {
                tasks
                    .iter()
                    .filter_map(|task| remote(task.clone()).ok())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// `GET /api/tasks/{id}`.
    pub(crate) async fn read_task(&self, id: &str) -> Result<RemoteTask, PeerError> {
        let path = format!("/api/tasks/{}", path_segment(id)?);
        let answer = self.json(Method::GET, &path, None).await?;
        remote(answer["task"].clone())
    }

    /// `POST /api/tasks/{id}/cancel`: the task as the cancel left it.
    pub(crate) async fn cancel_task(&self, id: &str) -> Result<RemoteTask, PeerError> {
        let path = format!("/api/tasks/{}/cancel", path_segment(id)?);
        remote(self.json(Method::POST, &path, Some(&json!({}))).await?)
    }

    /// `GET /api/ws`, then `{"t":"subscribe_tasks"}`: answers once B has
    /// confirmed the subscription, so no change after this returns is missed.
    /// What changed before it is the feed's first item, a `Resync`.
    pub(crate) async fn subscribe_tasks(&self) -> Result<TaskFeed, PeerError> {
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.open_task_socket(r#"{"t":"subscribe_tasks"}"#),
        )
        .await
        .unwrap_or_else(|_| Err(PeerError::Unreachable("socket timed out".into())))
    }

    pub(crate) async fn subscribe_home(&self, request: &str) -> Result<TaskFeed, PeerError> {
        tokio::time::timeout(REQUEST_TIMEOUT, self.open_task_socket(request))
            .await
            .unwrap_or_else(|_| Err(PeerError::Unreachable("socket timed out".into())))
    }

    async fn open_task_socket(&self, subscription: &str) -> Result<TaskFeed, PeerError> {
        require_protected_transport(&self.base, self.material.is_some())?;
        let mut request = ws_url(&self.base)?
            .into_client_request()
            .map_err(|_| PeerError::Unreachable("not a socket url".into()))?;
        let headers = request.headers_mut();
        let mut nonce = None;
        if let Some(material) = &self.material {
            // The phone's sealed upgrade: the token inside an envelope over
            // `GET /api/ws`, whose nonce the frame keys derive from.
            let payload = EncryptedRequestPayload {
                token: self.token.clone(),
                content_type: None,
                body: String::new(),
            };
            let envelope = transport::seal_version(
                self.transport_version,
                material,
                transport::Direction::Request,
                WS_AAD.as_bytes(),
                &serde_json::to_vec(&payload).expect("payload serializes"),
                now_unix_ms(),
            )
            .map_err(|_| PeerError::Unreachable("seal".into()))?;
            let sealed = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&envelope).expect("envelope"));
            for (name, value) in [
                (TRANSPORT_HEADER, "1"),
                (TRANSPORT_DEVICE_HEADER, self.device_id.as_str()),
                (TRANSPORT_ENVELOPE_HEADER, sealed.as_str()),
            ] {
                headers.insert(name, header_value(value)?);
            }
            nonce = Some(envelope.nonce);
        } else {
            headers.insert(
                "authorization",
                header_value(&format!("Bearer {}", self.token))?,
            );
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_ANSWER_BYTES))
            .max_frame_size(Some(MAX_ANSWER_BYTES));
        let (socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), true)
            .await
            .map_err(upgrade_refused)?;
        let mut feed = TaskFeed {
            socket,
            codec: None,
            heard: Instant::now(),
            pinged: false,
            resync: true,
        };
        let first = feed
            .next_frame()
            .await
            .ok_or_else(|| PeerError::Unreachable("no hello".into()))?;
        let hello = match (&self.material, nonce) {
            (Some(material), Some(nonce)) => {
                let (codec, hello) = ClientCodec::open_first(&first, material, &nonce)
                    .map_err(|_| PeerError::Unreachable("hello did not open".into()))?;
                feed.codec = Some(codec);
                serde_json::from_str(&hello).unwrap_or(Value::Null)
            }
            _ => first,
        };
        if hello["t"] != "hello" {
            return Err(PeerError::Unreachable("no hello".into()));
        }
        feed.send(subscription).await?;
        loop {
            let frame = feed
                .next_plaintext()
                .await
                .ok_or_else(|| PeerError::Unreachable("socket closed".into()))?;
            match frame["t"].as_str() {
                Some("subscribed") if frame["tasks"] == true || frame["home"] == true => {
                    return Ok(feed)
                }
                // Refused for what this token is; any other error (another
                // socket of this device took the slot) is a socket that ended.
                Some("error") if frame["code"] == "peer_forbidden" => {
                    return Err(refused(403, &json!({ "error": frame })))
                }
                Some("error") => return Err(PeerError::Unreachable("socket".into())),
                _ => continue,
            }
        }
    }
}

/// The `/api/ws` URL under a peer's base URL.
fn ws_url(base: &str) -> Result<String, PeerError> {
    if let Some(rest) = base.strip_prefix("http://") {
        Ok(format!("ws://{rest}{WS_PATH}"))
    } else if let Some(rest) = base.strip_prefix("https://") {
        Ok(format!("wss://{rest}{WS_PATH}"))
    } else {
        Err(PeerError::Unreachable("not a socket url".into()))
    }
}

fn header_value(value: &str) -> Result<tungstenite::http::HeaderValue, PeerError> {
    value
        .parse()
        .map_err(|_| PeerError::Unreachable("header".into()))
}

/// A refused upgrade is read as a refused call; anything else never reached
/// a gateway.
fn upgrade_refused(error: tungstenite::Error) -> PeerError {
    match error {
        tungstenite::Error::Http(response) => {
            let body = response
                .body()
                .as_deref()
                .and_then(|bytes| serde_json::from_slice(bytes).ok())
                .unwrap_or(Value::Null);
            refused(response.status().as_u16(), &body)
        }
        tungstenite::Error::Io(error) => PeerError::Unreachable(error.kind().to_string()),
        _ => PeerError::Unreachable("socket".into()),
    }
}

/// Whether `id` can be a task id: `t_` plus URL-safe base64, so at most 64
/// of `[A-Za-z0-9_-]`. Anything else names no task over there.
pub(crate) fn is_task_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// `id` as one path segment ([`is_task_id`]).
fn path_segment(id: &str) -> Result<&str, PeerError> {
    if is_task_id(id) {
        Ok(id)
    } else {
        Err(PeerError::Refused {
            status: 404,
            code: "task_not_found".into(),
        })
    }
}

/// A task view from the peer, refused unless its id is task-shaped: the id
/// is stored here and later put in a path.
fn remote(value: Value) -> Result<RemoteTask, PeerError> {
    serde_json::from_value::<RemoteTask>(value)
        .ok()
        .filter(|task| is_task_id(&task.task_id))
        .ok_or_else(|| PeerError::Unreachable("not a task".into()))
}

/// B's task view, as much of it as a mirror copies.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RemoteTask {
    #[serde(default)]
    pub reopened_ms: Option<u64>,
    #[serde(default)]
    pub reopen_count: u64,
    pub task_id: String,
    pub state: TaskState,
    /// The reference this gateway sent with it: how a create that got no
    /// answer finds its task.
    #[serde(default)]
    pub client_ref: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub progress: Option<String>,
    /// What the task is called over there: its label, or its session's
    /// name once it has one, else its goal's first sentence.
    #[serde(default)]
    pub title: Option<String>,
}

/// What a peer's task socket says.
#[derive(Debug)]
pub(crate) enum FeedItem {
    /// One of B's tasks changed; this is it now.
    Task(RemoteTask),
    /// B dropped task frames for this socket: read the mirrored tasks again.
    Resync,
}

const WS_PATH: &str = "/api/ws";
const WS_AAD: &str = "GET /api/ws";
/// Quiet this long, the feed pings; quiet as long again, it is over. A live
/// gateway pings its sockets every 25 s and answers a ping at once, so only
/// a half-open connection trips it.
const PING_AFTER: Duration = Duration::from_secs(25);

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// B's `subscribe_tasks` socket, subscribed. Dropping it closes the socket.
pub(crate) struct TaskFeed {
    socket: Socket,
    /// `Some` when the socket is sealed.
    codec: Option<ClientCodec>,
    /// When anything last arrived: the half-open bound runs from it, across
    /// however many `next` calls the caller cancels.
    heard: Instant,
    /// A ping went out and nothing has come back since.
    pinged: bool,
    /// The first item is a `Resync`: what changed before the subscription
    /// came through no frame.
    resync: bool,
}

impl TaskFeed {
    /// Summary consumers share the existing authenticated wire codec and heartbeat.
    pub(crate) async fn next_home(&mut self) -> Option<Value> {
        loop {
            let frame = self.next_plaintext().await?;
            match frame["t"].as_str() {
                Some("home" | "home_overview") => return Some(frame),
                Some("home_status") => return None,
                Some("error") => return None,
                _ => {}
            }
        }
    }

    /// When anything last arrived on the socket.
    pub(crate) fn heard(&self) -> Instant {
        self.heard
    }

    /// The next task change, a `Resync` first. `None`: the socket ended
    /// (closed, silent past a ping, a frame that did not open, or an `error`
    /// frame); open a new one. Cancel-safe between frames.
    pub(crate) async fn next(&mut self) -> Option<FeedItem> {
        if std::mem::take(&mut self.resync) {
            return Some(FeedItem::Resync);
        }
        loop {
            let frame = self.next_plaintext().await?;
            match frame["t"].as_str() {
                Some("task") => match remote(frame["data"].clone()) {
                    Ok(task) => return Some(FeedItem::Task(task)),
                    // A view this side cannot read: the follower's poll
                    // fetches it.
                    Err(_) => return Some(FeedItem::Resync),
                },
                Some("resync") if frame["tasks"] == true => return Some(FeedItem::Resync),
                Some("error") => return None,
                // hello, subscribed, defaults, pong, a session resync.
                _ => continue,
            }
        }
    }

    /// The next frame's plaintext JSON.
    async fn next_plaintext(&mut self) -> Option<Value> {
        let frame = self.next_frame().await?;
        let Some(codec) = &mut self.codec else {
            return Some(frame);
        };
        let plaintext = codec.open(&frame).ok()?;
        serde_json::from_str(&plaintext).ok()
    }

    /// The next text frame as JSON, as it arrived.
    async fn next_frame(&mut self) -> Option<Value> {
        loop {
            let quiet_for = if self.pinged {
                2 * PING_AFTER
            } else {
                PING_AFTER
            };
            let deadline = self.heard + quiet_for;
            let message = match tokio::time::timeout_at(deadline, self.socket.next()).await {
                Ok(message) => message?.ok()?,
                Err(_) if self.pinged => return None,
                Err(_) => {
                    // Marked first: a cancelled send still counts, and B's
                    // own pings keep a live socket talking anyway.
                    self.pinged = true;
                    self.send(r#"{"t":"ping"}"#).await.ok()?;
                    continue;
                }
            };
            self.heard = Instant::now();
            self.pinged = false;
            match message {
                Message::Text(text) => return serde_json::from_str(text.as_str()).ok(),
                Message::Binary(_) | Message::Close(_) => return None,
                // Pings are answered by the socket itself.
                _ => continue,
            }
        }
    }

    async fn send(&mut self, plaintext: &str) -> Result<(), PeerError> {
        let wire = match &self.codec {
            Some(codec) => codec
                .seal(plaintext)
                .map_err(|_| PeerError::Unreachable("seal".into()))?,
            None => plaintext.to_owned(),
        };
        self.socket
            .send(Message::Text(wire.into()))
            .await
            .map_err(|_| PeerError::Unreachable("socket closed".into()))?;
        // Counted once sent: a send the caller cancelled leaves no gap in
        // the sequence B checks.
        if let Some(codec) = &mut self.codec {
            codec.send_seq += 1;
        }
        Ok(())
    }
}

/// The client half of a sealed `/api/ws` (see `ws_routes::FrameCodec`): the
/// first server frame names the connection, both keys derive from it and the
/// upgrade's nonce, and each direction counts its own frames from zero.
struct ClientCodec {
    server_key: [u8; 32],
    client_key: [u8; 32],
    connection_id: String,
    recv_seq: u64,
    send_seq: u64,
}

impl ClientCodec {
    fn open_first(
        first: &Value,
        material: &[u8],
        request_nonce: &str,
    ) -> anyhow::Result<(Self, String)> {
        let connection_id = first["cid"]
            .as_str()
            .context("no connection id")?
            .to_owned();
        let mut codec = Self {
            server_key: transport::derive_ws_server_key(material, &connection_id, request_nonce)?,
            client_key: transport::derive_ws_client_key(material, &connection_id, request_nonce)?,
            connection_id,
            recv_seq: 0,
            send_seq: 0,
        };
        let hello = codec.open(first)?;
        Ok((codec, hello))
    }

    fn aad(&self, seq: u64) -> String {
        format!("{WS_AAD}\n{}\n{seq}", self.connection_id)
    }

    fn open(&mut self, frame: &Value) -> anyhow::Result<String> {
        let seq = frame["seq"].as_u64().context("no seq")?;
        anyhow::ensure!(seq == self.recv_seq, "frame out of order");
        let plaintext = transport::open_stream_event(
            &self.server_key,
            seq,
            self.aad(seq).as_bytes(),
            frame["c"].as_str().context("no ciphertext")?,
        )?;
        self.recv_seq += 1;
        Ok(String::from_utf8(plaintext)?)
    }

    /// The frame for `plaintext` at the next sequence number; the caller
    /// counts it once it is sent.
    fn seal(&self, plaintext: &str) -> anyhow::Result<String> {
        let seq = self.send_seq;
        let c = transport::seal_stream_event(
            &self.client_key,
            seq,
            self.aad(seq).as_bytes(),
            plaintext.as_bytes(),
        )?;
        Ok(format!(r#"{{"seq":{seq},"c":"{c}"}}"#))
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
        // A sealed call carries its time; the peer refuses one more than 5
        // minutes off its own clock, which reads like a plain refusal.
        (_, "invalid_envelope") => {
            tracing::warn!(
                "a linked computer refused a sealed call (invalid_envelope): its clock and this \
                 computer's may be more than 5 minutes apart"
            );
            PeerError::Refused { status, code }
        }
        _ => PeerError::Refused { status, code },
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tasks::domain::{
        now_ms, PrincipalKind, Task, TaskBinding, TaskChange, TaskId, TaskKind, TaskOrigin,
        TaskPrincipal,
    };
    use crate::test_support::{
        named, output_state, peer_router, serve, test_peer_device, FakeHerdr,
    };
    use crate::{generate_token, AppState, TransportEncryptionMode};

    #[tokio::test]
    async fn hostname_discovery_does_not_authorize_a_plaintext_code_claim() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = calls.clone();
        let url = serve(axum::Router::new().fallback(axum::routing::post(
            move |uri: axum::http::Uri,
                  headers: axum::http::HeaderMap,
                  axum::Json(body): axum::Json<Value>| {
                let received = received.clone();
                async move {
                    assert!(!headers.contains_key("authorization"));
                    assert!(!headers.contains_key("cookie"));
                    received.lock().unwrap().push((uri.path().to_owned(), body));
                    axum::Json(json!({
                        "server_label": "fixture", "server_id": "fixture-peer",
                        "label": "fixture", "token": "fixture-token",
                        "device_id": "fixture-device",
                    }))
                }
            },
        )))
        .await;
        let named_http = url.replace("127.0.0.1", "localhost");
        assert_ne!(url, named_http);
        let offer = request_pairing(&named_http, "fixture-request", "fixture", "fixture-id")
            .await
            .unwrap();
        assert_eq!(offer.label, "fixture");
        assert!(
            !offer.transport_required,
            "an answer that names no mode cannot demand a sealed claim answer"
        );
        assert_eq!(
            claim(&named_http, "fixture-request", "2345-6789", None, false)
                .await
                .unwrap_err(),
            PeerError::Refused {
                status: 426,
                code: "transport_encryption_required".into(),
            }
        );
        // Local fixtures may explicitly use a literal loopback address.
        let claimed = claim(&url, "fixture-request", "2345-6789", None, false)
            .await
            .unwrap();
        assert_eq!(claimed.device_id, "fixture-device");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                (
                    "/api/pair/request".into(),
                    json!({
                        "request_id": "fixture-request", "device_name": "fixture",
                        "kind": "gateway", "server_id": "fixture-id",
                    }),
                ),
                (
                    "/api/pair/claim".into(),
                    json!({ "request_id": "fixture-request", "code": "2345-6789" }),
                ),
            ]
        );
    }

    #[test]
    fn credentials_require_a_protected_channel_without_trusting_dns_or_private_ranges() {
        for base in [
            "http://localhost:23847",
            "http://127.0.0.1.example.test:23847",
            "http://192.168.1.12:23847",
            "http://100.64.0.12:23847",
            "http://[fd00::12]:23847",
            "http://gateway.example.test",
        ] {
            assert_eq!(
                require_protected_transport(base, false),
                Err(PeerError::Refused {
                    status: 426,
                    code: "transport_encryption_required".into(),
                }),
                "{base}"
            );
            assert!(require_protected_transport(base, true).is_ok());
        }
        for base in [
            "http://127.0.0.1:23847",
            "http://[::1]:23847",
            "http://[::ffff:127.0.0.1]:23847",
            "https://gateway.example.test",
        ] {
            assert!(require_protected_transport(base, false).is_ok(), "{base}");
        }
        for base in [
            "file:///tmp/gateway",
            "https://name:secret@gateway.example.test",
            "https://gateway.example.test?token=secret",
            "https://gateway.example.test#secret",
        ] {
            assert!(require_protected_transport(base, true).is_err(), "{base}");
        }
    }

    #[tokio::test]
    async fn insecure_http_and_socket_calls_are_refused_before_sending_credentials() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let calls = Arc::new(AtomicUsize::new(0));
        let received = calls.clone();
        let base = serve(axum::Router::new().fallback(axum::routing::any(move || {
            received.fetch_add(1, Ordering::SeqCst);
            async { axum::Json(json!({})) }
        })))
        .await
        .replace("127.0.0.1", "localhost");
        let client = PeerClient {
            http: http(),
            base,
            token: "synthetic-secret".into(),
            device_id: "synthetic-device".into(),
            material: None,
            transport_version: 1,
        };
        let refused = PeerError::Refused {
            status: 426,
            code: "transport_encryption_required".into(),
        };
        assert_eq!(client.ping().await.unwrap_err(), refused);
        assert_eq!(
            client
                .create_task(&json!({"goal":"sensitive task"}))
                .await
                .unwrap_err(),
            refused
        );
        assert!(matches!(client.subscribe_tasks().await, Err(error) if error == refused));
        assert!(
            matches!(client.subscribe_home(r#"{"t":"subscribe_home"}"#).await, Err(error) if error == refused)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn claimed_credentials_are_absent_from_debug_output() {
        let claimed = Claimed {
            server_id: "server".into(),
            label: "fixture".into(),
            token: "synthetic-bearer".into(),
            device_id: "device".into(),
            transport_key: Some("synthetic-key".into()),
        };
        let debug = format!("{claimed:?}");
        assert!(!debug.contains("synthetic-bearer") && !debug.contains("synthetic-key"));
    }

    #[tokio::test]
    async fn manual_pairing_does_not_follow_redirects() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let received = calls.clone();
        let destination = serve(axum::Router::new().fallback(axum::routing::any(move || {
            received.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { axum::Json(json!({})) }
        })))
        .await;
        let url = serve(axum::Router::new().fallback(axum::routing::post(move || {
            let destination = destination.clone();
            async move {
                (
                    axum::http::StatusCode::TEMPORARY_REDIRECT,
                    [(axum::http::header::LOCATION, destination)],
                    axum::Json(json!({})),
                )
            }
        })))
        .await;
        assert!(matches!(
            request_pairing(&url, "fixture-request", "fixture", "fixture-id").await,
            Err(PeerError::Refused { status: 307, .. })
        ));
        assert!(matches!(
            claim(&url, "fixture-request", "2345-6789", None, false).await,
            Err(PeerError::Refused { status: 307, .. })
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// A peer's claim answer, as `pair_claim` seals it for a QR-style claim:
    /// under the pairing key, bound to the claim's nonce.
    fn sealed_claim_answer(material: &[u8], request_nonce: &str, answer: &Value) -> Value {
        let aad = [CLAIM_AAD, b"\n", request_nonce.as_bytes()].concat();
        serde_json::to_value(
            transport::seal(
                material,
                transport::Direction::PairingResponse,
                &aad,
                answer.to_string().as_bytes(),
                now_unix_ms(),
            )
            .unwrap(),
        )
        .unwrap()
    }

    /// With the peer's pairing key in hand, the claim goes out sealed as a
    /// phone's QR claim does -- the code never crosses in the clear -- and
    /// only an answer sealed under that key and bound to this claim's nonce
    /// is taken. An answer in the clear, or one sealed to another nonce (a
    /// replay, or a stranger's), is a refusal, whatever it says.
    #[tokio::test]
    async fn a_claim_with_the_pairing_key_is_sealed_and_takes_only_a_sealed_answer() {
        let pairing_key = generate_token();
        let material = transport::decode_key(&pairing_key).unwrap();
        let answer = json!({
            "server_id": "fixture-peer", "label": "fixture",
            "token": "fixture-token", "device_id": "fixture-device",
            "transport_key": generate_token(),
        });
        // What the fake peer answers next: how it seals, if at all.
        #[derive(Clone, Copy)]
        enum Answer {
            Sealed,
            Plain,
            OtherNonce,
        }
        let mode = std::sync::Arc::new(std::sync::Mutex::new(Answer::Sealed));
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let url = {
            let (mode, opened, answer) = (mode.clone(), opened.clone(), answer.clone());
            let material = material.clone();
            serve(axum::Router::new().fallback(axum::routing::post(
                move |axum::Json(wire): axum::Json<Value>| {
                    let (mode, opened, answer, material) = (
                        mode.clone(),
                        opened.clone(),
                        answer.clone(),
                        material.clone(),
                    );
                    async move {
                        // Sealed on the wire: no code and no request id in sight.
                        assert!(wire.get("code").is_none(), "{wire}");
                        let envelope: transport::Envelope = serde_json::from_value(wire).unwrap();
                        let plaintext = transport::open(
                            &material,
                            transport::Direction::PairingRequest,
                            CLAIM_AAD,
                            &envelope,
                            now_unix_ms(),
                        )
                        .expect("the claim opens under the pairing key");
                        opened
                            .lock()
                            .unwrap()
                            .push(serde_json::from_slice(&plaintext).unwrap());
                        let mode = *mode.lock().unwrap();
                        axum::Json(match mode {
                            Answer::Sealed => {
                                sealed_claim_answer(&material, &envelope.nonce, &answer)
                            }
                            Answer::Plain => answer,
                            Answer::OtherNonce => {
                                sealed_claim_answer(&material, "AAAAAAAAAAAAAAAA", &answer)
                            }
                        })
                    }
                },
            )))
            .await
        };

        let url = url.replace("127.0.0.1", "localhost");
        let claimed = claim(&url, "mint-1", "2345-6789", Some(&pairing_key), false)
            .await
            .unwrap();
        assert_eq!(claimed.device_id, "fixture-device");
        assert_eq!(claimed.token, "fixture-token");
        assert_eq!(
            *opened.lock().unwrap(),
            vec![json!({ "request_id": "mint-1", "code": "2345-6789" })]
        );

        for (answer_mode, code) in [
            (Answer::Plain, "claim_unsealed"),
            (Answer::OtherNonce, "claim_not_opened"),
        ] {
            *mode.lock().unwrap() = answer_mode;
            let refused = claim(&url, "mint-1", "2345-6789", Some(&pairing_key), false)
                .await
                .unwrap_err();
            assert_eq!(
                refused,
                PeerError::Refused {
                    status: 200,
                    code: code.into()
                }
            );
        }
        // A key that is not one is refused before anything is sent.
        assert!(matches!(
            claim(&url, "mint-1", "2345-6789", Some("not-a-key"), false).await,
            Err(PeerError::Unreachable(_))
        ));
        assert_eq!(opened.lock().unwrap().len(), 3);
    }

    /// Without the pairing key (an older app, `peers link`), a peer that said
    /// `required` of itself seals its answer under the code; one that arrives
    /// in the clear is not that peer's and is refused. A peer that said
    /// `disabled` answers in the clear, and that is taken as it always was.
    #[tokio::test]
    async fn without_the_pairing_key_a_required_peer_may_not_answer_in_the_clear() {
        let url = serve(axum::Router::new().fallback(axum::routing::post(|| async {
            axum::Json(json!({
                "server_id": "fixture-peer", "label": "fixture",
                "token": "fixture-token", "device_id": "fixture-device",
            }))
        })))
        .await;
        assert_eq!(
            claim(&url, "mint-1", "2345-6789", None, true)
                .await
                .unwrap_err(),
            PeerError::Refused {
                status: 200,
                code: "claim_unsealed".into()
            }
        );
        assert_eq!(
            claim(&url, "mint-1", "2345-6789", None, false)
                .await
                .unwrap()
                .device_id,
            "fixture-device"
        );
    }

    /// A task the peer opened on `state`, as if through `POST /api/tasks`.
    async fn peer_task(state: &AppState, device_id: &str) -> TaskId {
        let now = now_ms();
        state
            .tasks
            .insert_for_test(&Task {
                id: TaskId::new(),
                kind: TaskKind::Delegation,
                goal: "screenshot Home".into(),
                cwd: None,
                binding: TaskBinding::None,
                state: TaskState::Queued,
                summary: None,
                exit_code: None,
                created_by: TaskPrincipal {
                    kind: PrincipalKind::Gateway,
                    id: device_id.into(),
                },
                created_ms: now,
                updated_ms: now,
                seq: 0,
                parent: None,
                origin: TaskOrigin::Agent,
                label: None,
                requester_name: None,
                session_title: None,
                mentions: Vec::new(),
                client_ref: None,
                requester_server_id: None,
                reopened_ms: None,
                reopen_count: 0,
            })
            .await
    }

    /// Reads, cancels and follows a task on another gateway, in both of the
    /// transports a gateway can run: sealed with the key it issued at
    /// pairing, and plain.
    #[tokio::test]
    async fn a_peer_reads_cancels_and_follows_its_tasks_in_either_transport() {
        for version in [Some(1), Some(2), None] {
            let sealed = version.is_some();
            let herdr = FakeHerdr::start(vec!["$ "], None);
            let mut mini = named(output_state(&herdr), "srv-mini", "mini");
            let mut device = test_peer_device("coding-1", "peer-token");
            if sealed {
                device.transport_key = Some(generate_token());
            } else {
                mini.config.transport_encryption = TransportEncryptionMode::Disabled;
            }
            mini.devices.lock().unwrap().push(device.clone());
            let url = serve(peer_router(mini.clone())).await;
            let client = PeerClient::for_peer(&Peer {
                peer_id: "p-mini".into(),
                server_id: "srv-mini".into(),
                name: "mini".into(),
                url,
                token: "peer-token".into(),
                device_id: "coding-1".into(),
                transport_key: device.transport_key.clone(),
                transport_min_version: version.unwrap_or(1),
                added_ms: 0,
                last_seen_ms: None,
                capabilities: vec![],
                manifest: None,
                unauthorized: false,
            })
            .unwrap();

            let mut feed = client
                .subscribe_tasks()
                .await
                .unwrap_or_else(|error| panic!("sealed={sealed}: no socket: {error}"));
            assert!(
                matches!(feed.next().await, Some(FeedItem::Resync)),
                "sealed={sealed}: a fresh feed starts with a resync"
            );
            let id = peer_task(&mini, "coding-1").await;
            mini.tasks
                .move_to(
                    &id,
                    TaskChange {
                        to: TaskState::Working,
                        source: "test".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let followed = loop {
                match tokio::time::timeout(Duration::from_secs(5), feed.next())
                    .await
                    .expect("a frame")
                {
                    Some(FeedItem::Task(task))
                        if task.task_id == id.0 && task.state == TaskState::Working =>
                    {
                        break task
                    }
                    Some(_) => continue,
                    None => panic!("sealed={sealed}: the socket closed"),
                }
            };
            assert_eq!(followed.state, TaskState::Working);

            assert_eq!(
                client.read_task(&id.0).await.unwrap().state,
                TaskState::Working,
                "sealed={sealed}"
            );
            assert_eq!(
                client.cancel_task(&id.0).await.unwrap().state,
                TaskState::Cancelled,
                "sealed={sealed}"
            );
            for missing in ["t_missing", "../health", ""] {
                assert!(
                    matches!(
                        client.read_task(missing).await,
                        Err(PeerError::Refused { status: 404, .. })
                    ),
                    "sealed={sealed}: {missing:?}"
                );
            }

            mini.devices.lock().unwrap().retain(|d| d.id != "coding-1");
            assert!(
                matches!(client.read_task(&id.0).await, Err(PeerError::Unauthorized)),
                "sealed={sealed}: a revoked link"
            );
            assert!(
                matches!(client.subscribe_tasks().await, Err(PeerError::Unauthorized)),
                "sealed={sealed}: a revoked link's socket"
            );
        }
    }

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
                426,
                json!("transport_version_required"),
                PeerError::Refused {
                    status: 426,
                    code: "transport_version_required".into(),
                },
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

    #[tokio::test]
    async fn a_v2_peer_refuses_a_v1_answer_without_retrying_the_mutation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let material = transport::decode_key(&generate_token()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let received = calls.clone();
        let server_material = material.clone();
        let router = axum::Router::new().route(
            "/api/tasks",
            axum::routing::post(
                move |axum::Json(envelope): axum::Json<transport::Envelope>| {
                    let received = received.clone();
                    let material = server_material.clone();
                    async move {
                        received.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(envelope.version, 2);
                        transport::open(
                            &material,
                            transport::Direction::Request,
                            b"POST /api/tasks",
                            &envelope,
                            now_unix_ms(),
                        )
                        .unwrap();
                        axum::Json(
                            transport::seal_version(
                                1,
                                &material,
                                transport::Direction::Response,
                                format!("POST /api/tasks\n{}", envelope.nonce).as_bytes(),
                                br#"{"status":200,"body":"e30"}"#,
                                now_unix_ms(),
                            )
                            .unwrap(),
                        )
                    }
                },
            ),
        );
        let client = PeerClient::for_peer(&Peer {
            peer_id: "peer-test".into(),
            server_id: "server-test".into(),
            name: "test".into(),
            url: serve(router).await,
            token: "test-token".into(),
            device_id: "test-device".into(),
            transport_key: Some(URL_SAFE_NO_PAD.encode(material)),
            transport_min_version: 2,
            added_ms: 0,
            last_seen_ms: None,
            capabilities: vec![],
            manifest: None,
            unauthorized: false,
        })
        .unwrap();
        assert!(matches!(
            client
                .json(Method::POST, "/api/tasks", Some(&json!({})))
                .await,
            Err(PeerError::NoAnswer(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn authenticated_discovery_confirms_v2_before_pinning_and_recovers_a_lost_probe() {
        check_discovery_upgrade(false).await;
        check_discovery_upgrade(true).await;
    }

    async fn check_discovery_upgrade(lose_probe_response: bool) {
        use axum::response::IntoResponse as _;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let herdr = FakeHerdr::start(vec!["$ "], None);
        let coding = named(output_state(&herdr), "srv-coding", "coding");
        let mini = named(output_state(&herdr), "srv-mini", "mini");
        let mut device = test_peer_device("coding-1", "peer-token");
        device.transport_key = Some(generate_token());
        mini.devices.lock().unwrap().push(device.clone());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let registry = coding.peers.clone();
        let lose_response = Arc::new(AtomicBool::new(lose_probe_response));
        let router = peer_router(mini).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let registry = registry.clone();
                let captured = captured.clone();
                let lose_response = lose_response.clone();
                async move {
                    let header = request.headers()[TRANSPORT_ENVELOPE_HEADER]
                        .to_str()
                        .unwrap();
                    let envelope: transport::Envelope =
                        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).unwrap()).unwrap();
                    let persisted = registry.get("p-mini").await.unwrap().transport_min_version;
                    captured.lock().unwrap().push((envelope.version, persisted));
                    let response = next.run(request).await;
                    if envelope.version == 2 && lose_response.swap(false, Ordering::SeqCst) {
                        // The real server accepted and pinned the v2 probe,
                        // but its answer never reached this client intact.
                        axum::http::StatusCode::BAD_GATEWAY.into_response()
                    } else {
                        response
                    }
                }
            },
        ));
        let peer = Peer {
            peer_id: "p-mini".into(),
            server_id: "srv-mini".into(),
            name: "mini".into(),
            url: serve(router).await,
            token: "peer-token".into(),
            device_id: "coding-1".into(),
            transport_key: device.transport_key,
            transport_min_version: 1,
            added_ms: 0,
            last_seen_ms: Some(now_ms()),
            capabilities: vec![],
            manifest: None,
            unauthorized: false,
        };
        coding.peers.store_for_test(peer.clone()).await;
        let first = coding.peers.checked(&coding, peer.clone()).await.unwrap();
        let upgraded = if lose_probe_response {
            assert_eq!(first.transport_min_version, 1);
            assert_eq!(
                coding
                    .peers
                    .get("p-mini")
                    .await
                    .unwrap()
                    .transport_min_version,
                1
            );
            assert!(!coding.peers.reachable(&first, now_ms()));
            coding.peers.checked(&coding, peer.clone()).await.unwrap()
        } else {
            first
        };
        assert_eq!(upgraded.transport_min_version, 2);
        let expected = if lose_probe_response {
            vec![(1, 1), (2, 1), (1, 1), (2, 1), (2, 2)]
        } else {
            vec![(1, 1), (2, 1), (2, 2)]
        };
        assert_eq!(*requests.lock().unwrap(), expected);
        // A caller retaining its old Peer snapshot uses the persisted pin.
        assert_eq!(
            coding
                .peers
                .checked(&coding, peer)
                .await
                .unwrap()
                .transport_min_version,
            2
        );
    }

    #[tokio::test]
    async fn unsigned_discovery_cannot_upgrade_an_encrypted_or_plain_link() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let pings = Arc::new(AtomicUsize::new(0));
        let received = pings.clone();
        let router = axum::Router::new()
            .route(
                "/api/capabilities",
                axum::routing::get(|| async {
                    axum::Json(json!({"capabilities": [], "transport_versions": [1, 2]}))
                }),
            )
            .route(
                "/api/tasks",
                axum::routing::get(move || {
                    let received = received.clone();
                    async move {
                        received.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({"tasks": []}))
                    }
                }),
            );
        let mut peer = Peer {
            peer_id: "peer-test".into(),
            server_id: "server-test".into(),
            name: "test".into(),
            url: serve(router).await,
            token: "test-token".into(),
            device_id: "test-device".into(),
            transport_key: Some(generate_token()),
            transport_min_version: 1,
            added_ms: 0,
            last_seen_ms: None,
            capabilities: vec![],
            manifest: None,
            unauthorized: false,
        };
        assert!(PeerClient::for_peer(&peer)
            .unwrap()
            .capabilities()
            .await
            .is_err());
        let herdr = FakeHerdr::start(vec!["$ "], None);
        let state = output_state(&herdr);
        state.peers.store_for_test(peer.clone()).await;
        let checked = state.peers.checked(&state, peer.clone()).await.unwrap();
        assert!(!state.peers.reachable(&checked, now_ms()));
        assert_eq!(
            pings.load(Ordering::SeqCst),
            0,
            "no v1 fallback after failed discovery"
        );
        peer.transport_key = None;
        assert_eq!(
            PeerClient::for_peer(&peer)
                .unwrap()
                .capabilities()
                .await
                .unwrap()
                .transport_version,
            1
        );
    }

    #[tokio::test]
    async fn a_forged_upgrade_hint_with_a_failed_v2_probe_never_pins() {
        use std::sync::{Arc, Mutex};

        let versions = Arc::new(Mutex::new(Vec::new()));
        let captured = versions.clone();
        let router = axum::Router::new().route(
            "/api/capabilities",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let captured = captured.clone();
                async move {
                    let envelope: transport::Envelope = serde_json::from_slice(
                        &URL_SAFE_NO_PAD
                            .decode(headers[TRANSPORT_ENVELOPE_HEADER].as_bytes())
                            .unwrap(),
                    )
                    .unwrap();
                    captured.lock().unwrap().push(envelope.version);
                    let (status, code) = if envelope.version == 1 {
                        (
                            axum::http::StatusCode::UPGRADE_REQUIRED,
                            "transport_version_required",
                        )
                    } else {
                        (axum::http::StatusCode::BAD_GATEWAY, "unavailable")
                    };
                    (status, axum::Json(json!({"error": {"code": code}})))
                }
            }),
        );
        let peer = Peer {
            peer_id: "peer-test".into(),
            server_id: "server-test".into(),
            name: "test".into(),
            url: serve(router).await,
            token: "test-token".into(),
            device_id: "test-device".into(),
            transport_key: Some(generate_token()),
            transport_min_version: 1,
            added_ms: 0,
            last_seen_ms: None,
            capabilities: vec![],
            manifest: None,
            unauthorized: false,
        };
        let herdr = FakeHerdr::start(vec!["$ "], None);
        let state = output_state(&herdr);
        state.peers.store_for_test(peer.clone()).await;
        let checked = state.peers.checked(&state, peer).await.unwrap();
        assert_eq!(*versions.lock().unwrap(), vec![1, 2]);
        assert_eq!(
            state
                .peers
                .get("peer-test")
                .await
                .unwrap()
                .transport_min_version,
            1
        );
        assert!(!state.peers.reachable(&checked, now_ms()));
    }
}
