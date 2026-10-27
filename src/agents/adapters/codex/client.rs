mod events;
#[cfg(unix)]
pub(crate) mod websocket;
pub use events::CodexEvents;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agents::ports::agent::AgentError;

pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
// A stored item page can aggregate several multi-MiB image results. Match the
// existing traversal byte budget, not the outbound/text-row bound. Only a
// correlated shared thread/items/list result may use this larger wire budget.
pub(crate) const MAX_SHARED_READ_FRAME_BYTES: usize = 32 * 1024 * 1024;
pub(super) const MAX_HISTORY_FRAME_BYTES: usize = 256 * 1024 * 1024;
// One failed bounded decoder (message + frame), then one complete single item.
// This is total wire work, not an allocation or primary/control frame limit.
pub(super) const MAX_HISTORY_WIRE_WORK_BYTES: usize = 3 * MAX_HISTORY_FRAME_BYTES;
const MAX_PENDING: usize = 32;
const MAX_SERVER_REQUEST_BYTES: usize = 8 * 1024 * 1024;
// Slots for distinct events. Contiguous text deltas share one slot (see
// `events`), so a token stream costs a slot per interruption, not per token.
const QUEUE_SIZE: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

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
    /// Bounded current-turn reconstruction after an accepted shared subscription.
    RecoveredQuestions {
        ownership: u64,
        thread: String,
        turn: String,
        items: Vec<Value>,
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
    diagnostic_method: &'static str,
    turn_thread: Option<String>,
    selection_config: bool,
    mcp_config: bool,
    attaches_thread: bool,
    attach_expected: Option<String>,
}

struct State {
    generation: String,
    shared: bool,
    history_only: bool,
    read_limit: usize,
    subscribed: Mutex<HashMap<String, u64>>,
    ownership_revision: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    requests: Mutex<HashMap<String, ServerRequest>>,
    turns: Mutex<HashMap<String, String>>,
    /// Active compaction, bound to both turn and item identity.
    compactions: Mutex<HashMap<String, (String, String)>>,
    completed_turns: Mutex<HashMap<String, String>>,
    // Only completions of observed active turns authorize lifecycle cleanup.
    lifecycle_completed_turns: Mutex<HashMap<String, String>>,
    next_id: AtomicU64,
    ready: AtomicBool,
    disconnect_reason: std::sync::OnceLock<&'static str>,
    history_capacity: std::sync::OnceLock<HistoryCapacity>,
    cancel: CancellationToken,
    events: events::EventSender,
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
    #[track_caller]
    fn stop(&self) {
        self.stop_reason("local_stop");
    }

    #[track_caller]
    fn stop_reason(&self, reason: &'static str) {
        self.stop_request(reason, None, "none");
    }

    #[track_caller]
    fn stop_request(&self, reason: &'static str, rpc_id: Option<u64>, method: &'static str) {
        // One first-cause record per connection, before cancellation wakes other
        // owners. Never log params, upstream errors or native callback ids.
        if self.disconnect_reason.set(reason).is_ok() {
            let pending = locked(&self.pending);
            let inflight: Vec<_> = pending
                .iter()
                .map(|(id, request)| (*id, request.diagnostic_method))
                .collect();
            tracing::warn!(
                connection = %self.generation,
                shared = self.shared,
                history_only = self.history_only,
                reason,
                ?rpc_id,
                method,
                ?inflight,
                source = %std::panic::Location::caller(),
                "Codex connection stopped; requests are not retried"
            );
        }
        self.ready.store(false, Ordering::Release);
        self.cancel.cancel();
        for (_, pending) in locked(&self.pending).drain() {
            let _ = pending.reply.send(Err(disconnected()));
        }
        locked(&self.requests).clear();
        locked(&self.subscribed).clear();
        locked(&self.turns).clear();
        locked(&self.compactions).clear();
        locked(&self.completed_turns).clear();
        locked(&self.lifecycle_completed_turns).clear();
    }

    fn emit(&self, event: CodexEvent) -> Result<(), AgentError> {
        if self.cancel.is_cancelled() {
            return Err(disconnected());
        }
        if let Err(error) = self.events.try_send(event) {
            // Never skip a delta or approval while claiming a connected stream.
            self.stop_reason(match error {
                mpsc::error::TrySendError::Full(_) => "event_queue_overflow",
                mpsc::error::TrySendError::Closed(_) => "event_consumer_closed",
            });
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
    shared_socket: Option<std::path::PathBuf>,
    #[cfg(unix)]
    pub(crate) shared_identity: Option<super::daemon::SharedIdentity>,
    #[cfg(unix)]
    history: Arc<tokio::sync::Mutex<HistoryConnection>>,
    #[cfg(unix)]
    history_admission: Arc<tokio::sync::Semaphore>,
}

#[cfg(unix)]
#[derive(Default)]
struct HistoryConnection {
    client: Option<Arc<CodexClient>>,
    retry_at: Option<tokio::time::Instant>,
    item_limit: Option<u64>,
}

#[derive(Clone, Copy)]
struct HistoryCapacity {
    wire_work: usize,
}

/// These are the only image-bearing history operations. Central dispatch also
/// covers bounded rollout/question readers which do not use Budget::call.
pub(super) fn image_history_request(method: &str, params: &Value) -> bool {
    method == "thread/items/list"
        || (method == "thread/turns/list" && params["itemsView"] == "full")
        || (method == "thread/read" && params["includeTurns"] == true)
}

fn history_error() -> AgentError {
    AgentError::RequestFailed(
        "Codex history read failed; primary connection retained; page not consumed; no mutation retried"
            .into(),
    )
}

struct WritePacket {
    bytes: Vec<u8>,
    done: oneshot::Sender<Result<(), AgentError>>,
}
struct WriteGuard {
    state: Arc<State>,
    finished: bool,
    fence_on_cancel: bool,
    deadline: tokio::time::Instant,
}
impl Drop for WriteGuard {
    fn drop(&mut self) {
        if !self.finished && self.fence_on_cancel {
            self.state
                .stop_reason(if tokio::time::Instant::now() >= self.deadline {
                    "mutation_write_deadline"
                } else {
                    "mutation_write_cancelled"
                });
        }
    }
}

impl Drop for CodexClient {
    fn drop(&mut self) {
        self.state.stop_reason("last_client_dropped");
    }
}

struct PendingGuard {
    state: Arc<State>,
    id: u64,
    fence_on_cancel: bool,
    enqueued: AtomicBool,
    diagnostic_method: &'static str,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.fence_on_cancel && self.enqueued.load(Ordering::Acquire) {
            // Keep abandoned reads counted against MAX_PENDING until their
            // response arrives. Repeated cancellation must not create an
            // unbounded stream of outstanding daemon work. The closed receiver
            // discards the late result; transport shutdown drains it as usual.
            return;
        }
        let unfinished = locked(&self.state.pending).remove(&self.id).is_some();
        if unfinished && self.fence_on_cancel {
            // Caller cancellation after a write is also ambiguous. Do not keep
            // accepting prompts while an unacknowledged mutation may run.
            self.state.stop_request(
                "mutation_caller_cancelled",
                Some(self.id),
                self.diagnostic_method,
            );
        }
    }
}

/// Only known read-only RPCs may outlive their caller. The transport actor
/// still owns the complete frame and fences real I/O failures. A late reply
/// to an abandoned read has a closed receiver and is discarded by route_frame.
/// Unknown methods, initialization, resumes, turns and callback replies retain
/// the conservative mutation fence; nothing is automatically resubmitted.
fn read_only_request(method: &str, params: &Value) -> bool {
    matches!(
        method,
        "config/read"
            | "mcpServerStatus/list"
            | "account/rateLimits/read"
            | "model/list"
            | "skills/list"
            | "thread/list"
            | "thread/read"
            | "thread/loaded/list"
            | "thread/turns/list"
            | "thread/items/list"
    ) || (method == "account/read" && params["refreshToken"] == false)
}

// Unknown input is never a log field, even when it is used as an RPC method.
fn diagnostic_method(method: &str) -> &'static str {
    match method {
        "initialize" => "initialize",
        "initialized" => "initialized",
        "account/read" => "account/read",
        "account/rateLimits/read" => "account/rateLimits/read",
        "config/read" => "config/read",
        "model/list" => "model/list",
        "skills/list" => "skills/list",
        "thread/list" => "thread/list",
        "thread/read" => "thread/read",
        "thread/loaded/list" => "thread/loaded/list",
        "thread/turns/list" => "thread/turns/list",
        "thread/items/list" => "thread/items/list",
        "thread/start" => "thread/start",
        "thread/resume" => "thread/resume",
        "thread/fork" => "thread/fork",
        "thread/name/set" => "thread/name/set",
        "thread/delete" => "thread/delete",
        "thread/compact/start" => "thread/compact/start",
        "turn/start" => "turn/start",
        "turn/steer" => "turn/steer",
        "turn/interrupt" => "turn/interrupt",
        _ => "other",
    }
}

impl CodexClient {
    pub(crate) fn from_io_at<R, W>(reader: R, writer: W, cwd: String) -> (Arc<Self>, CodexEvents)
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

    pub fn from_io<R, W>(reader: R, writer: W) -> (Arc<Self>, CodexEvents)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::with_timeout(reader, writer, REQUEST_TIMEOUT)
    }

    fn with_timeout<R, W>(reader: R, mut writer: W, timeout: Duration) -> (Arc<Self>, CodexEvents)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (events, rx) = events::channel(QUEUE_SIZE);
        let (outgoing, mut writes) = mpsc::channel::<WritePacket>(MAX_PENDING);
        let state = Arc::new(State {
            generation: uuid::Uuid::new_v4().to_string(),
            shared: false,
            history_only: false,
            read_limit: MAX_FRAME_BYTES,
            subscribed: Mutex::new(HashMap::new()),
            ownership_revision: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            requests: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            compactions: Mutex::new(HashMap::new()),
            completed_turns: Mutex::new(HashMap::new()),
            lifecycle_completed_turns: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ready: AtomicBool::new(false),
            disconnect_reason: std::sync::OnceLock::new(),
            history_capacity: std::sync::OnceLock::new(),
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
                shared_socket: None,
                #[cfg(unix)]
                shared_identity: None,
                #[cfg(unix)]
                history: Arc::default(),
                #[cfg(unix)]
                history_admission: Arc::new(tokio::sync::Semaphore::new(1)),
            }),
            rx,
        )
    }

    pub(crate) fn codex_home(&self) -> Option<&std::path::Path> {
        self.codex_home.get().map(std::path::PathBuf::as_path)
    }

    pub(crate) fn terminal_endpoint(&self) -> Option<String> {
        self.is_ready().then_some(())?;
        let path = self.shared_socket.as_ref()?.to_str()?;
        Some(format!("unix://{path}"))
    }

    pub(crate) fn is_shared(&self) -> bool {
        self.state.shared
    }

    pub(crate) fn is_subscribed(&self, id: &str) -> bool {
        self.is_ready() && locked(&self.state.subscribed).contains_key(id)
    }

    /// Connection-local fence for unload/rejoin while a canonical read is in flight.
    pub(crate) fn subscription_revision(&self, thread: &str) -> Option<u64> {
        let subscribed = locked(&self.state.subscribed);
        subscribed.get(thread).copied().filter(|_| self.is_ready())
    }

    /// The event consumer can lag behind the reader; a queued completion must
    /// not finish a newer turn that the reader has already observed.
    pub(crate) fn is_current_turn_completion(&self, thread: &str, turn: &str) -> bool {
        if !self.is_ready() || (self.is_shared() && !self.is_subscribed(thread)) {
            return false;
        }
        let active = locked(&self.state.turns);
        match active.get(thread) {
            Some(current) => current == turn,
            None => locked(&self.state.lifecycle_completed_turns)
                .get(thread)
                .is_some_and(|completed| completed == turn),
        }
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

    pub(crate) fn recovered_questions(
        &self,
        ownership: u64,
        thread: &str,
        turn: &str,
        items: Vec<Value>,
    ) -> Result<(), AgentError> {
        self.state.emit(CodexEvent::RecoveredQuestions {
            ownership,
            thread: thread.into(),
            turn: turn.into(),
            items,
        })
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.state.cancel.clone()
    }
    #[track_caller]
    pub fn stop(&self) {
        self.state.stop();
        #[cfg(unix)]
        if let Ok(mut history) = self.history.try_lock() {
            if let Some(client) = history.client.take() {
                client.stop();
            }
        }
    }

    pub async fn initialize(&self) -> Result<(), AgentError> {
        let response = self.call_inner("initialize", json!({
            "clientInfo": {"name":"muqun_gateway", "title":"Muqun Gateway", "version":env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi":false, "requestAttestation":false, "explicitGatewayOauth":true}
        })).await?;
        let agent = response["userAgent"]
            .as_str()
            .ok_or_else(|| AgentError::Protocol("invalid Codex initialize response".into()))?;
        if !super::accepts_server_version(agent) {
            self.stop();
            return Err(AgentError::Unsupported(
                "Codex app-server version; expected 0.163.0 or newer".into(),
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
        self.call_sized_budgeted(
            method,
            params,
            tokio::time::Instant::now() + self.timeout,
            MAX_HISTORY_WIRE_WORK_BYTES,
        )
        .await
    }

    pub(super) async fn call_sized_budgeted(
        &self,
        method: &str,
        params: Value,
        deadline: tokio::time::Instant,
        wire_allowance: usize,
    ) -> Result<(Value, usize), AgentError> {
        if self.state.cancel.is_cancelled() {
            return Err(AgentError::NotAvailable(
                "Codex connection is no longer ready; refresh its status before continuing".into(),
            ));
        }
        if !self.is_ready() {
            return Err(AgentError::NotAvailable(
                "Codex is not initialized and authenticated".into(),
            ));
        }
        #[cfg(unix)]
        if self.is_shared() && !self.state.history_only && image_history_request(method, &params) {
            return self
                .history_call(method, params, deadline, wire_allowance)
                .await;
        }
        self.call_inner_sized(method, params).await
    }

    #[cfg(unix)]
    async fn history_call(
        &self,
        method: &str,
        mut params: Value,
        deadline: tokio::time::Instant,
        wire_allowance: usize,
    ) -> Result<(Value, usize), AgentError> {
        if wire_allowance == 0 || tokio::time::Instant::now() >= deadline {
            return Err(history_error());
        }
        let identity = self.shared_identity.clone().ok_or_else(|| AgentError::RequestFailed("Codex history endpoint identity has not been verified; primary connection retained".into()))?;
        // Fail fast: the owned actor, not the request future, retains this slot
        // through a late reply or its bounded deadline after caller cancellation.
        let permit = self
            .history_admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                AgentError::RequestFailed(
                    "Codex history read already pending; primary connection retained".into(),
                )
            })?;
        let slot = self.history.clone();
        let parent = self.cancel_token();
        let method = diagnostic_method(method);
        let (reply, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let mut slot = slot.lock().await;
            if slot
                .retry_at
                .is_some_and(|at| tokio::time::Instant::now() < at)
            {
                drop(slot);
                drop(permit);
                let _ = reply.send(Err(AgentError::RequestFailed(
                    "Codex history reconnect cooling down; primary connection retained".into(),
                )));
                return;
            }
            // Only known item-count pages can be split. Cursor, turn and sort
            // direction remain unchanged until a complete accepted reply.
            let mut limit = (method == "thread/items/list")
                .then(|| params["limit"].as_u64())
                .flatten()
                .filter(|limit| (1..=100).contains(limit))
                .map(|limit| limit.min(8));
            if let (Some(requested), Some(learned)) = (limit, slot.item_limit) {
                limit = Some(requested.min(learned));
            }
            let mut wire_work = 0usize;
            let allowance = wire_allowance.min(MAX_HISTORY_WIRE_WORK_BYTES);
            let operation = async {
                for attempt in 0..2 {
                    if reply.is_closed()
                        || wire_work >= allowance
                        || tokio::time::Instant::now() >= deadline
                    {
                        return Err(history_error());
                    }
                    identity.verify()?;
                    let read_limit = MAX_HISTORY_FRAME_BYTES.min(allowance - wire_work);
                    if slot
                        .client
                        .as_ref()
                        .is_some_and(|client| client.state.read_limit != read_limit)
                    {
                        if let Some(client) = slot.client.take() {
                            client.stop();
                        }
                    }
                    if slot
                        .client
                        .as_ref()
                        .is_some_and(|client| !client.is_ready())
                    {
                        return Err(history_error());
                    }
                    if slot.client.is_none() {
                        slot.client = Some(
                            super::daemon::connect_history(&identity, parent.clone(), read_limit)
                                .await?,
                        );
                    }
                    if reply.is_closed() || tokio::time::Instant::now() >= deadline {
                        return Err(history_error());
                    }
                    if let Some(limit) = limit {
                        params["limit"] = json!(limit);
                    }
                    // No recursive dispatch. The history role still validates
                    // every method and disallows callback replies/mutations.
                    let client = slot.client.as_ref().expect("connected history client");
                    let result = client.call_inner_sized(method, params.clone()).await;
                    let capacity = client.state.history_capacity.get().copied();
                    match result {
                        Ok((value, bytes)) => {
                            wire_work = wire_work.saturating_add(bytes);
                            if wire_work > allowance {
                                return Err(history_error());
                            }
                            return Ok((value, wire_work));
                        }
                        Err(_) => {
                            let capacity = capacity.ok_or_else(history_error)?;
                            wire_work = wire_work.saturating_add(capacity.wire_work);
                            let smaller = match limit {
                                Some(n) if n > 1 => 1,
                                _ => return Err(history_error()),
                            };
                            if attempt == 1
                                || wire_work >= allowance
                                || reply.is_closed()
                                || tokio::time::Instant::now() >= deadline
                            {
                                return Err(history_error());
                            }
                            // Capacity consumes decoder state. Never continue
                            // that socket, nor wait out the terminal-failure
                            // cooldown inside this bounded shrinking operation.
                            if let Some(client) = slot.client.take() {
                                client.stop();
                            }
                            limit = Some(smaller);
                            slot.item_limit = Some(smaller);
                            tracing::debug!(method, attempt, item_limit = smaller, wire_work,
                                "Codex history capacity recovery; smaller read-only page on fresh reader");
                        }
                    }
                }
                Err(history_error())
            };
            let result = tokio::select! {
                biased;
                _ = parent.cancelled() => Err(history_error()),
                result = tokio::time::timeout_at(deadline, operation) => result.unwrap_or_else(|_| Err(history_error())),
            };
            if result.is_err() {
                if let Some(client) = slot.client.take() {
                    client.stop();
                }
                slot.retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(3));
            } else {
                slot.retry_at = None;
            }
            let result = result.map_err(|_| {
                // These are bounded local diagnostics, not an upstream payload.
                tracing::warn!(
                    method,
                    "Codex isolated history read failed; primary retained; no mutation replay"
                );
                history_error()
            });
            // Publish only after releasing admission; a promptly scheduled next
            // page must not see a phantom previous request after its reply.
            drop(slot);
            drop(permit);
            let _ = reply.send(result);
        });
        receiver.await.unwrap_or_else(|_| Err(history_error()))
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
        if self.state.history_only {
            let allowed = image_history_request(method, &params)
                || (method == "account/read" && params["refreshToken"] == false)
                || (method == "initialize" && !self.is_ready());
            if !allowed {
                return Err(AgentError::Unsupported(
                    "Codex history connection is read-only".into(),
                ));
            }
        }
        if self.state.cancel.is_cancelled() {
            return Err(disconnected());
        }
        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        let fence_on_cancel = !read_only_request(method, &params);
        let (reply, receiver) = oneshot::channel();
        {
            let mut pending = locked(&self.state.pending);
            if pending.len()
                >= if self.state.history_only {
                    1
                } else {
                    MAX_PENDING
                }
            {
                return Err(AgentError::NotAvailable(
                    "Codex request capacity exhausted".into(),
                ));
            }
            pending.insert(
                id,
                Pending {
                    reply,
                    diagnostic_method: diagnostic_method(method),
                    selection_config: method == "config/read",
                    mcp_config: method == "config/read" && params["includeLayers"] == true,
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
            fence_on_cancel,
            enqueued: AtomicBool::new(false),
            diagnostic_method: diagnostic_method(method),
        };
        let deadline = tokio::time::Instant::now() + self.timeout;
        tokio::time::timeout_at(
            deadline,
            self.write_guarded(
                json!({"id":id,"method":method,"params":params}),
                fence_on_cancel,
                Some(&_guard.enqueued),
                deadline,
            ),
        )
        .await
        .map_err(|_| disconnected())??;
        match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(disconnected()),
            Err(_) => {
                if !fence_on_cancel {
                    tracing::warn!(connection = %self.state.generation, rpc_id = id,
                        method = diagnostic_method(method), "Codex read deadline exceeded; connection retained; not retried");
                    return Err(AgentError::RequestFailed(
                        "Codex read deadline exceeded; not retried".into(),
                    ));
                }
                // A mutation may have been accepted. Close this connection;
                // do not submit it again or allow further work to race it.
                self.state.stop_request(
                    "mutation_response_deadline",
                    Some(id),
                    diagnostic_method(method),
                );
                Err(AgentError::RequestFailed(
                    "Codex request deadline exceeded; outcome unknown; not retried".into(),
                ))
            }
        }
    }

    async fn write(&self, value: Value) -> Result<(), AgentError> {
        self.write_guarded(
            value,
            true,
            None,
            tokio::time::Instant::now() + self.timeout,
        )
        .await
    }

    async fn write_guarded(
        &self,
        value: Value,
        fence_on_cancel: bool,
        enqueued: Option<&AtomicBool>,
        deadline: tokio::time::Instant,
    ) -> Result<(), AgentError> {
        if self.state.history_only {
            let method = value["method"].as_str().unwrap_or("");
            let allowed = (value.get("id").is_some()
                && (image_history_request(method, &value["params"])
                    || (method == "initialize" && !self.is_ready())
                    || (method == "account/read" && value["params"]["refreshToken"] == false)))
                || (method == "initialized" && !self.is_ready() && value.get("id").is_none());
            if !allowed {
                return Err(AgentError::Unsupported(
                    "Codex history connection is read-only".into(),
                ));
            }
        }
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
            fence_on_cancel,
            deadline,
        };
        tokio::select! {
            biased;
            _ = self.state.cancel.cancelled() => return Err(disconnected()),
            result = tokio::time::timeout_at(deadline, self.outgoing.send(WritePacket {bytes,done})) => {
                if !matches!(result,Ok(Ok(()))) {return Err(disconnected());}
                if let Some(enqueued) = enqueued {
                    enqueued.store(true, Ordering::Release);
                }
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
    pub(crate) fn is_compacting(&self, thread: &str) -> bool {
        let turns = locked(&self.state.turns);
        locked(&self.state.compactions)
            .get(thread)
            .is_some_and(|(turn, _)| turns.get(thread) == Some(turn))
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

    pub(crate) fn resolve_form(&self, thread: &str, id: &str) -> Result<(), AgentError> {
        self.state.emit(CodexEvent::Resolved {
            thread: thread.into(),
            id: id.into(),
            form: true,
        })
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

fn route_frame(state: &State, mut value: Value, frame_bytes: usize) -> Result<(), AgentError> {
    if state.cancel.is_cancelled() {
        return Err(disconnected());
    }
    if frame_bytes > MAX_FRAME_BYTES {
        let item_page = state.shared
            && frame_bytes <= state.read_limit
            && value.get("method").is_none()
            && value.get("error").is_none()
            && value["id"].as_u64().is_some_and(|id| {
                locked(&state.pending).get(&id).is_some_and(|pending| {
                    match pending.diagnostic_method {
                        "thread/items/list" => value["result"]["data"]
                            .as_array()
                            .is_some_and(|rows| rows.len() <= 100),
                        "thread/turns/list" if state.history_only => value["result"]["data"]
                            .as_array()
                            .is_some_and(|rows| rows.len() <= 100),
                        "thread/read" if state.history_only => {
                            value["result"]["thread"]["turns"].is_array()
                        }
                        _ => false,
                    }
                })
            });
        if !item_page {
            return Err(AgentError::Protocol(
                "Codex response exceeds allowed frame limit".into(),
            ));
        }
    }
    if let Some(method) = value["method"].as_str() {
        // A read-only history socket never subscribes or handles approvals.
        // Its disconnect/events have no route to the primary manager.
        if state.history_only {
            return Ok(());
        }
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        // Shared servers broadcast metadata for other clients. Only an accepted
        // explicit start/fork/rejoin authorizes this connection's live projection.
        if state.shared {
            let thread = params["threadId"]
                .as_str()
                .or_else(|| params["thread"]["id"].as_str());
            if thread.is_none_or(|id| !locked(&state.subscribed).contains_key(id)) {
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
                let current = turn.is_some() && turns.get(thread).map(String::as_str) == turn;
                if let Some(turn) = turn.filter(|_| current) {
                    let mut completed = locked(&state.lifecycle_completed_turns);
                    if completed.len() >= 4096 && !completed.contains_key(thread) {
                        return Err(AgentError::Protocol(
                            "Codex lifecycle completed-turn capacity exhausted".into(),
                        ));
                    }
                    completed.insert(thread.into(), turn.into());
                    turns.remove(thread);
                    locked(&state.compactions).remove(thread);
                }
                drop(turns);
                // Keep the separate response fence even for an unobserved turn:
                // completion can arrive before the turn/start acknowledgement.
                if let Some(turn) = turn {
                    let mut completed = locked(&state.completed_turns);
                    if completed.len() >= 4096 && !completed.contains_key(thread) {
                        return Err(AgentError::Protocol(
                            "Codex completed-turn capacity exhausted".into(),
                        ));
                    }
                    completed.insert(thread.into(), turn.into());
                }
                if let Some(turn) = turn {
                    locked(&state.requests).retain(|_, r| {
                        !(r.params["threadId"] == thread && r.params["turnId"] == turn)
                    });
                }
            } else if matches!(method, "item/started" | "item/completed")
                && params["item"]["type"] == "contextCompaction"
            {
                if let (Some(turn), Some(item)) =
                    (params["turnId"].as_str(), params["item"]["id"].as_str())
                {
                    let turns = locked(&state.turns);
                    if turns.get(thread).map(String::as_str) == Some(turn) {
                        let mut compactions = locked(&state.compactions);
                        if method == "item/started" {
                            if compactions.len() >= 4096 && !compactions.contains_key(thread) {
                                return Err(AgentError::Protocol(
                                    "Codex compaction capacity exhausted".into(),
                                ));
                            }
                            compactions.insert(thread.into(), (turn.into(), item.into()));
                        } else if compactions
                            .get(thread)
                            .is_some_and(|(held_turn, held_item)| {
                                held_turn == turn && held_item == item
                            })
                        {
                            compactions.remove(thread);
                        }
                    }
                }
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
                locked(&state.compactions).remove(thread);
                locked(&state.completed_turns).remove(thread);
                locked(&state.lifecycle_completed_turns).remove(thread);
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
                    .get_mut("result")
                    .map(Value::take)
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
                    if subscribed.len() >= 4096 && !subscribed.contains_key(thread) {
                        return Err(AgentError::Protocol(
                            "Codex subscription capacity exhausted".into(),
                        ));
                    }
                    let first = !subscribed.contains_key(thread);
                    if first {
                        let revision = state.ownership_revision.fetch_add(1, Ordering::AcqRel);
                        subscribed.insert(thread.into(), revision);
                    }
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
                if pending.mcp_config {
                    value = super::mcp::registration_read(&value);
                } else if pending.selection_config {
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

    #[tokio::test]
    async fn unobserved_completion_fences_resurrection_without_authorizing_lifecycle_cleanup() {
        let mut fixture = crate::agents::adapters::codex::tests::Fixture::new(|request| {
            assert_eq!(request["method"], "thread/start");
            json!({"thread":{"id":"thread"}})
        })
        .await;
        fixture.ready().await;
        fixture
            .client
            .call("thread/start", json!({}))
            .await
            .unwrap();
        let client = &fixture.client;
        // Exercise the metadata-rejoin fence with an accepted subscription.
        locked(&client.state.subscribed).insert("thread".into(), 1);
        let complete = |turn: &str| json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":turn,"status":"completed"}}});
        route_frame(&client.state, json!({"id":77,"method":"item/fileChange/requestApproval","params":{"threadId":"thread"}}), 0).unwrap();
        route_frame(&client.state, json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"status":"completed"}}}), 0).unwrap();
        assert_eq!(
            client.server_requests("thread").len(),
            1,
            "missing turn identity cannot resolve an unknown callback"
        );
        route_frame(&client.state, complete("unobserved"), 0).unwrap();
        assert!(!client.is_current_turn_completion("thread", "unobserved"));
        client.observe_active_turn("thread", &json!({"id":"unobserved","status":"inProgress"}));
        assert!(
            client.active_turn("thread").is_none(),
            "late metadata must not resurrect the unobserved completed turn"
        );
        client.observe_active_turn("thread", &json!({"id":"next-turn","status":"inProgress"}));
        route_frame(&client.state, complete("next-turn"), 0).unwrap();
        assert!(client.is_current_turn_completion("thread", "next-turn"));
        route_frame(&client.state, complete("unobserved"), 0).unwrap();
        assert!(client.is_current_turn_completion("thread", "next-turn"));
        assert!(!client.is_current_turn_completion("thread", "unobserved"));
        route_frame(
            &client.state,
            json!({"method":"thread/closed","params":{"threadId":"thread"}}),
            0,
        )
        .unwrap();
        assert!(!client.is_current_turn_completion("thread", "next-turn"));
    }

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
        CodexEvents,
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
    async fn abandoned_reads_remain_bounded_until_late_replies_arrive() {
        let (io, server) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(io);
        let (client, _events) = CodexClient::from_io(reader, writer);
        client.state.ready.store(true, Ordering::Release);
        let (read, mut write) = tokio::io::split(server);
        let mut lines = BufReader::new(read).lines();
        let mut ids = Vec::new();
        for _ in 0..MAX_PENDING {
            let call = tokio::spawn({
                let client = client.clone();
                async move {
                    client
                        .call("thread/read", json!({"threadId":"fixture"}))
                        .await
                }
            });
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            ids.push(request["id"].clone());
            call.abort();
            assert!(call.await.unwrap_err().is_cancelled());
            assert!(client.is_ready());
        }
        assert_eq!(locked(&client.state.pending).len(), MAX_PENDING);
        assert!(matches!(
            client.call("thread/start", json!({})).await,
            Err(AgentError::NotAvailable(_))
        ));
        for id in ids {
            write
                .write_all(format!("{}\n", json!({"id":id,"result":{}})).as_bytes())
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while !locked(&client.state.pending).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(client.is_ready());
    }

    #[tokio::test]
    async fn cancelled_read_during_flush_and_expired_read_do_not_close_transport() {
        let (reader, mut peer) = tokio::io::duplex(4096);
        let probe = Arc::new(WriteProbe::default());
        let (client, _events) = CodexClient::with_timeout(
            reader,
            ProbedWriter {
                probe: probe.clone(),
                fail_after: None,
                fail_flush: false,
            },
            Duration::from_secs(2),
        );
        client.state.ready.store(true, Ordering::Release);
        let read = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .call("config/read", json!({"includeLayers":false}))
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !probe.flush_started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        read.abort();
        assert!(read.await.unwrap_err().is_cancelled());
        assert!(client.is_ready());
        probe.release.store(true, Ordering::Release);
        locked(&probe.waker).take().unwrap().wake();
        peer.write_all(b"{\"id\":1,\"result\":{\"config\":{\"model_provider\":\"openai\"}}}\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !locked(&client.state.pending).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(client.is_ready());

        let (io, _server) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(io);
        let (client, _events) =
            CodexClient::with_timeout(reader, writer, Duration::from_millis(25));
        client.state.ready.store(true, Ordering::Release);
        assert!(matches!(
            client.call("model/list", json!({})).await,
            Err(AgentError::RequestFailed(_))
        ));
        assert!(client.is_ready());
        assert_eq!(locked(&client.state.pending).len(), 1);
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
