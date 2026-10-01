//! Effect RPC over a WebSocket, as T3 Code speaks it.
//!
//! Every WebSocket text frame is one JSON object (or an array of them).
//! Frames the client sends:
//!
//! ```json
//! {"_tag":"Request","id":0,"tag":"server.getConfig","payload":{},"headers":[]}
//! {"_tag":"Ack","requestId":0}
//! {"_tag":"Interrupt","requestId":0,"interruptors":[]}
//! {"_tag":"Ping"}
//! {"_tag":"Eof"}
//! ```
//!
//! Frames the server sends:
//!
//! ```json
//! {"_tag":"Chunk","requestId":0,"values":[...]}
//! {"_tag":"Exit","requestId":0,"exit":{"_tag":"Success","value":...}}
//! {"_tag":"Exit","requestId":0,"exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{...}}]}}
//! {"_tag":"Defect","defect":...}
//! {"_tag":"Pong"}
//! ```
//!
//! A unary RPC answers with one `Exit`. A streaming RPC answers with
//! `Chunk`s and the server does not send the next chunk until the client has
//! acknowledged the previous one with `Ack`, so a client that forgets to ack
//! stalls its own subscription. `Interrupt` ends a stream; the server
//! confirms with `Exit{Failure, cause:[{_tag:"Interrupt"}]}`. The reference
//! client pings every five seconds and treats a missing pong by the next tick
//! as a dead socket; this one does the same.
//!
//! [`RpcConnection`] is an actor: one task owns the socket, correlates
//! responses to requests by id, fans chunks out to subscriptions, reconnects
//! with backoff and stops on its cancellation token. Handles are cheap
//! clones.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::MAX_PAYLOAD_BYTES;

pub type RequestId = u64;

/// Frames the client writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum ClientFrame {
    Request {
        id: RequestId,
        tag: String,
        payload: Value,
        /// `[[name, value], ...]`; always empty here, the ticket already
        /// authenticated the socket.
        headers: Vec<(String, String)>,
    },
    Ack {
        #[serde(rename = "requestId")]
        request_id: RequestId,
    },
    Interrupt {
        #[serde(rename = "requestId")]
        request_id: RequestId,
        #[serde(default)]
        interruptors: Vec<Value>,
    },
    Ping,
    Eof,
}

/// Frames the server writes.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "_tag")]
pub enum ServerFrame {
    Chunk {
        #[serde(rename = "requestId")]
        request_id: RequestId,
        values: Vec<Value>,
    },
    Exit {
        #[serde(rename = "requestId")]
        request_id: RequestId,
        exit: RpcExit,
    },
    Defect {
        defect: Value,
    },
    Pong,
    /// Anything a newer server might add; ignored.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "_tag")]
pub enum RpcExit {
    Success {
        #[serde(default)]
        value: Value,
    },
    Failure {
        #[serde(default)]
        cause: Vec<CauseEntry>,
    },
}

/// One entry of an Effect `Cause` as `Schema.Exit` encodes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "_tag")]
pub enum CauseEntry {
    /// A typed error the RPC declared: `{_tag:"OrchestrationDispatchCommandError", message, ...}`.
    Fail {
        #[serde(default)]
        error: Value,
    },
    Die {
        #[serde(default)]
        defect: Value,
    },
    Interrupt {
        #[serde(default, rename = "fiberId")]
        fiber_id: Option<Value>,
    },
    #[serde(other)]
    Other,
}

/// A declared RPC failure, decoded from the `Fail` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcFailure {
    /// The error's `_tag`, e.g. `OrchestrationDispatchCommandError` or
    /// `EnvironmentAuthorizationError`.
    pub tag: String,
    pub message: String,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RpcError {
    /// The server answered with a typed failure.
    Failed(RpcFailure),
    /// The request or stream was interrupted (by us or the server).
    Interrupted,
    /// The server died on the request, or reported a transport defect.
    Defect(String),
    /// The socket is not connected and did not come back in time.
    Disconnected,
    Timeout,
    Protocol(String),
    /// The connection actor has been stopped.
    Closed,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(e) => write!(f, "{}: {}", e.tag, e.message),
            Self::Interrupted => write!(f, "interrupted"),
            Self::Defect(d) => write!(f, "defect: {d}"),
            Self::Disconnected => write!(f, "not connected"),
            Self::Timeout => write!(f, "timed out"),
            Self::Protocol(p) => write!(f, "protocol error: {p}"),
            Self::Closed => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for RpcError {}

impl RpcExit {
    /// Fold an exit into a result the way the reference client does: a
    /// `Fail` entry is the declared error, an `Interrupt`-only cause is an
    /// interruption, a `Die` is a defect.
    pub fn into_result(self) -> Result<Value, RpcError> {
        match self {
            Self::Success { value } => Ok(value),
            Self::Failure { cause } => {
                for entry in &cause {
                    if let CauseEntry::Fail { error } = entry {
                        return Err(RpcError::Failed(RpcFailure {
                            tag: error
                                .get("_tag")
                                .and_then(Value::as_str)
                                .unwrap_or("Error")
                                .to_string(),
                            message: error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("request failed")
                                .to_string(),
                            raw: error.clone(),
                        }));
                    }
                }
                for entry in &cause {
                    if let CauseEntry::Die { defect } = entry {
                        return Err(RpcError::Defect(defect_summary(defect)));
                    }
                }
                if cause
                    .iter()
                    .any(|c| matches!(c, CauseEntry::Interrupt { .. }))
                    || cause.is_empty()
                {
                    return Err(RpcError::Interrupted);
                }
                Err(RpcError::Defect("unknown cause".into()))
            }
        }
    }
}

pub fn defect_summary(defect: &Value) -> String {
    match defect {
        Value::String(s) => s.chars().take(200).collect(),
        Value::Object(o) => o
            .get("message")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(200).collect())
            .unwrap_or_else(|| "defect".into()),
        _ => "defect".into(),
    }
}

pub fn encode_frame(frame: &ClientFrame) -> String {
    serde_json::to_string(frame).unwrap_or_default()
}

/// Decode one WebSocket text frame: one object, or an array of objects when
/// the server batches.
pub fn decode_frames(text: &str) -> Result<Vec<ServerFrame>, RpcError> {
    if text.len() > MAX_PAYLOAD_BYTES {
        return Err(RpcError::Protocol("frame too large".into()));
    }
    let value: Value =
        serde_json::from_str(text).map_err(|e| RpcError::Protocol(format!("bad json: {e}")))?;
    let items = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    items
        .into_iter()
        .map(|item| {
            serde_json::from_value::<ServerFrame>(item)
                .map_err(|e| RpcError::Protocol(format!("bad frame: {e}")))
        })
        .collect()
}

/// One item of a stream subscription.
#[derive(Debug)]
pub enum StreamItem {
    Value(Value),
    /// The stream ended. `Ok` when the server completed it, `Err` for an
    /// interrupt, failure or a lost socket. No more items follow.
    End(Result<(), RpcError>),
}

/// A live stream subscription. Dropping it interrupts the request.
pub struct Subscription {
    pub id: RequestId,
    rx: mpsc::Receiver<StreamItem>,
    cmd_tx: mpsc::Sender<Command>,
}

impl Subscription {
    pub async fn next(&mut self) -> Option<StreamItem> {
        self.rx.recv().await
    }

    /// Ask the server to end the stream; the `End` item follows.
    pub fn interrupt(&self) {
        let _ = self.cmd_tx.try_send(Command::Interrupt { id: self.id });
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.cmd_tx.try_send(Command::Interrupt { id: self.id });
    }
}

/// How the actor obtains a socket URL for each (re)connect. The ticket is
/// single-use and short-lived, so it is minted per attempt.
pub type UrlSource =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct RpcConfig {
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    pub ping_interval: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// How long a request waits for the socket to (re)connect before it
    /// fails with `Disconnected`.
    pub connect_wait: Duration,
    pub stream_buffer: usize,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(15),
            ping_interval: Duration::from_secs(5),
            backoff_initial: Duration::from_millis(500),
            backoff_max: Duration::from_secs(10),
            connect_wait: Duration::from_secs(10),
            stream_buffer: 64,
        }
    }
}

enum Command {
    Request {
        tag: String,
        payload: Value,
        reply: oneshot::Sender<Result<Value, RpcError>>,
    },
    Subscribe {
        tag: String,
        payload: Value,
        sink: mpsc::Sender<StreamItem>,
        reply: oneshot::Sender<Result<RequestId, RpcError>>,
    },
    Interrupt {
        id: RequestId,
    },
}

/// A handle to the connection actor.
#[derive(Clone)]
pub struct RpcConnection {
    cmd_tx: mpsc::Sender<Command>,
    connected: watch::Receiver<bool>,
    cancel: CancellationToken,
    config: RpcConfig,
}

impl RpcConnection {
    /// Spawn the actor. It connects lazily on the first command and keeps
    /// reconnecting until `cancel` fires.
    pub fn spawn(url_source: UrlSource, config: RpcConfig, cancel: CancellationToken) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (connected_tx, connected_rx) = watch::channel(false);
        let actor = Actor {
            url_source,
            config: config.clone(),
            cancel: cancel.clone(),
            cmd_rx,
            connected_tx,
            next_id: 0,
            pending: HashMap::new(),
            subscriptions: HashMap::new(),
            queued: VecDeque::new(),
        };
        tokio::spawn(actor.run());
        Self {
            cmd_tx,
            connected: connected_rx,
            cancel,
            config,
        }
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// A unary RPC.
    pub async fn request(&self, tag: &str, payload: Value) -> Result<Value, RpcError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Request {
                tag: tag.to_string(),
                payload,
                reply: reply_tx,
            })
            .await
            .map_err(|_| RpcError::Closed)?;
        match tokio::time::timeout(
            self.config.request_timeout + self.config.connect_wait,
            reply_rx,
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::Closed),
            Err(_) => Err(RpcError::Timeout),
        }
    }

    /// A streaming RPC. Items arrive on the returned subscription; every
    /// chunk is acknowledged by the actor before the next is read.
    pub async fn subscribe(&self, tag: &str, payload: Value) -> Result<Subscription, RpcError> {
        let (sink, rx) = mpsc::channel(self.config.stream_buffer);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Subscribe {
                tag: tag.to_string(),
                payload,
                sink,
                reply: reply_tx,
            })
            .await
            .map_err(|_| RpcError::Closed)?;
        let id = match tokio::time::timeout(self.config.connect_wait, reply_rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => return Err(RpcError::Closed),
            Err(_) => return Err(RpcError::Timeout),
        };
        Ok(Subscription {
            id,
            rx,
            cmd_tx: self.cmd_tx.clone(),
        })
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Pending {
    reply: oneshot::Sender<Result<Value, RpcError>>,
    deadline: tokio::time::Instant,
}

struct Actor {
    url_source: UrlSource,
    config: RpcConfig,
    cancel: CancellationToken,
    cmd_rx: mpsc::Receiver<Command>,
    connected_tx: watch::Sender<bool>,
    next_id: RequestId,
    pending: HashMap<RequestId, Pending>,
    subscriptions: HashMap<RequestId, mpsc::Sender<StreamItem>>,
    /// Commands received while the socket was down, replayed on connect.
    queued: VecDeque<(Command, tokio::time::Instant)>,
}

enum Disconnect {
    Cancelled,
    Lost(String),
}

impl Actor {
    async fn run(mut self) {
        let mut backoff = self.config.backoff_initial;
        loop {
            if self.cancel.is_cancelled() {
                break;
            }
            match self.connect().await {
                Ok(socket) => {
                    backoff = self.config.backoff_initial;
                    let _ = self.connected_tx.send(true);
                    tracing::info!("t3 rpc socket connected");
                    let outcome = self.run_connected(socket).await;
                    let _ = self.connected_tx.send(false);
                    self.fail_inflight(RpcError::Disconnected);
                    match outcome {
                        Disconnect::Cancelled => break,
                        Disconnect::Lost(reason) => {
                            tracing::warn!(%reason, "t3 rpc socket lost; reconnecting")
                        }
                    }
                }
                Err(reason) => {
                    tracing::warn!(%reason, wait_ms = backoff.as_millis() as u64, "t3 rpc connect failed");
                }
            }
            // Back off, but keep accepting commands so callers' own timeouts
            // decide how long they wait rather than a slow retry schedule.
            let sleep = tokio::time::sleep(backoff);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = self.cancel.cancelled() => { self.fail_inflight(RpcError::Closed); return; }
                    _ = &mut sleep => break,
                    cmd = self.cmd_rx.recv() => match cmd {
                        Some(cmd) => self.queue(cmd),
                        None => { self.fail_inflight(RpcError::Closed); return; }
                    }
                }
            }
            self.expire_queued();
            backoff = (backoff * 2).min(self.config.backoff_max);
        }
        self.fail_inflight(RpcError::Closed);
        tracing::info!("t3 rpc actor stopped");
    }

    fn queue(&mut self, cmd: Command) {
        if let Command::Interrupt { id } = &cmd {
            // Nothing to interrupt on a dead socket; the subscription already
            // got its End when the socket went.
            self.subscriptions.remove(id);
            return;
        }
        if self.queued.len() >= 256 {
            match cmd {
                Command::Request { reply, .. } => {
                    let _ = reply.send(Err(RpcError::Disconnected));
                }
                Command::Subscribe { reply, .. } => {
                    let _ = reply.send(Err(RpcError::Disconnected));
                }
                Command::Interrupt { .. } => {}
            }
            return;
        }
        let deadline = tokio::time::Instant::now() + self.config.connect_wait;
        self.queued.push_back((cmd, deadline));
    }

    fn expire_queued(&mut self) {
        let now = tokio::time::Instant::now();
        let mut keep = VecDeque::new();
        while let Some((cmd, deadline)) = self.queued.pop_front() {
            if deadline > now {
                keep.push_back((cmd, deadline));
                continue;
            }
            match cmd {
                Command::Request { reply, .. } => {
                    let _ = reply.send(Err(RpcError::Disconnected));
                }
                Command::Subscribe { reply, .. } => {
                    let _ = reply.send(Err(RpcError::Disconnected));
                }
                Command::Interrupt { .. } => {}
            }
        }
        self.queued = keep;
    }

    fn fail_inflight(&mut self, error: RpcError) {
        for (_, p) in self.pending.drain() {
            let _ = p.reply.send(Err(error.clone()));
        }
        for (_, sink) in self.subscriptions.drain() {
            let _ = sink.try_send(StreamItem::End(Err(error.clone())));
        }
    }

    async fn connect(&mut self) -> Result<Socket, String> {
        let url = tokio::select! {
            _ = self.cancel.cancelled() => return Err("cancelled".into()),
            url = (self.url_source)() => url?,
        };
        let request = url
            .into_client_request()
            .map_err(|e| format!("bad socket url: {e}"))?;
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(MAX_PAYLOAD_BYTES))
            .max_frame_size(Some(MAX_PAYLOAD_BYTES));
        let connect = tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false);
        tokio::select! {
            _ = self.cancel.cancelled() => Err("cancelled".into()),
            result = tokio::time::timeout(self.config.connect_timeout, connect) => match result {
                Ok(Ok((socket, _))) => Ok(socket),
                Ok(Err(e)) => Err(super::endpoint::short(&e.to_string())),
                Err(_) => Err("connect timeout".into()),
            },
        }
    }

    async fn run_connected(&mut self, socket: Socket) -> Disconnect {
        let (mut write, mut read) = socket.split();
        // Replay what arrived while we were down.
        let queued: Vec<_> = self.queued.drain(..).map(|(c, _)| c).collect();
        for cmd in queued {
            if let Err(reason) = self.handle_command(cmd, &mut write).await {
                return Disconnect::Lost(reason);
            }
        }
        let mut ping = tokio::time::interval(self.config.ping_interval);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // the first tick fires immediately
        let mut pong_outstanding = false;
        let mut expiry = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    let _ = write.send(Message::Text(encode_frame(&ClientFrame::Eof).into())).await;
                    let _ = write.close().await;
                    return Disconnect::Cancelled;
                }
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(cmd) => if let Err(reason) = self.handle_command(cmd, &mut write).await {
                        return Disconnect::Lost(reason);
                    },
                    None => {
                        let _ = write.close().await;
                        return Disconnect::Cancelled;
                    }
                },
                frame = read.next() => match frame {
                    Some(Ok(Message::Text(text))) => {
                        match decode_frames(&text) {
                            Ok(frames) => for f in frames {
                                if let Err(reason) = self.handle_frame(f, &mut write, &mut pong_outstanding).await {
                                    return Disconnect::Lost(reason);
                                }
                            },
                            Err(e) => tracing::warn!(error = %e, "t3 rpc frame ignored"),
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if write.send(Message::Pong(data)).await.is_err() {
                            return Disconnect::Lost("pong write failed".into());
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        return Disconnect::Lost(format!("server closed: {}", frame.map(|f| f.code.to_string()).unwrap_or_default()));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Disconnect::Lost(super::endpoint::short(&e.to_string())),
                    None => return Disconnect::Lost("eof".into()),
                },
                _ = ping.tick() => {
                    if pong_outstanding {
                        return Disconnect::Lost("ping timeout".into());
                    }
                    pong_outstanding = true;
                    if write.send(Message::Text(encode_frame(&ClientFrame::Ping).into())).await.is_err() {
                        return Disconnect::Lost("ping write failed".into());
                    }
                }
                _ = expiry.tick() => self.expire_pending(),
            }
        }
    }

    fn expire_pending(&mut self) {
        let now = tokio::time::Instant::now();
        let expired: Vec<RequestId> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(p) = self.pending.remove(&id) {
                let _ = p.reply.send(Err(RpcError::Timeout));
            }
        }
    }

    async fn send(
        &mut self,
        write: &mut futures::stream::SplitSink<Socket, Message>,
        frame: &ClientFrame,
    ) -> Result<(), String> {
        write
            .send(Message::Text(encode_frame(frame).into()))
            .await
            .map_err(|e| super::endpoint::short(&e.to_string()))
    }

    async fn handle_command(
        &mut self,
        cmd: Command,
        write: &mut futures::stream::SplitSink<Socket, Message>,
    ) -> Result<(), String> {
        match cmd {
            Command::Request {
                tag,
                payload,
                reply,
            } => {
                let id = self.next_id;
                self.next_id += 1;
                self.pending.insert(
                    id,
                    Pending {
                        reply,
                        deadline: tokio::time::Instant::now() + self.config.request_timeout,
                    },
                );
                self.send(
                    write,
                    &ClientFrame::Request {
                        id,
                        tag,
                        payload,
                        headers: vec![],
                    },
                )
                .await
            }
            Command::Subscribe {
                tag,
                payload,
                sink,
                reply,
            } => {
                let id = self.next_id;
                self.next_id += 1;
                self.subscriptions.insert(id, sink);
                let sent = self
                    .send(
                        write,
                        &ClientFrame::Request {
                            id,
                            tag,
                            payload,
                            headers: vec![],
                        },
                    )
                    .await;
                match sent {
                    Ok(()) => {
                        let _ = reply.send(Ok(id));
                        Ok(())
                    }
                    Err(reason) => {
                        self.subscriptions.remove(&id);
                        let _ = reply.send(Err(RpcError::Disconnected));
                        Err(reason)
                    }
                }
            }
            Command::Interrupt { id } => {
                if self.subscriptions.contains_key(&id) || self.pending.contains_key(&id) {
                    self.send(
                        write,
                        &ClientFrame::Interrupt {
                            request_id: id,
                            interruptors: vec![],
                        },
                    )
                    .await
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn handle_frame(
        &mut self,
        frame: ServerFrame,
        write: &mut futures::stream::SplitSink<Socket, Message>,
        pong_outstanding: &mut bool,
    ) -> Result<(), String> {
        match frame {
            ServerFrame::Pong => {
                *pong_outstanding = false;
                Ok(())
            }
            ServerFrame::Chunk { request_id, values } => {
                if let Some(sink) = self.subscriptions.get(&request_id) {
                    for v in values {
                        // A slow consumer stalls this connection rather than
                        // dropping events: the stream must stay lossless.
                        if sink.send(StreamItem::Value(v)).await.is_err() {
                            self.subscriptions.remove(&request_id);
                            return self
                                .send(
                                    write,
                                    &ClientFrame::Interrupt {
                                        request_id,
                                        interruptors: vec![],
                                    },
                                )
                                .await;
                        }
                    }
                }
                self.send(write, &ClientFrame::Ack { request_id }).await
            }
            ServerFrame::Exit { request_id, exit } => {
                if let Some(p) = self.pending.remove(&request_id) {
                    let _ = p.reply.send(exit.into_result());
                } else if let Some(sink) = self.subscriptions.remove(&request_id) {
                    let end = exit.into_result().map(|_| ());
                    let _ = sink.send(StreamItem::End(end)).await;
                }
                Ok(())
            }
            ServerFrame::Defect { defect } => {
                let summary = defect_summary(&defect);
                tracing::warn!(defect = %summary, "t3 rpc server defect; dropping connection");
                Err(format!("defect: {summary}"))
            }
            ServerFrame::Unknown => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_client_frames_like_the_reference_client() {
        let req = ClientFrame::Request {
            id: 0,
            tag: "server.probe".into(),
            payload: json!({}),
            headers: vec![],
        };
        assert_eq!(
            encode_frame(&req),
            r#"{"_tag":"Request","id":0,"tag":"server.probe","payload":{},"headers":[]}"#
        );
        assert_eq!(
            encode_frame(&ClientFrame::Ack { request_id: 3 }),
            r#"{"_tag":"Ack","requestId":3}"#
        );
        assert_eq!(
            encode_frame(&ClientFrame::Interrupt {
                request_id: 3,
                interruptors: vec![]
            }),
            r#"{"_tag":"Interrupt","requestId":3,"interruptors":[]}"#
        );
        assert_eq!(encode_frame(&ClientFrame::Ping), r#"{"_tag":"Ping"}"#);
        assert_eq!(encode_frame(&ClientFrame::Eof), r#"{"_tag":"Eof"}"#);
    }

    /// Every server frame shape observed on the wire (`fixtures/frames.jsonl`).
    #[test]
    fn decodes_every_captured_server_frame() {
        let raw = include_str!("fixtures/frames.jsonl");
        let mut tags = std::collections::BTreeSet::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let entry: Value = serde_json::from_str(line).expect("capture line");
            if entry["dir"] != "in" {
                continue;
            }
            let text = serde_json::to_string(&entry["body"]).unwrap();
            let frames = decode_frames(&text).expect("decodes");
            assert_eq!(frames.len(), 1);
            tags.insert(entry["body"]["_tag"].as_str().unwrap().to_string());
            assert!(
                !matches!(frames[0], ServerFrame::Unknown),
                "unknown: {text}"
            );
        }
        for expected in ["Chunk", "Exit", "Pong"] {
            assert!(tags.contains(expected), "capture lacks {expected}");
        }
    }

    #[test]
    fn folds_exits_into_results() {
        let ok: RpcExit =
            serde_json::from_value(json!({"_tag":"Success","value":{"sequence":4}})).unwrap();
        assert_eq!(ok.into_result().unwrap()["sequence"], 4);

        let interrupted: RpcExit = serde_json::from_value(
            json!({"_tag":"Failure","cause":[{"_tag":"Interrupt","fiberId":1558}]}),
        )
        .unwrap();
        assert_eq!(
            interrupted.into_result().unwrap_err(),
            RpcError::Interrupted
        );

        let failed: RpcExit =
            serde_json::from_value(json!({"_tag":"Failure","cause":[{"_tag":"Fail","error":{
            "_tag":"OrchestrationDispatchCommandError","message":"Thread not found"}}]}))
            .unwrap();
        match failed.into_result().unwrap_err() {
            RpcError::Failed(f) => {
                assert_eq!(f.tag, "OrchestrationDispatchCommandError");
                assert_eq!(f.message, "Thread not found");
            }
            other => panic!("{other:?}"),
        }

        let died: RpcExit = serde_json::from_value(
            json!({"_tag":"Failure","cause":[{"_tag":"Die","defect":"Unknown request tag: nope"}]}),
        )
        .unwrap();
        assert!(
            matches!(died.into_result().unwrap_err(), RpcError::Defect(d) if d.contains("Unknown request tag"))
        );
    }

    #[test]
    fn decodes_a_batched_array_and_tolerates_new_tags() {
        let frames = decode_frames(r#"[{"_tag":"Pong"},{"_tag":"Whatever","x":1}]"#).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(matches!(frames[0], ServerFrame::Pong));
        assert!(matches!(frames[1], ServerFrame::Unknown));
        assert!(decode_frames("not json").is_err());
    }

    /// A tiny in-process Effect-RPC-shaped server: proves request
    /// correlation, chunk acks, interrupts, ping/pong and reconnection
    /// without a T3 instance.
    #[tokio::test]
    async fn actor_correlates_requests_acks_chunks_and_reconnects() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let conns = connections.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let n = conns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(msg)) = ws.next().await {
                        let Message::Text(text) = msg else { continue };
                        let v: Value = serde_json::from_str(&text).unwrap();
                        match v["_tag"].as_str().unwrap() {
                            "Ping" => ws
                                .send(Message::Text(r#"{"_tag":"Pong"}"#.into()))
                                .await
                                .unwrap(),
                            "Request" => {
                                let id = v["id"].clone();
                                match v["tag"].as_str().unwrap() {
                                    "echo" => {
                                        let exit = json!({"_tag":"Exit","requestId":id,"exit":{"_tag":"Success","value":{"echo":v["payload"],"conn":n}}});
                                        ws.send(Message::Text(exit.to_string().into()))
                                            .await
                                            .unwrap();
                                    }
                                    "fail" => {
                                        let exit = json!({"_tag":"Exit","requestId":id,"exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{"_tag":"Boom","message":"no"}}]}});
                                        ws.send(Message::Text(exit.to_string().into()))
                                            .await
                                            .unwrap();
                                    }
                                    "stream" => {
                                        // Two chunks; the second only after an Ack.
                                        let c1 =
                                            json!({"_tag":"Chunk","requestId":id,"values":[1,2]});
                                        ws.send(Message::Text(c1.to_string().into()))
                                            .await
                                            .unwrap();
                                        let ack = ws.next().await.unwrap().unwrap();
                                        assert!(ack.to_text().unwrap().contains(r#""_tag":"Ack""#));
                                        let c2 =
                                            json!({"_tag":"Chunk","requestId":id,"values":[3]});
                                        ws.send(Message::Text(c2.to_string().into()))
                                            .await
                                            .unwrap();
                                        let _ack2 = ws.next().await.unwrap().unwrap();
                                        // Then wait for the interrupt.
                                        let intr = ws.next().await.unwrap().unwrap();
                                        assert!(intr
                                            .to_text()
                                            .unwrap()
                                            .contains(r#""_tag":"Interrupt""#));
                                        let exit = json!({"_tag":"Exit","requestId":id,"exit":{"_tag":"Failure","cause":[{"_tag":"Interrupt","fiberId":1}]}});
                                        ws.send(Message::Text(exit.to_string().into()))
                                            .await
                                            .unwrap();
                                    }
                                    "drop" => {
                                        let _ = ws.close(None).await;
                                        return;
                                    }
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
        });

        let url_source: UrlSource = Arc::new(move || {
            let url = format!("ws://127.0.0.1:{port}/ws?wsTicket=t");
            Box::pin(async move { Ok(url) })
        });
        let cancel = CancellationToken::new();
        let config = RpcConfig {
            request_timeout: Duration::from_secs(5),
            connect_wait: Duration::from_secs(5),
            backoff_initial: Duration::from_millis(50),
            ping_interval: Duration::from_millis(200),
            ..RpcConfig::default()
        };
        let rpc = RpcConnection::spawn(url_source, config, cancel.clone());

        let echoed = rpc.request("echo", json!({"a":1})).await.unwrap();
        assert_eq!(echoed["echo"]["a"], 1);
        assert_eq!(echoed["conn"], 0);
        assert!(rpc.is_connected());

        match rpc.request("fail", json!({})).await.unwrap_err() {
            RpcError::Failed(f) => assert_eq!(f.tag, "Boom"),
            other => panic!("{other:?}"),
        }

        let mut sub = rpc.subscribe("stream", json!({})).await.unwrap();
        let mut got = vec![];
        for _ in 0..3 {
            match sub.next().await.unwrap() {
                StreamItem::Value(v) => got.push(v),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(got, vec![json!(1), json!(2), json!(3)]);
        sub.interrupt();
        match sub.next().await.unwrap() {
            StreamItem::End(Err(RpcError::Interrupted)) => {}
            other => panic!("{other:?}"),
        }

        // The pinger has had time to exchange at least one pong by now.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(rpc.is_connected(), "ping/pong keeps the socket alive");

        // Server drops the socket: the next request rides the reconnect.
        let _ = rpc.request("drop", json!({})).await;
        let again = rpc.request("echo", json!({"b":2})).await.unwrap();
        assert_eq!(again["conn"], 1, "answered by the second connection");

        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(
            rpc.request("echo", json!({})).await,
            Err(RpcError::Closed)
        ));
    }
}
