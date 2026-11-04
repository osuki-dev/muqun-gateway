//! [`RemoteHub`]: the gateway service on this computer, over loopback.
//!
//! The same HTTP + `/api/ws` contract the phone App uses, with the local
//! client token the gateway publishes in `local-client.json`. The gateway owns
//! the one `AgentRuntime`; this hub holds no agent process and no session
//! state beyond the history cursors of the sessions it has read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use muqun_gateway::agents::directories::{Completion, DirectoryEntry};
use muqun_gateway::agents::ports::{AgentError, AgentSessionSnapshot, FileDiffItem};
use muqun_gateway::agents::{
    AgentCatalog, AgentDomainEvent, AgentProject, AgentSessionId, AgentSessionInfo, ModelRef,
    PermissionDecision, SessionQuery, TimelineItem,
};
use muqun_gateway::platform::discovery::{AgentAvailability, AgentDiscoveryInfo};
use muqun_gateway::platform::StoredUpload;
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::local::enrich_local_media;
use super::{
    CatalogView, HubError, HubFuture, HubMode, PeerMention, SessionHub, TaskDefaults, TaskHubEvent,
    TaskRow, TimelineCatchUp,
};

const EVENT_CAPACITY: usize = 4096;
const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(15);

/// What `local-client.json` holds.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LocalGateway {
    pub url: String,
    pub token: String,
}

/// The local gateway's credential, when its file names one that answers
/// `GET /api/meta`. `MUQUN_LOCAL_GATEWAY` overrides the file's path; the
/// default is the gateway service's own state directory.
pub async fn discover_local_gateway() -> Option<LocalGateway> {
    let path = std::env::var_os("MUQUN_LOCAL_GATEWAY")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            dirs::data_dir()
                .or_else(dirs::config_dir)
                .map(|d| d.join("muqun-gateway").join("local-client.json"))
        })?;
    let found = read_credential(&path)?;
    match probe(&found).await {
        Ok(()) => Some(found),
        Err(why) => {
            tracing::info!(file = %path.display(), "local gateway not usable: {why}");
            None
        }
    }
}

fn read_credential(path: &Path) -> Option<LocalGateway> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

async fn probe(gateway: &LocalGateway) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    let answer = client
        .get(format!("{}/api/meta", gateway.url.trim_end_matches('/')))
        .bearer_auth(&gateway.token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if answer.status().is_success() {
        Ok(())
    } else {
        Err(format!("/api/meta answered {}", answer.status()))
    }
}

pub struct RemoteHub {
    http: reqwest::Client,
    base: reqwest::Url,
    token: String,
    events: broadcast::Sender<AgentDomainEvent>,
    /// `task.*` frames, which are not the session plane's events.
    task_events: broadcast::Sender<TaskHubEvent>,
    /// Per session, the cursor of the next older history page.
    cursors: Mutex<HashMap<String, String>>,
}

impl RemoteHub {
    /// Connect to `gateway` and start the event socket (which reconnects).
    pub fn start(gateway: LocalGateway) -> anyhow::Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(120))
            .build()?;
        let hub = Arc::new(Self {
            http,
            base: reqwest::Url::parse(gateway.url.trim_end_matches('/'))?,
            token: gateway.token,
            events: broadcast::channel(EVENT_CAPACITY).0,
            task_events: broadcast::channel(EVENT_CAPACITY).0,
            cursors: Mutex::new(HashMap::new()),
        });
        tokio::spawn(event_socket(hub.clone()));
        Ok(hub)
    }

    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> reqwest::Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.clear().extend(segments);
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    /// The whole JSON answer of one call.
    async fn call(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, HubError> {
        let mut request = self
            .http
            .request(method, self.url(segments, query))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let answer = request.send().await.map_err(|err| {
            HubError::Agent(AgentError::Network(format!(
                "the local gateway did not answer: {err}"
            )))
        })?;
        let status = answer.status();
        let text = answer.text().await.unwrap_or_default();
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(json);
        }
        let asid = match segments {
            ["api", "agent-sessions", asid, ..] => asid.to_string(),
            _ => String::new(),
        };
        Err(map_error(status.as_u16(), &json, &asid, &text))
    }

    /// `data` of the envelope, as `T`.
    async fn data<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<T, HubError> {
        let json = self.call(method, segments, query, body).await?;
        decode(json.get("data").cloned().unwrap_or(Value::Null))
    }

    async fn session_call(
        &self,
        method: Method,
        asid: &AgentSessionId,
        tail: &[&str],
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, HubError> {
        let mut segments = vec!["api", "agent-sessions", asid.0.as_str()];
        segments.extend_from_slice(tail);
        self.call(method, &segments, query, body).await
    }

    async fn read_snapshot(&self, asid: &AgentSessionId) -> Result<AgentSessionSnapshot, HubError> {
        self.read_window(asid, &[]).await
    }

    /// The newest history window; `retained=true` in `query` asks the gateway
    /// to answer from what it already holds, without preparing the session.
    async fn read_window(
        &self,
        asid: &AgentSessionId,
        query: &[(&str, &str)],
    ) -> Result<AgentSessionSnapshot, HubError> {
        let json = self
            .session_call(Method::GET, asid, &["timeline", "window"], query, None)
            .await?;
        let data = json.get("data").cloned().unwrap_or(Value::Null);
        {
            let mut cursors = self.cursors.lock().unwrap();
            match data.get("history_cursor").and_then(Value::as_str) {
                Some(cursor) => cursors.insert(asid.0.clone(), cursor.to_string()),
                None => cursors.remove(&asid.0),
            };
        }
        let mut snapshot: AgentSessionSnapshot = decode(data)?;
        enrich_local_media(&mut snapshot);
        Ok(snapshot)
    }

    fn unit(result: Result<Value, HubError>) -> Result<(), HubError> {
        result.map(|_| ())
    }
}

/// `data.enabled` of an auto-approve answer; `null` (no session control) is off.
fn enabled_of(json: &Value) -> bool {
    json.pointer("/data/enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, HubError> {
    serde_json::from_value(value).map_err(|err| {
        HubError::Agent(AgentError::Protocol(format!(
            "unexpected answer from the local gateway: {err}"
        )))
    })
}

/// `{"tasks": [TaskView]}` as rows.
fn task_rows(json: &Value) -> Result<Vec<TaskRow>, HubError> {
    json.get("tasks")
        .and_then(Value::as_array)
        .map(|tasks| tasks.iter().map(task_row).collect())
        .unwrap_or_else(|| Ok(Vec::new()))
}

/// One `TaskView` (a list entry, or the answer of `POST /api/tasks`).
fn task_row(json: &Value) -> Result<TaskRow, HubError> {
    TaskRow::from_json(json).map_err(|err| {
        HubError::Agent(AgentError::Protocol(format!(
            "unexpected task from the local gateway: {err}"
        )))
    })
}

/// The `POST /api/tasks` body: only what was given, so the gateway's own
/// defaults (primary agent, the device's last folder) apply to the rest.
fn create_task_body(
    goal: &str,
    agent_id: Option<&str>,
    cwd: Option<&str>,
    peer_server_id: Option<&str>,
) -> Value {
    let mut body = json!({ "goal": goal });
    if let Some(agent) = agent_id.filter(|a| !a.is_empty()) {
        body["agent_id"] = json!(agent);
    }
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        body["cwd"] = json!(cwd);
    }
    if let Some(peer) = peer_server_id.filter(|p| !p.is_empty()) {
        body["computer_ids"] = json!([peer]);
    }
    body
}

/// The gateway's `{"error": {"code", "message"}}` as the hub's own errors.
fn map_error(status: u16, body: &Value, asid: &str, raw: &str) -> HubError {
    let code = body.pointer("/error/code").and_then(Value::as_str);
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "HTTP {status}: {}",
                raw.chars().take(200).collect::<String>()
            )
        });
    match (status, code) {
        (_, Some("session_not_found")) => HubError::SessionNotFound(asid.to_string()),
        (503, Some("agent_unavailable")) => HubError::AgentUnavailable {
            agent: message.split('\'').nth(1).unwrap_or_default().to_string(),
            reason: Some(message),
        },
        (_, Some("workspace_missing")) => HubError::Agent(AgentError::WorkspaceMissing(
            body.pointer("/error/directory")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )),
        (_, Some("history_cursor_lost")) => HubError::Agent(AgentError::HistoryCursorLost),
        // A retained-only read of a session the gateway holds nothing for.
        (404, Some("not_retained")) => {
            HubError::Agent(AgentError::SessionNotFound(asid.to_string()))
        }
        (_, Some("inbox_item_not_found")) => {
            HubError::Agent(AgentError::InboxItemNotFound(message))
        }
        (_, Some("session_open_required")) => HubError::Agent(AgentError::OpenRequired(message)),
        (501, _) => HubError::Agent(AgentError::Unsupported(message)),
        (400, _) => HubError::Agent(AgentError::InvalidRequest(message)),
        _ => HubError::Agent(AgentError::RequestFailed(message)),
    }
}

impl SessionHub for RemoteHub {
    fn mode(&self) -> HubMode {
        HubMode::LocalGateway
    }

    fn agents(&self) -> HubFuture<'_, Vec<AgentDiscoveryInfo>> {
        Box::pin(async move {
            let json = self
                .call(Method::GET, &["api", "discovery"], &[], None)
                .await?;
            let agents = json
                .pointer("/planes/agents/agents")
                .cloned()
                .unwrap_or(Value::Array(Vec::new()));
            decode(agents)
        })
    }

    fn attached(&self) -> HubFuture<'_, Vec<String>> {
        Box::pin(async move {
            let agents = self.agents().await?;
            let ready = |a: &&AgentDiscoveryInfo| {
                a.enabled
                    && matches!(
                        a.status,
                        AgentAvailability::Connected | AgentAvailability::Reachable
                    )
            };
            Ok(super::AGENT_PREFERENCE
                .iter()
                .filter(|id| agents.iter().filter(ready).any(|a| a.id == **id))
                .map(|id| id.to_string())
                .collect())
        })
    }

    fn sessions<'a>(
        &'a self,
        agent: &'a str,
        query: SessionQuery,
    ) -> HubFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            let limit = query.limit.map(|l| l.to_string());
            let mut pairs = vec![("agent_id", agent)];
            for (key, value) in [
                ("directory", query.directory.as_deref()),
                ("parent_id", query.parent_id.as_deref()),
                ("limit", limit.as_deref()),
                ("order", query.order.as_deref()),
                ("search", query.search.as_deref()),
                ("cursor", query.cursor.as_deref()),
            ] {
                if let Some(value) = value {
                    pairs.push((key, value));
                }
            }
            self.data(Method::GET, &["api", "agent-sessions"], &pairs, None)
                .await
        })
    }

    fn projects<'a>(&'a self, agent: &'a str) -> HubFuture<'a, Vec<AgentProject>> {
        Box::pin(async move {
            self.data(
                Method::GET,
                &["api", "agent-projects"],
                &[("agent_id", agent)],
                None,
            )
            .await
        })
    }

    fn directories<'a>(&'a self, prefix: &'a str) -> HubFuture<'a, Completion> {
        Box::pin(async move {
            let json = self
                .call(
                    Method::GET,
                    &["api", "agent-directories"],
                    &[("prefix", prefix)],
                    None,
                )
                .await?;
            let directories = json
                .get("data")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| {
                            Some(DirectoryEntry {
                                name: row.get("name")?.as_str()?.to_string(),
                                path: row.get("path")?.as_str()?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(Completion {
                directories,
                truncated: json
                    .get("truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
    }

    fn create<'a>(&'a self, agent: &'a str, cwd: &'a Path) -> HubFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            self.data(
                Method::POST,
                &["api", "agent-sessions"],
                &[],
                Some(json!({"agent_id": agent, "directory": cwd.to_string_lossy()})),
            )
            .await
        })
    }

    fn open<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, AgentSessionSnapshot> {
        Box::pin(async move {
            if let Err(err) = self
                .session_call(Method::POST, asid, &["prepare"], &[], None)
                .await
            {
                tracing::debug!(asid = %asid.0, %err, "session preparation failed; reading history anyway");
            }
            self.read_snapshot(asid).await
        })
    }

    /// What the gateway already retains, read without preparing the session
    /// (`retained=true`). `None` when it holds nothing: the open is cold.
    fn peek<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Option<AgentSessionSnapshot>> {
        Box::pin(async move {
            match self.read_window(asid, &[("retained", "true")]).await {
                Ok(snapshot) => Ok(Some(snapshot)),
                Err(HubError::Agent(AgentError::SessionNotFound(_))) => Ok(None),
                Err(err) => Err(err),
            }
        })
    }

    fn snapshot<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, AgentSessionSnapshot> {
        Box::pin(async move { self.read_snapshot(asid).await })
    }

    fn earlier<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let cursor = self.cursors.lock().unwrap().get(&asid.0).cloned();
            let (json, paged) = match cursor {
                Some(cursor) => (
                    self.session_call(
                        Method::POST,
                        asid,
                        &["timeline", "page"],
                        &[],
                        Some(json!({"cursor": cursor})),
                    )
                    .await?,
                    true,
                ),
                None => (
                    self.session_call(Method::POST, asid, &["timeline", "earlier"], &[], None)
                        .await?,
                    false,
                ),
            };
            let data = json.get("data").cloned().unwrap_or(Value::Null);
            if paged {
                let mut cursors = self.cursors.lock().unwrap();
                match data.get("history_cursor").and_then(Value::as_str) {
                    Some(next) => cursors.insert(asid.0.clone(), next.to_string()),
                    None => cursors.remove(&asid.0),
                };
            }
            let mut items: Vec<TimelineItem> =
                decode(data.get("items").cloned().unwrap_or(Value::Null))?;
            let directory = self.snapshot_directory(asid).await;
            muqun_gateway::agents::message_images::enrich_timeline(
                &asid.0,
                directory.as_deref(),
                &mut items,
            );
            Ok(items)
        })
    }

    fn prompt<'a>(&'a self, asid: &'a AgentSessionId, text: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move { self.send(asid, text, &[], None).await })
    }

    fn interrupt<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(Method::POST, asid, &["interrupt"], &[], None)
                    .await,
            )
        })
    }

    fn reply_permission<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        request_id: &'a str,
        decision: PermissionDecision,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(
                    Method::POST,
                    asid,
                    &["permissions", request_id, "reply"],
                    &[],
                    Some(json!({"decision": decision.as_str()})),
                )
                .await,
            )
        })
    }

    fn reply_form<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        form_id: &'a str,
        answers: Value,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(
                    Method::POST,
                    asid,
                    &["forms", form_id, "reply"],
                    &[],
                    Some(json!({"answers": answers})),
                )
                .await,
            )
        })
    }

    fn send<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let mut body = json!({"text": text});
            if !attachments.is_empty() {
                body["attachments"] = json!(attachments);
            }
            if let Some(delivery) = delivery {
                body["delivery"] = json!(delivery);
            }
            Self::unit(
                self.session_call(Method::POST, asid, &["prompt"], &[], Some(body))
                    .await,
            )
        })
    }

    fn find_files<'a>(
        &'a self,
        agent: &'a str,
        directory: &'a str,
        query: &'a str,
        limit: usize,
    ) -> HubFuture<'a, Vec<Value>> {
        Box::pin(async move {
            if !Path::new(directory).is_absolute() || !Path::new(directory).is_dir() {
                return Ok(Vec::new());
            }
            let limit = limit.to_string();
            let found: Result<Vec<Value>, HubError> = self
                .data(
                    Method::GET,
                    &["api", "agent-files"],
                    &[
                        ("agent_id", agent),
                        ("directory", directory),
                        ("query", query),
                        ("limit", &limit),
                    ],
                    None,
                )
                .await;
            match found {
                // The route names the kind `kind`; the trait's hits say `type`.
                Ok(hits) if !hits.is_empty() => Ok(hits
                    .into_iter()
                    .map(|mut hit| {
                        if let Some(kind) = hit.get("kind").cloned() {
                            hit["type"] = kind;
                        }
                        hit
                    })
                    .collect()),
                Ok(_) => Ok(crate::platform::filesearch::find_files(
                    directory,
                    query,
                    limit.parse().unwrap_or(20),
                )
                .await),
                Err(err) => {
                    tracing::debug!(agent, %err, "gateway find_files; local fallback");
                    Ok(crate::platform::filesearch::find_files(
                        directory,
                        query,
                        limit.parse().unwrap_or(20),
                    )
                    .await)
                }
            }
        })
    }

    fn catalog<'a>(
        &'a self,
        agent: &'a str,
        asid: Option<&'a AgentSessionId>,
        directory: Option<&'a str>,
    ) -> HubFuture<'a, CatalogView> {
        Box::pin(async move {
            let mut pairs = vec![("agent_id", agent)];
            if let Some(asid) = asid {
                pairs.push(("asid", asid.0.as_str()));
            }
            if let Some(directory) = directory {
                pairs.push(("directory", directory));
            }
            let catalog: AgentCatalog = self
                .data(Method::GET, &["api", "agent-catalog"], &pairs, None)
                .await?;
            Ok(CatalogView { catalog })
        })
    }

    fn run_command<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        name: &'a str,
        arguments: &'a str,
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, Value> {
        Box::pin(async move {
            let mut body = json!({"name": name, "arguments": arguments});
            if let Some(delivery) = delivery {
                body["delivery"] = json!(delivery);
            }
            let json = self
                .session_call(Method::POST, asid, &["command"], &[], Some(body))
                .await?;
            Ok(json.get("data").cloned().unwrap_or(Value::Null))
        })
    }

    fn activate_skill<'a>(&'a self, asid: &'a AgentSessionId, skill: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(
                    Method::POST,
                    asid,
                    &["skill"],
                    &[],
                    Some(json!({"skill": skill})),
                )
                .await,
            )
        })
    }

    fn switch_model<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        model: &'a ModelRef,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(
                    Method::POST,
                    asid,
                    &["model"],
                    &[],
                    Some(json!({"model": model})),
                )
                .await,
            )
        })
    }

    fn switch_mode<'a>(&'a self, asid: &'a AgentSessionId, mode: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(
                    Method::POST,
                    asid,
                    &["mode"],
                    &[],
                    Some(json!({"mode": mode})),
                )
                .await,
            )
        })
    }

    fn auto_approve<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, bool> {
        Box::pin(async move {
            let json = self
                .session_call(Method::GET, asid, &["auto-approve"], &[], None)
                .await?;
            Ok(enabled_of(&json))
        })
    }

    fn set_auto_approve<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        enabled: bool,
    ) -> HubFuture<'a, bool> {
        Box::pin(async move {
            let json = self
                .session_call(
                    Method::POST,
                    asid,
                    &["auto-approve"],
                    &[],
                    Some(json!({"enabled": enabled})),
                )
                .await?;
            Ok(enabled_of(&json))
        })
    }

    fn rename<'a>(&'a self, asid: &'a AgentSessionId, title: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(
                    Method::POST,
                    asid,
                    &["rename"],
                    &[],
                    Some(json!({"title": title})),
                )
                .await,
            )
        })
    }

    fn delete<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            self.cursors.lock().unwrap().remove(&asid.0);
            Self::unit(
                self.session_call(Method::DELETE, asid, &[], &[], None)
                    .await,
            )
        })
    }

    fn fork<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        message_id: Option<&'a str>,
    ) -> HubFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let body = match message_id {
                Some(id) => json!({"message_id": id}),
                None => json!({}),
            };
            let json = self
                .session_call(Method::POST, asid, &["fork"], &[], Some(body))
                .await?;
            decode(json.get("data").cloned().unwrap_or(Value::Null))
        })
    }

    fn inbox_cancel<'a>(&'a self, asid: &'a AgentSessionId, item: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(Method::DELETE, asid, &["inbox", item], &[], None)
                    .await,
            )
        })
    }

    fn inbox_delivery<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        item: &'a str,
        delivery: &'a str,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            if !matches!(delivery, "steer" | "queue") {
                return Err(HubError::Agent(AgentError::InvalidRequest(format!(
                    "unknown delivery {delivery}"
                ))));
            }
            Self::unit(
                self.session_call(Method::POST, asid, &["inbox", item, delivery], &[], None)
                    .await,
            )
        })
    }

    fn store_attachment<'a>(
        &'a self,
        name: &'a str,
        bytes: Vec<u8>,
    ) -> HubFuture<'a, StoredUpload> {
        Box::pin(async move {
            let boundary = format!("muqun{}", rand_suffix());
            let safe_name: String = name
                .chars()
                .map(|c| if c == '"' || c.is_control() { '_' } else { c })
                .collect();
            let mut body = format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{safe_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .into_bytes();
            body.extend_from_slice(&bytes);
            body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
            let answer = self
                .http
                .post(self.url(&["api", "uploads"], &[]))
                .bearer_auth(&self.token)
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(body)
                .send()
                .await
                .map_err(|err| HubError::Agent(AgentError::Network(err.to_string())))?;
            let status = answer.status();
            let text = answer.text().await.unwrap_or_default();
            let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(map_error(status.as_u16(), &json, "", &text));
            }
            let field = |key: &str| json.get(key).and_then(Value::as_str).unwrap_or_default();
            let stored_name = field("url")
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            Ok(StoredUpload {
                path: PathBuf::from(field("path")),
                stored_name,
                name: field("name").to_string(),
                size: json.get("size").and_then(Value::as_u64).unwrap_or(0) as usize,
                mime: intern(field("mime")),
            })
        })
    }

    fn vcs_diff<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<FileDiffItem>> {
        Box::pin(async move {
            let json = self
                .session_call(
                    Method::GET,
                    asid,
                    &["vcs", "diff"],
                    &[("mode", "working")],
                    None,
                )
                .await?;
            decode(
                json.pointer("/data/files")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new())),
            )
        })
    }

    fn view<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            // The route defaults `idle` to now.
            Self::unit(
                self.session_call(Method::POST, asid, &["view"], &[], Some(json!({})))
                    .await,
            )
        })
    }

    fn stage_revert<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        message_id: &'a str,
        files: bool,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(
                    Method::POST,
                    asid,
                    &["revert", "stage"],
                    &[],
                    Some(json!({"message_id": message_id, "files": files})),
                )
                .await,
            )
        })
    }

    fn commit_revert<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(Method::POST, asid, &["revert", "commit"], &[], None)
                    .await,
            )
        })
    }

    fn clear_revert<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.session_call(Method::POST, asid, &["revert", "clear"], &[], None)
                    .await,
            )
        })
    }

    fn compact<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let body = match delivery {
                Some(delivery) => json!({"delivery": delivery}),
                None => json!({}),
            };
            Self::unit(
                self.session_call(Method::POST, asid, &["compact"], &[], Some(body))
                    .await,
            )
        })
    }

    fn descendants<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            let json = self
                .session_call(Method::GET, asid, &["descendants"], &[], None)
                .await?;
            decode(
                json.pointer("/data/data")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new())),
            )
        })
    }

    fn timeline_after<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        after: u64,
    ) -> HubFuture<'a, TimelineCatchUp> {
        Box::pin(async move {
            let after = after.to_string();
            let json = self
                .session_call(Method::GET, asid, &["timeline"], &[("after", &after)], None)
                .await?;
            let data = json.get("data").cloned().unwrap_or(Value::Null);
            let mut items: Vec<TimelineItem> = decode(
                data.get("items")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new())),
            )?;
            let directory = self.snapshot_directory(asid).await;
            muqun_gateway::agents::message_images::enrich_timeline(
                &asid.0,
                directory.as_deref(),
                &mut items,
            );
            Ok(TimelineCatchUp {
                items,
                latest_seq: data.get("latest_seq").and_then(Value::as_u64).unwrap_or(0),
                resync: data.get("resync").and_then(Value::as_bool).unwrap_or(false),
            })
        })
    }

    fn list_tasks<'a>(
        &'a self,
        root: Option<&'a str>,
        state: Option<&'a str>,
    ) -> HubFuture<'a, Vec<TaskRow>> {
        Box::pin(async move {
            let mut pairs = Vec::new();
            if let Some(root) = root {
                pairs.push(("root", root));
            }
            if let Some(state) = state {
                pairs.push(("state", state));
            }
            let json = self
                .call(Method::GET, &["api", "tasks"], &pairs, None)
                .await?;
            task_rows(&json)
        })
    }

    fn create_task<'a>(
        &'a self,
        goal: &'a str,
        agent_id: Option<&'a str>,
        cwd: Option<&'a str>,
        peer_server_id: Option<&'a str>,
    ) -> HubFuture<'a, TaskRow> {
        Box::pin(async move {
            let json = self
                .call(
                    Method::POST,
                    &["api", "tasks"],
                    &[],
                    Some(create_task_body(goal, agent_id, cwd, peer_server_id)),
                )
                .await?;
            task_row(&json)
        })
    }

    fn cancel_task<'a>(&'a self, id: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            Self::unit(
                self.call(Method::POST, &["api", "tasks", id, "cancel"], &[], None)
                    .await,
            )
        })
    }

    fn complete_task<'a>(&'a self, id: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            // The body is optional; an empty object keeps the task's own hint.
            Self::unit(
                self.call(
                    Method::POST,
                    &["api", "tasks", id, "complete"],
                    &[],
                    Some(json!({})),
                )
                .await,
            )
        })
    }

    fn task_defaults(&self) -> HubFuture<'_, TaskDefaults> {
        Box::pin(async move {
            let json = self
                .call(Method::GET, &["api", "tasks", "defaults"], &[], None)
                .await?;
            decode(json)
        })
    }

    fn list_peers(&self) -> HubFuture<'_, Vec<PeerMention>> {
        Box::pin(async move {
            let mut peers = vec![PeerMention {
                server_id: "self".into(),
                name: super::local::computer_name(),
                reachable: true,
                current: true,
            }];
            if let Ok(json) = self.call(Method::GET, &["api", "peers"], &[], None).await {
                for peer in json
                    .get("peers")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let text = |key: &str| peer.get(key).and_then(Value::as_str);
                    if let (Some(server_id), Some(name)) = (text("server_id"), text("name")) {
                        peers.push(PeerMention {
                            server_id: server_id.to_string(),
                            name: name.to_string(),
                            reachable: peer
                                .get("reachable")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            current: false,
                        });
                    }
                }
            }
            Ok(peers)
        })
    }

    fn events(&self) -> broadcast::Receiver<AgentDomainEvent> {
        self.events.subscribe()
    }

    fn task_events(&self) -> Option<broadcast::Receiver<TaskHubEvent>> {
        Some(self.task_events.subscribe())
    }
}

impl RemoteHub {
    async fn snapshot_directory(&self, asid: &AgentSessionId) -> Option<String> {
        self.data::<AgentSessionInfo>(Method::GET, &["api", "agent-sessions", &asid.0], &[], None)
            .await
            .ok()
            .and_then(|info| info.directory)
    }
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}")
}

/// `StoredUpload::mime` is a `&'static str`; the set of types is small.
fn intern(mime: &str) -> &'static str {
    static KNOWN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut known = KNOWN.lock().unwrap();
    if let Some(found) = known.iter().find(|m| **m == mime) {
        return found;
    }
    let leaked: &'static str = Box::leak(mime.to_string().into_boxed_str());
    known.push(leaked);
    leaked
}

// ---------------------------------------------------------------------------
// Event socket
// ---------------------------------------------------------------------------

/// One `/api/ws` connection after another, for the life of the process. After
/// every reconnect the projection is told to read again (an unnamed
/// `agent.resync`): the open session's snapshot and every list, which covers
/// whatever the socket missed.
async fn event_socket(hub: Arc<RemoteHub>) {
    let mut backoff = BACKOFF_START;
    let mut first = true;
    loop {
        let started = tokio::time::Instant::now();
        match run_socket(&hub, &mut first).await {
            Ok(()) => tracing::info!("event socket closed"),
            Err(why) => tracing::warn!("event socket: {why}"),
        }
        // A connection that lived a while starts the backoff over.
        if started.elapsed() > Duration::from_secs(30) {
            backoff = BACKOFF_START;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn run_socket(hub: &RemoteHub, first: &mut bool) -> Result<(), String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::Message;

    let mut ws_url = hub.url(&["api", "ws"], &[]);
    ws_url
        .set_scheme(if hub.base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| "bad gateway url".to_string())?;
    let mut request = ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| e.to_string())?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", hub.token)
            .parse()
            .map_err(|_| "bad token".to_string())?,
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| e.to_string())?;
    socket
        .send(Message::text(r#"{"t":"subscribe_all"}"#))
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("event socket connected");
    if !std::mem::take(first) {
        let _ = hub.events.send(AgentDomainEvent::Resync {
            asid: AgentSessionId(String::new()),
            reason: "reconnected".into(),
        });
        let _ = hub.task_events.send(TaskHubEvent::Resync);
    }
    while let Some(message) = socket.next().await {
        match message.map_err(|e| e.to_string())? {
            Message::Text(text) => match parse_frame(text.as_str()) {
                Some(Frame::Agent(event)) => {
                    let _ = hub.events.send(*event);
                }
                Some(Frame::Task(event)) => {
                    let _ = hub.task_events.send(event);
                }
                None => {}
            },
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

/// What one server frame carries: a session event, or a task change.
#[derive(Debug, PartialEq)]
enum Frame {
    Agent(Box<AgentDomainEvent>),
    Task(TaskHubEvent),
}

/// One server frame. `event` frames carry the event's own JSON in `data`: a
/// session event, or -- named `task.updated` / `task.deleted` -- the task as
/// `GET /api/tasks` shows it. `resync` asks for a re-read, of the tasks when
/// it says `tasks: true`. Everything else (hello, subscribed) is neither's.
fn parse_frame(text: &str) -> Option<Frame> {
    let frame: Value = serde_json::from_str(text).ok()?;
    match frame.get("t")?.as_str()? {
        "event" if frame_event_name(&frame).is_some_and(|n| n.starts_with("task.")) => {
            parse_task_frame(&frame).map(Frame::Task)
        }
        "event" => match serde_json::from_value(frame.get("data")?.clone()) {
            Ok(event) => Some(Frame::Agent(Box::new(event))),
            Err(err) => {
                tracing::debug!(%err, "event frame not understood");
                None
            }
        },
        "resync" if frame.get("tasks").and_then(Value::as_bool) == Some(true) => {
            Some(Frame::Task(TaskHubEvent::Resync))
        }
        "resync" => Some(Frame::Agent(Box::new(AgentDomainEvent::Resync {
            asid: AgentSessionId(frame.get("asid")?.as_str()?.to_string()),
            reason: "gateway".into(),
        }))),
        _ => None,
    }
}

fn frame_event_name(frame: &Value) -> Option<&str> {
    frame.get("event")?.as_str()
}

fn parse_task_frame(frame: &Value) -> Option<TaskHubEvent> {
    let data = frame.get("data")?;
    match frame_event_name(frame)? {
        "task.updated" => match TaskRow::from_json(data) {
            Ok(row) => Some(TaskHubEvent::Updated(Box::new(row))),
            Err(err) => {
                tracing::debug!(%err, "task frame not understood");
                None
            }
        },
        "task.deleted" => Some(TaskHubEvent::Deleted {
            task_id: data.get("task_id")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_approve_answers_read_as_on_or_off() {
        for (body, want) in [
            (
                r#"{"data":{"enabled":true,"native":"bypassPermissions"}}"#,
                true,
            ),
            (
                r#"{"data":{"enabled":false,"native":"bypassPermissions"}}"#,
                false,
            ),
            // An agent without a session control answers null: off.
            (r#"{"data":{"enabled":null,"native":null}}"#, false),
            (r#"{}"#, false),
        ] {
            assert_eq!(
                enabled_of(&serde_json::from_str(body).unwrap()),
                want,
                "{body}"
            );
        }
    }

    #[test]
    fn task_lists_and_creates_read_as_rows() {
        let json = json!({"tasks": [{
            "task_id": "t_1", "state": "working", "kind": "delegation", "goal": "fix it",
            "title": "fix it", "summary": null, "origin": "device", "updated_ms": 5,
            "binding": {"kind": "agent_session", "agent_id": "opencode", "asid": "ses_1"},
            "children": {"total": 2, "active": 1, "waiting": 0, "failed": 1},
            "future_field": true
        }, {
            "task_id": "t_2", "state": "idle_unconfirmed", "goal": "g", "origin": "device",
            "binding": {"kind": "pane", "pane_id": "w1:p9", "agent_id": "codex"}
        }]});
        let rows = task_rows(&json).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "t_1");
        assert_eq!(rows[0].agent_id.as_deref(), Some("opencode"));
        assert_eq!(rows[0].asid.as_deref(), Some("ses_1"));
        assert_eq!(rows[0].children.failed, 1);
        assert!(!rows[0].completable && !rows[0].is_ended());
        assert!(rows[1].completable, "a paused device task can be confirmed");
        assert_eq!(rows[1].agent_id.as_deref(), Some("codex"));
        assert!(task_rows(&json!({})).unwrap().is_empty());
        let created = task_row(&json!({"task_id": "t_3", "state": "working", "asid": "ses_3"}));
        assert_eq!(created.unwrap().asid.as_deref(), Some("ses_3"));
    }

    #[test]
    fn a_new_task_sends_only_what_was_given() {
        assert_eq!(
            create_task_body("g", None, None, None),
            json!({"goal": "g"})
        );
        assert_eq!(
            create_task_body("g", Some("codex"), Some("~/w"), Some("srv")),
            json!({"goal": "g", "agent_id": "codex", "cwd": "~/w", "computer_ids": ["srv"]})
        );
        assert_eq!(
            create_task_body("g", Some(""), Some(""), None),
            json!({"goal": "g"})
        );
    }

    #[test]
    fn frames_become_events() {
        for (frame, want) in [
            (
                r#"{"t":"event","asid":"a","seq":3,"event":"agent.permission.resolved","data":{"type":"agent.permission.resolved","asid":"a","request_id":"r","seq":3}}"#,
                Some("agent.permission.resolved"),
            ),
            (r#"{"t":"resync","asid":"a"}"#, Some("agent.resync")),
            (r#"{"t":"resync","asid":"","tasks":true}"#, None),
            (r#"{"t":"hello","connection_id":"c"}"#, None),
            (r#"{"t":"event","data":{"type":"nonsense"}}"#, None),
            (
                r#"{"t":"event","asid":null,"seq":1,"event":"task.updated","data":{"task_id":"t"}}"#,
                None,
            ),
        ] {
            let got = match parse_frame(frame) {
                Some(Frame::Agent(event)) => Some(event.event_name()),
                _ => None,
            };
            assert_eq!(got, want, "{frame}");
        }
    }

    #[test]
    fn task_frames_become_task_events() {
        let updated = r#"{"t":"event","asid":null,"seq":3,"event":"task.updated","data":{"task_id":"t_1","state":"working","goal":"g","title":"g","origin":"device","binding":{"kind":"agent_session","agent_id":"codex","asid":"ses_1"}}}"#;
        let Some(Frame::Task(TaskHubEvent::Updated(row))) = parse_frame(updated) else {
            panic!("a task.updated frame is a task update");
        };
        assert_eq!((row.id.as_str(), row.state.as_str()), ("t_1", "working"));
        assert_eq!(row.asid.as_deref(), Some("ses_1"));
        assert_eq!(
            parse_frame(
                r#"{"t":"event","asid":null,"seq":4,"event":"task.deleted","data":{"task_id":"t_1"}}"#
            ),
            Some(Frame::Task(TaskHubEvent::Deleted {
                task_id: "t_1".into()
            }))
        );
        assert_eq!(
            parse_frame(r#"{"t":"resync","asid":"","tasks":true}"#),
            Some(Frame::Task(TaskHubEvent::Resync))
        );
        for bad in [
            r#"{"t":"event","event":"task.deleted","data":{}}"#,
            r#"{"t":"event","event":"task.unknown","data":{}}"#,
        ] {
            assert_eq!(parse_frame(bad), None, "{bad}");
        }
    }

    #[test]
    fn gateway_errors_map_to_hub_errors() {
        let body = |code: &str| json!({"error": {"code": code, "message": "m"}});
        assert!(matches!(
            map_error(404, &body("session_not_found"), "x", ""),
            HubError::SessionNotFound(a) if a == "x"
        ));
        assert!(matches!(
            map_error(404, &body("not_retained"), "x", ""),
            HubError::Agent(AgentError::SessionNotFound(a)) if a == "x"
        ));
        assert!(matches!(
            map_error(409, &body("history_cursor_lost"), "x", ""),
            HubError::Agent(AgentError::HistoryCursorLost)
        ));
        assert!(matches!(
            map_error(501, &body("feature_unsupported"), "x", ""),
            HubError::Agent(AgentError::Unsupported(_))
        ));
        assert!(matches!(
            map_error(503, &json!({"error": {"code": "agent_unavailable", "message": "Agent 'opencode' is not available"}}), "", ""),
            HubError::AgentUnavailable { agent, .. } if agent == "opencode"
        ));
    }
}
