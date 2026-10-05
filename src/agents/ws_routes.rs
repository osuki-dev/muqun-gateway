//! `GET /api/ws`: one WebSocket per device carrying the agent events of any
//! number of sessions.
//!
//! The per-session SSE stream (`GET /api/agent-sessions/{asid}/stream`) shows
//! one session at a time, so approvals and status changes on every other
//! session reached the App only through push or a refetch. This socket
//! carries the same events -- the same `event` name and byte-identical `data`
//! -- for whichever sessions the client subscribes to, or all of them.
//! Requests stay on HTTP; SSE stays the fallback; the `seq` catch-up contract
//! (`timeline?after=seq`, `410` resync) is unchanged.
//!
//! Authentication is the SSE stream's: a paired device, and on an encrypted
//! device the sealed empty-GET envelope. On such a device every frame in both
//! directions is sealed under a per-connection key (see
//! `transport::derive_ws_server_key` / `derive_ws_client_key`) with its
//! direction's sequence number as the nonce, so a frame that is modified,
//! dropped, reordered or replayed from any other connection fails.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    extract::{
        ws::{
            close_code, rejection::WebSocketUpgradeRejection, CloseFrame, Message, WebSocket,
            WebSocketUpgrade,
        },
        Extension, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

use super::domain::AgentDomainEvent;
use super::routes::agent_event_record;
use crate::{
    api_error, require_device, still_paired, transport, AppState, EncryptedStreamContext,
    STREAM_DEVICE_RECHECK_INTERVAL,
};

pub const WS_PATH: &str = "/api/ws";
/// The frame vocabulary's version, sent in `hello` and announced in
/// discovery. Bumped only for a change an older client would misread.
pub const WS_PROTOCOL: u64 = 1;

/// Largest client message accepted, sealed or not. Client frames are a
/// subscribe or a ping; this is two orders of magnitude of slack.
pub const MAX_INBOUND_FRAME_BYTES: usize = 16 * 1024;
/// Sessions one connection may name individually. `subscribe_all` has no
/// such bound because it names none.
pub const MAX_SUBSCRIPTIONS: usize = 256;
/// Open sockets per device. The newest wins: the device's oldest socket is
/// told `superseded` and closed. A phone that changed networks has a dead
/// socket the gateway cannot yet tell is dead; refusing the new one would
/// lock the device out until the heartbeat noticed.
pub const MAX_CONNECTIONS_PER_DEVICE: usize = 4;
/// Open sockets across all devices. Past it a new upgrade is refused with
/// `429 too_many_connections`, since no other device's socket is ours to cut.
pub const MAX_CONNECTIONS_TOTAL: usize = 64;
/// Longest a single asid may be. Real ids are around thirty bytes.
const MAX_ASID_BYTES: usize = 128;

/// The timers one connection runs on. A value, not constants, so a test can
/// run the revocation and heartbeat paths in milliseconds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WsTimings {
    /// How often the server pings. Two unanswered pings drop the connection.
    pub(crate) heartbeat: Duration,
    /// How often the device behind the socket is checked to still be paired.
    pub(crate) device_recheck: Duration,
    /// Longest one outbound frame may take to write before the peer is
    /// treated as gone.
    pub(crate) send_timeout: Duration,
}

impl Default for WsTimings {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(25),
            device_recheck: STREAM_DEVICE_RECHECK_INTERVAL,
            send_timeout: Duration::from_secs(10),
        }
    }
}

/// Why a connection is being closed, as the `code` of the `error` frame sent
/// before the close. Bounded and generic, like every client-facing error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// Not JSON, not the expected shape, not text, or failed to open.
    InvalidFrame,
    /// A sealed client frame whose `seq` is not the next one expected.
    OutOfOrder,
    /// A `t` this protocol does not define.
    UnknownType,
    InvalidAsid,
    TooManySubscriptions,
    FrameTooLarge,
    /// A newer socket from the same device took this one's place.
    Superseded,
}

impl FrameError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InvalidFrame => "invalid_frame",
            Self::OutOfOrder => "out_of_order",
            Self::UnknownType => "unknown_type",
            Self::InvalidAsid => "invalid_asid",
            Self::TooManySubscriptions => "too_many_subscriptions",
            Self::FrameTooLarge => "frame_too_large",
            Self::Superseded => "superseded",
        }
    }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// What a client may send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClientFrame {
    Subscribe(String),
    Unsubscribe(String),
    SubscribeAll,
    Ping,
}

#[derive(Deserialize)]
struct AsidField {
    asid: String,
}

impl ClientFrame {
    /// Parse one (opened) client frame. Extra fields are ignored so a later
    /// client can add one without breaking this gateway; an unknown `t` is
    /// not, because it names something this gateway would silently not do.
    pub(crate) fn parse(text: &str) -> Result<Self, FrameError> {
        let value: Value = serde_json::from_str(text).map_err(|_| FrameError::InvalidFrame)?;
        let kind = value
            .get("t")
            .and_then(Value::as_str)
            .ok_or(FrameError::InvalidFrame)?;
        let asid = || -> Result<String, FrameError> {
            let AsidField { asid } =
                serde_json::from_value(value.clone()).map_err(|_| FrameError::InvalidFrame)?;
            validate_asid(&asid)?;
            Ok(asid)
        };
        match kind {
            "subscribe" => Ok(Self::Subscribe(asid()?)),
            "unsubscribe" => Ok(Self::Unsubscribe(asid()?)),
            "subscribe_all" => Ok(Self::SubscribeAll),
            "ping" => Ok(Self::Ping),
            _ => Err(FrameError::UnknownType),
        }
    }
}

fn validate_asid(asid: &str) -> Result<(), FrameError> {
    if asid.is_empty() || asid.len() > MAX_ASID_BYTES || asid.chars().any(char::is_control) {
        return Err(FrameError::InvalidAsid);
    }
    Ok(())
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
}

/// `generation` is the gateway's instance generation (see
/// `AppState::generation`): a client that finds it changed since its last
/// connection is talking to a restarted gateway and drops what it holds.
pub(crate) fn hello_frame(connection_id: &str, generation: &str) -> String {
    format!(
        r#"{{"t":"hello","connection_id":{},"protocol":{WS_PROTOCOL},"generation":{}}}"#,
        json_string(connection_id),
        json_string(generation)
    )
}

/// One domain event. `event` and `data` are exactly the SSE stream's `event:`
/// and `data:` for the same event -- `data` embedded as the JSON it is, not
/// re-serialized, so the bytes are the same ones.
pub(crate) fn event_frame(event: &AgentDomainEvent) -> String {
    let (name, data) = agent_event_record(event);
    format!(
        r#"{{"t":"event","asid":{},"seq":{},"event":{},"data":{}}}"#,
        json_string(&event.asid().0),
        event.seq(),
        json_string(name),
        data
    )
}

pub(crate) fn resync_frame(asid: &str) -> String {
    format!(r#"{{"t":"resync","asid":{}}}"#, json_string(asid))
}

pub(crate) fn subscribed_frame(asid: Option<&str>) -> String {
    match asid {
        Some(asid) => format!(r#"{{"t":"subscribed","asid":{}}}"#, json_string(asid)),
        None => String::from(r#"{"t":"subscribed","all":true}"#),
    }
}

pub(crate) fn error_frame(error: FrameError) -> String {
    format!(r#"{{"t":"error","code":"{}"}}"#, error.code())
}

pub(crate) const PONG_FRAME: &str = r#"{"t":"pong"}"#;

// ---------------------------------------------------------------------------
// Sealing
// ---------------------------------------------------------------------------

/// Turns plaintext frames into what goes on the wire and back.
///
/// Plain when the device has no transport key. Sealed otherwise: each
/// direction has its own key and its own sequence counter, both from zero,
/// and the AAD is `{request_aad}\n{connection_id}\n{seq}`.
pub(crate) enum FrameCodec {
    Plain,
    Sealed(Box<SealedCodec>),
}

pub(crate) struct SealedCodec {
    server_key: [u8; 32],
    client_key: [u8; 32],
    connection_id: String,
    request_aad: String,
    send_seq: u64,
    recv_seq: u64,
}

#[derive(Deserialize)]
struct SealedFrame {
    seq: u64,
    c: String,
}

impl SealedCodec {
    pub(crate) fn new(
        material: &[u8],
        request_aad: &str,
        request_nonce: &str,
        connection_id: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            server_key: transport::derive_ws_server_key(material, connection_id, request_nonce)?,
            client_key: transport::derive_ws_client_key(material, connection_id, request_nonce)?,
            connection_id: connection_id.to_owned(),
            request_aad: request_aad.to_owned(),
            send_seq: 0,
            recv_seq: 0,
        })
    }

    fn aad(&self, seq: u64) -> String {
        format!("{}\n{}\n{}", self.request_aad, self.connection_id, seq)
    }
}

impl FrameCodec {
    pub(crate) fn for_context(
        context: Option<&EncryptedStreamContext>,
        connection_id: &str,
    ) -> anyhow::Result<Self> {
        Ok(match context {
            Some(context) => Self::Sealed(Box::new(SealedCodec::new(
                &context.material,
                &context.request_aad,
                &context.request_nonce,
                connection_id,
            )?)),
            None => Self::Plain,
        })
    }

    /// The wire text of one outbound frame. The first sealed frame also
    /// carries the connection id in the clear, because the client needs it to
    /// derive the key that opens that frame; it is authenticated all the same,
    /// through the key and the AAD.
    pub(crate) fn encode(&mut self, plaintext: &str) -> anyhow::Result<String> {
        let Self::Sealed(codec) = self else {
            return Ok(plaintext.to_owned());
        };
        let seq = codec.send_seq;
        let ciphertext = transport::seal_stream_event(
            &codec.server_key,
            seq,
            codec.aad(seq).as_bytes(),
            plaintext.as_bytes(),
        )?;
        // Counted only once sealing succeeded, so a failure is not a gap.
        codec.send_seq += 1;
        Ok(if seq == 0 {
            format!(
                r#"{{"seq":0,"cid":{},"c":"{ciphertext}"}}"#,
                json_string(&codec.connection_id)
            )
        } else {
            format!(r#"{{"seq":{seq},"c":"{ciphertext}"}}"#)
        })
    }

    /// The plaintext of one inbound frame. A sealed frame must carry exactly
    /// the next client seq; anything else -- a gap, a repeat, a reorder -- is
    /// refused before any decryption is attempted.
    pub(crate) fn decode(&mut self, text: &str) -> Result<String, FrameError> {
        let Self::Sealed(codec) = self else {
            return Ok(text.to_owned());
        };
        let frame: SealedFrame =
            serde_json::from_str(text).map_err(|_| FrameError::InvalidFrame)?;
        if frame.seq != codec.recv_seq {
            return Err(FrameError::OutOfOrder);
        }
        let plaintext = transport::open_stream_event(
            &codec.client_key,
            frame.seq,
            codec.aad(frame.seq).as_bytes(),
            &frame.c,
        )
        .map_err(|_| FrameError::InvalidFrame)?;
        codec.recv_seq += 1;
        String::from_utf8(plaintext).map_err(|_| FrameError::InvalidFrame)
    }
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// Which sessions one connection wants.
#[derive(Debug, Default)]
pub(crate) struct Subscriptions {
    all: bool,
    sessions: BTreeSet<String>,
}

impl Subscriptions {
    /// Apply a subscription frame, answering the acknowledgement to send.
    /// `subscribe_all` is sticky for the life of the connection; an
    /// `unsubscribe` after it only drops the asid from the explicit set.
    pub(crate) fn apply(&mut self, frame: &ClientFrame) -> Result<Option<String>, FrameError> {
        match frame {
            ClientFrame::Subscribe(asid) => {
                if !self.sessions.contains(asid) && self.sessions.len() >= MAX_SUBSCRIPTIONS {
                    return Err(FrameError::TooManySubscriptions);
                }
                self.sessions.insert(asid.clone());
                Ok(Some(subscribed_frame(Some(asid))))
            }
            ClientFrame::Unsubscribe(asid) => {
                self.sessions.remove(asid);
                Ok(None)
            }
            ClientFrame::SubscribeAll => {
                self.all = true;
                Ok(Some(subscribed_frame(None)))
            }
            ClientFrame::Ping => Ok(Some(PONG_FRAME.to_owned())),
        }
    }

    /// Whether an event for `asid` goes out on this connection. An event with
    /// no session -- a global resync, a worktree change -- reaches every
    /// connection that is watching anything, as it reaches every SSE stream.
    pub(crate) fn wants(&self, asid: &str) -> bool {
        if self.all {
            return true;
        }
        if asid.is_empty() {
            return !self.sessions.is_empty();
        }
        self.sessions.contains(asid)
    }

    /// The sessions to tell to resync after this connection fell behind the
    /// broadcast. `""` means every session, as it does on `agent.resync`.
    pub(crate) fn resync_targets(&self) -> Vec<String> {
        if self.all {
            vec![String::new()]
        } else {
            self.sessions.iter().cloned().collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Connection registry
// ---------------------------------------------------------------------------

/// The open sockets, for the per-device and total caps.
#[derive(Default)]
pub(crate) struct WsRegistry {
    inner: Mutex<RegistryInner>,
    pub(crate) timings: WsTimings,
}

#[derive(Default)]
struct RegistryInner {
    next_id: u64,
    open: Vec<OpenSocket>,
}

struct OpenSocket {
    id: u64,
    device_id: String,
    evict: oneshot::Sender<()>,
}

/// One admitted connection. Dropping it frees the slot -- including when the
/// upgrade never completes and the callback holding it is dropped unrun.
pub(crate) struct WsSlot {
    registry: Arc<WsRegistry>,
    id: u64,
    evicted: oneshot::Receiver<()>,
}

impl Drop for WsSlot {
    fn drop(&mut self) {
        let mut inner = self.registry.lock();
        inner.open.retain(|socket| socket.id != self.id);
    }
}

impl WsRegistry {
    #[cfg(test)]
    pub(crate) fn with_timings(timings: WsTimings) -> Self {
        Self {
            inner: Mutex::default(),
            timings,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        // The registry only counts; a poisoned lock still holds a usable list.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Admit a new socket for `device_id`, superseding that device's oldest
    /// when it is at its cap. `None` when the gateway as a whole is full.
    pub(crate) fn admit(self: &Arc<Self>, device_id: &str) -> Option<WsSlot> {
        let mut inner = self.lock();
        let mine = inner
            .open
            .iter()
            .filter(|socket| socket.device_id == device_id)
            .count();
        if mine >= MAX_CONNECTIONS_PER_DEVICE {
            if let Some(oldest) = inner
                .open
                .iter()
                .position(|socket| socket.device_id == device_id)
            {
                // `open` is in admission order, so the first is the oldest.
                let socket = inner.open.remove(oldest);
                let _ = socket.evict.send(());
            }
        }
        if inner.open.len() >= MAX_CONNECTIONS_TOTAL {
            return None;
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let (evict, evicted) = oneshot::channel();
        inner.open.push(OpenSocket {
            id,
            device_id: device_id.to_owned(),
            evict,
        });
        Some(WsSlot {
            registry: Arc::clone(self),
            id,
            evicted,
        })
    }

    #[cfg(test)]
    pub(crate) fn open_count(&self) -> usize {
        self.lock().open.len()
    }
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router.route(WS_PATH, get(open_event_socket))
}

/// The upgrade. Authorised before anything about the upgrade itself is
/// looked at, so a caller without a device learns nothing but `401`/`403`.
async fn open_event_socket(
    State(state): State<AppState>,
    stream_crypto: Option<Extension<EncryptedStreamContext>>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let device_id = match require_device(&state, &headers) {
        Ok(device_id) => device_id,
        Err(error) => return error.into_response(),
    };
    let Ok(upgrade) = upgrade else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "websocket_upgrade_required",
            "this route only answers a WebSocket upgrade",
        )
        .into_response();
    };
    let connection_id = uuid::Uuid::new_v4().to_string();
    let codec = match FrameCodec::for_context(
        stream_crypto.as_ref().map(|Extension(context)| context),
        &connection_id,
    ) {
        Ok(codec) => codec,
        Err(_) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "transport_key_unavailable",
                "encrypted transport is unavailable",
            )
            .into_response()
        }
    };
    let Some(slot) = state.ws_connections.admit(&device_id) else {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_connections",
            "too many open event sockets",
        )
        .into_response();
    };
    // Subscribed before the upgrade completes, so nothing published between
    // the 101 and the first poll of the connection is missed.
    let events = state.agent_runtime.subscribe_events();
    upgrade
        .max_message_size(MAX_INBOUND_FRAME_BYTES)
        .max_frame_size(MAX_INBOUND_FRAME_BYTES)
        .on_upgrade(move |socket| async move {
            let connection = Connection {
                socket,
                codec,
                timings: state.ws_connections.timings,
            };
            connection
                .run(state, device_id, connection_id, events, slot)
                .await;
        })
}

struct Connection {
    socket: WebSocket,
    codec: FrameCodec,
    timings: WsTimings,
}

/// Why `send` gave up; either way the connection is over.
struct Gone;

impl Connection {
    /// Seal (or not) and write one frame. A frame that cannot be sealed is
    /// dropped, never sent in the clear -- the SSE stream's rule.
    async fn send(&mut self, plaintext: &str) -> Result<(), Gone> {
        let wire = match self.codec.encode(plaintext) {
            Ok(wire) => wire,
            Err(error) => {
                tracing::warn!("failed to seal event socket frame: {error}");
                return Ok(());
            }
        };
        self.write(Message::Text(wire.into())).await
    }

    async fn write(&mut self, message: Message) -> Result<(), Gone> {
        match tokio::time::timeout(self.timings.send_timeout, self.socket.send(message)).await {
            Ok(Ok(())) => Ok(()),
            _ => Err(Gone),
        }
    }

    async fn close(&mut self) {
        let _ = self
            .write(Message::Close(Some(CloseFrame {
                code: close_code::POLICY,
                reason: "".into(),
            })))
            .await;
    }

    /// Say why, then close.
    async fn fail(&mut self, error: FrameError) {
        if self.send(&error_frame(error)).await.is_ok() {
            self.close().await;
        }
    }

    async fn run(
        mut self,
        state: AppState,
        device_id: String,
        connection_id: String,
        mut events: broadcast::Receiver<AgentDomainEvent>,
        mut slot: WsSlot,
    ) {
        if self
            .send(&hello_frame(&connection_id, &state.generation))
            .await
            .is_err()
        {
            return;
        }
        let mut subscriptions = Subscriptions::default();
        let start = tokio::time::Instant::now();
        let mut heartbeat =
            tokio::time::interval_at(start + self.timings.heartbeat, self.timings.heartbeat);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The authorisation this socket was opened on has to be rechecked for
        // as long as it is open; see `STREAM_DEVICE_RECHECK_INTERVAL`.
        let mut recheck = tokio::time::interval_at(
            start + self.timings.device_recheck,
            self.timings.device_recheck,
        );
        recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut unanswered_pings = 0_u8;

        loop {
            tokio::select! {
                _ = &mut slot.evicted => {
                    self.fail(FrameError::Superseded).await;
                    return;
                }
                _ = recheck.tick() => {
                    if !still_paired(&state, &device_id) {
                        // Closed without an `error` frame, as the SSE stream
                        // closes without an event: a revoked device is not
                        // owed an explanation, and a legitimate client learns
                        // the same thing from the 403 its reconnect earns.
                        self.close().await;
                        return;
                    }
                }
                _ = heartbeat.tick() => {
                    if unanswered_pings >= 2 {
                        return;
                    }
                    unanswered_pings += 1;
                    if self.write(Message::Ping(Default::default())).await.is_err() {
                        return;
                    }
                }
                inbound = self.socket.recv() => {
                    let text = match inbound {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Pong(_))) => {
                            unanswered_pings = 0;
                            continue;
                        }
                        // Answered by the socket itself.
                        Some(Ok(Message::Ping(_))) => continue,
                        Some(Ok(Message::Binary(_))) => {
                            self.fail(FrameError::InvalidFrame).await;
                            return;
                        }
                        Some(Err(error)) => {
                            if is_capacity_error(&error) {
                                self.fail(FrameError::FrameTooLarge).await;
                            }
                            return;
                        }
                        Some(Ok(Message::Close(_))) | None => return,
                    };
                    // Any frame from the client is proof it is alive.
                    unanswered_pings = 0;
                    let reply = self
                        .codec
                        .decode(text.as_str())
                        .and_then(|plaintext| ClientFrame::parse(&plaintext))
                        .and_then(|frame| subscriptions.apply(&frame));
                    match reply {
                        Ok(Some(reply)) => {
                            if self.send(&reply).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            self.fail(error).await;
                            return;
                        }
                    }
                }
                event = events.recv() => match event {
                    Ok(event) => {
                        if subscriptions.wants(&event.asid().0)
                            && self.send(&event_frame(&event)).await.is_err()
                        {
                            return;
                        }
                    }
                    // Fell behind the broadcast: some events for some
                    // sessions are gone, and which ones is unknowable. Every
                    // session this socket watches refetches, exactly as the
                    // SSE stream's `agent.resync` asks its one session to.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for asid in subscriptions.resync_targets() {
                            if self.send(&resync_frame(&asid)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        self.close().await;
                        return;
                    }
                },
            }
        }
    }
}

fn is_capacity_error(error: &axum::Error) -> bool {
    // axum wraps tungstenite's error without re-exporting its kinds; the
    // capacity errors are the ones that name the size.
    let text = error.to_string().to_ascii_lowercase();
    text.contains("capacity") || text.contains("too long") || text.contains("too big")
}

#[cfg(test)]
mod tests;
