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

    /// `POST /api/tasks`: B opens `body` as its own task.
    #[allow(dead_code)] // the mirrors of Task G3
    pub(crate) async fn create_task(&self, body: &Value) -> Result<RemoteTask, PeerError> {
        remote(self.json(Method::POST, "/api/tasks", Some(body)).await?)
    }

    /// `GET /api/tasks/{id}`.
    #[allow(dead_code)] // the mirrors of Task G3
    pub(crate) async fn read_task(&self, id: &str) -> Result<RemoteTask, PeerError> {
        let path = format!("/api/tasks/{}", path_segment(id)?);
        let answer = self.json(Method::GET, &path, None).await?;
        remote(answer["task"].clone())
    }

    /// `POST /api/tasks/{id}/cancel`: the task as the cancel left it.
    #[allow(dead_code)] // the mirrors of Task G3
    pub(crate) async fn cancel_task(&self, id: &str) -> Result<RemoteTask, PeerError> {
        let path = format!("/api/tasks/{}/cancel", path_segment(id)?);
        remote(self.json(Method::POST, &path, Some(&json!({}))).await?)
    }

    /// `GET /api/ws`, then `{"t":"subscribe_tasks"}`: answers once B has
    /// confirmed the subscription, so no change after this returns is missed.
    /// What changed before it is the feed's first item, a `Resync`.
    #[allow(dead_code)] // the mirrors of Task G3
    pub(crate) async fn subscribe_tasks(&self) -> Result<TaskFeed, PeerError> {
        tokio::time::timeout(REQUEST_TIMEOUT, self.open_task_socket())
            .await
            .unwrap_or_else(|_| Err(PeerError::Unreachable("socket timed out".into())))
    }

    async fn open_task_socket(&self) -> Result<TaskFeed, PeerError> {
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
            let envelope = transport::seal(
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
        feed.send(r#"{"t":"subscribe_tasks"}"#).await?;
        loop {
            let frame = feed
                .next_plaintext()
                .await
                .ok_or_else(|| PeerError::Unreachable("socket closed".into()))?;
            match frame["t"].as_str() {
                Some("subscribed") if frame["tasks"] == true => return Ok(feed),
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

/// `id` as one path segment. A task id is `t_` plus URL-safe base64, so
/// anything else names no task over there.
fn path_segment(id: &str) -> Result<&str, PeerError> {
    if !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        Ok(id)
    } else {
        Err(PeerError::Refused {
            status: 404,
            code: "task_not_found".into(),
        })
    }
}

fn remote(value: Value) -> Result<RemoteTask, PeerError> {
    serde_json::from_value(value).map_err(|_| PeerError::Unreachable("not a task".into()))
}

/// B's task view, as much of it as a mirror copies.
#[allow(dead_code)] // the mirrors of Task G3
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RemoteTask {
    pub task_id: String,
    pub state: TaskState,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub progress: Option<String>,
}

/// What a peer's task socket says.
#[allow(dead_code)] // the mirrors of Task G3
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
    /// The next task change, a `Resync` first. `None`: the socket ended
    /// (closed, silent past a ping, a frame that did not open, or an `error`
    /// frame); open a new one. Cancel-safe between frames.
    #[allow(dead_code)] // the mirrors of Task G3
    pub(crate) async fn next(&mut self) -> Option<FeedItem> {
        if std::mem::take(&mut self.resync) {
            return Some(FeedItem::Resync);
        }
        loop {
            let frame = self.next_plaintext().await?;
            match frame["t"].as_str() {
                Some("task") => match serde_json::from_value(frame["data"].clone()) {
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
        _ => PeerError::Refused { status, code },
    }
}

#[cfg(test)]
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
            })
            .await
    }

    /// Reads, cancels and follows a task on another gateway, in both of the
    /// transports a gateway can run: sealed with the key it issued at
    /// pairing, and plain.
    #[tokio::test]
    async fn a_peer_reads_cancels_and_follows_its_tasks_in_either_transport() {
        for sealed in [true, false] {
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
                added_ms: 0,
                last_seen_ms: None,
                capabilities: vec![],
                manifest: None,
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
