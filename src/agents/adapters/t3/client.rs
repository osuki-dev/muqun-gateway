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

pub const RPC_DISPATCH: &str = "orchestration.dispatchCommand";
pub const RPC_GET_TURN_DIFF: &str = "orchestration.getTurnDiff";
pub const RPC_GET_FULL_THREAD_DIFF: &str = "orchestration.getFullThreadDiff";
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
const MAX_ID_LEN: usize = 128;
const MAX_PROMPT_CHARS: usize = 120_000;

/// A validated opaque id: T3 ids are UUIDs the client mints, but the server
/// only requires a trimmed non-empty string, so this accepts the characters a
/// URL path segment and a JSON string can carry safely and nothing else.
pub fn validate_id(id: &str, what: &str) -> Result<String, AgentError> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_ID_LEN {
        return Err(AgentError::RequestFailed(format!("invalid {what}")));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return Err(AgentError::RequestFailed(format!("invalid {what}")));
    }
    Ok(trimmed.to_string())
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

/// Builders for the orchestration commands this adapter dispatches. Each
/// returns the JSON body of `orchestration.dispatchCommand`.
pub mod commands {
    use super::*;

    pub fn project_create(project_id: &str, title: &str, workspace_root: &str) -> Value {
        json!({
            "type": "project.create",
            "commandId": new_id(),
            "projectId": project_id,
            "title": title,
            "workspaceRoot": workspace_root,
            "createdAt": now_iso(),
        })
    }

    pub fn thread_create(
        thread_id: &str,
        project_id: &str,
        title: &str,
        model: &ModelSelection,
        runtime_mode: &str,
        interaction_mode: &str,
    ) -> Value {
        json!({
            "type": "thread.create",
            "commandId": new_id(),
            "threadId": thread_id,
            "projectId": project_id,
            "title": title,
            "modelSelection": model.to_value(),
            "runtimeMode": runtime_mode,
            "interactionMode": interaction_mode,
            "branch": null,
            "worktreePath": null,
            "createdAt": now_iso(),
        })
    }

    pub fn turn_start(
        thread_id: &str,
        message_id: &str,
        text: &str,
        model: Option<&ModelSelection>,
        runtime_mode: &str,
        interaction_mode: &str,
    ) -> Value {
        let mut v = json!({
            "type": "thread.turn.start",
            "commandId": new_id(),
            "threadId": thread_id,
            "message": {
                "messageId": message_id,
                "role": "user",
                "text": text,
                "attachments": [],
            },
            "runtimeMode": runtime_mode,
            "interactionMode": interaction_mode,
            "createdAt": now_iso(),
        });
        if let Some(m) = model {
            v["modelSelection"] = m.to_value();
        }
        v
    }

    pub fn turn_interrupt(thread_id: &str) -> Value {
        json!({
            "type": "thread.turn.interrupt",
            "commandId": new_id(),
            "threadId": thread_id,
            "createdAt": now_iso(),
        })
    }

    /// `decision` is one of `accept`, `acceptForSession`, `acceptAlways`,
    /// `decline`, `cancel`.
    pub fn approval_respond(thread_id: &str, request_id: &str, decision: &str) -> Value {
        json!({
            "type": "thread.approval.respond",
            "commandId": new_id(),
            "threadId": thread_id,
            "requestId": request_id,
            "decision": decision,
            "createdAt": now_iso(),
        })
    }

    pub fn user_input_respond(thread_id: &str, request_id: &str, answers: Value) -> Value {
        json!({
            "type": "thread.user-input.respond",
            "commandId": new_id(),
            "threadId": thread_id,
            "requestId": request_id,
            "answers": answers,
            "createdAt": now_iso(),
        })
    }

    pub fn checkpoint_revert(thread_id: &str, turn_count: u64) -> Value {
        json!({
            "type": "thread.checkpoint.revert",
            "commandId": new_id(),
            "threadId": thread_id,
            "turnCount": turn_count,
            "createdAt": now_iso(),
        })
    }

    pub fn thread_delete(thread_id: &str) -> Value {
        json!({
            "type": "thread.delete",
            "commandId": new_id(),
            "threadId": thread_id,
        })
    }

    pub fn thread_rename(thread_id: &str, title: &str) -> Value {
        json!({
            "type": "thread.meta.update",
            "commandId": new_id(),
            "threadId": thread_id,
            "title": title,
        })
    }

    pub fn thread_set_model(thread_id: &str, model: &ModelSelection) -> Value {
        json!({
            "type": "thread.meta.update",
            "commandId": new_id(),
            "threadId": thread_id,
            "modelSelection": model.to_value(),
        })
    }

    pub fn runtime_mode_set(thread_id: &str, runtime_mode: &str) -> Value {
        json!({
            "type": "thread.runtime-mode.set",
            "commandId": new_id(),
            "threadId": thread_id,
            "runtimeMode": runtime_mode,
            "createdAt": now_iso(),
        })
    }

    pub fn interaction_mode_set(thread_id: &str, interaction_mode: &str) -> Value {
        json!({
            "type": "thread.interaction-mode.set",
            "commandId": new_id(),
            "threadId": thread_id,
            "interactionMode": interaction_mode,
            "createdAt": now_iso(),
        })
    }
}

/// The bearer held for the server, and the pairing credential it came from
/// if the exchange has not happened yet.
struct Auth {
    bearer: Option<String>,
    pairing: Option<String>,
}

pub struct T3Client {
    pub endpoint: T3Endpoint,
    http: Client,
    auth: Arc<RwLock<Auth>>,
    rpc: RpcConnection,
    cancel: CancellationToken,
}

impl T3Client {
    pub fn new(endpoint: T3Endpoint) -> Self {
        Self::with_config(endpoint, RpcConfig::default())
    }

    pub fn with_config(endpoint: T3Endpoint, rpc_config: RpcConfig) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
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
        }
    }

    pub fn http(&self) -> &Client {
        &self.http
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
    pub async fn dispatch(&self, command: Value) -> Result<u64, AgentError> {
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

    pub async fn get_turn_diff(
        &self,
        thread_id: &str,
        from: u64,
        to: u64,
    ) -> Result<Value, AgentError> {
        self.rpc
            .request(
                RPC_GET_TURN_DIFF,
                json!({ "threadId": thread_id, "fromTurnCount": from, "toTurnCount": to }),
            )
            .await
            .map_err(map_rpc_error)
    }

    pub async fn get_full_thread_diff(
        &self,
        thread_id: &str,
        to: u64,
    ) -> Result<Value, AgentError> {
        self.rpc
            .request(
                RPC_GET_FULL_THREAD_DIFF,
                json!({ "threadId": thread_id, "toTurnCount": to }),
            )
            .await
            .map_err(map_rpc_error)
    }

    pub async fn search_threads(&self, query: &str, limit: usize) -> Result<Value, AgentError> {
        let q: String = query.trim().chars().take(200).collect();
        if q.chars().count() < 2 {
            return Ok(json!({ "matches": [] }));
        }
        self.rpc
            .request(
                RPC_SEARCH_THREADS,
                json!({ "query": q, "limit": limit.clamp(1, 50) }),
            )
            .await
            .map_err(map_rpc_error)
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
        let mut payload = json!({
            "threadId": thread_id,
            "reasoningMessages": true,
            "requestCompletionMarker": true,
        });
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
        turn_limit: Option<usize>,
    ) -> Result<Value, AgentError> {
        let id = validate_id(thread_id, "session id")?;
        let mut path = format!("{HTTP_THREAD_SNAPSHOT}/{id}?reasoningMessages=true");
        if let Some(limit) = turn_limit {
            path.push_str(&format!("&turnLimit={}", limit.max(1)));
        }
        match self.get_json(path).await {
            Err(AgentError::RequestFailed(msg)) if msg.starts_with("HTTP 404") => {
                Err(AgentError::SessionNotFound(thread_id.to_string()))
            }
            other => other,
        }
    }

    async fn get_json(&self, path: String) -> Result<Value, AgentError> {
        let bearer = self.bearer().await?;
        let resp = self
            .http
            .get(format!("{}{path}", self.endpoint.url))
            .bearer_auth(&bearer)
            .send()
            .await
            .map_err(|e| AgentError::Network(short(&e.to_string())))?;
        let status = resp.status();
        let body = read_bounded(resp, MAX_HTTP_BODY).await?;
        if status.as_u16() == 401 {
            return Err(AgentError::NotAvailable(
                "the stored T3 credential was rejected; pair again".into(),
            ));
        }
        if !status.is_success() {
            tracing::debug!(%status, path = %path.split('?').next().unwrap_or(""), "t3 http read failed");
            return Err(AgentError::RequestFailed(format!("HTTP {status}")));
        }
        serde_json::from_slice(&body).map_err(|e| AgentError::Protocol(short(&e.to_string())))
    }
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
        Err(e) => {
            // The credential may still be valid (network blip); keep it for
            // the next attempt. A rejection consumed it either way.
            if matches!(e, AgentError::Network(_)) {
                guard.pairing = Some(pairing);
            }
            Err(e)
        }
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
        assert!(validate_id(&"x".repeat(200), "thread").is_err());
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
    fn command_builders_match_the_captured_wire_shapes() {
        let ms = ModelSelection {
            instance_id: "claudeAgent".into(),
            model: "claude-haiku-4-5".into(),
            options: vec![],
        };
        let c = commands::thread_create("t1", "p1", "title", &ms, "full-access", "default");
        for key in [
            "type",
            "commandId",
            "threadId",
            "projectId",
            "title",
            "modelSelection",
            "runtimeMode",
            "interactionMode",
            "branch",
            "worktreePath",
            "createdAt",
        ] {
            assert!(c.get(key).is_some(), "thread.create lacks {key}");
        }
        assert_eq!(c["type"], "thread.create");
        assert!(c["branch"].is_null());
        let t = commands::turn_start("t1", "m1", "hi", None, "full-access", "default");
        assert_eq!(t["message"]["role"], "user");
        assert_eq!(t["message"]["attachments"], json!([]));
        assert!(t.get("modelSelection").is_none());
        let a = commands::approval_respond("t1", "r1", "accept");
        assert_eq!(a["decision"], "accept");
        let r = commands::checkpoint_revert("t1", 0);
        assert_eq!(r["turnCount"], 0);
        assert!(commands::thread_delete("t1").get("createdAt").is_none());
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
