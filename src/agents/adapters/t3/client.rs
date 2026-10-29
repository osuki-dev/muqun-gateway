//! Typed wrappers over the T3 Code orchestration API.
//!
//! Commands and queries go over the RPC socket (`rpc.rs`); read-model
//! snapshots come from the HTTP endpoints the T3 clients themselves use for
//! initial loads (`GET /api/orchestration/shell`, `/threads/:id`). The
//! bearer token is obtained lazily: a pairing credential is exchanged once
//! and the resulting bearer kept in memory. The runtime normally does that
//! exchange itself before building a client (`agents/runtime/t3.rs`), so it
//! can persist the bearer; this lazy path serves a client built straight
//! from a pairing credential, such as the live test.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::endpoint::{read_bounded, short, BearerGrant, T3Credential, T3Endpoint, MAX_HTTP_BODY};
use super::rpc::{RpcConfig, RpcConnection, RpcError, Subscription};
use crate::agents::ports::agent::AgentError;

mod history;
use history::HistoryCursor;

pub const RPC_DISPATCH: &str = "orchestration.dispatchCommand";
pub const RPC_SEARCH_THREADS: &str = "orchestration.searchThreads";
pub const RPC_SUBSCRIBE_SHELL: &str = "orchestration.subscribeShell";
pub const RPC_SUBSCRIBE_THREAD: &str = "orchestration.subscribeThread";
pub const RPC_SERVER_PROBE: &str = "server.probe";
pub const RPC_SERVER_GET_CONFIG: &str = "server.getConfig";

pub const HTTP_SHELL_SNAPSHOT: &str = "/api/orchestration/shell";
pub const HTTP_THREAD_SNAPSHOT: &str = "/api/orchestration/threads";

/// The runtime modes T3 accepts, in the order the catalog offers them.
pub const RUNTIME_MODES: [&str; 4] = [
    "full-access",
    "approval-required",
    "auto-accept-edits",
    "auto",
];
pub const DEFAULT_RUNTIME_MODE: &str = "full-access";
pub const INTERACTION_MODES: [&str; 2] = ["default", "plan"];
pub const DEFAULT_INTERACTION_MODE: &str = "default";

const CLIENT_LABEL: &str = "muqun-gateway";
const MAX_ID_LEN: usize = 4096;
const MAX_PROMPT_CHARS: usize = 120_000;

/// T3 also emits composite provider-native IDs containing percent-encoded
/// fragments. Preserve these opaque IDs; encode them separately for HTTP paths.
pub fn validate_id(id: &str, what: &str) -> Result<String, AgentError> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_ID_LEN || matches!(trimmed, "." | "..") {
        return Err(AgentError::RequestFailed(format!("invalid {what}")));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '%'))
    {
        return Err(AgentError::RequestFailed(format!("invalid {what}")));
    }
    if what == "session id" {
        return Ok(super::session_identity::public_id(
            &super::session_identity::native_id(trimmed)?,
        ));
    }
    Ok(trimmed.to_string())
}

fn thread_snapshot_path(id: &str) -> Result<String, AgentError> {
    Ok(format!(
        "{HTTP_THREAD_SNAPSHOT}/{}/bounded",
        super::session_identity::path_segment(id)?
    ))
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The current time as the ISO 8601 UTC string T3 stores (`2026-09-29T08:30:26.601Z`).
pub fn now_iso() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    iso_from_ms(ms)
}

pub fn iso_from_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Parse the ISO 8601 UTC timestamps T3 emits into epoch milliseconds.
/// Accepts `YYYY-MM-DDTHH:MM:SS[.fff]Z`; anything else is `None`.
pub fn ms_from_iso(s: &str) -> Option<u64> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let m: u32 = dp.next()?.parse().ok()?;
    let d: u32 = dp.next()?.parse().ok()?;
    let (hms, frac) = match time.split_once('.') {
        Some((a, b)) => (a, b),
        None => (time, ""),
    };
    let mut tp = hms.split(':');
    let h: u64 = tp.next()?.parse().ok()?;
    let mi: u64 = tp.next()?.parse().ok()?;
    let sec: u64 = tp.next()?.parse().ok()?;
    let mut millis: u64 = 0;
    if !frac.is_empty() {
        let digits: String = frac.chars().take(3).collect();
        let padded = format!("{digits:0<3}");
        millis = padded.parse().ok()?;
    }
    let days = days_from_civil(y, m, d);
    if days < 0 {
        return None;
    }
    Some(((days as u64 * 86_400 + h * 3600 + mi * 60 + sec) * 1000) + millis)
}

// Howard Hinnant's algorithms; proleptic Gregorian, days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `ModelSelection` on the wire: `{instanceId, model, options?}`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSelection {
    pub instance_id: String,
    pub model: String,
    /// `[{id, value}]`, e.g. a reasoning effort.
    pub options: Vec<(String, Value)>,
}

impl ModelSelection {
    pub fn to_value(&self) -> Value {
        let mut v = json!({ "instanceId": self.instance_id, "model": self.model });
        if !self.options.is_empty() {
            v["options"] = Value::Array(
                self.options
                    .iter()
                    .map(|(id, value)| json!({ "id": id, "value": value }))
                    .collect(),
            );
        }
        v
    }

    pub fn from_value(v: &Value) -> Option<Self> {
        let instance_id = v
            .get("instanceId")
            .or_else(|| v.get("provider"))
            .and_then(Value::as_str)?;
        let model = v.get("model").and_then(Value::as_str)?;
        let mut options = Vec::new();
        match v.get("options") {
            Some(Value::Array(items)) => {
                for item in items {
                    if let (Some(id), Some(value)) =
                        (item.get("id").and_then(Value::as_str), item.get("value"))
                    {
                        options.push((id.to_string(), value.clone()));
                    }
                }
            }
            Some(Value::Object(map)) => {
                for (id, value) in map {
                    options.push((id.clone(), value.clone()));
                }
            }
            _ => {}
        }
        Some(Self {
            instance_id: instance_id.to_string(),
            model: model.to_string(),
            options,
        })
    }
}

/// The bearer held for the server, and the pairing credential it came from
/// if the exchange has not happened yet.
struct Auth {
    bearer: Option<String>,
    pairing: Option<String>,
}

#[derive(Clone)]
struct TimelineState {
    generation: String,
    order: super::v2::projection::TimelineOrder,
    revision: u64,
    committed: bool,
    mirror_sequence: u64,
    used: std::time::Instant,
    readers: Arc<()>,
    history: Option<HistoryCursor>,
}

impl TimelineState {
    fn retained_bytes(&self) -> usize {
        self.order.retained_bytes().saturating_add(
            self.history
                .as_ref()
                .map_or(0, HistoryCursor::retained_bytes),
        )
    }
}

pub(crate) struct PreparedT3Timeline {
    pub detail: Value,
    generation: String,
    revision: u64,
    order: super::v2::projection::TimelineOrder,
    _reader: Arc<()>,
    history: Option<HistoryCursor>,
}

/// Cache pressure evicts inactive incarnations, never an in-flight read. The
/// mirror keeps its old window until refresh accepts a replacement incarnation.
fn reserve_timeline_space(
    orders: &mut std::collections::HashMap<String, TimelineState>,
    id: &str,
    bytes: usize,
) -> Result<(), AgentError> {
    while orders.len() + usize::from(!orders.contains_key(id)) > 128
        || orders
            .iter()
            .filter(|(thread, _)| thread.as_str() != id)
            .map(|(_, state)| state.retained_bytes())
            .fold(bytes, usize::saturating_add)
            > 2 * MAX_HTTP_BODY
    {
        let oldest = orders
            .iter()
            .filter(|(thread, state)| {
                thread.as_str() != id && Arc::strong_count(&state.readers) == 1
            })
            .min_by_key(|(_, state)| state.used)
            .map(|(thread, _)| thread.clone());
        let Some(oldest) = oldest else {
            return Err(AgentError::NotAvailable(
                "T3 history readers are busy; retry the read".into(),
            ));
        };
        orders.remove(&oldest);
    }
    Ok(())
}

pub struct T3Client {
    pub endpoint: T3Endpoint,
    http: Client,
    auth: Arc<RwLock<Auth>>,
    rpc: RpcConnection,
    cancel: CancellationToken,
    timeline_orders: std::sync::Mutex<std::collections::HashMap<String, TimelineState>>,
}

impl T3Client {
    pub fn new(endpoint: T3Endpoint) -> Self {
        Self::with_config(endpoint, RpcConfig::default())
    }

    pub fn with_config(endpoint: T3Endpoint, rpc_config: RpcConfig) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        let auth = Arc::new(RwLock::new(match &endpoint.credential {
            T3Credential::Bearer(t) => Auth {
                bearer: Some(t.clone()),
                pairing: None,
            },
            T3Credential::Pairing(t) => Auth {
                bearer: None,
                pairing: Some(t.clone()),
            },
            T3Credential::None => Auth {
                bearer: None,
                pairing: None,
            },
        }));
        let cancel = CancellationToken::new();
        let url_source = {
            let endpoint = endpoint.clone();
            let http = http.clone();
            let auth = auth.clone();
            Arc::new(move || {
                let endpoint = endpoint.clone();
                let http = http.clone();
                let auth = auth.clone();
                Box::pin(async move {
                    // A reconnect must not carry credentials or V1 cursors into a
                    // replaced environment. The runtime adopts a fresh driver.
                    verify_environment(&endpoint, &http)
                        .await
                        .map_err(|e| e.to_string())?;
                    let bearer = bearer_for(&endpoint, &http, &auth)
                        .await
                        .map_err(|e| e.to_string())?;
                    let ticket = endpoint
                        .websocket_ticket(&http, &bearer)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(endpoint.ws_url_with_ticket(&ticket))
                })
                    as std::pin::Pin<
                        Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
                    >
            })
        };
        let rpc = RpcConnection::spawn(url_source, rpc_config, cancel.clone());
        Self {
            endpoint,
            http,
            auth,
            rpc,
            cancel,
            timeline_orders: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn http(&self) -> &Client {
        &self.http
    }

    /// Capture before starting I/O. A forgotten/recreated thread gets a new
    /// incarnation, so an old in-flight read cannot populate the new cache.
    pub async fn timeline_generation(&self, id: &str) -> Result<String, AgentError> {
        if self.cancel.is_cancelled() {
            return Err(AgentError::NotAvailable(
                "T3 client closed; reload required".into(),
            ));
        }
        let mut orders = self
            .timeline_orders
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let bytes = orders.get(id).map_or(0, TimelineState::retained_bytes);
        reserve_timeline_space(&mut orders, id, bytes)?;
        let state = orders.entry(id.into()).or_insert_with(|| TimelineState {
            generation: new_id(),
            order: Default::default(),
            revision: 0,
            committed: false,
            mirror_sequence: 0,
            used: std::time::Instant::now(),
            readers: Arc::new(()),
            history: None,
        });
        state.used = std::time::Instant::now();
        Ok(state.generation.clone())
    }

    #[cfg(test)]
    pub async fn order_snapshot(&self, id: &str, detail: Value) -> Result<Value, AgentError> {
        let generation = match detail["gatewayTimelineGeneration"].as_str() {
            Some(generation) => generation.into(),
            None => self.timeline_generation(id).await?,
        };
        self.order_snapshot_in_generation(id, detail, &generation, false)
            .await
    }

    pub async fn order_live_snapshot(&self, id: &str, detail: Value) -> Result<Value, AgentError> {
        let generation = match detail["gatewayTimelineGeneration"].as_str() {
            Some(generation) => generation.into(),
            None => self.timeline_generation(id).await?,
        };
        self.order_snapshot_in_generation(id, detail, &generation, true)
            .await
    }

    pub async fn order_snapshot_in_generation(
        &self,
        id: &str,
        detail: Value,
        generation: &str,
        live: bool,
    ) -> Result<Value, AgentError> {
        let prepared = self.prepare_order_snapshot(id, detail, generation, live)?;
        let detail = prepared.detail.clone();
        self.commit_timeline(id, prepared, false)?;
        Ok(detail)
    }

    fn prepare_order_snapshot(
        &self,
        id: &str,
        mut detail: Value,
        generation: &str,
        live: bool,
    ) -> Result<PreparedT3Timeline, AgentError> {
        let orders = self
            .timeline_orders
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if self.cancel.is_cancelled() {
            return Err(AgentError::NotAvailable(
                "T3 client closed; reload required".into(),
            ));
        }
        let state = orders
            .get(id)
            .filter(|state| state.generation == generation)
            .ok_or_else(|| {
                AgentError::Protocol("V2 timeline incarnation changed; reload required".into())
            })?;
        if live && state.order.is_stale(&detail) {
            return Err(AgentError::Protocol(
                "stale V2 live snapshot requires resync".into(),
            ));
        }
        let mut order = state.order.clone();
        order.install(&mut detail)?;
        detail["gatewayTimelineGeneration"] = Value::String(generation.into());
        detail["gatewayOrderCommitted"] = Value::Bool(state.committed);
        Ok(PreparedT3Timeline {
            detail,
            generation: generation.into(),
            revision: state.revision,
            order,
            _reader: state.readers.clone(),
            history: None,
        })
    }

    pub(crate) fn commit_timeline(
        &self,
        id: &str,
        prepared: PreparedT3Timeline,
        accepted_by_mirror: bool,
    ) -> Result<crate::agents::ports::agent::HistoryCommit, AgentError> {
        if self.cancel.is_cancelled() {
            return Err(AgentError::NotAvailable(
                "T3 client closed; reload required".into(),
            ));
        }
        let mut orders = self
            .timeline_orders
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let state = orders
            .get(id)
            .filter(|state| {
                state.generation == prepared.generation && state.revision == prepared.revision
            })
            .ok_or(AgentError::HistoryCursorLost)?;
        let committed = state.committed || accepted_by_mirror;
        let mirror_sequence = if accepted_by_mirror {
            state
                .mirror_sequence
                .max(prepared.detail["snapshotSequence"].as_u64().unwrap_or(0))
        } else {
            state.mirror_sequence
        };
        let history = if accepted_by_mirror {
            prepared.history.or_else(|| state.history.clone())
        } else {
            state.history.clone()
        };
        let bytes = prepared
            .order
            .retained_bytes()
            .saturating_add(history.as_ref().map_or(0, HistoryCursor::retained_bytes));
        reserve_timeline_space(&mut orders, id, bytes)?;
        orders.insert(
            id.into(),
            TimelineState {
                generation: prepared.generation,
                order: prepared.order,
                revision: prepared.revision + 1,
                committed,
                mirror_sequence,
                used: std::time::Instant::now(),
                readers: prepared._reader,
                history,
            },
        );
        Ok(Default::default())
    }

    pub(crate) fn accept_live_generation(&self, id: &str, generation: &str, sequence: u64) {
        if let Some(state) = self
            .timeline_orders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(id)
            .filter(|state| state.generation == generation)
        {
            state.committed = true;
            state.mirror_sequence = state.mirror_sequence.max(sequence);
        }
    }

    pub(crate) fn mirrored_sequence(&self, id: &str) -> u64 {
        self.timeline_orders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .map_or(0, |state| state.mirror_sequence)
    }

    pub(crate) fn require_timeline_refresh(&self, id: &str, sequence: u64) {
        if let Some(state) = self
            .timeline_orders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(id)
        {
            state.order.invalidate_before(sequence);
            state.revision += 1;
            state.committed = false;
            state.history = None;
        }
    }

    pub async fn forget_timeline(&self, id: &str) {
        self.timeline_orders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }

    pub async fn create_project(
        &self,
        id: &str,
        title: &str,
        root: &str,
    ) -> Result<(), AgentError> {
        let bearer = self.bearer().await?;
        let response = self
            .http
            .post(format!("{}/api/projects/mutate", self.endpoint.url))
            .bearer_auth(bearer)
            .json(&super::v2::commands::project_create(id, title, root))
            .send()
            .await
            .map_err(|e| AgentError::Network(short(&e.to_string())))?;
        let status = response.status();
        read_bounded(response, MAX_HTTP_BODY).await?;
        if !status.is_success() {
            return Err(AgentError::RequestFailed(format!(
                "project mutation HTTP {status}"
            )));
        }
        Ok(())
    }

    pub fn rpc(&self) -> &RpcConnection {
        &self.rpc
    }

    /// Stop the socket task and every subscription.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    /// The bearer currently held, exchanging the pairing credential on first
    /// use.
    pub async fn bearer(&self) -> Result<String, AgentError> {
        if self.endpoint.environment_id.is_some() {
            verify_environment(&self.endpoint, &self.http).await?;
        }
        bearer_for(&self.endpoint, &self.http, &self.auth).await
    }

    /// The bearer without triggering an exchange; `None` until one happened.
    pub async fn current_bearer(&self) -> Option<String> {
        self.auth.read().await.bearer.clone()
    }

    // ---- RPC -------------------------------------------------------------

    pub async fn probe(&self) -> Result<(), AgentError> {
        self.rpc
            .request(RPC_SERVER_PROBE, json!({}))
            .await
            .map(|_| ())
            .map_err(map_rpc_error)
    }

    /// `server.getConfig`: environment, providers with their models, settings.
    pub async fn get_config(&self) -> Result<Value, AgentError> {
        self.rpc
            .request(RPC_SERVER_GET_CONFIG, json!({}))
            .await
            .map_err(map_rpc_error)
    }

    /// Dispatch one orchestration command; returns the event sequence the
    /// command committed at. Acceptance means the intent was recorded, not
    /// that the provider finished acting on it.
    pub async fn dispatch(&self, mut command: Value) -> Result<u64, AgentError> {
        if let Some(id) = command.get("threadId").and_then(Value::as_str) {
            command["threadId"] = json!(super::session_identity::native_id(id)?);
        }
        let kind = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let result = self.rpc.request(RPC_DISPATCH, command).await.map_err(|e| {
            tracing::warn!(command = %kind, error = %e, "t3 dispatch failed");
            map_rpc_error(e)
        })?;
        Ok(result.get("sequence").and_then(Value::as_u64).unwrap_or(0))
    }

    pub async fn search_threads(&self, query: &str, limit: usize) -> Result<Value, AgentError> {
        let q: String = query.trim().chars().take(200).collect();
        if q.chars().count() < 2 {
            return Ok(json!({ "matches": [] }));
        }
        let mut result = self
            .rpc
            .request(
                RPC_SEARCH_THREADS,
                json!({ "query": q, "limit": limit.clamp(1, 50) }),
            )
            .await
            .map_err(map_rpc_error)?;
        super::session_identity::expose(&mut result);
        Ok(result)
    }

    /// Subscribe to the shell: first a `snapshot` (or a replay after
    /// `after_sequence`), a `synchronized` marker, then live
    /// `project-*`/`thread-*` items.
    pub async fn subscribe_shell(
        &self,
        after_sequence: Option<u64>,
    ) -> Result<Subscription, AgentError> {
        let mut payload = json!({ "requestCompletionMarker": true });
        if let Some(seq) = after_sequence {
            payload["afterSequence"] = json!(seq);
        }
        self.rpc
            .subscribe(RPC_SUBSCRIBE_SHELL, payload)
            .await
            .map_err(map_rpc_error)
    }

    /// Subscribe to one thread: a `snapshot` (or replay), `synchronized`,
    /// then `event` items.
    pub async fn subscribe_thread(
        &self,
        thread_id: &str,
        after_sequence: Option<u64>,
    ) -> Result<Subscription, AgentError> {
        let native = super::session_identity::native_id(thread_id)?;
        let mut payload = json!({
            "threadId": native,
            "reasoningMessages": true,
            "requestCompletionMarker": true,
        });
        {
            payload = json!({"threadId":native,"requestCompletionMarker":true,"acceptBoundedSnapshot":true});
        }
        if let Some(seq) = after_sequence {
            payload["afterSequence"] = json!(seq);
        }
        self.rpc
            .subscribe(RPC_SUBSCRIBE_THREAD, payload)
            .await
            .map_err(map_rpc_error)
    }

    // ---- HTTP read model -------------------------------------------------

    /// `GET /api/orchestration/shell`: every project and thread summary.
    pub async fn shell_snapshot(&self) -> Result<Value, AgentError> {
        self.get_json(HTTP_SHELL_SNAPSHOT.to_string()).await
    }

    /// `GET /api/orchestration/threads/:id`: the full thread (messages,
    /// activities, checkpoints, session), windowed to the last `turn_limit`
    /// user turns when given.
    pub async fn thread_snapshot(
        &self,
        thread_id: &str,
        _turn_limit: Option<usize>,
    ) -> Result<Value, AgentError> {
        let id = validate_id(thread_id, "session id")?;
        {
            let detail = self.get_json(thread_snapshot_path(&id)?).await?;
            super::v2::projection::validate_snapshot(&detail, &id)?;
            Ok(detail)
        }
    }

    async fn get_json(&self, path: String) -> Result<Value, AgentError> {
        self.get_json_counted(path, MAX_HTTP_BODY)
            .await?
            .map(|(value, _)| value)
            .ok_or_else(|| AgentError::Protocol("T3 response exceeds byte budget".into()))
    }

    async fn get_json_counted(
        &self,
        path: String,
        remaining: usize,
    ) -> Result<Option<(Value, usize)>, AgentError> {
        if remaining == 0 {
            return Ok(None);
        }
        let bearer = self.bearer().await?;
        let mut request = self
            .http
            .get(format!("{}{path}", self.endpoint.url))
            .bearer_auth(&bearer);
        if path.starts_with("/api/orchestration/") {
            request = request.header("x-t3-orchestration-protocol", "2");
        }
        let mut resp = request
            .send()
            .await
            .map_err(|e| AgentError::Network(short(&e.to_string())))?;
        let status = resp.status();
        let cap = MAX_HTTP_BODY.min(remaining);
        if resp
            .content_length()
            .is_some_and(|length| length > cap as u64)
        {
            return if cap < MAX_HTTP_BODY {
                Ok(None)
            } else {
                Err(AgentError::Protocol(
                    "T3 response exceeds byte budget".into(),
                ))
            };
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| AgentError::Network(short(&e.to_string())))?
        {
            if chunk.len() > cap - body.len() {
                // A cumulative walk may stop before this page and retain its
                // cursor. An individual oversized response remains refused.
                return if cap < MAX_HTTP_BODY {
                    Ok(None)
                } else {
                    Err(AgentError::Protocol(
                        "T3 response exceeds byte budget".into(),
                    ))
                };
            }
            body.extend_from_slice(&chunk);
        }
        if status.as_u16() == 401 {
            return Err(AgentError::NotAvailable(
                "the stored T3 credential was rejected; pair again".into(),
            ));
        }
        if !status.is_success() {
            tracing::debug!(%status, path = %path.split('?').next().unwrap_or(""), "t3 http read failed");
            return Err(AgentError::RequestFailed(format!("HTTP {status}")));
        }
        let mut value = serde_json::from_slice(&body)
            .map_err(|e| AgentError::Protocol(short(&e.to_string())))?;
        super::session_identity::expose(&mut value);
        Ok(Some((value, body.len())))
    }

    /// Historical rows only. Page sequences never replace live control state.
    pub async fn timeline_snapshot(
        &self,
        thread_id: &str,
        limit: usize,
    ) -> Result<Value, AgentError> {
        let prepared = self.prepare_timeline_snapshot(thread_id, limit).await?;
        let detail = prepared.detail.clone();
        self.commit_timeline(thread_id, prepared, false)?;
        Ok(detail)
    }

    pub(crate) async fn prepare_timeline_snapshot(
        &self,
        thread_id: &str,
        limit: usize,
    ) -> Result<PreparedT3Timeline, AgentError> {
        let id = validate_id(thread_id, "session id")?;
        let generation = self.timeline_generation(&id).await?;
        let _reader = self
            .timeline_orders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
            .filter(|state| state.generation == generation)
            .ok_or(AgentError::HistoryCursorLost)?
            .readers
            .clone();
        let (detail, bytes) = self
            .get_json_counted(thread_snapshot_path(&id)?, MAX_HTTP_BODY)
            .await?
            .ok_or_else(|| {
                AgentError::Protocol("T3 bounded snapshot exceeds byte budget".into())
            })?;
        super::v2::projection::validate_snapshot(&detail, &id)?;
        let detail = self.walk_history(&id, detail, limit, bytes).await?;
        let (cursor, detail) = HistoryCursor::select(detail, limit)?;
        let mut prepared = self.prepare_order_snapshot(thread_id, detail, &generation, false)?;
        prepared.history = Some(cursor);
        Ok(prepared)
    }
}

async fn verify_environment(endpoint: &T3Endpoint, http: &Client) -> Result<(), AgentError> {
    let descriptor = endpoint.describe(http).await?;
    let protocol = descriptor.protocol()?;
    if protocol != endpoint.protocol
        || endpoint
            .environment_id
            .as_ref()
            .is_some_and(|id| id != &descriptor.environment_id)
    {
        return Err(AgentError::NotAvailable(
            "T3 environment or protocol changed; reattach required".into(),
        ));
    }
    Ok(())
}

async fn bearer_for(
    endpoint: &T3Endpoint,
    http: &Client,
    auth: &RwLock<Auth>,
) -> Result<String, AgentError> {
    if let Some(b) = auth.read().await.bearer.clone() {
        return Ok(b);
    }
    let mut guard = auth.write().await;
    if let Some(b) = guard.bearer.clone() {
        return Ok(b);
    }
    let Some(pairing) = guard.pairing.take() else {
        return Err(AgentError::NotAvailable(
            "no T3 credential configured; pair with the server first".into(),
        ));
    };
    verify_environment(endpoint, http).await?;
    match endpoint
        .exchange_pairing(http, &pairing, CLIENT_LABEL)
        .await
    {
        Ok(BearerGrant {
            token,
            expires_in_secs,
            scope,
        }) => {
            tracing::info!(expires_in_secs, %scope, "t3 pairing credential exchanged for a bearer");
            guard.bearer = Some(token.clone());
            Ok(token)
        }
        // The server may have consumed the one-time token before a lost reply.
        Err(e) => Err(e),
    }
}

/// Reduce an RPC error to the port's vocabulary with a bounded message.
pub fn map_rpc_error(e: RpcError) -> AgentError {
    match e {
        RpcError::Failed(f) => {
            let msg = short(&f.message);
            let lower = msg.to_ascii_lowercase();
            if f.tag == "EnvironmentAuthorizationError" {
                AgentError::NotAvailable("the T3 credential lacks the required scope".into())
            } else if lower.contains("not found") || lower.contains("unknown thread") {
                AgentError::SessionNotFound(msg)
            } else {
                AgentError::RequestFailed(msg)
            }
        }
        RpcError::Interrupted => AgentError::RequestFailed("request interrupted".into()),
        RpcError::Defect(d) => AgentError::Protocol(short(&d)),
        RpcError::Disconnected | RpcError::Closed => {
            AgentError::Network("T3 Code server not connected".into())
        }
        RpcError::Timeout => AgentError::Network("T3 Code request timed out".into()),
        RpcError::Protocol(p) => AgentError::Protocol(short(&p)),
    }
}

/// Bound a prompt to what T3 accepts (`PROVIDER_SEND_TURN_MAX_INPUT_CHARS`).
pub fn bounded_prompt(text: &str) -> Result<&str, AgentError> {
    if text.chars().count() > MAX_PROMPT_CHARS {
        return Err(AgentError::RequestFailed("prompt too long".into()));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trips() {
        assert_eq!(
            ms_from_iso("2026-09-29T08:30:26.601Z"),
            Some(1_790_670_626_601)
        );
        assert_eq!(iso_from_ms(1_790_670_626_601), "2026-09-29T08:30:26.601Z");
        assert_eq!(ms_from_iso("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(ms_from_iso("2026-09-29T08:30:26Z"), Some(1_790_670_626_000));
        assert_eq!(
            ms_from_iso("2026-09-29T08:30:26.6Z"),
            Some(1_790_670_626_600)
        );
        assert_eq!(ms_from_iso("nonsense"), None);
        assert_eq!(ms_from_iso("2026-09-29 08:30:26"), None);
        let now = now_iso();
        assert!(now.ends_with('Z') && now.len() == 24, "{now}");
    }

    #[test]
    fn ids_are_validated_before_use() {
        assert!(validate_id("1cf9c53f-ee94-407f-ac68-bd0b6e700836", "thread").is_ok());
        assert_eq!(validate_id(" abc ", "thread").unwrap(), "abc");
        assert!(validate_id("", "thread").is_err());
        assert!(validate_id("a/b", "thread").is_err());
        assert!(validate_id("a b", "thread").is_err());
        assert!(validate_id(&"x".repeat(MAX_ID_LEN + 1), "thread").is_err());
    }

    #[test]
    fn provider_native_child_ids_keep_encoded_fragments() {
        let id = format!(
            "thread:provider:cursor:native-thread:{}%3Atask%3Achild%0Aitem",
            "a".repeat(150)
        );
        assert_eq!(validate_id(&id, "thread").unwrap(), id);
        assert_eq!(
            thread_snapshot_path(&id).unwrap(),
            format!(
                "{HTTP_THREAD_SNAPSHOT}/{}/bounded",
                id.replace('%', "%25").replace(':', "%3A")
            )
        );
        for invalid in [".", "..", "a/b", "a?b", "a#b", "a\\b", "a\nb"] {
            assert!(validate_id(invalid, "thread").is_err());
        }
    }

    #[test]
    fn model_selection_round_trips_both_option_shapes() {
        let m = ModelSelection {
            instance_id: "claudeAgent".into(),
            model: "claude-haiku-4-5".into(),
            options: vec![("effort".into(), json!("high"))],
        };
        let v = m.to_value();
        assert_eq!(v["instanceId"], "claudeAgent");
        assert_eq!(v["options"][0]["id"], "effort");
        assert_eq!(ModelSelection::from_value(&v).unwrap(), m);
        // Legacy object options and the legacy `provider` key still decode.
        let legacy =
            json!({ "provider": "codex", "model": "gpt-5.5", "options": { "effort": "max" } });
        let parsed = ModelSelection::from_value(&legacy).unwrap();
        assert_eq!(parsed.instance_id, "codex");
        assert_eq!(parsed.options, vec![("effort".to_string(), json!("max"))]);
        let bare = ModelSelection {
            instance_id: "x".into(),
            model: "y".into(),
            options: vec![],
        };
        assert!(bare.to_value().get("options").is_none());
    }

    #[test]
    fn attached_paths_ride_in_the_turn_text_not_its_attachments() {
        let text = crate::agents::ports::agent::append_attachment_paths(
            "look",
            &["/up/a.png".to_string(), "/up/b.pdf".to_string()],
        );
        let t = super::super::v2::commands::message_dispatch("t1", "m1", &text, None, false);
        assert_eq!(t["attachments"], json!([]));
        assert_eq!(
            t["text"],
            "look\n\nAttached files (on this host):\n- /up/a.png\n- /up/b.pdf"
        );
    }

    #[test]
    fn rpc_errors_reduce_to_the_port_vocabulary() {
        let nf = RpcError::Failed(super::super::rpc::RpcFailure {
            tag: "OrchestrationDispatchCommandError".into(),
            message: "Thread not found".into(),
            raw: Value::Null,
        });
        assert!(matches!(map_rpc_error(nf), AgentError::SessionNotFound(_)));
        let scope = RpcError::Failed(super::super::rpc::RpcFailure {
            tag: "EnvironmentAuthorizationError".into(),
            message: "needs orchestration:operate".into(),
            raw: Value::Null,
        });
        assert!(matches!(map_rpc_error(scope), AgentError::NotAvailable(_)));
        assert!(matches!(
            map_rpc_error(RpcError::Disconnected),
            AgentError::Network(_)
        ));
    }
}
