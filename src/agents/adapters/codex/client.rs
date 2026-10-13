#[cfg(unix)]
mod websocket;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agents::ports::agent::AgentError;

pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_PENDING: usize = 32;
const MAX_SERVER_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const QUEUE_SIZE: usize = 16;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub enum CodexEvent {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Snapshot {
        thread: Value,
        revision: u64,
    },
    /// Initial shared subscription status, ordered before subsequent events.
    Attached {
        thread: Value,
    },
    MetadataRefreshFailed {
        thread: String,
        revision: u64,
    },
    Resolved {
        thread: String,
        id: String,
        form: bool,
    },
    Disconnected,
}

#[derive(Clone)]
pub struct ServerRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
    bytes: usize,
    answering: bool,
}

type Reply = oneshot::Sender<Result<(Value, usize), AgentError>>;

struct Pending {
    reply: Reply,
    turn_thread: Option<String>,
    selection_config: bool,
    attaches_thread: bool,
    attach_expected: Option<String>,
}

struct State {
    generation: String,
    shared: bool,
    subscribed: Mutex<HashSet<String>>,
    pending: Mutex<HashMap<u64, Pending>>,
    requests: Mutex<HashMap<String, ServerRequest>>,
    turns: Mutex<HashMap<String, String>>,
    completed_turns: Mutex<HashMap<String, String>>,
    next_id: AtomicU64,
    ready: AtomicBool,
    cancel: CancellationToken,
    events: mpsc::Sender<CodexEvent>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn disconnected() -> AgentError {
    AgentError::NotAvailable(
        "Codex connection lost; request outcome may be unknown; not retried".into(),
    )
}

impl State {
    fn request_id(&self, id: &Value) -> Option<String> {
        request_key(id).map(|key| format!("{}:{}", self.generation, hex::encode(key.as_bytes())))
    }
    fn stop(&self) {
        self.ready.store(false, Ordering::Release);
        self.cancel.cancel();
        for (_, pending) in locked(&self.pending).drain() {
            let _ = pending.reply.send(Err(disconnected()));
        }
        locked(&self.requests).clear();
        locked(&self.subscribed).clear();
        locked(&self.turns).clear();
        locked(&self.completed_turns).clear();
    }

    fn emit(&self, event: CodexEvent) -> Result<(), AgentError> {
        if self.cancel.is_cancelled() {
            return Err(disconnected());
        }
        if self.events.try_send(event).is_err() {
            // Never skip a delta or approval while claiming a connected stream.
            self.stop();
            return Err(AgentError::NotAvailable(
                "Codex event backlog overflow; resync required".into(),
            ));
        }
        Ok(())
    }
}

/// External ownership is separate from the pumps: dropping the last client
/// cancels them even while they are blocked reading a silent child.
pub struct CodexClient {
    state: Arc<State>,
    outgoing: mpsc::Sender<WritePacket>,
    timeout: Duration,
    config_cwd: Option<String>,
    configured_provider: std::sync::OnceLock<String>,
    expected_home: Option<std::path::PathBuf>,
    codex_home: std::sync::OnceLock<std::path::PathBuf>,
}

struct WritePacket {
    bytes: Vec<u8>,
    done: oneshot::Sender<Result<(), AgentError>>,
}
struct WriteGuard {
    state: Arc<State>,
    finished: bool,
}
impl Drop for WriteGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.state.stop();
        }
    }
}

impl Drop for CodexClient {
    fn drop(&mut self) {
        self.state.stop();
    }
}

struct PendingGuard {
    state: Arc<State>,
    id: u64,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        let unfinished = locked(&self.state.pending).remove(&self.id).is_some();
        if unfinished {
            // Caller cancellation after a write is also ambiguous. Do not keep
            // accepting prompts while an unacknowledged mutation may run.
            self.state.stop();
        }
    }
}

impl CodexClient {
    pub(crate) fn from_io_at<R, W>(
        reader: R,
        writer: W,
        cwd: String,
    ) -> (Arc<Self>, mpsc::Receiver<CodexEvent>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (mut client, events) = Self::from_io(reader, writer);
        // Pumps own State, not CodexClient; the caller is its sole owner here.
        Arc::get_mut(&mut client)
            .expect("new client has one owner")
            .config_cwd = Some(cwd);
        (client, events)
    }

    pub(crate) fn config_cwd(&self) -> Option<&str> {
        self.config_cwd.as_deref()
    }

    pub(crate) fn configured_provider(&self) -> Option<&str> {
        self.configured_provider.get().map(String::as_str)
    }

    pub fn from_io<R, W>(reader: R, writer: W) -> (Arc<Self>, mpsc::Receiver<CodexEvent>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::with_timeout(reader, writer, REQUEST_TIMEOUT)
    }

    fn with_timeout<R, W>(
        reader: R,
        mut writer: W,
        timeout: Duration,
    ) -> (Arc<Self>, mpsc::Receiver<CodexEvent>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (events, rx) = mpsc::channel(QUEUE_SIZE);
        let (outgoing, mut writes) = mpsc::channel::<WritePacket>(MAX_PENDING);
        let state = Arc::new(State {
            generation: uuid::Uuid::new_v4().to_string(),
            shared: false,
            subscribed: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashMap::new()),
            requests: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            completed_turns: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ready: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            events,
        });
        let write_state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = write_state.cancel.cancelled() => break,
                    packet = writes.recv() => {
                        let Some(packet) = packet else { break; };
                        let result = tokio::select! {
                            _ = write_state.cancel.cancelled() => Err(disconnected()),
                            result = tokio::time::timeout(timeout, async {
                                writer.write_all(&packet.bytes).await?;
                                writer.flush().await
                            }) => match result {Ok(Ok(()))=>Ok(()),_=>Err(disconnected())},
                        };
                        let failed = result.is_err();
                        let _ = packet.done.send(result);
                        if failed { break; }
                    }
                }
            }
            write_state.stop();
        });
        let read_state = state.clone();
        tokio::spawn(async move {
            read_frames(reader, read_state.clone()).await;
            read_state.stop();
            // The queue may be full. The manager also observes the cancellation
            // token, rather than depending on space for this final event.
            let _ = read_state.events.try_send(CodexEvent::Disconnected);
        });
        (
            Arc::new(Self {
                state,
                outgoing,
                timeout,
                config_cwd: None,
                configured_provider: std::sync::OnceLock::new(),
                expected_home: None,
                codex_home: std::sync::OnceLock::new(),
            }),
            rx,
        )
    }

    pub(crate) fn codex_home(&self) -> Option<&std::path::Path> {
        self.codex_home.get().map(std::path::PathBuf::as_path)
    }

    pub(crate) fn is_shared(&self) -> bool {
        self.state.shared
    }

    pub(crate) fn is_subscribed(&self, id: &str) -> bool {
        self.is_ready() && locked(&self.state.subscribed).contains(id)
    }

    pub(crate) fn observe_active_turn(&self, thread: &str, turn: &Value) {
        if !self.is_subscribed(thread) || turn["status"] != "inProgress" {
            return;
        }
        let Some(id) = turn["id"].as_str() else {
            return;
        };
        // A completion/new turn notification may overtake the read response.
        let mut active = locked(&self.state.turns);
        if locked(&self.state.completed_turns)
            .get(thread)
            .map(String::as_str)
            != Some(id)
        {
            active.entry(thread.into()).or_insert_with(|| id.into());
        }
    }

    pub(crate) fn endpoint(&self) -> &'static str {
        if self.is_shared() {
            "unix://shared-codex-app-server"
        } else {
            "stdio://private-codex-app-server"
        }
    }

    pub fn is_ready(&self) -> bool {
        self.state.ready.load(Ordering::Acquire) && !self.state.cancel.is_cancelled()
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.state.cancel.clone()
    }
    pub fn stop(&self) {
        self.state.stop();
    }

    pub async fn initialize(&self) -> Result<(), AgentError> {
        let response = self.call_inner("initialize", json!({
            "clientInfo": {"name":"muqun_gateway", "title":"Muqun Gateway", "version":env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi":false, "requestAttestation":false, "explicitGatewayOauth":true}
        })).await?;
        let agent = response["userAgent"]
            .as_str()
            .ok_or_else(|| AgentError::Protocol("invalid Codex initialize response".into()))?;
        if !agent.split(['/', ' ', '(']).any(|s| s == super::VERSION) {
            self.stop();
            return Err(AgentError::Unsupported(
                "Codex app-server version; expected 0.162.1".into(),
            ));
        }
        let actual = response["codexHome"]
            .as_str()
            .filter(|p| std::path::Path::new(p).is_absolute())
            .and_then(|p| std::fs::canonicalize(p).ok());
        if let Some(actual) = actual {
            let _ = self.codex_home.set(actual);
        }
        if let Some(expected) = &self.expected_home {
            if self.codex_home() != Some(expected.as_path()) {
                self.stop();
                return Err(AgentError::Protocol("Codex daemon home mismatch".into()));
            }
        }
        self.write(json!({"method":"initialized"})).await?;
        // Only app-server reads its auth configuration. Gateway neither reads
        // credential files nor refreshes/logs in on the owner's behalf.
        let account = self
            .call_inner("account/read", json!({"refreshToken":false}))
            .await?;
        if account["requiresOpenaiAuth"].as_bool().is_none() {
            return Err(AgentError::Protocol(
                "invalid Codex account response".into(),
            ));
        }
        let valid_account = match account.get("account") {
            Some(Value::Null) => true,
            Some(value) => match value["type"].as_str() {
                Some("apiKey") => true,
                Some("chatgpt") => {
                    value
                        .get("email")
                        .is_some_and(|email| email.is_null() || email.is_string())
                        && value["planType"].is_string()
                }
                Some("amazonBedrock") => value["usesCodexManagedCredentials"].is_boolean(),
                _ => false,
            },
            None => false,
        };
        if !valid_account {
            return Err(AgentError::Protocol(
                "invalid Codex account response".into(),
            ));
        }
        if account["requiresOpenaiAuth"] == true && account["account"].is_null() {
            self.stop();
            return Err(AgentError::NotAvailable(
                "Configure Codex authentication on this host before enabling the Gateway adapter"
                    .into(),
            ));
        }
        if let Some(cwd) = self.config_cwd() {
            let config = self
                .call_inner("config/read", json!({"includeLayers":false,"cwd":cwd}))
                .await?;
            let provider = provider_from_config(&config)?;
            let _ = self.configured_provider.set(provider);
        }
        self.state.ready.store(true, Ordering::Release);
        Ok(())
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.call_sized(method, params)
            .await
            .map(|(value, _)| value)
    }

    pub async fn call_sized(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(Value, usize), AgentError> {
        if !self.is_ready() {
            return Err(AgentError::NotAvailable(
                "Codex is not initialized and authenticated".into(),
            ));
        }
        self.call_inner_sized(method, params).await
    }

    async fn call_inner(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.call_inner_sized(method, params)
            .await
            .map(|(value, _)| value)
    }

    async fn call_inner_sized(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(Value, usize), AgentError> {
        if self.state.cancel.is_cancelled() {
            return Err(disconnected());
        }
        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, receiver) = oneshot::channel();
        {
            let mut pending = locked(&self.state.pending);
            if pending.len() >= MAX_PENDING {
                return Err(AgentError::NotAvailable(
                    "Codex request capacity exhausted".into(),
                ));
            }
            pending.insert(
                id,
                Pending {
                    reply,
                    selection_config: method == "config/read",
                    attach_expected: (method == "thread/resume")
                        .then(|| params["threadId"].as_str().unwrap_or("").to_owned()),
                    attaches_thread: matches!(
                        method,
                        "thread/start" | "thread/resume" | "thread/fork"
                    ),
                    turn_thread: if method == "turn/start" {
                        params["threadId"].as_str().map(str::to_owned)
                    } else {
                        None
                    },
                },
            );
        }
        let _guard = PendingGuard {
            state: self.state.clone(),
            id,
        };
        let deadline = tokio::time::Instant::now() + self.timeout;
        tokio::time::timeout_at(
            deadline,
            self.write(json!({"id":id,"method":method,"params":params})),
        )
        .await
        .map_err(|_| disconnected())??;
        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(disconnected()),
            Err(_) => {
                // A mutation may have been accepted. Close this connection;
                // do not submit it again or allow further work to race it.
                self.stop();
                Err(AgentError::RequestFailed(
                    "Codex request deadline exceeded; outcome unknown; not retried".into(),
                ))
            }
        }
    }

    async fn write(&self, value: Value) -> Result<(), AgentError> {
        let mut bytes = serde_json::to_vec(&value)
            .map_err(|_| AgentError::Protocol("invalid Codex request".into()))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(AgentError::InvalidRequest(
                "Codex request exceeds frame limit".into(),
            ));
        }
        bytes.push(b'\n');
        let (done, ack) = oneshot::channel();
        let mut guard = WriteGuard {
            state: self.state.clone(),
            finished: false,
        };
        let deadline = tokio::time::Instant::now() + self.timeout;
        tokio::select! {
            biased;
            _ = self.state.cancel.cancelled() => return Err(disconnected()),
            result = tokio::time::timeout_at(deadline, self.outgoing.send(WritePacket {bytes,done})) => {
                if !matches!(result,Ok(Ok(()))) {return Err(disconnected());}
            }
        };
        let result = match tokio::time::timeout_at(deadline, ack).await {
            Ok(Ok(result)) => result,
            _ => Err(disconnected()),
        };
        guard.finished = result.is_ok();
        result
    }

    pub fn snapshot(&self, value: Value, revision: u64) -> Result<(), AgentError> {
        self.state.emit(CodexEvent::Snapshot {
            thread: value,
            revision,
        })
    }
    pub fn metadata_refresh_failed(&self, thread: &str, revision: u64) -> Result<(), AgentError> {
        self.state.emit(CodexEvent::MetadataRefreshFailed {
            thread: thread.into(),
            revision,
        })
    }
    pub fn active_turn(&self, thread: &str) -> Option<String> {
        locked(&self.state.turns).get(thread).cloned()
    }
    pub fn server_requests(&self, thread: &str) -> Vec<ServerRequest> {
        locked(&self.state.requests)
            .values()
            .filter(|r| r.params["threadId"] == thread && !r.answering)
            .cloned()
            .collect()
    }

    /// Public callback handles include the connection generation. Native RPC
    /// ids can be reused after a restart and must not authorize a new callback.
    pub fn request_id(&self, id: &Value) -> Option<String> {
        self.state.request_id(id)
    }

    pub async fn reply(&self, thread: &str, id: &str, result: Value) -> Result<(), AgentError> {
        let request = {
            let mut requests = locked(&self.state.requests);
            let request = requests
                .get_mut(id)
                .filter(|request| !request.answering)
                .ok_or_else(|| {
                    AgentError::InvalidRequest("Codex request expired or already answered".into())
                })?;
            if request.params["threadId"] != thread {
                return Err(AgentError::InvalidRequest(
                    "Codex request belongs to another thread".into(),
                ));
            }
            request.answering = true;
            request.clone()
        };
        // Claim before writing, but keep the capacity reservation until actual
        // write acknowledgment. An ambiguous write never permits another answer.
        if let Err(error) = self.write(json!({"id":request.id,"result":result})).await {
            self.stop();
            return Err(error);
        }
        locked(&self.state.requests).remove(id);
        self.state.emit(CodexEvent::Resolved {
            thread: thread.into(),
            id: id.into(),
            form: request.method == "item/tool/requestUserInput",
        })
    }

    pub async fn reject_unsupported(&self, id: &Value) -> Result<(), AgentError> {
        let key = self
            .request_id(id)
            .ok_or_else(|| AgentError::Protocol("invalid Codex request id".into()))?;
        locked(&self.state.requests).remove(&key);
        self.write(json!({"id":id,"error":{"code":-32601,"message":"Gateway does not support this server request"}})).await
    }
}

pub(crate) fn provider_from_config(config: &Value) -> Result<String, AgentError> {
    match config["config"].get("model_provider") {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        // Pinned core/src/config/mod.rs uses openai only when selection is null.
        Some(Value::Null) => Ok("openai".into()),
        _ => Err(AgentError::Protocol(
            "invalid Codex configured provider".into(),
        )),
    }
}

pub fn request_key(id: &Value) -> Option<String> {
    match id {
        Value::String(s) if s.len() <= 512 => Some(id.to_string()),
        Value::Number(n) if n.as_i64().is_some() || n.as_u64().is_some() => Some(id.to_string()),
        _ => None,
    }
}

async fn read_frames<R: AsyncRead + Unpin>(mut reader: R, state: Arc<State>) {
    let mut chunk = [0; 8192];
    let mut frame = Vec::new();
    loop {
        let count = tokio::select! {
            biased;
            _ = state.cancel.cancelled() => return,
            result = reader.read(&mut chunk) => match result { Ok(0) | Err(_) => return, Ok(n) => n },
        };
        for &byte in &chunk[..count] {
            if byte == b'\n' {
                if frame.is_empty() {
                    continue;
                }
                let value = match serde_json::from_slice::<Value>(&frame) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let frame_bytes = frame.len();
                frame.clear();
                if route_frame(&state, value, frame_bytes).is_err() {
                    return;
                }
                // A single pipe read can hold many tiny deltas. Let the mirror
                // pump run between frames instead of mistaking a legitimate
                // burst in one read for an unrecoverable backlog.
                tokio::task::yield_now().await;
            } else {
                if frame.len() >= MAX_FRAME_BYTES {
                    return;
                }
                frame.push(byte);
            }
        }
    }
}

fn route_frame(state: &State, value: Value, frame_bytes: usize) -> Result<(), AgentError> {
    if state.cancel.is_cancelled() {
        return Err(disconnected());
    }
    if let Some(method) = value["method"].as_str() {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        // Shared servers broadcast metadata for other clients. Only an accepted
        // explicit start/fork/rejoin authorizes this connection's live projection.
        if state.shared {
            let thread = params["threadId"]
                .as_str()
                .or_else(|| params["thread"]["id"].as_str());
            if thread.is_none_or(|id| !locked(&state.subscribed).contains(id)) {
                return Ok(());
            }
        }
        if let Some(id) = value.get("id") {
            let key = state
                .request_id(id)
                .ok_or_else(|| AgentError::Protocol("invalid Codex request id".into()))?;
            let mut requests = locked(&state.requests);
            let bytes = serde_json::to_vec(&params)
                .map_err(|_| AgentError::Protocol("invalid Codex callback".into()))?
                .len();
            if requests.len() >= MAX_PENDING
                || requests.values().map(|r| r.bytes).sum::<usize>() + bytes
                    > MAX_SERVER_REQUEST_BYTES
                || requests.contains_key(&key)
            {
                return Err(AgentError::Protocol(
                    "Codex server request capacity or duplicate id".into(),
                ));
            }
            requests.insert(
                key,
                ServerRequest {
                    id: id.clone(),
                    method: method.into(),
                    params: params.clone(),
                    bytes,
                    answering: false,
                },
            );
            drop(requests);
            state.emit(CodexEvent::Request {
                id: id.clone(),
                method: method.into(),
                params,
            })
        } else {
            let thread = params["threadId"].as_str().unwrap_or("");
            if method == "turn/started" {
                if let Some(turn) = params["turn"]["id"].as_str() {
                    locked(&state.turns).insert(thread.into(), turn.into());
                }
            } else if method == "turn/completed" {
                let turn = params["turn"]["id"].as_str();
                let mut turns = locked(&state.turns);
                if turns.get(thread).map(String::as_str) == turn {
                    turns.remove(thread);
                }
                drop(turns);
                if let Some(turn) = turn {
                    let mut completed = locked(&state.completed_turns);
                    if completed.len() >= 4096 && !completed.contains_key(thread) {
                        return Err(AgentError::Protocol(
                            "Codex completed-turn capacity exhausted".into(),
                        ));
                    }
                    completed.insert(thread.into(), turn.into());
                }
                locked(&state.requests).retain(|_, r| {
                    !(r.params["threadId"] == thread && r.params["turnId"].as_str() == turn)
                });
            } else if method == "serverRequest/resolved" {
                if let Some(key) = state.request_id(&params["requestId"]) {
                    let mut requests = locked(&state.requests);
                    // Local replies publish their own resolution after flush.
                    // Do not duplicate it or race a blocked/failed local write.
                    if requests.get(&key).is_none_or(|request| request.answering) {
                        return Ok(());
                    }
                    requests.remove(&key);
                }
            } else if matches!(method, "thread/deleted" | "thread/closed")
                || (method == "thread/status/changed" && params["status"]["type"] == "notLoaded")
            {
                locked(&state.subscribed).remove(thread);
                locked(&state.turns).remove(thread);
                locked(&state.completed_turns).remove(thread);
                locked(&state.requests).retain(|_, r| r.params["threadId"] != thread);
            }
            state.emit(CodexEvent::Notification {
                method: method.into(),
                params,
            })
        }
    } else if let Some(id) = value["id"].as_u64() {
        let has_result = value.get("result").is_some();
        let has_error = value.get("error").is_some();
        if has_result == has_error
            || (has_error
                && (value["error"]["code"].as_i64().is_none()
                    || value["error"]["message"].as_str().is_none()))
        {
            return Err(AgentError::Protocol(
                "invalid Codex RPC response envelope".into(),
            ));
        }
        let reply = locked(&state.pending).remove(&id);
        if let Some(pending) = reply {
            let result = if value.get("error").is_some() {
                // Upstream diagnostics can contain prompts, paths and tokens.
                Err(AgentError::RequestFailed(
                    "Codex rejected the request".into(),
                ))
            } else {
                value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| AgentError::Protocol("Codex response missing result".into()))
            };
            if pending.attaches_thread && state.shared {
                if let Ok(result) = &result {
                    let thread = result["thread"]["id"].as_str().ok_or_else(|| {
                        AgentError::Protocol("Codex attach response missing identity".into())
                    })?;
                    if pending
                        .attach_expected
                        .as_deref()
                        .is_some_and(|expected| expected != thread)
                    {
                        return Err(AgentError::Protocol(
                            "Codex rejoin identity mismatch".into(),
                        ));
                    }
                    let mut subscribed = locked(&state.subscribed);
                    if subscribed.len() >= 4096 && !subscribed.contains(thread) {
                        return Err(AgentError::Protocol(
                            "Codex subscription capacity exhausted".into(),
                        ));
                    }
                    let first = subscribed.insert(thread.into());
                    drop(subscribed);
                    if first {
                        // Publish at the response boundary, before subsequent
                        // notifications and before waking the requesting task.
                        // A later metadata snapshot must not roll status back.
                        state.emit(CodexEvent::Attached {
                            thread: result["thread"].clone(),
                        })?;
                    }
                }
            }
            if let (Some(thread), Ok(result)) = (&pending.turn_thread, &result) {
                if let Some(turn) = result["turn"]["id"].as_str() {
                    // The acknowledgement can race a completion notification.
                    // Never resurrect a turn that the stream already closed.
                    if result["turn"]["status"] == "inProgress"
                        && locked(&state.completed_turns)
                            .get(thread)
                            .map(String::as_str)
                            != Some(turn)
                    {
                        locked(&state.turns).insert(thread.clone(), turn.into());
                    }
                }
            }
            let result = result.map(|mut value| {
                if pending.selection_config {
                    let mut selected = serde_json::Map::new();
                    for key in ["model_provider", "model", "model_reasoning_effort"] {
                        if let Some(field) = value["config"].get(key) {
                            selected.insert(key.into(), field.clone());
                        }
                    }
                    // Discard all other config, origins and layer payloads.
                    value = json!({"config":selected});
                }
                (value, frame_bytes)
            });
            let _ = pending.reply.send(result);
        }
        Ok(())
    } else {
        Err(AgentError::Protocol("invalid Codex RPC envelope".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[derive(Default)]
    struct WriteProbe {
        bytes: Mutex<Vec<u8>>,
        flush_started: AtomicBool,
        release: AtomicBool,
        waker: Mutex<Option<std::task::Waker>>,
    }
    struct ProbedWriter {
        probe: Arc<WriteProbe>,
        fail_after: Option<usize>,
        fail_flush: bool,
    }
    impl AsyncWrite for ProbedWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let mut written = locked(&self.probe.bytes);
            let remaining = self
                .fail_after
                .map(|n| n.saturating_sub(written.len()))
                .unwrap_or(bytes.len());
            if remaining == 0 {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            let n = remaining.min(bytes.len());
            written.extend_from_slice(&bytes[..n]);
            std::task::Poll::Ready(Ok(n))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.probe.flush_started.store(true, Ordering::Release);
            if self.fail_flush {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            *locked(&self.probe.waker) = Some(cx.waker().clone());
            if self.probe.release.load(Ordering::Acquire) {
                std::task::Poll::Ready(Ok(()))
            } else {
                std::task::Poll::Pending
            }
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    async fn callback_probe(
        fail_after: Option<usize>,
        fail_flush: bool,
        timeout: Duration,
    ) -> (
        Arc<CodexClient>,
        mpsc::Receiver<CodexEvent>,
        Arc<WriteProbe>,
        tokio::io::DuplexStream,
        String,
    ) {
        let (reader, mut peer) = tokio::io::duplex(4096);
        let probe = Arc::new(WriteProbe::default());
        let (client, mut events) = CodexClient::with_timeout(
            reader,
            ProbedWriter {
                probe: probe.clone(),
                fail_after,
                fail_flush,
            },
            timeout,
        );
        client.state.ready.store(true, Ordering::Release);
        let id = json!("synthetic/native-callback");
        peer.write_all(format!("{}\n",json!({"id":id,"method":"item/commandExecution/requestApproval","params":{"threadId":"synthetic-thread"}})).as_bytes()).await.unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            CodexEvent::Request { .. }
        ));
        let handle = client.request_id(&id).unwrap();
        (client, events, probe, peer, handle)
    }

    #[tokio::test]
    async fn codex_callback_reply_waits_for_flush_ack_then_resolves_once_with_native_id() {
        let (client, mut events, probe, mut peer, id) =
            callback_probe(None, false, Duration::from_secs(2)).await;
        let reply = client.reply("synthetic-thread", &id, json!({"decision":"accept"}));
        tokio::pin!(reply);
        assert!(tokio::time::timeout(Duration::from_millis(25), &mut reply)
            .await
            .is_err());
        assert!(probe.flush_started.load(Ordering::Acquire));
        peer.write_all(format!("{}\n",json!({"method":"serverRequest/resolved","params":{"threadId":"synthetic-thread","requestId":"synthetic/native-callback"}})).as_bytes()).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut reply)
            .await
            .is_err());
        assert!(
            events.try_recv().is_err(),
            "enqueue/write without flush is not resolution"
        );
        probe.release.store(true, Ordering::Release);
        locked(&probe.waker).take().unwrap().wake();
        reply.await.unwrap();
        assert!(
            matches!(events.recv().await.unwrap(),CodexEvent::Resolved {id:resolved,..} if resolved==id)
        );
        let bytes = locked(&probe.bytes).clone();
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"id":"synthetic/native-callback","result":{"decision":"accept"}})
        );
        assert!(client
            .reply("synthetic-thread", &id, json!({"decision":"accept"}))
            .await
            .is_err());
        assert_eq!(*locked(&probe.bytes), bytes);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn codex_callback_partial_write_or_flush_failure_fences_without_success_resolution_or_retry(
    ) {
        for (fail_after, fail_flush) in [(Some(5), false), (None, true)] {
            let (client, mut events, probe, _peer, id) =
                callback_probe(fail_after, fail_flush, Duration::from_secs(2)).await;
            assert!(client
                .reply("synthetic-thread", &id, json!({"decision":"accept"}))
                .await
                .is_err());
            assert!(!client.is_ready());
            let bytes = locked(&probe.bytes).clone();
            assert!(client
                .reply("synthetic-thread", &id, json!({"decision":"accept"}))
                .await
                .is_err());
            assert_eq!(*locked(&probe.bytes), bytes);
            while let Ok(event) = events.try_recv() {
                assert!(!matches!(event, CodexEvent::Resolved { .. }));
            }
            if fail_after.is_some() {
                assert_eq!(bytes.len(), 5);
            }
        }
    }

    #[tokio::test]
    async fn codex_callback_blocked_write_deadline_and_caller_cancel_fence_connection() {
        let (client, mut events, _probe, _peer, id) =
            callback_probe(None, false, Duration::from_millis(30)).await;
        assert!(client
            .reply("synthetic-thread", &id, json!({"decision":"accept"}))
            .await
            .is_err());
        assert!(!client.is_ready());
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event, CodexEvent::Resolved { .. }));
        }
        let (client, mut events, _probe, _peer, id) =
            callback_probe(None, false, Duration::from_secs(2)).await;
        {
            let reply = client.reply("synthetic-thread", &id, json!({"decision":"accept"}));
            tokio::pin!(reply);
            assert!(tokio::time::timeout(Duration::from_millis(10), &mut reply)
                .await
                .is_err());
        }
        assert!(!client.is_ready());
        while let Ok(event) = events.try_recv() {
            assert!(!matches!(event, CodexEvent::Resolved { .. }));
        }
    }

    #[tokio::test]
    async fn codex_out_of_order_replies_and_server_callbacks_share_duplex_without_cross_routing() {
        let (io, server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(io);
        let (client, mut events) = CodexClient::from_io(reader, writer);
        client.state.ready.store(true, Ordering::Release);
        let (read, mut write) = tokio::io::split(server);
        let server = tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            let first: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let second: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            for frame in [
                json!({"id":first["id"],"method":"item/commandExecution/requestApproval","params":{"threadId":"synthetic-a","turnId":"synthetic-turn","itemId":"synthetic-tool"}}),
                json!({"method":"synthetic/unknown","params":{}}),
                json!({"id":second["id"],"result":second["params"]}),
                json!({"id":first["id"],"result":first["params"]}),
            ] {
                write
                    .write_all(format!("{frame}\n").as_bytes())
                    .await
                    .unwrap();
            }
            // Keep the pipe open until the explicit client shutdown.
            let _ = lines.next_line().await;
        });
        let (a, b) = tokio::join!(
            client.call("read/a", json!({"thread":"synthetic-a"})),
            client.call("read/b", json!({"thread":"synthetic-b"}))
        );
        assert_eq!(a.unwrap()["thread"], "synthetic-a");
        assert_eq!(b.unwrap()["thread"], "synthetic-b");
        assert!(matches!(
            events.recv().await,
            Some(CodexEvent::Request { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(CodexEvent::Notification { .. })
        ));
        client.stop();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn codex_cancelled_rpc_closes_ambiguous_connection_and_removes_pending() {
        let (io, mut server) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(io);
        let (client, _events) = CodexClient::from_io(read, write);
        client.state.ready.store(true, Ordering::Release);
        let pending = tokio::spawn({
            let c = client.clone();
            async move {
                c.call("turn/start", json!({"threadId":"synthetic-a"}))
                    .await
            }
        });
        let mut bytes = [0; 1024];
        assert!(server.read(&mut bytes).await.unwrap() > 0);
        pending.abort();
        let _ = pending.await;
        assert!(client.cancel_token().is_cancelled());
        assert!(locked(&client.state.pending).is_empty());
        assert!(client.call("turn/start", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn codex_jsonl_accepts_split_utf8_and_closes_on_malformed_or_oversize_frames() {
        let (io, mut server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(io);
        let (client, mut events) = CodexClient::from_io(reader, writer);
        let bytes = "{\"method\":\"synthetic/unknown\",\"params\":{\"delta\":\"é\"}}\n".as_bytes();
        for byte in bytes {
            server.write_all(&[*byte]).await.unwrap();
        }
        assert!(
            matches!(events.recv().await,Some(CodexEvent::Notification {params,..}) if params["delta"] == "é")
        );
        server.write_all(b"not-json\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), client.cancel_token().cancelled())
            .await
            .unwrap();

        let (io, mut server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(io);
        let (client, _events) = CodexClient::from_io(reader, writer);
        let send = tokio::spawn(async move {
            let _ = server.write_all(&vec![b'x'; MAX_FRAME_BYTES + 1]).await;
        });
        tokio::time::timeout(Duration::from_secs(2), client.cancel_token().cancelled())
            .await
            .unwrap();
        send.await.unwrap();
    }

    #[tokio::test]
    async fn codex_deadline_disconnects_pending_without_resubmitting_and_drop_cancels() {
        let (io, server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(io);
        let (client, _events) =
            CodexClient::with_timeout(reader, writer, Duration::from_millis(25));
        client.state.ready.store(true, Ordering::Release);
        let (read, _write) = tokio::io::split(server);
        let mut lines = BufReader::new(read).lines();
        let request = client.call("turn/start", json!({"threadId":"synthetic-a"}));
        let (result, line) = tokio::join!(request, lines.next_line());
        assert!(matches!(result, Err(AgentError::RequestFailed(_))));
        assert_eq!(
            serde_json::from_str::<Value>(&line.unwrap().unwrap()).unwrap()["method"],
            "turn/start"
        );
        assert!(locked(&client.state.pending).is_empty());
        assert!(client.call("turn/start", json!({})).await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );

        let (io, _server) = tokio::io::duplex(8);
        let (read, write) = tokio::io::split(io);
        let (client, _events) = CodexClient::from_io(read, write);
        let token = client.cancel_token();
        drop(client);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn codex_overload_and_pending_disconnect_fail_closed() {
        let (io, mut server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(io);
        let (client, _events) = CodexClient::from_io(reader, writer);
        client.state.ready.store(true, Ordering::Release);
        for _ in 0..=QUEUE_SIZE {
            if server
                .write_all(b"{\"method\":\"synthetic/unknown\",\"params\":{}}\n")
                .await
                .is_err()
            {
                break;
            }
        }
        tokio::time::timeout(Duration::from_secs(2), client.cancel_token().cancelled())
            .await
            .unwrap();
        assert!(!client.is_ready());

        let (io, mut server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(io);
        let (client, _events) = CodexClient::from_io(reader, writer);
        client.state.ready.store(true, Ordering::Release);
        let pending = tokio::spawn({
            let c = client.clone();
            async move { c.call("turn/start", json!({})).await }
        });
        let mut bytes = [0; 1024];
        assert!(server.read(&mut bytes).await.unwrap() > 0);
        drop(server);
        assert!(matches!(
            pending.await.unwrap(),
            Err(AgentError::NotAvailable(_))
        ));
        assert!(locked(&client.state.pending).is_empty());
    }

    #[tokio::test]
    async fn codex_auth_absence_and_bad_rpc_do_not_leak_upstream_diagnostics() {
        let (io, server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(io);
        let (client, _events) = CodexClient::from_io(reader, writer);
        tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let r: Value = serde_json::from_str(&line).unwrap();
                let result = match r["method"].as_str() {
                    Some("initialize") => super::super::tests::golden()["initialize"].clone(),
                    Some("initialized") => continue,
                    Some("account/read") => json!({"requiresOpenaiAuth":true,"account":null}),
                    _ => panic!("must not start auth"),
                };
                let bytes = format!("{}\n", json!({"id":r["id"],"result":result}));
                if write.write_all(bytes.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let err = client.initialize().await.unwrap_err();
        assert!(err.to_string().contains("authentication"));
        assert!(!client.is_ready());

        let (io, mut server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(io);
        let (client, _events) = CodexClient::from_io(reader, writer);
        client.state.ready.store(true, Ordering::Release);
        let request = tokio::spawn({
            let c = client.clone();
            async move { c.call("thread/read", json!({})).await }
        });
        let mut bytes = [0; 1024];
        assert!(server.read(&mut bytes).await.unwrap() > 0);
        server.write_all(b"{\"id\":1,\"error\":{\"code\":-32000,\"message\":\"synthetic-secret-that-must-not-be-returned\"}}\n").await.unwrap();
        assert!(!request
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("synthetic-secret"));
    }

    #[tokio::test]
    async fn codex_malformed_account_cannot_pass_readiness() {
        let (io, server) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(io);
        let (client, _events) = CodexClient::from_io(read, write);
        let task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = match request["method"].as_str() {
                    Some("initialize") => super::super::tests::golden()["initialize"].clone(),
                    Some("initialized") => continue,
                    Some("account/read") => json!({"requiresOpenaiAuth":true,"account":false}),
                    _ => panic!("malformed account must not admit ordinary requests"),
                };
                write
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        assert!(matches!(
            client.initialize().await,
            Err(AgentError::Protocol(_))
        ));
        assert!(!client.is_ready());
        client.stop();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }
}
