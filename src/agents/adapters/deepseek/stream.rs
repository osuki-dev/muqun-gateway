//! The live side of the DeepSeek Harness adapter: one WebSocket to
//! `/api/remote.mux`, the Typert Remote stream multiplexer.
//!
//! # What DeepSeek Harness offers
//!
//! Checked against the installed `@deepseek-ai/dsh` packages rather than
//! guessed:
//!
//! - `dsh-api-gateway/lib/index.js` owns the mux. A client frame is
//!   `{type:"open", streamId, endpoint, payload}` where `payload` must be
//!   exactly `{args: {...}}`, `{type:"cancel", streamId}`,
//!   `{type:"item", streamId, value?}` or `{type:"end", streamId}`. The host
//!   answers `{type:"item", streamId, value}`, `{type:"error", streamId,
//!   error}` and `{type:"end", streamId}`. Any number of logical streams share
//!   one socket, which is why this module keeps exactly one.
//! - `session/follow` (`dsh-api-session-controller/lib/index.js`,
//!   `SessionHistoryController.follow`) is **per session**: its request names
//!   one `SessionAddress`. It opens with a `snapshot` frame (header, recent
//!   records, projections, optionally the in-flight assistant attempt), then
//!   yields every durable `event` and, with `assistantStream: true`, the
//!   process-local `assistant-stream` frames that carry text, reasoning and
//!   tool-argument deltas. Following a cold session also *activates* its
//!   agent (`promote`), so the listener follows only sessions the host says
//!   are live rather than everything `session/list` returns.
//! - `$events` is the host-wide stream (`openRemoteEvents`). It begins with
//!   `{type:"ready", clientId, host}` and then forwards the Cordis events
//!   allow-listed in `dsh-api-remotes/lib/index.js`: `api-session/added`,
//!   `api-session/removed`, `api-session/status`, `api-session/activity` and
//!   `api-session/error` as `{type:"emit", event, args}`, and the two
//!   interactive waterfalls `approval/request` and `user-questions/request`
//!   as `{type:"waterfall", event, eventId, agentId, request}`, withdrawn by
//!   `{type:"cancel", eventId}`. This is the session-list stream: it is how
//!   the listener learns which sessions to follow and when to stop.
//! - There is no RPC for answering an approval. A waterfall is settled with
//!   the unary `$events/result` call carrying `{clientId, eventId, outcome}`;
//!   `clientId` is the one the current `$events` generation was given, so it
//!   changes on every reconnect and is kept in [`DeepseekInteractions`] for
//!   the driver to read.
//! - Admission (`dsh-client-connection/lib/index.js`, `admit`) is the
//!   Host/Origin fence plus the signed `dsh-auth-<authority>` cookie. The
//!   launch token is only exchanged on `GET /`; neither a bearer header nor
//!   a `?token=` query is honoured on the mux, so the token is never put in
//!   the URL. The host pings every two seconds and closes after missed pongs.
//!
//! # Shape
//!
//! The listener parses frames into [`DeepseekStreamEvent`]s and publishes
//! them on a broadcast channel; `manager/deepseek.rs` turns them into domain
//! events and updates the mirror, the same split as the OpenCode SSE
//! listener. `follow`/`unfollow` are commands from that side; the followed
//! set survives a reconnect and is re-opened on every new socket.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use super::endpoint::DeepseekEndpoint;
use crate::agents::domain::{AgentSessionId, FormRequest, PermissionRequest};

/// The host-wide forwarded-event stream endpoint.
pub const EVENTS_ENDPOINT: &str = "$events";
/// The per-session live stream endpoint.
pub const FOLLOW_ENDPOINT: &str = "session/follow";
/// How many recent messages a follow's opening snapshot carries. The
/// timeline is otherwise read back through `session/page`.
const FOLLOW_SNAPSHOT_MESSAGES: u64 = 20;
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Messages sent from the gateway to the DeepSeek Harness stream mux.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum RemoteStreamClientMessage {
    #[serde(rename = "open")]
    Open {
        #[serde(rename = "streamId")]
        stream_id: String,
        endpoint: String,
        payload: Value,
    },
    #[serde(rename = "cancel")]
    Cancel {
        #[serde(rename = "streamId")]
        stream_id: String,
    },
    #[serde(rename = "item")]
    Item {
        #[serde(rename = "streamId")]
        stream_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
    },
    #[serde(rename = "end")]
    End {
        #[serde(rename = "streamId")]
        stream_id: String,
    },
}

/// Messages received from the DeepSeek Harness stream mux.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum RemoteStreamServerMessage {
    #[serde(rename = "item")]
    Item {
        #[serde(rename = "streamId")]
        stream_id: String,
        #[serde(default)]
        value: Value,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(rename = "streamId")]
        stream_id: String,
        error: Value,
    },
    #[serde(rename = "end")]
    End {
        #[serde(rename = "streamId")]
        stream_id: String,
    },
}

/// Parse a raw text WebSocket frame into a `RemoteStreamServerMessage`.
pub fn parse_stream_server_message(text: &str) -> Result<RemoteStreamServerMessage, String> {
    serde_json::from_str::<RemoteStreamServerMessage>(text)
        .map_err(|e| format!("Failed to parse stream message: {e}"))
}

/// Format an `open` frame.
pub fn format_open_stream(stream_id: &str, endpoint: &str, payload: Value) -> String {
    let msg = RemoteStreamClientMessage::Open {
        stream_id: stream_id.to_string(),
        endpoint: endpoint.to_string(),
        payload,
    };
    serde_json::to_string(&msg).unwrap_or_default()
}

/// Format a `cancel` frame.
pub fn format_cancel_stream(stream_id: &str) -> String {
    let msg = RemoteStreamClientMessage::Cancel {
        stream_id: stream_id.to_string(),
    };
    serde_json::to_string(&msg).unwrap_or_default()
}

/// The `session/follow` request for one ordinary session, with the live
/// assistant frames opted in.
pub fn follow_open_payload(session_id: &str) -> Value {
    json!({
        "args": {
            "request": {
                "address": { "kind": "session", "sessionId": session_id },
                "maxMessages": FOLLOW_SNAPSHOT_MESSAGES,
                "assistantStream": true
            }
        }
    })
}

/// The `$events` open payload: the host insists on an empty `args`.
pub fn events_open_payload() -> Value {
    json!({ "args": {} })
}

/// One item of the `$events` stream, as `dsh-api-gateway` sends it.
#[derive(Debug, Clone, PartialEq)]
pub enum EventsFrame {
    Ready {
        client_id: String,
    },
    Emit {
        event: String,
        args: Vec<Value>,
    },
    Waterfall {
        event: String,
        event_id: String,
        agent_id: String,
        request: Value,
    },
    Cancel {
        event_id: String,
    },
}

/// Parse one `$events` item. Unknown shapes are `None`, not errors: the
/// allow-list is the host's to grow.
pub fn parse_events_frame(value: &Value) -> Option<EventsFrame> {
    let kind = value.get("type").and_then(Value::as_str)?;
    match kind {
        "ready" => Some(EventsFrame::Ready {
            client_id: value.get("clientId").and_then(Value::as_str)?.to_string(),
        }),
        "emit" => Some(EventsFrame::Emit {
            event: value.get("event").and_then(Value::as_str)?.to_string(),
            args: value
                .get("args")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        }),
        "waterfall" => Some(EventsFrame::Waterfall {
            event: value.get("event").and_then(Value::as_str)?.to_string(),
            event_id: value.get("eventId").and_then(Value::as_str)?.to_string(),
            agent_id: value.get("agentId").and_then(Value::as_str)?.to_string(),
            request: value.get("request").cloned().unwrap_or(Value::Null),
        }),
        "cancel" => Some(EventsFrame::Cancel {
            event_id: value.get("eventId").and_then(Value::as_str)?.to_string(),
        }),
        _ => None,
    }
}

/// What the listener publishes. `Follow` carries a raw `SessionFollowFrame`
/// (`snapshot`, `event` or `assistant-stream`); the manager maps it.
#[derive(Debug, Clone, PartialEq)]
pub enum DeepseekStreamEvent {
    /// The socket is open. The manager seeds the followed set on this.
    Connected,
    Disconnected,
    Events(EventsFrame),
    Follow {
        session_id: String,
        frame: Value,
    },
    /// The host closed one follow stream, with its error when it failed.
    FollowEnded {
        session_id: String,
        error: Option<Value>,
    },
}

/// A permission prompt the host is waiting on, with the session it belongs
/// to.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub asid: AgentSessionId,
    pub request: PermissionRequest,
}

/// A question batch the host is waiting on. `questions` is DeepSeek Harness's own
/// request, kept so the answer can be rebuilt in its vocabulary.
#[derive(Debug, Clone)]
pub struct PendingQuestion {
    pub asid: AgentSessionId,
    pub request: FormRequest,
    pub questions: Value,
}

#[derive(Default)]
struct InteractionState {
    client_id: Option<String>,
    approvals: HashMap<String, PendingApproval>,
    questions: HashMap<String, PendingQuestion>,
    /// Ids seen before the current `$events` generation and not yet
    /// re-delivered by it.
    unconfirmed: HashSet<String>,
    generation: u64,
}

/// The interactive requests the host has forwarded and not yet settled, and
/// the `clientId` that settles them.
///
/// DeepSeek Harness has no query for pending approvals -- they live in the
/// gateway's memory until answered -- so this is the record. It is shared by
/// the listener, which fills it, and the driver, which answers from it; the
/// two only meet through [`DeepseekInteractions::for_endpoint`], keyed by the
/// endpoint URL, because the driver is built before the listener exists.
#[derive(Default)]
pub struct DeepseekInteractions {
    state: Mutex<InteractionState>,
}

static INTERACTIONS: OnceLock<Mutex<HashMap<String, Arc<DeepseekInteractions>>>> = OnceLock::new();

impl DeepseekInteractions {
    /// The registry for one DeepSeek Harness endpoint, created on first use.
    pub fn for_endpoint(endpoint_url: &str) -> Arc<Self> {
        let map = INTERACTIONS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut map = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        map.entry(endpoint_url.trim_end_matches('/').to_string())
            .or_default()
            .clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InteractionState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The `clientId` of the live `$events` generation, if there is one.
    pub fn client_id(&self) -> Option<String> {
        self.lock().client_id.clone()
    }

    /// A new `$events` generation opened. Everything pending is now
    /// unconfirmed until the host re-delivers it; returns the generation
    /// number for [`Self::expire_unconfirmed`].
    pub fn begin_generation(&self, client_id: String) -> u64 {
        let mut state = self.lock();
        state.client_id = Some(client_id);
        state.generation += 1;
        state.unconfirmed = state
            .approvals
            .keys()
            .chain(state.questions.keys())
            .cloned()
            .collect();
        state.generation
    }

    /// The socket dropped: nothing can be answered until the next `ready`.
    pub fn clear_client(&self) {
        self.lock().client_id = None;
    }

    /// Forget the pending requests the generation `generation` never
    /// re-delivered: the host settled them while the gateway was away.
    /// Returns them so the caller can resolve them in the mirror.
    pub fn expire_unconfirmed(
        &self,
        generation: u64,
    ) -> (Vec<PendingApproval>, Vec<PendingQuestion>) {
        let mut state = self.lock();
        if state.generation != generation {
            return (Vec::new(), Vec::new());
        }
        let stale = std::mem::take(&mut state.unconfirmed);
        let mut approvals = Vec::new();
        let mut questions = Vec::new();
        for id in stale {
            if let Some(a) = state.approvals.remove(&id) {
                approvals.push(a);
            }
            if let Some(q) = state.questions.remove(&id) {
                questions.push(q);
            }
        }
        (approvals, questions)
    }

    pub fn add_approval(&self, event_id: &str, pending: PendingApproval) {
        let mut state = self.lock();
        state.unconfirmed.remove(event_id);
        state.approvals.insert(event_id.to_string(), pending);
    }

    pub fn add_question(&self, event_id: &str, pending: PendingQuestion) {
        let mut state = self.lock();
        state.unconfirmed.remove(event_id);
        state.questions.insert(event_id.to_string(), pending);
    }

    pub fn approval(&self, event_id: &str) -> Option<PendingApproval> {
        self.lock().approvals.get(event_id).cloned()
    }

    pub fn question(&self, event_id: &str) -> Option<PendingQuestion> {
        self.lock().questions.get(event_id).cloned()
    }

    /// Drop one settled request, whichever kind it was.
    pub fn remove(&self, event_id: &str) -> (Option<PendingApproval>, Option<PendingQuestion>) {
        let mut state = self.lock();
        state.unconfirmed.remove(event_id);
        (
            state.approvals.remove(event_id),
            state.questions.remove(event_id),
        )
    }

    pub fn approvals_for(&self, asid: &str) -> Vec<PermissionRequest> {
        self.lock()
            .approvals
            .values()
            .filter(|p| p.asid.0 == asid)
            .map(|p| p.request.clone())
            .collect()
    }

    pub fn questions_for(&self, asid: &str) -> Vec<FormRequest> {
        self.lock()
            .questions
            .values()
            .filter(|p| p.asid.0 == asid)
            .map(|p| p.request.clone())
            .collect()
    }
}

enum ListenerCommand {
    Follow(String),
    Unfollow(String),
}

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// Per-socket bookkeeping: which logical stream id is which.
#[derive(Default)]
struct OpenStreams {
    events_stream_id: Option<String>,
    /// stream id -> session id
    follows: HashMap<String, String>,
    /// session id -> stream id
    by_session: HashMap<String, String>,
}

pub struct DeepseekStreamListener {
    endpoint: DeepseekEndpoint,
    sender: broadcast::Sender<DeepseekStreamEvent>,
    running: watch::Sender<bool>,
    connected: Arc<AtomicBool>,
    commands: mpsc::UnboundedSender<ListenerCommand>,
    commands_rx: Mutex<Option<mpsc::UnboundedReceiver<ListenerCommand>>>,
    /// The sessions meant to be followed, kept here so a reconnect re-opens
    /// them all.
    followed: Arc<Mutex<HashSet<String>>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DeepseekStreamListener {
    pub fn new(endpoint: DeepseekEndpoint) -> (Self, broadcast::Receiver<DeepseekStreamEvent>) {
        let (sender, rx) = broadcast::channel(1024);
        let (running, _) = watch::channel(false);
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let listener = Self {
            endpoint,
            sender,
            running,
            connected: Arc::new(AtomicBool::new(false)),
            commands,
            commands_rx: Mutex::new(Some(commands_rx)),
            followed: Arc::new(Mutex::new(HashSet::new())),
            task: Mutex::new(None),
        };
        (listener, rx)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DeepseekStreamEvent> {
        self.sender.subscribe()
    }

    /// Whether this listener is still meant to be reading.
    pub fn is_running(&self) -> bool {
        *self.running.borrow()
    }

    /// True while the mux WebSocket is actually open.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// The sessions currently meant to be followed.
    pub fn followed(&self) -> Vec<String> {
        self.followed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Follow a session's live stream. Idempotent; takes effect on the
    /// current socket and again on every reconnect.
    pub fn follow(&self, session_id: &str) {
        let inserted = self
            .followed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(session_id.to_string());
        if inserted {
            let _ = self
                .commands
                .send(ListenerCommand::Follow(session_id.to_string()));
        }
    }

    pub fn unfollow(&self, session_id: &str) {
        let removed = self
            .followed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(session_id);
        if removed {
            let _ = self
                .commands
                .send(ListenerCommand::Unfollow(session_id.to_string()));
        }
    }

    /// Stop reading. The task notices within one scheduler tick, whether it
    /// is connecting, reading, or sleeping out a backoff.
    pub fn stop(&self) {
        let _ = self.running.send(false);
        self.connected.store(false, Ordering::SeqCst);
    }

    /// Wait for the reader task to finish, for tests of `stop`.
    #[cfg(test)]
    pub async fn join(&self) {
        let task = self.task.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    pub fn start(&self) {
        if self.running.send_replace(true) {
            return;
        }
        let Some(mut commands_rx) = self
            .commands_rx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        else {
            return;
        };

        let endpoint = self.endpoint.clone();
        let sender = self.sender.clone();
        let mut running = self.running.subscribe();
        let connected = self.connected.clone();
        let followed = self.followed.clone();

        let task = tokio::spawn(async move {
            let mut backoff = INITIAL_BACKOFF;
            let stop = |running: &mut watch::Receiver<bool>| !*running.borrow_and_update();
            loop {
                if stop(&mut running) {
                    break;
                }
                // Anything queued while there was no socket is subsumed by
                // the followed set, which is re-opened wholesale below.
                while commands_rx.try_recv().is_ok() {}

                let request = match build_request(&endpoint) {
                    Ok(request) => request,
                    Err(err) => {
                        tracing::warn!(%err, "deepseek stream request could not be built");
                        break;
                    }
                };

                tracing::info!(url = %endpoint.ws_url, "connecting to deepseek stream mux");
                let connection = tokio::select! {
                    result = connect_async(request) => result,
                    _ = running.changed() => break,
                };

                match connection {
                    Ok((ws, _)) => {
                        backoff = INITIAL_BACKOFF;
                        connected.store(true, Ordering::SeqCst);
                        tracing::info!("deepseek stream mux connected");
                        let _ = sender.send(DeepseekStreamEvent::Connected);
                        run_socket(ws, &sender, &mut running, &mut commands_rx, &followed).await;
                        connected.store(false, Ordering::SeqCst);
                        let _ = sender.send(DeepseekStreamEvent::Disconnected);
                        tracing::warn!("deepseek stream mux disconnected");
                    }
                    Err(err) => {
                        connected.store(false, Ordering::SeqCst);
                        tracing::warn!(%err, "deepseek stream mux unreachable");
                    }
                }

                if stop(&mut running) {
                    break;
                }
                // Always sleep before the next attempt: a failed open used to
                // `continue` straight back into `connect_async`.
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = running.changed() => break,
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            connected.store(false, Ordering::SeqCst);
            tracing::info!("deepseek stream listener stopped");
        });
        *self.task.lock().unwrap_or_else(|p| p.into_inner()) = Some(task);
    }
}

/// The upgrade request: the signed cookie is the credential and `Host` must
/// name the authority the cookie was minted for. No token in the URL.
fn build_request(
    endpoint: &DeepseekEndpoint,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    let mut request = endpoint
        .ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| e.to_string())?;
    match endpoint.auth_cookie() {
        Some(cookie) => {
            let value = HeaderValue::from_str(&cookie).map_err(|e| e.to_string())?;
            request.headers_mut().insert("Cookie", value);
        }
        None => {
            tracing::warn!(
                "deepseek stream: no signing secret, the mux upgrade will be refused (401)"
            );
        }
    }
    if let Ok(value) = HeaderValue::from_str(&endpoint.authority()) {
        request.headers_mut().insert("Host", value);
    }
    Ok(request)
}

fn new_stream_id() -> String {
    format!("stream_{}", Uuid::new_v4().simple())
}

async fn open_follow(write: &mut WsSink, open: &mut OpenStreams, session_id: &str) -> bool {
    if open.by_session.contains_key(session_id) {
        return true;
    }
    let stream_id = new_stream_id();
    let frame = format_open_stream(&stream_id, FOLLOW_ENDPOINT, follow_open_payload(session_id));
    if let Err(err) = write.send(Message::Text(frame.into())).await {
        tracing::warn!(%err, session_id, "deepseek follow open failed");
        return false;
    }
    open.follows
        .insert(stream_id.clone(), session_id.to_string());
    open.by_session.insert(session_id.to_string(), stream_id);
    true
}

async fn cancel_follow(write: &mut WsSink, open: &mut OpenStreams, session_id: &str) {
    let Some(stream_id) = open.by_session.remove(session_id) else {
        return;
    };
    open.follows.remove(&stream_id);
    let frame = format_cancel_stream(&stream_id);
    if let Err(err) = write.send(Message::Text(frame.into())).await {
        tracing::debug!(%err, session_id, "deepseek follow cancel failed");
    }
}

/// Drive one open socket until it closes or `stop` is called.
async fn run_socket(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    sender: &broadcast::Sender<DeepseekStreamEvent>,
    running: &mut watch::Receiver<bool>,
    commands_rx: &mut mpsc::UnboundedReceiver<ListenerCommand>,
    followed: &Arc<Mutex<HashSet<String>>>,
) {
    let (mut write, mut read) = ws.split();
    let mut open = OpenStreams::default();

    // The host-wide stream first: it is what tells us which sessions exist.
    let events_stream_id = new_stream_id();
    let frame = format_open_stream(&events_stream_id, EVENTS_ENDPOINT, events_open_payload());
    if let Err(err) = write.send(Message::Text(frame.into())).await {
        tracing::warn!(%err, "deepseek $events open failed");
        return;
    }
    open.events_stream_id = Some(events_stream_id);

    let wanted: Vec<String> = followed
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .cloned()
        .collect();
    for session_id in wanted {
        if !open_follow(&mut write, &mut open, &session_id).await {
            return;
        }
    }

    loop {
        tokio::select! {
            _ = running.changed() => {
                if !*running.borrow() {
                    let _ = write.send(Message::Close(None)).await;
                    return;
                }
            }
            command = commands_rx.recv() => {
                match command {
                    Some(ListenerCommand::Follow(session_id)) => {
                        if !open_follow(&mut write, &mut open, &session_id).await {
                            return;
                        }
                    }
                    Some(ListenerCommand::Unfollow(session_id)) => {
                        cancel_follow(&mut write, &mut open, &session_id).await;
                    }
                    None => return,
                }
            }
            frame = read.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        if !handle_text_frame(&text, &mut open, sender) {
                            return;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if write.send(Message::Pong(data)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        tracing::info!("deepseek stream mux closed by host");
                        return;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        tracing::warn!(%err, "deepseek stream mux read failed");
                        return;
                    }
                    None => return,
                }
            }
        }
    }
}

/// Route one text frame to its logical stream. Returns false when the
/// socket is no longer useful (the `$events` stream ended).
fn handle_text_frame(
    text: &str,
    open: &mut OpenStreams,
    sender: &broadcast::Sender<DeepseekStreamEvent>,
) -> bool {
    let message = match parse_stream_server_message(text) {
        Ok(message) => message,
        Err(err) => {
            tracing::debug!(%err, "deepseek stream frame not understood");
            return true;
        }
    };
    let is_events = |id: &str| open.events_stream_id.as_deref() == Some(id);
    match message {
        RemoteStreamServerMessage::Item { stream_id, value } => {
            if is_events(&stream_id) {
                match parse_events_frame(&value) {
                    Some(frame) => {
                        let _ = sender.send(DeepseekStreamEvent::Events(frame));
                    }
                    None => {
                        tracing::trace!(?value, "deepseek $events item not mapped");
                    }
                }
            } else if let Some(session_id) = open.follows.get(&stream_id) {
                let _ = sender.send(DeepseekStreamEvent::Follow {
                    session_id: session_id.clone(),
                    frame: value,
                });
            } else {
                tracing::trace!(stream_id, "item for an unknown stream");
            }
            true
        }
        RemoteStreamServerMessage::Error { stream_id, error } => {
            if is_events(&stream_id) {
                tracing::warn!(?error, "deepseek $events stream failed, reconnecting");
                return false;
            }
            if let Some(session_id) = open.follows.remove(&stream_id) {
                open.by_session.remove(&session_id);
                tracing::warn!(session_id, ?error, "deepseek follow stream failed");
                let _ = sender.send(DeepseekStreamEvent::FollowEnded {
                    session_id,
                    error: Some(error),
                });
            }
            true
        }
        RemoteStreamServerMessage::End { stream_id } => {
            if is_events(&stream_id) {
                tracing::warn!("deepseek $events stream ended, reconnecting");
                return false;
            }
            if let Some(session_id) = open.follows.remove(&stream_id) {
                open.by_session.remove(&session_id);
                let _ = sender.send(DeepseekStreamEvent::FollowEnded {
                    session_id,
                    error: None,
                });
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    #[test]
    fn serializes_open_stream_message() {
        let msg = format_open_stream("stream_1", FOLLOW_ENDPOINT, follow_open_payload("s_1"));
        let parsed: Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["type"], "open");
        assert_eq!(parsed["streamId"], "stream_1");
        assert_eq!(parsed["endpoint"], "session/follow");
        // `remoteRequest` in dsh-api-gateway insists on exactly one `args`
        // key, and the follow descriptor on exactly one `request` argument.
        assert_eq!(parsed["payload"].as_object().unwrap().len(), 1);
        assert_eq!(parsed["payload"]["args"].as_object().unwrap().len(), 1);
        assert_eq!(
            parsed["payload"]["args"]["request"]["address"],
            json!({ "kind": "session", "sessionId": "s_1" })
        );
        assert_eq!(
            parsed["payload"]["args"]["request"]["assistantStream"],
            true
        );
    }

    #[test]
    fn the_events_open_payload_is_an_empty_args_object() {
        // `openRemoteEvents` rejects anything but `{args: {}}`.
        assert_eq!(events_open_payload(), json!({ "args": {} }));
        let cancel: Value = serde_json::from_str(&format_cancel_stream("stream_2")).unwrap();
        assert_eq!(cancel, json!({ "type": "cancel", "streamId": "stream_2" }));
    }

    #[test]
    fn parses_server_stream_item() {
        let raw = json!({
            "type": "item",
            "streamId": "stream_1",
            "value": { "type": "snapshot", "header": { "version": 1 } }
        });
        let msg = parse_stream_server_message(&serde_json::to_string(&raw).unwrap()).unwrap();
        match msg {
            RemoteStreamServerMessage::Item { stream_id, value } => {
                assert_eq!(stream_id, "stream_1");
                assert_eq!(value["type"], "snapshot");
            }
            _ => panic!("Expected Item variant"),
        }
        let err = json!({ "type": "error", "streamId": "stream_1",
                          "error": { "code": "gateway/internal", "message": "x", "details": {} } });
        assert!(matches!(
            parse_stream_server_message(&err.to_string()).unwrap(),
            RemoteStreamServerMessage::Error { .. }
        ));
        assert!(parse_stream_server_message("{not json").is_err());
    }

    #[test]
    fn parses_every_events_frame_shape() {
        // Shapes from `openRemoteEvents`, `broadcastRemoteEvent`,
        // `startRemoteEvent` and `finishRemoteEvent` in dsh-api-gateway.
        assert_eq!(
            parse_events_frame(
                &json!({ "type": "ready", "clientId": "c1", "host": { "home": "/h" } })
            ),
            Some(EventsFrame::Ready {
                client_id: "c1".into()
            })
        );
        assert_eq!(
            parse_events_frame(
                &json!({ "type": "emit", "event": "api-session/status", "args": ["s1", true] })
            ),
            Some(EventsFrame::Emit {
                event: "api-session/status".into(),
                args: vec![json!("s1"), json!(true)]
            })
        );
        assert_eq!(
            parse_events_frame(&json!({
                "type": "waterfall", "event": "approval/request", "eventId": "e1",
                "agentId": "s1", "request": { "toolName": "bash" }
            })),
            Some(EventsFrame::Waterfall {
                event: "approval/request".into(),
                event_id: "e1".into(),
                agent_id: "s1".into(),
                request: json!({ "toolName": "bash" })
            })
        );
        assert_eq!(
            parse_events_frame(&json!({ "type": "cancel", "eventId": "e1" })),
            Some(EventsFrame::Cancel {
                event_id: "e1".into()
            })
        );
        assert_eq!(parse_events_frame(&json!({ "type": "mystery" })), None);
        assert_eq!(parse_events_frame(&json!({ "type": "waterfall" })), None);
    }

    #[test]
    fn frames_are_routed_by_stream_id() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut open = OpenStreams {
            events_stream_id: Some("ev".into()),
            follows: HashMap::from([("f1".to_string(), "ses_1".to_string())]),
            by_session: HashMap::from([("ses_1".to_string(), "f1".to_string())]),
        };
        let item = json!({ "type": "item", "streamId": "f1", "value": { "type": "event", "event": { "type": "turn/start", "seq": 3, "time": 1, "data": { "turn": 1 } } } });
        assert!(handle_text_frame(&item.to_string(), &mut open, &tx));
        assert!(matches!(
            rx.try_recv().unwrap(),
            DeepseekStreamEvent::Follow { session_id, frame } if session_id == "ses_1" && frame["type"] == "event"
        ));

        let ready = json!({ "type": "item", "streamId": "ev", "value": { "type": "ready", "clientId": "c", "host": { "home": "/" } } });
        assert!(handle_text_frame(&ready.to_string(), &mut open, &tx));
        assert_eq!(
            rx.try_recv().unwrap(),
            DeepseekStreamEvent::Events(EventsFrame::Ready {
                client_id: "c".into()
            })
        );

        let end = json!({ "type": "end", "streamId": "f1" });
        assert!(handle_text_frame(&end.to_string(), &mut open, &tx));
        assert_eq!(
            rx.try_recv().unwrap(),
            DeepseekStreamEvent::FollowEnded {
                session_id: "ses_1".into(),
                error: None
            }
        );
        assert!(open.follows.is_empty() && open.by_session.is_empty());

        // The host-wide stream ending means the socket is no longer useful.
        let end = json!({ "type": "end", "streamId": "ev" });
        assert!(!handle_text_frame(&end.to_string(), &mut open, &tx));
        // Garbage is skipped, not fatal.
        assert!(handle_text_frame("nope", &mut open, &tx));
    }

    #[test]
    fn the_upgrade_request_carries_the_cookie_and_never_the_token() {
        let endpoint = DeepseekEndpoint::new(
            "http://127.0.0.1:3080",
            Some("launch-token".into()),
            Some("Qz1iQUfg3Hve5G6HgLLfie-xeSvi016I1X24SBL-WC8".into()),
        );
        let request = build_request(&endpoint).unwrap();
        assert_eq!(request.uri().path(), "/api/remote.mux");
        assert!(request.uri().query().is_none(), "no token in the query");
        let cookie = request.headers().get("Cookie").unwrap().to_str().unwrap();
        assert!(cookie.starts_with("dsh-auth-"));
        assert!(!cookie.contains("launch-token"));
        assert!(request.headers().get("Authorization").is_none());
        assert_eq!(request.headers().get("Host").unwrap(), "127.0.0.1:3080");
    }

    #[test]
    fn the_interaction_registry_tracks_generations() {
        let reg = DeepseekInteractions::default();
        let asid = AgentSessionId("s1".into());
        let request = PermissionRequest {
            id: "e1".into(),
            asid: asid.clone(),
            action: "bash".into(),
            resources: vec![],
            save: vec![],
            prompt: "p".into(),
            tool: None,
            source_message_id: None,
            source_tool_call_id: None,
            metadata: None,
            message: None,
            options: vec![],
        };
        reg.add_approval(
            "e1",
            PendingApproval {
                asid: asid.clone(),
                request: request.clone(),
            },
        );
        reg.add_approval(
            "e2",
            PendingApproval {
                asid: asid.clone(),
                request: request.clone(),
            },
        );
        assert_eq!(reg.approvals_for("s1").len(), 2);
        assert!(reg.client_id().is_none());

        let generation = reg.begin_generation("client-a".into());
        assert_eq!(reg.client_id().as_deref(), Some("client-a"));
        // The host re-delivers e1 but not e2: e2 was settled while away.
        reg.add_approval("e1", PendingApproval { asid, request });
        let (stale, _) = reg.expire_unconfirmed(generation);
        assert_eq!(stale.len(), 1);
        assert_eq!(
            stale[0].request.id, "e1",
            "the entry is the same request under the settled id"
        );
        assert_eq!(reg.approvals_for("s1").len(), 1);
        assert!(reg.approval("e1").is_some() && reg.approval("e2").is_none());

        // A stale generation number expires nothing.
        assert!(reg.expire_unconfirmed(generation + 5).0.is_empty());
        reg.clear_client();
        assert!(reg.client_id().is_none());
    }

    #[test]
    fn the_registry_is_shared_per_endpoint() {
        let a = DeepseekInteractions::for_endpoint("http://127.0.0.1:65001");
        let b = DeepseekInteractions::for_endpoint("http://127.0.0.1:65001/");
        let c = DeepseekInteractions::for_endpoint("http://127.0.0.1:65002");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
    }

    /// The server accepts the TCP connection and never completes the
    /// WebSocket handshake, which is the case that used to block `stop`.
    #[tokio::test]
    async fn stop_interrupts_a_hanging_handshake() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        let hold = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (socket, _) = server.accept().await.unwrap();
                held.push(socket);
            }
        });

        let endpoint = DeepseekEndpoint::new(format!("http://{addr}"), None, None);
        let (listener, _rx) = DeepseekStreamListener::new(endpoint);
        listener.start();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(listener.is_running());
        assert!(!listener.is_connected());

        listener.stop();
        tokio::time::timeout(Duration::from_secs(2), listener.join())
            .await
            .expect("the reader task ends promptly after stop");
        assert!(!listener.is_running());
        hold.abort();
    }

    /// Nothing listens on the port: the loop is in its backoff sleep when
    /// `stop` arrives, and must not wait the sleep out.
    #[tokio::test]
    async fn stop_interrupts_the_reconnect_backoff() {
        let endpoint = DeepseekEndpoint::new("http://127.0.0.1:1", None, None);
        let (listener, _rx) = DeepseekStreamListener::new(endpoint);
        listener.start();
        tokio::time::sleep(Duration::from_millis(150)).await;
        listener.stop();
        tokio::time::timeout(Duration::from_millis(400), listener.join())
            .await
            .expect("stop returns before the backoff elapses");
    }

    /// A minimal mux: accept the upgrade, answer the `$events` open with
    /// `ready` and one waterfall, echo follow opens with a snapshot, then
    /// hold the socket. Checks that connection state is real, that frames
    /// reach subscribers, and that `stop` ends an idle read.
    #[tokio::test]
    async fn a_connected_socket_reports_connected_and_delivers_frames() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (socket, _) = server.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let mut seen_opens = Vec::new();
            while let Some(Ok(message)) = ws.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let frame: Value = serde_json::from_str(&text).unwrap();
                if frame["type"] != "open" {
                    continue;
                }
                let stream_id = frame["streamId"].as_str().unwrap().to_string();
                seen_opens.push(frame["endpoint"].as_str().unwrap().to_string());
                if frame["endpoint"] == EVENTS_ENDPOINT {
                    assert_eq!(frame["payload"], json!({ "args": {} }));
                    let ready = json!({ "type": "item", "streamId": stream_id, "value": {
                        "type": "ready", "clientId": "gen-1", "host": { "home": "/home/x" } } });
                    ws.send(Message::Text(ready.to_string().into()))
                        .await
                        .unwrap();
                    let ask = json!({ "type": "item", "streamId": stream_id, "value": {
                        "type": "waterfall", "event": "approval/request", "eventId": "evt-1",
                        "agentId": "ses_1", "request": { "toolName": "bash", "callId": "call_1" } } });
                    ws.send(Message::Text(ask.to_string().into()))
                        .await
                        .unwrap();
                } else {
                    let session_id = frame["payload"]["args"]["request"]["address"]["sessionId"]
                        .as_str()
                        .unwrap();
                    let snapshot = json!({ "type": "item", "streamId": stream_id, "value": {
                        "type": "snapshot", "header": { "version": 4, "id": session_id, "createdAt": 1, "isSeeded": false },
                        "cursor": 0, "records": [], "hasMore": false,
                        "projections": { "asOfSeq": 0, "values": {} } } });
                    ws.send(Message::Text(snapshot.to_string().into()))
                        .await
                        .unwrap();
                }
            }
            seen_opens
        });

        let endpoint = DeepseekEndpoint::new(format!("http://{addr}"), None, None);
        let (listener, mut rx) = DeepseekStreamListener::new(endpoint);
        listener.follow("ses_1");
        listener.start();

        async fn next(rx: &mut broadcast::Receiver<DeepseekStreamEvent>) -> DeepseekStreamEvent {
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("an event arrives")
                .expect("channel open")
        }
        assert_eq!(next(&mut rx).await, DeepseekStreamEvent::Connected);
        assert!(listener.is_connected());
        assert_eq!(
            next(&mut rx).await,
            DeepseekStreamEvent::Events(EventsFrame::Ready {
                client_id: "gen-1".into()
            })
        );
        match next(&mut rx).await {
            DeepseekStreamEvent::Events(EventsFrame::Waterfall {
                event_id, agent_id, ..
            }) => {
                assert_eq!(event_id, "evt-1");
                assert_eq!(agent_id, "ses_1");
            }
            other => panic!("expected the waterfall, got {other:?}"),
        }
        match next(&mut rx).await {
            DeepseekStreamEvent::Follow { session_id, frame } => {
                assert_eq!(session_id, "ses_1");
                assert_eq!(frame["type"], "snapshot");
            }
            other => panic!("expected the snapshot, got {other:?}"),
        }

        // A follow requested while connected is opened on the live socket.
        listener.follow("ses_2");
        match next(&mut rx).await {
            DeepseekStreamEvent::Follow { session_id, .. } => assert_eq!(session_id, "ses_2"),
            other => panic!("expected the second snapshot, got {other:?}"),
        }

        listener.stop();
        tokio::time::timeout(Duration::from_secs(2), listener.join())
            .await
            .expect("stop ends an idle read promptly");
        assert!(!listener.is_connected());
        assert_eq!(next(&mut rx).await, DeepseekStreamEvent::Disconnected);

        let opens = tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("the server sees the close")
            .unwrap();
        assert_eq!(
            opens,
            vec![EVENTS_ENDPOINT, FOLLOW_ENDPOINT, FOLLOW_ENDPOINT]
        );
    }

    /// A refused upgrade (DeepSeek Harness answers 401 without a cookie) is a
    /// failed connect, not a connected stream.
    #[tokio::test]
    async fn a_refused_upgrade_is_not_connected() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        let refuse = tokio::spawn(async move {
            loop {
                let (mut socket, _) = server.accept().await.unwrap();
                let _ = socket
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\nConnection: close\r\nContent-Length: 12\r\n\r\nunauthorized")
                    .await;
            }
        });
        let endpoint = DeepseekEndpoint::new(format!("http://{addr}"), None, None);
        let (listener, mut rx) = DeepseekStreamListener::new(endpoint);
        listener.start();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!listener.is_connected());
        assert!(rx.try_recv().is_err(), "no Connected event was published");
        listener.stop();
        tokio::time::timeout(Duration::from_secs(2), listener.join())
            .await
            .expect("stops");
        refuse.abort();
    }
}
