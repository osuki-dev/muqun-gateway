//! Muqun Unified MCP Hub: connect external HTTP (Streamable HTTP) and stdio MCP
//! servers once in Gateway and expose them to every Muqun-connected agent via
//! Tool Search (`hub.search_tools`) and scoped, secret-redacted proxy execution
//! (`hub.call_tool`).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader, BufWriter,
};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;

use crate::mcp::reducer::redact_secrets;
use crate::mcp::tokens::{AssistantSession, TokenBinding};
use crate::platform::config::{
    UpstreamIsolation, UpstreamMcpConfig, UpstreamScope, UpstreamTransport,
};
use crate::tasks::domain::{now_ms, TaskFilter, TaskId, TaskOrigin};
use crate::{api_error, require_device, ApiResult, AppState};

pub(crate) const CAPABILITY: &str = "mcp_hub_v1";

pub(crate) const MASKED_SECRET: &str = "••••••••";
const DEFAULT_UPSTREAM_TIMEOUT_S: u64 = 30;
const MIN_UPSTREAM_TIMEOUT_S: u64 = 1;
const MAX_UPSTREAM_TIMEOUT_S: u64 = 120;
const LAZY_PROBE_MAX_TIMEOUT_S: u64 = 10;
const MAX_UPSTREAMS: usize = 64;
const MAX_STDIO_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_HTTP_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOLS_PER_UPSTREAM: usize = 256;
const MAX_SKILLS_PER_UPSTREAM: usize = 64;
const MAX_TOOL_DESC_CHARS: usize = 512;
const MAX_TOOL_SCHEMA_BYTES: usize = 16 * 1024;
const MCP_CLIENT_PROTOCOL_VERSION: &str = "2025-03-26";

/// Effective caller scope for evaluating [`UpstreamScope`] policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallerScope {
    /// Local interactive owner session (not executing inside a delegated task).
    LocalOwner,
    /// Local delegated sub-task (not originated by a remote peer).
    LocalTask,
    /// Remote peer-originated task or unresolved task ancestry.
    RemoteOrRestricted,
}

impl CallerScope {
    pub(crate) fn permits(self, scope: UpstreamScope) -> bool {
        match scope {
            UpstreamScope::LocalOnly => matches!(self, Self::LocalOwner),
            UpstreamScope::Tasks => matches!(self, Self::LocalOwner | Self::LocalTask),
            UpstreamScope::All => true,
        }
    }
}

/// Resolve the caller's effective hub scope from its assistant token and task ancestry.
pub(crate) async fn resolve_caller_scope(
    state: &AppState,
    session: &AssistantSession,
    call: Option<&crate::mcp::lineage::Call<'_>>,
) -> CallerScope {
    match &session.binding {
        TokenBinding::Task { task_id } => match state.tasks.task(&TaskId(task_id.clone())).await {
            Ok(Some(task)) => {
                if task.origin == TaskOrigin::Peer || task.requester_server_id.is_some() {
                    CallerScope::RemoteOrRestricted
                } else {
                    CallerScope::LocalTask
                }
            }
            _ => CallerScope::RemoteOrRestricted,
        },
        TokenBinding::Pane {
            session_id,
            pane_id,
        } => {
            let filter = TaskFilter {
                pane: Some((session_id.clone(), pane_id.clone())),
                limit: 16,
                ..Default::default()
            };
            match state.tasks.open_tasks(filter).await {
                Ok(tasks) if !tasks.is_empty() => {
                    if tasks
                        .iter()
                        .any(|t| t.origin == TaskOrigin::Peer || t.requester_server_id.is_some())
                    {
                        CallerScope::RemoteOrRestricted
                    } else {
                        CallerScope::LocalTask
                    }
                }
                Ok(_) => CallerScope::LocalOwner,
                Err(_) => CallerScope::RemoteOrRestricted,
            }
        }
        TokenBinding::Runtime { agent_id } | TokenBinding::Agent { agent_id } => {
            // If this call can be attributed to a specific open task, enforce its task scope.
            if let Some(call) = call {
                if let Some(task_id) =
                    crate::mcp::lineage::own_task(state, session, call, None).await
                {
                    return match state.tasks.task(&task_id).await {
                        Ok(Some(task)) => {
                            if task.origin == TaskOrigin::Peer || task.requester_server_id.is_some()
                            {
                                CallerScope::RemoteOrRestricted
                            } else {
                                CallerScope::LocalTask
                            }
                        }
                        _ => CallerScope::RemoteOrRestricted,
                    };
                }
            }
            // Fail closed on shared daemons while any remote peer-originated task is open on this agent.
            let open = state
                .tasks
                .open_tasks(TaskFilter {
                    limit: 64,
                    ..Default::default()
                })
                .await;
            match open {
                Ok(tasks) => {
                    let has_peer_task_on_agent = tasks.iter().any(|t| {
                        let matches_agent = match &t.binding {
                            crate::tasks::domain::TaskBinding::AgentSession {
                                agent_id: a, ..
                            } => a == agent_id,
                            crate::tasks::domain::TaskBinding::Pane {
                                agent_id: Some(a), ..
                            } => a == agent_id,
                            _ => false,
                        };
                        (t.origin == TaskOrigin::Peer || t.requester_server_id.is_some())
                            && matches_agent
                    });
                    if has_peer_task_on_agent {
                        CallerScope::RemoteOrRestricted
                    } else {
                        CallerScope::LocalOwner
                    }
                }
                Err(_) => CallerScope::RemoteOrRestricted,
            }
        }
    }
}

/// One tool discovered from an upstream MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UpstreamTool {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default, rename = "inputSchema")]
    pub(crate) input_schema: Value,
}

/// One skill discovered from an upstream MCP server (best-effort SEP-2640).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UpstreamSkill {
    pub(crate) server: String,
    pub(crate) name: String,
    pub(crate) uri: String,
    pub(crate) description: String,
}

#[derive(Debug, Clone, Default)]
struct UpstreamRuntimeState {
    tools: Vec<UpstreamTool>,
    skills: Vec<UpstreamSkill>,
    last_probed_ms: Option<u64>,
    last_error: Option<String>,
    healthy: bool,
}

/// Persistent stdio process for a `Shared` stdio upstream.
struct StdioProcess {
    _child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

/// Per-upstream live transport state (independent lock so one slow upstream never blocks others).
#[derive(Default)]
struct UpstreamWorker {
    http_session_id: AsyncMutex<Option<String>>,
    stdio: AsyncMutex<Option<StdioProcess>>,
}

/// Gateway's Unified MCP Hub manager.
pub(crate) struct McpHub {
    config_path: Option<PathBuf>,
    upstreams: RwLock<Vec<UpstreamMcpConfig>>,
    runtime: RwLock<HashMap<String, UpstreamRuntimeState>>,
    workers: RwLock<HashMap<String, Arc<UpstreamWorker>>>,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpstreamToolPreview {
    pub(crate) name: String,
    pub(crate) description: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpstreamView {
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) scope: UpstreamScope,
    pub(crate) isolation: UpstreamIsolation,
    pub(crate) timeout_s: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) denied_tools: Vec<String>,
    pub(crate) transport: UpstreamTransport,
    pub(crate) healthy: bool,
    pub(crate) tool_count: usize,
    pub(crate) skill_count: usize,
    pub(crate) tools: Vec<UpstreamToolPreview>,
    pub(crate) last_probed_ms: Option<u64>,
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpstreamUpsertBody {
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default = "default_true_hub")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) scope: UpstreamScope,
    #[serde(default)]
    pub(crate) isolation: UpstreamIsolation,
    #[serde(default)]
    pub(crate) timeout_s: Option<u64>,
    #[serde(default)]
    pub(crate) allowed_tools: Vec<String>,
    #[serde(default)]
    pub(crate) denied_tools: Vec<String>,
    pub(crate) transport: UpstreamTransport,
}

fn default_true_hub() -> bool {
    true
}

#[derive(Debug)]
pub(crate) enum HubCallError {
    NotFound(String),
    Disabled(String),
    ScopeDenied {
        server: String,
        scope: UpstreamScope,
    },
    ToolDenied {
        server: String,
        tool: String,
    },
    UpstreamFailed(String),
}

impl McpHub {
    pub(crate) fn new(upstreams: Vec<UpstreamMcpConfig>, config_path: Option<PathBuf>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(format!("muqun-gateway/{}", env!("CARGO_PKG_VERSION")))
            // Never follow HTTP redirects automatically: prevents leaking Authorization headers
            // or bouncing requests onto loopback/cloud-metadata endpoints.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            config_path,
            upstreams: RwLock::new(upstreams),
            runtime: RwLock::new(HashMap::new()),
            workers: RwLock::new(HashMap::new()),
            http,
        }
    }

    pub(crate) fn list_views(&self) -> Vec<UpstreamView> {
        let configs = self
            .upstreams
            .read()
            .map(|list| list.clone())
            .unwrap_or_default();
        let states = self
            .runtime
            .read()
            .map(|map| map.clone())
            .unwrap_or_default();
        configs
            .into_iter()
            .map(|cfg| {
                let st = states.get(&cfg.id).cloned().unwrap_or_default();
                upstream_view(cfg, st)
            })
            .collect()
    }

    pub(crate) fn upsert(
        &self,
        id: String,
        body: UpstreamUpsertBody,
    ) -> Result<UpstreamView, String> {
        if !valid_upstream_id(&id) {
            return Err(
                "id must be 1..=64 lowercase alphanumeric/dash/underscore characters and not 'muqun'"
                    .into(),
            );
        }
        let existing = self
            .upstreams
            .read()
            .ok()
            .and_then(|list| list.iter().find(|u| u.id == id).cloned());

        let mut transport = body.transport;
        preserve_masked_secrets(existing.as_ref().map(|p| &p.transport), &mut transport)?;
        validate_transport(&transport)?;

        let label = body
            .label
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        if label.as_ref().is_some_and(|l| l.len() > 120) {
            return Err("label must be at most 120 characters".into());
        }
        let timeout_s = body
            .timeout_s
            .map(|s| s.clamp(MIN_UPSTREAM_TIMEOUT_S, MAX_UPSTREAM_TIMEOUT_S));
        let allowed_tools = sanitize_tool_patterns(body.allowed_tools, "allowed_tools")?;
        let denied_tools = sanitize_tool_patterns(body.denied_tools, "denied_tools")?;

        let next = UpstreamMcpConfig {
            id: id.clone(),
            label,
            enabled: body.enabled,
            scope: body.scope,
            isolation: body.isolation,
            timeout_s,
            allowed_tools,
            denied_tools,
            transport,
        };

        let updated_list = {
            let mut list = self
                .upstreams
                .write()
                .map_err(|_| "upstream registry lock poisoned".to_owned())?;
            if let Some(slot) = list.iter_mut().find(|u| u.id == id) {
                *slot = next.clone();
            } else {
                if list.len() >= MAX_UPSTREAMS {
                    return Err(format!(
                        "at most {MAX_UPSTREAMS} upstream MCP servers allowed"
                    ));
                }
                list.push(next.clone());
            }
            list.clone()
        };

        if let Err(err) = self.persist(&updated_list) {
            // Roll back in-memory state if persistence failed.
            if let Ok(mut list) = self.upstreams.write() {
                if let Some(prev) = existing {
                    if let Some(slot) = list.iter_mut().find(|u| u.id == id) {
                        *slot = prev;
                    }
                } else {
                    list.retain(|u| u.id != id);
                }
            }
            return Err(format!("failed to save config: {err}"));
        }

        // Drop stale worker & runtime catalog so changes take effect immediately.
        if let Ok(mut workers) = self.workers.write() {
            workers.remove(&id);
        }
        if let Ok(mut runtime) = self.runtime.write() {
            runtime.remove(&id);
        }

        Ok(upstream_view(next, UpstreamRuntimeState::default()))
    }

    pub(crate) fn delete(&self, id: &str) -> Result<bool, String> {
        let (removed, updated_list) = {
            let mut list = self
                .upstreams
                .write()
                .map_err(|_| "upstream registry lock poisoned".to_owned())?;
            let before = list.len();
            let prev = list.iter().find(|u| u.id == id).cloned();
            list.retain(|u| u.id != id);
            if list.len() == before {
                return Ok(false);
            }
            (prev, list.clone())
        };

        if let Err(err) = self.persist(&updated_list) {
            if let (Some(prev), Ok(mut list)) = (removed, self.upstreams.write()) {
                list.push(prev);
            }
            return Err(format!("failed to save config: {err}"));
        }

        if let Ok(mut workers) = self.workers.write() {
            workers.remove(id);
        }
        if let Ok(mut runtime) = self.runtime.write() {
            runtime.remove(id);
        }
        Ok(true)
    }

    fn persist(&self, upstreams: &[UpstreamMcpConfig]) -> anyhow::Result<()> {
        let Some(path) = &self.config_path else {
            return Ok(());
        };
        let next = upstreams.to_vec();
        crate::platform::store::edit_config(path, |config| {
            config.mcp.upstreams = next;
        })
    }

    fn worker_for(&self, id: &str) -> Arc<UpstreamWorker> {
        if let Ok(map) = self.workers.read() {
            if let Some(w) = map.get(id) {
                return w.clone();
            }
        }
        let mut map = self
            .workers
            .write()
            .expect("upstream workers lock poisoned");
        map.entry(id.to_owned())
            .or_insert_with(|| Arc::new(UpstreamWorker::default()))
            .clone()
    }

    fn get_config(&self, id: &str) -> Option<UpstreamMcpConfig> {
        self.upstreams
            .read()
            .ok()
            .and_then(|list| list.iter().find(|u| u.id == id).cloned())
    }

    /// Connect to `id`, refresh its cached tools/skills catalog, and return its updated view.
    pub(crate) async fn probe(&self, id: &str) -> Result<UpstreamView, HubCallError> {
        let cfg = self
            .get_config(id)
            .ok_or_else(|| HubCallError::NotFound(id.to_owned()))?;
        let state = self.probe_config(&cfg, None).await;
        Ok(upstream_view(cfg, state))
    }

    async fn probe_config(
        &self,
        cfg: &UpstreamMcpConfig,
        max_timeout: Option<Duration>,
    ) -> UpstreamRuntimeState {
        let timeout = upstream_timeout(cfg, max_timeout);
        let worker = self.worker_for(&cfg.id);
        let now = now_ms();

        let tools_res = self
            .rpc_call(cfg, &worker, "tools/list", json!({}), None, timeout)
            .await;

        let mut next = UpstreamRuntimeState {
            last_probed_ms: Some(now),
            ..Default::default()
        };

        match tools_res {
            Ok(result) => {
                let raw_tools = result
                    .get("tools")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                next.tools = raw_tools
                    .into_iter()
                    .take(MAX_TOOLS_PER_UPSTREAM)
                    .filter_map(|t| {
                        let name = t.get("name")?.as_str()?.trim().to_owned();
                        if name.is_empty() || name.len() > 128 {
                            return None;
                        }
                        let raw_desc = t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let (clean_desc, _) = redact_secrets(raw_desc);
                        let description: String =
                            clean_desc.chars().take(MAX_TOOL_DESC_CHARS).collect();
                        let mut input_schema = t
                            .get("inputSchema")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object" }));
                        if input_schema.to_string().len() > MAX_TOOL_SCHEMA_BYTES {
                            input_schema = json!({
                                "type": "object",
                                "description": "Schema omitted (exceeded 16 KiB limit)"
                            });
                        }
                        Some(UpstreamTool {
                            name,
                            description,
                            input_schema,
                        })
                    })
                    .collect();
                next.healthy = true;
                next.last_error = None;

                // Best-effort SEP-2640 skills discovery; ignore servers that do not implement it.
                if let Ok(skills_val) = self
                    .rpc_call(
                        cfg,
                        &worker,
                        "skills/list",
                        json!({}),
                        None,
                        timeout.min(Duration::from_secs(5)),
                    )
                    .await
                {
                    let raw_skills = skills_val
                        .get("skills")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    next.skills = raw_skills
                        .into_iter()
                        .take(MAX_SKILLS_PER_UPSTREAM)
                        .filter_map(|s| {
                            let uri = s.get("uri")?.as_str()?.to_owned();
                            if uri.len() > 512 {
                                return None;
                            }
                            let name = s
                                .get("frontmatter")
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                                .or_else(|| s.get("name").and_then(Value::as_str))?
                                .to_owned();
                            if name.len() > 128 {
                                return None;
                            }
                            let raw_desc = s
                                .get("frontmatter")
                                .and_then(|f| f.get("description"))
                                .and_then(Value::as_str)
                                .or_else(|| s.get("description").and_then(Value::as_str))
                                .unwrap_or_default();
                            let (clean_desc, _) = redact_secrets(raw_desc);
                            let description: String =
                                clean_desc.chars().take(MAX_TOOL_DESC_CHARS).collect();
                            Some(UpstreamSkill {
                                server: cfg.id.clone(),
                                name,
                                uri,
                                description,
                            })
                        })
                        .collect();
                }
            }
            Err(err) => {
                let (clean_err, _) = redact_secrets(&err);
                next.healthy = false;
                next.last_error = Some(clean_err);
            }
        }

        if let Ok(mut map) = self.runtime.write() {
            map.insert(cfg.id.clone(), next.clone());
        }
        next
    }

    /// Tool Search over permitted, enabled upstreams.
    pub(crate) async fn search_tools(
        &self,
        caller: CallerScope,
        query: Option<&str>,
        server_filter: Option<&str>,
        include_schema: bool,
        limit: usize,
    ) -> Value {
        let permitted: Vec<UpstreamMcpConfig> = self
            .upstreams
            .read()
            .map(|list| {
                list.iter()
                    .filter(|u| {
                        u.enabled
                            && caller.permits(u.scope)
                            && server_filter.is_none_or(|s| u.id.eq_ignore_ascii_case(s))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        // Lazy-probe any enabled permitted upstreams that have never been probed yet.
        let unprobed: Vec<UpstreamMcpConfig> = {
            let states = self.runtime.read().ok();
            permitted
                .iter()
                .filter(|u| {
                    states
                        .as_ref()
                        .and_then(|map| map.get(&u.id))
                        .and_then(|st| st.last_probed_ms)
                        .is_none()
                })
                .cloned()
                .collect()
        };
        if !unprobed.is_empty() {
            let probes = unprobed.iter().map(|cfg| {
                self.probe_config(cfg, Some(Duration::from_secs(LAZY_PROBE_MAX_TIMEOUT_S)))
            });
            futures::future::join_all(probes).await;
        }

        let states = self
            .runtime
            .read()
            .map(|map| map.clone())
            .unwrap_or_default();

        let tokens: Vec<String> = query
            .unwrap_or_default()
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();

        let mut servers_summary = Vec::new();
        let mut matches: Vec<(u8, Value)> = Vec::new();

        for cfg in &permitted {
            let st = states.get(&cfg.id).cloned().unwrap_or_default();
            let permitted_tools: Vec<&UpstreamTool> = st
                .tools
                .iter()
                .filter(|tool| tool_permitted(cfg, &tool.name))
                .collect();
            servers_summary.push(json!({
                "id": cfg.id,
                "label": cfg.label,
                "healthy": st.healthy,
                "tool_count": permitted_tools.len(),
                "error": st.last_error,
            }));
            for tool in permitted_tools {
                let score = match_score(&cfg.id, tool, &tokens);
                if score == 0 {
                    continue;
                }
                let mut entry = json!({
                    "server": cfg.id,
                    "name": tool.name,
                    "description": tool.description,
                });
                if include_schema {
                    entry["inputSchema"] = tool.input_schema.clone();
                }
                matches.push((score, entry));
            }
        }

        // Highest match score first, then stable server/name order.
        matches.sort_by_key(|a| std::cmp::Reverse(a.0));
        let total_matches = matches.len();
        let tools: Vec<Value> = matches
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry)
            .collect();

        json!({
            "servers": servers_summary,
            "tools": tools,
            "total_matches": total_matches,
        })
    }

    /// Invoke one tool on a registered upstream with scope enforcement and secret redaction.
    pub(crate) async fn call_tool(
        &self,
        caller: CallerScope,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, HubCallError> {
        let cfg = self
            .get_config(server)
            .ok_or_else(|| HubCallError::NotFound(server.to_owned()))?;
        if !cfg.enabled {
            return Err(HubCallError::Disabled(cfg.id));
        }
        if !caller.permits(cfg.scope) {
            return Err(HubCallError::ScopeDenied {
                server: cfg.id,
                scope: cfg.scope,
            });
        }
        if !tool_permitted(&cfg, tool) {
            return Err(HubCallError::ToolDenied {
                server: cfg.id,
                tool: tool.to_owned(),
            });
        }
        let args_obj = if arguments.is_object() {
            arguments
        } else {
            json!({})
        };
        let worker = self.worker_for(&cfg.id);
        let timeout = upstream_timeout(&cfg, None);
        let mut raw = self
            .rpc_call(
                &cfg,
                &worker,
                "tools/call",
                json!({
                    "name": tool,
                    "arguments": args_obj,
                }),
                Some(tool),
                timeout,
            )
            .await
            .map_err(|err| {
                let (clean, _) = redact_secrets(&err);
                HubCallError::UpstreamFailed(clean)
            })?;

        let redacted = redact_value_in_place(&mut raw);
        let is_error = raw.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let mut out = json!({
            "server": cfg.id,
            "tool": tool,
            "isError": is_error,
            "result": raw,
        });
        if redacted {
            out["redacted"] = json!(true);
        }
        Ok(out)
    }

    /// Cached skills from enabled upstreams permitted for `caller`.
    pub(crate) fn cached_skills(&self, caller: CallerScope) -> Vec<UpstreamSkill> {
        let permitted_ids: Vec<String> = self
            .upstreams
            .read()
            .map(|list| {
                list.iter()
                    .filter(|u| u.enabled && caller.permits(u.scope))
                    .map(|u| u.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let states = self
            .runtime
            .read()
            .map(|map| map.clone())
            .unwrap_or_default();
        let mut out = Vec::new();
        for id in permitted_ids {
            if let Some(st) = states.get(&id) {
                out.extend(st.skills.clone());
            }
        }
        out
    }

    /// Read the body of an upstream MCP skill via `resources/read` with scope enforcement
    /// and secret redaction. Accepts `server` + `skill_name_or_uri`.
    pub(crate) async fn read_upstream_skill(
        &self,
        caller: CallerScope,
        server: &str,
        skill_key: &str,
    ) -> Result<Value, HubCallError> {
        let cfg = self
            .get_config(server)
            .ok_or_else(|| HubCallError::NotFound(server.to_owned()))?;
        if !cfg.enabled {
            return Err(HubCallError::Disabled(cfg.id));
        }
        if !caller.permits(cfg.scope) {
            return Err(HubCallError::ScopeDenied {
                server: cfg.id,
                scope: cfg.scope,
            });
        }

        // Lazy-probe once if this upstream hasn't been probed yet.
        let need_probe = self
            .runtime
            .read()
            .ok()
            .and_then(|map| map.get(&cfg.id).and_then(|st| st.last_probed_ms))
            .is_none();
        if need_probe {
            self.probe_config(&cfg, Some(Duration::from_secs(LAZY_PROBE_MAX_TIMEOUT_S)))
                .await;
        }

        let skill = self
            .runtime
            .read()
            .ok()
            .and_then(|map| {
                map.get(&cfg.id).and_then(|st| {
                    st.skills
                        .iter()
                        .find(|s| s.name == skill_key || s.uri == skill_key)
                        .cloned()
                })
            })
            .ok_or_else(|| {
                HubCallError::NotFound(format!("skill '{skill_key}' on upstream '{}'", cfg.id))
            })?;

        let worker = self.worker_for(&cfg.id);
        let timeout = upstream_timeout(&cfg, None);
        let raw = self
            .rpc_call(
                &cfg,
                &worker,
                "resources/read",
                json!({ "uri": skill.uri }),
                Some(&skill.uri),
                timeout,
            )
            .await
            .map_err(|err| {
                let (clean, _) = redact_secrets(&err);
                HubCallError::UpstreamFailed(clean)
            })?;

        let contents = raw
            .get("contents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let text = contents
            .iter()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n");
        let (clean_text, redacted) = redact_secrets(&text);
        let mut out = json!({
            "uri": format!("skill://hub/{}/{}/SKILL.md", cfg.id, skill.name),
            "upstream_uri": skill.uri,
            "server": cfg.id,
            "name": skill.name,
            "namespace": format!("hub/{}", cfg.id),
            "content": clean_text,
        });
        if redacted {
            out["redacted"] = json!(true);
        }
        Ok(out)
    }

    async fn rpc_call(
        &self,
        cfg: &UpstreamMcpConfig,
        worker: &Arc<UpstreamWorker>,
        method: &str,
        params: Value,
        mcp_name: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, String> {
        match &cfg.transport {
            UpstreamTransport::Http { url, headers } => {
                self.http_rpc(
                    url,
                    headers,
                    cfg.isolation,
                    worker,
                    method,
                    params,
                    mcp_name,
                    timeout,
                )
                .await
            }
            UpstreamTransport::Stdio { command, args, env } => tokio::time::timeout(
                timeout,
                stdio_rpc(command, args, env, cfg.isolation, worker, method, params),
            )
            .await
            .map_err(|_| {
                format!(
                    "upstream '{}' timed out after {}s",
                    cfg.id,
                    timeout.as_secs()
                )
            })?,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn http_rpc(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        isolation: UpstreamIsolation,
        worker: &Arc<UpstreamWorker>,
        method: &str,
        params: Value,
        mcp_name: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, String> {
        let mut session_id = if isolation == UpstreamIsolation::Shared {
            worker.http_session_id.lock().await.clone()
        } else {
            None
        };

        if session_id.is_none() {
            let init_session = self.http_initialize(url, headers, timeout).await?;
            session_id = init_session.clone();
            if isolation == UpstreamIsolation::Shared {
                *worker.http_session_id.lock().await = init_session;
            }
        }

        let first = self
            .http_post_jsonrpc(
                url,
                headers,
                session_id.as_deref(),
                2,
                method,
                params.clone(),
                mcp_name,
                timeout,
            )
            .await;

        match first {
            Ok((val, maybe_sid)) => {
                if isolation == UpstreamIsolation::Shared {
                    if let Some(sid) = maybe_sid {
                        *worker.http_session_id.lock().await = Some(sid);
                    }
                }
                Ok(val)
            }
            Err(HttpRpcErr::SessionExpired) if isolation == UpstreamIsolation::Shared => {
                // Re-initialize once if a stateful HTTP upstream expired our session.
                let fresh_sid = self.http_initialize(url, headers, timeout).await?;
                *worker.http_session_id.lock().await = fresh_sid.clone();
                let (val, _) = self
                    .http_post_jsonrpc(
                        url,
                        headers,
                        fresh_sid.as_deref(),
                        2,
                        method,
                        params,
                        mcp_name,
                        timeout,
                    )
                    .await
                    .map_err(|e| e.into_message())?;
                Ok(val)
            }
            Err(err) => Err(err.into_message()),
        }
    }

    async fn http_initialize(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Option<String>, String> {
        let (_, session_id) = self
            .http_post_jsonrpc(
                url,
                headers,
                None,
                1,
                "initialize",
                json!({
                    "protocolVersion": MCP_CLIENT_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "muqun-gateway-hub",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
                None,
                timeout,
            )
            .await
            .map_err(|e| e.into_message())?;

        // Best-effort notifications/initialized (some servers expect it, stateless servers ignore it).
        let mut req = self
            .http
            .post(url)
            .timeout(timeout.min(Duration::from_secs(5)))
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("Mcp-Method", "notifications/initialized");
        if let Some(sid) = session_id.as_deref() {
            req = req.header("Mcp-Session-Id", sid);
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let _ = req
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .send()
            .await;

        Ok(session_id)
    }

    #[allow(clippy::too_many_arguments)]
    async fn http_post_jsonrpc(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        session_id: Option<&str>,
        id: u64,
        method: &str,
        params: Value,
        mcp_name: Option<&str>,
        timeout: Duration,
    ) -> Result<(Value, Option<String>), HttpRpcErr> {
        let mut req = self
            .http
            .post(url)
            .timeout(timeout)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("Mcp-Method", method);
        if let Some(name) = mcp_name {
            req = req.header("Mcp-Name", name);
        }
        if let Some(sid) = session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| HttpRpcErr::Other(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if status.is_redirection() {
            return Err(HttpRpcErr::Other(format!(
                "upstream HTTP redirect ({status}) refused for security"
            )));
        }
        let next_sid = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_lowercase();
        let is_sse = content_type.contains("text/event-stream");
        let text = read_bounded_http_body(resp, is_sse, id, MAX_HTTP_RESPONSE_BYTES)
            .await
            .map_err(HttpRpcErr::Other)?;

        if status == reqwest::StatusCode::NOT_FOUND && session_id.is_some() {
            return Err(HttpRpcErr::SessionExpired);
        }
        if !status.is_success() {
            if status == reqwest::StatusCode::BAD_REQUEST
                && session_id.is_some()
                && text.to_lowercase().contains("session")
            {
                return Err(HttpRpcErr::SessionExpired);
            }
            let snippet: String = text.chars().take(240).collect();
            return Err(HttpRpcErr::Other(format!(
                "upstream HTTP {status}: {snippet}"
            )));
        }

        let rpc_msg = if content_type.contains("text/event-stream") {
            parse_sse_jsonrpc(&text, id)
        } else {
            serde_json::from_str::<Value>(&text).map_err(|e| format!("invalid JSON response: {e}"))
        }
        .map_err(HttpRpcErr::Other)?;

        extract_jsonrpc_result(rpc_msg)
            .map(|val| (val, next_sid))
            .map_err(HttpRpcErr::Other)
    }
}

enum HttpRpcErr {
    SessionExpired,
    Other(String),
}

impl HttpRpcErr {
    fn into_message(self) -> String {
        match self {
            Self::SessionExpired => "upstream MCP session expired".into(),
            Self::Other(msg) => msg,
        }
    }
}

async fn stdio_rpc(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    isolation: UpstreamIsolation,
    worker: &Arc<UpstreamWorker>,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    if isolation == UpstreamIsolation::PerSession {
        let mut proc = spawn_stdio_process(command, args, env).await?;
        return stdio_send_and_read(&mut proc, method, params).await;
    }

    let mut guard = worker.stdio.lock().await;
    if guard.is_none() {
        *guard = Some(spawn_stdio_process(command, args, env).await?);
    }
    let proc = guard.as_mut().expect("stdio process initialized");
    match stdio_send_and_read(proc, method, params).await {
        Ok(val) => Ok(val),
        Err(err) => {
            // Drop broken process so the next request gets a clean respawn.
            *guard = None;
            Err(err)
        }
    }
}

async fn spawn_stdio_process(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<StdioProcess, String> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn stdio MCP '{command}': {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "stdio MCP stdin unavailable".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdio MCP stdout unavailable".to_owned())?;

    let mut proc = StdioProcess {
        _child: child,
        stdin: BufWriter::new(stdin),
        stdout: BufReader::new(stdout),
        next_id: 1,
    };

    // MCP initialize handshake
    stdio_send_and_read(
        &mut proc,
        "initialize",
        json!({
            "protocolVersion": MCP_CLIENT_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "muqun-gateway-hub",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }),
    )
    .await?;

    // Send notifications/initialized (no id, no reply expected)
    let notif = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }))
    .map_err(|e| format!("failed to encode notification: {e}"))?;
    proc.stdin
        .write_all(notif.as_bytes())
        .await
        .map_err(|e| format!("stdio write failed: {e}"))?;
    proc.stdin
        .write_all(b"\n")
        .await
        .map_err(|e| format!("stdio newline write failed: {e}"))?;
    proc.stdin
        .flush()
        .await
        .map_err(|e| format!("stdio flush failed: {e}"))?;

    Ok(proc)
}

async fn stdio_send_and_read(
    proc: &mut StdioProcess,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let id = proc.next_id;
    proc.next_id = proc.next_id.saturating_add(1);
    let line = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .map_err(|e| format!("failed to encode JSON-RPC request: {e}"))?;
    proc.stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("stdio write failed: {e}"))?;
    proc.stdin
        .write_all(b"\n")
        .await
        .map_err(|e| format!("stdio write failed: {e}"))?;
    proc.stdin
        .flush()
        .await
        .map_err(|e| format!("stdio flush failed: {e}"))?;

    let mut buf = Vec::new();
    for _ in 0..256 {
        buf.clear();
        let n = (&mut proc.stdout)
            .take((MAX_STDIO_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| format!("stdio read failed: {e}"))?;
        if n == 0 {
            return Err("stdio MCP closed stdout before answering".into());
        }
        if buf.len() > MAX_STDIO_LINE_BYTES {
            return Err("stdio MCP response exceeded 4 MiB limit".into());
        }
        let text = String::from_utf8_lossy(&buf);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            // Skip stray non-JSON log lines emitted on stdout.
            continue;
        };
        if msg.get("id").and_then(Value::as_u64) == Some(id) {
            return extract_jsonrpc_result(msg);
        }
    }
    Err("stdio MCP did not answer matching request id".into())
}

async fn read_bounded_http_body(
    mut resp: reqwest::Response,
    is_sse: bool,
    expected_id: u64,
    max_bytes: usize,
) -> Result<String, String> {
    let mut out = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("failed reading HTTP body: {e}"))?
    {
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!(
                "upstream HTTP response exceeded {} MiB limit",
                max_bytes / (1024 * 1024)
            ));
        }
        out.extend_from_slice(&chunk);
        // For long-lived SSE streams, return immediately once the matching JSON-RPC id arrives.
        if is_sse {
            if let Ok(text) = std::str::from_utf8(&out) {
                if let Ok(val) = parse_sse_jsonrpc(text, expected_id) {
                    if val.get("id").and_then(Value::as_u64) == Some(expected_id) {
                        return Ok(text.to_owned());
                    }
                }
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn parse_sse_jsonrpc(body: &str, expected_id: u64) -> Result<Value, String> {
    let mut last_json = None;
    let mut data_buf = String::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(rest.trim_start());
        } else if line.trim().is_empty() && !data_buf.is_empty() {
            if let Ok(val) = serde_json::from_str::<Value>(&data_buf) {
                if val.get("id").and_then(Value::as_u64) == Some(expected_id) {
                    return Ok(val);
                }
                last_json = Some(val);
            }
            data_buf.clear();
        }
    }
    if !data_buf.is_empty() {
        if let Ok(val) = serde_json::from_str::<Value>(&data_buf) {
            if val.get("id").and_then(Value::as_u64) == Some(expected_id) {
                return Ok(val);
            }
            last_json = Some(val);
        }
    }
    last_json.ok_or_else(|| "no JSON-RPC payload found in SSE stream".into())
}

fn extract_jsonrpc_result(msg: Value) -> Result<Value, String> {
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32000);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream JSON-RPC error");
        return Err(format!("MCP error {code}: {message}"));
    }
    msg.get("result")
        .cloned()
        .ok_or_else(|| "JSON-RPC response missing 'result'".into())
}

fn upstream_timeout(cfg: &UpstreamMcpConfig, max_cap: Option<Duration>) -> Duration {
    let secs = cfg
        .timeout_s
        .unwrap_or(DEFAULT_UPSTREAM_TIMEOUT_S)
        .clamp(MIN_UPSTREAM_TIMEOUT_S, MAX_UPSTREAM_TIMEOUT_S);
    let dur = Duration::from_secs(secs);
    match max_cap {
        Some(cap) => dur.min(cap),
        None => dur,
    }
}

fn match_score(server_id: &str, tool: &UpstreamTool, tokens: &[String]) -> u8 {
    if tokens.is_empty() {
        return 1;
    }
    let server_lc = server_id.to_lowercase();
    let name_lc = tool.name.to_lowercase();
    let desc_lc = tool.description.to_lowercase();
    let mut score: u8 = 0;
    for tok in tokens {
        if name_lc == *tok {
            score = score.saturating_add(10);
        } else if name_lc.contains(tok) {
            score = score.saturating_add(6);
        } else if server_lc.contains(tok) {
            score = score.saturating_add(4);
        } else if desc_lc.contains(tok) {
            score = score.saturating_add(2);
        } else {
            return 0;
        }
    }
    score.max(1)
}

pub(crate) fn valid_upstream_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id != "muqun"
        && (id.as_bytes()[0].is_ascii_lowercase() || id.as_bytes()[0].is_ascii_digit())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

fn http_origin(raw: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(raw.trim()).ok()?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    let port = parsed.port_or_known_default()?;
    Some(format!("{scheme}://{host}:{port}"))
}

fn is_forbidden_header(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "upgrade"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "mcp-session-id"
            | "mcp-method"
            | "mcp-name"
    )
}

fn is_blocked_metadata_host(host: &str) -> bool {
    let h = host.trim_matches(['[', ']']).to_ascii_lowercase();
    matches!(
        h.as_str(),
        "169.254.169.254"
            | "169.254.170.2"
            | "fd00:ec2::254"
            | "metadata.google.internal"
            | "metadata.goog"
    )
}

fn validate_transport(transport: &UpstreamTransport) -> Result<(), String> {
    match transport {
        UpstreamTransport::Http { url, headers } => {
            let trimmed = url.trim();
            if trimmed.is_empty() || trimmed.len() > 2048 {
                return Err("HTTP upstream url must be 1..=2048 characters".into());
            }
            let parsed = reqwest::Url::parse(trimmed)
                .map_err(|_| "HTTP upstream url is not a valid URL".to_owned())?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err("HTTP upstream url must start with http:// or https://".into());
            }
            if !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(
                    "HTTP upstream url must not embed credentials; use Authorization header".into(),
                );
            }
            let Some(host) = parsed.host_str() else {
                return Err("HTTP upstream url must include a host".into());
            };
            if is_blocked_metadata_host(host) {
                return Err("HTTP upstream host is blocked".into());
            }
            if headers.len() > 32 {
                return Err("at most 32 HTTP headers allowed".into());
            }
            for (k, v) in headers {
                if k.trim().is_empty()
                    || k.len() > 128
                    || k.contains('\r')
                    || k.contains('\n')
                    || v.len() > 4096
                    || v.contains('\r')
                    || v.contains('\n')
                {
                    return Err("invalid HTTP header name or value".into());
                }
                if is_forbidden_header(k) {
                    return Err(format!(
                        "HTTP header '{k}' is reserved and cannot be overridden"
                    ));
                }
            }
            Ok(())
        }
        UpstreamTransport::Stdio { command, args, env } => {
            let trimmed = command.trim();
            if trimmed.is_empty()
                || trimmed.len() > 1024
                || trimmed.contains('\0')
                || trimmed.contains('\n')
                || trimmed.contains('\r')
            {
                return Err("stdio command must be non-empty and at most 1024 characters".into());
            }
            if args.len() > 64 {
                return Err("at most 64 stdio args allowed".into());
            }
            if args.iter().any(|a| a.len() > 4096 || a.contains('\0')) {
                return Err("invalid stdio argument".into());
            }
            if env.len() > 64 {
                return Err("at most 64 stdio env entries allowed".into());
            }
            for (k, v) in env {
                if k.trim().is_empty()
                    || k.len() > 128
                    || k.contains('=')
                    || k.contains('\0')
                    || v.len() > 4096
                    || v.contains('\0')
                {
                    return Err("invalid stdio env entry".into());
                }
            }
            Ok(())
        }
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let lc = key.to_ascii_lowercase();
    [
        "auth",
        "token",
        "secret",
        "key",
        "pass",
        "pwd",
        "cookie",
        "cred",
        "bearer",
        "jwt",
        "session",
        "signature",
        "private",
    ]
    .iter()
    .any(|needle| lc.contains(needle))
}

fn mask_transport(transport: UpstreamTransport) -> UpstreamTransport {
    match transport {
        UpstreamTransport::Http { url, headers } => UpstreamTransport::Http {
            url,
            headers: headers
                .into_iter()
                .map(|(k, v)| {
                    if is_sensitive_key(&k) && !v.is_empty() {
                        (k, MASKED_SECRET.to_owned())
                    } else {
                        (k, v)
                    }
                })
                .collect(),
        },
        UpstreamTransport::Stdio { command, args, env } => UpstreamTransport::Stdio {
            command,
            args,
            env: env
                .into_iter()
                .map(|(k, v)| {
                    if is_sensitive_key(&k) && !v.is_empty() {
                        (k, MASKED_SECRET.to_owned())
                    } else {
                        (k, v)
                    }
                })
                .collect(),
        },
    }
}

/// Only restore masked secret placeholders when the destination origin (HTTP) or
/// executable (Stdio) is unchanged, preventing credential replay onto a new host/command.
fn preserve_masked_secrets(
    prev: Option<&UpstreamTransport>,
    next: &mut UpstreamTransport,
) -> Result<(), String> {
    match next {
        UpstreamTransport::Http {
            url: next_url,
            headers: next_headers,
        } => {
            let has_masked = next_headers.values().any(|v| v == MASKED_SECRET);
            if !has_masked {
                return Ok(());
            }
            let Some(UpstreamTransport::Http {
                url: prev_url,
                headers: prev_headers,
            }) = prev
            else {
                return Err("masked secret placeholder has no existing HTTP upstream".into());
            };
            if http_origin(prev_url).is_none() || http_origin(prev_url) != http_origin(next_url) {
                return Err(
                    "re-enter secret headers when changing the HTTP endpoint origin".into(),
                );
            }
            for (k, v) in next_headers.iter_mut() {
                if v == MASKED_SECRET {
                    let old = prev_headers
                        .iter()
                        .find(|(pk, _)| pk.eq_ignore_ascii_case(k))
                        .map(|(_, pv)| pv.clone())
                        .ok_or_else(|| format!("masked secret header '{k}' has no stored value"))?;
                    *v = old;
                }
            }
            Ok(())
        }
        UpstreamTransport::Stdio {
            command: next_cmd,
            env: next_env,
            ..
        } => {
            let has_masked = next_env.values().any(|v| v == MASKED_SECRET);
            if !has_masked {
                return Ok(());
            }
            let Some(UpstreamTransport::Stdio {
                command: prev_cmd,
                env: prev_env,
                ..
            }) = prev
            else {
                return Err("masked secret placeholder has no existing stdio upstream".into());
            };
            if prev_cmd.trim() != next_cmd.trim() {
                return Err("re-enter secret env values when changing the stdio command".into());
            }
            for (k, v) in next_env.iter_mut() {
                if v == MASKED_SECRET {
                    let old = prev_env
                        .get(k)
                        .cloned()
                        .ok_or_else(|| format!("masked secret env '{k}' has no stored value"))?;
                    *v = old;
                }
            }
            Ok(())
        }
    }
}

fn sanitize_tool_patterns(raw: Vec<String>, field: &str) -> Result<Vec<String>, String> {
    if raw.len() > 64 {
        return Err(format!("at most 64 {field} patterns allowed"));
    }
    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() > 128 || trimmed.chars().any(char::is_control) {
            return Err(format!("invalid {field} pattern '{trimmed}'"));
        }
        if !out.iter().any(|existing: &String| existing == trimmed) {
            out.push(trimmed.to_owned());
        }
    }
    Ok(out)
}

/// Wildcard glob matcher supporting `*` (0 or more chars), case-sensitive for tool names.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star_pi, mut star_ti) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub(crate) fn tool_permitted(cfg: &UpstreamMcpConfig, tool: &str) -> bool {
    if cfg.denied_tools.iter().any(|pat| glob_match(pat, tool)) {
        return false;
    }
    if !cfg.allowed_tools.is_empty() {
        return cfg.allowed_tools.iter().any(|pat| glob_match(pat, tool));
    }
    true
}

/// Parse `skill://hub/<server>/<skill>(/SKILL.md)?` or `<server>/<skill>` into `(server, skill)`.
pub(crate) fn parse_hub_skill_ref(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix("skill://hub/") {
        let mut parts = rest.split('/');
        let server = parts.next().filter(|s| !s.is_empty())?;
        let skill = parts.next().filter(|s| !s.is_empty())?;
        return Some((server, skill));
    }
    if let Some((server, skill)) = trimmed.split_once('/') {
        if !server.is_empty()
            && !skill.is_empty()
            && server != "muqun"
            && server != "user"
            && !skill.contains('/')
        {
            return Some((server, skill));
        }
    }
    None
}

fn upstream_view(cfg: UpstreamMcpConfig, st: UpstreamRuntimeState) -> UpstreamView {
    let tools = st
        .tools
        .iter()
        .take(50)
        .map(|t| UpstreamToolPreview {
            name: t.name.clone(),
            description: t.description.clone(),
        })
        .collect();
    UpstreamView {
        id: cfg.id,
        label: cfg.label,
        enabled: cfg.enabled,
        scope: cfg.scope,
        isolation: cfg.isolation,
        timeout_s: cfg
            .timeout_s
            .unwrap_or(DEFAULT_UPSTREAM_TIMEOUT_S)
            .clamp(MIN_UPSTREAM_TIMEOUT_S, MAX_UPSTREAM_TIMEOUT_S),
        allowed_tools: cfg.allowed_tools,
        denied_tools: cfg.denied_tools,
        transport: mask_transport(cfg.transport),
        healthy: st.healthy,
        tool_count: st.tools.len(),
        skill_count: st.skills.len(),
        tools,
        last_probed_ms: st.last_probed_ms,
        last_error: st.last_error,
    }
}

fn redact_value_in_place(value: &mut Value) -> bool {
    match value {
        Value::String(text) => {
            let (clean, changed) = redact_secrets(text);
            if changed {
                *text = clean;
            }
            changed
        }
        Value::Array(items) => {
            let mut any = false;
            for item in items {
                any |= redact_value_in_place(item);
            }
            any
        }
        Value::Object(map) => {
            let mut any = false;
            for (_, v) in map.iter_mut() {
                any |= redact_value_in_place(v);
            }
            any
        }
        _ => false,
    }
}

// ---- HTTP Management Routes (paired devices) --------------------------------

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/mcp/upstreams", get(list_upstreams))
        .route(
            "/api/mcp/upstreams/{id}",
            put(upsert_upstream).delete(delete_upstream),
        )
        .route("/api/mcp/upstreams/{id}/probe", post(probe_upstream))
}

async fn list_upstreams(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let upstreams = state.mcp_hub.list_views();
    Ok(Json(json!({
        "capability": CAPABILITY,
        "upstreams": upstreams,
    })))
}

async fn upsert_upstream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<UpstreamUpsertBody>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let Json(payload) = body.map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid upstream MCP payload",
        )
    })?;
    if !valid_upstream_id(&id) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_upstream_id",
            "id must be 1..=64 lowercase alphanumeric/dash/underscore characters and not 'muqun'",
        ));
    }
    if let UpstreamTransport::Http { url, .. } = &payload.transport {
        let self_url = crate::mcp::local_url(&state.config.listen);
        if http_origin(url).is_some()
            && http_origin(url) == http_origin(&self_url)
            && reqwest::Url::parse(url.trim())
                .ok()
                .is_some_and(|u| u.path().trim_end_matches('/') == crate::mcp::MCP_PATH)
        {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "self_loop_upstream",
                "cannot register this Gateway's own /mcp endpoint as an upstream",
            ));
        }
    }
    let view = state
        .mcp_hub
        .upsert(id, payload)
        .map_err(|msg| api_error(StatusCode::BAD_REQUEST, "invalid_upstream", &msg))?;
    Ok(Json(json!({ "upstream": view })))
}

async fn delete_upstream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    require_device(&state, &headers)?;
    match state.mcp_hub.delete(&id) {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(api_error(
            StatusCode::NOT_FOUND,
            "upstream_not_found",
            "Upstream MCP server not found",
        )),
        Err(msg) => Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upstream_save_failed",
            &msg,
        )),
    }
}

async fn probe_upstream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    match state.mcp_hub.probe(&id).await {
        Ok(view) => Ok(Json(json!({ "upstream": view }))),
        Err(HubCallError::NotFound(_)) => Err(api_error(
            StatusCode::NOT_FOUND,
            "upstream_not_found",
            "Upstream MCP server not found",
        )),
        Err(_) => Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "probe_failed",
            "Upstream probe failed",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bearer_headers, test_device, test_state};
    use axum::body::Body;
    use axum::http::Request;
    use axum::response::IntoResponse as _;
    use axum::routing::post as axum_post;
    use tower::ServiceExt as _;

    /// Spins up a mock Streamable HTTP MCP server returning JSON or SSE responses
    /// and enforcing an expected Authorization header.
    async fn start_mock_http_mcp(expected_auth: &'static str, use_sse: bool) -> String {
        let handler = move |headers: HeaderMap, Json(req): Json<Value>| async move {
            let auth = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if auth != expected_auth {
                return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
            }
            let method = req
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if method == "notifications/initialized" {
                return StatusCode::ACCEPTED.into_response();
            }
            let id = req.get("id").cloned().unwrap_or(json!(1));
            let resp_json = match method {
                "initialize" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock-http-mcp", "version": "1.0.0" }
                    }
                }),
                "tools/list" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [
                            {
                                "name": "create_issue",
                                "description": "Create a GitHub issue in a repository",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": { "title": { "type": "string" } },
                                    "required": ["title"]
                                }
                            },
                            {
                                "name": "leak_secret",
                                "description": "Returns a string containing a secret token",
                                "inputSchema": { "type": "object" }
                            }
                        ]
                    }
                }),
                "skills/list" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "skills": [
                            {
                                "uri": "skill://github/triage/SKILL.md",
                                "frontmatter": {
                                    "name": "triage",
                                    "description": "Triage GitHub issues"
                                }
                            }
                        ]
                    }
                }),
                "resources/read" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "contents": [{
                            "uri": "skill://github/triage/SKILL.md",
                            "mimeType": "text/markdown",
                            "text": "# Triage Playbook\nCheck labels; sample ghp_1234567890abcdefghijklmnop1234567890"
                        }]
                    }
                }),
                "tools/call" => {
                    let tool = req["params"]["name"].as_str().unwrap_or_default();
                    if tool == "leak_secret" {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "token=ghp_1234567890abcdefghijklmnop1234567890"
                                }],
                                "isError": false
                            }
                        })
                    } else {
                        let title = req["params"]["arguments"]["title"]
                            .as_str()
                            .unwrap_or("untitled");
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": format!("created:{title}")
                                }],
                                "isError": false
                            }
                        })
                    }
                }
                _ => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" }
                }),
            };

            if use_sse {
                let body = format!("event: message\ndata: {}\n\n", resp_json);
                (
                    StatusCode::OK,
                    [
                        ("content-type", "text/event-stream"),
                        ("mcp-session-id", "sess-sse-1"),
                    ],
                    body,
                )
                    .into_response()
            } else {
                (
                    StatusCode::OK,
                    [
                        ("content-type", "application/json"),
                        ("mcp-session-id", "sess-json-1"),
                    ],
                    resp_json.to_string(),
                )
                    .into_response()
            }
        };

        let app = Router::new().route("/mcp", axum_post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/mcp")
    }

    #[tokio::test]
    async fn http_upstream_search_call_redact_and_scope_enforcement() {
        let url = start_mock_http_mcp("Bearer super-secret-upstream", true).await;
        let hub = McpHub::new(Vec::new(), None);
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".into(),
            "Bearer super-secret-upstream".into(),
        );
        let view = hub
            .upsert(
                "github".into(),
                UpstreamUpsertBody {
                    label: Some("GitHub".into()),
                    enabled: true,
                    scope: UpstreamScope::LocalOnly,
                    isolation: UpstreamIsolation::Shared,
                    timeout_s: Some(15),
                    allowed_tools: Vec::new(),
                    denied_tools: Vec::new(),
                    transport: UpstreamTransport::Http { url, headers },
                },
            )
            .unwrap();
        // Secret header is masked in view.
        match &view.transport {
            UpstreamTransport::Http { headers, .. } => {
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some(MASKED_SECRET)
                );
            }
            _ => panic!("expected http"),
        }

        // Add a broken upstream to verify fault isolation.
        hub.upsert(
            "broken".into(),
            UpstreamUpsertBody {
                label: None,
                enabled: true,
                scope: UpstreamScope::All,
                isolation: UpstreamIsolation::Shared,
                timeout_s: Some(2),
                allowed_tools: Vec::new(),
                denied_tools: Vec::new(),
                transport: UpstreamTransport::Http {
                    url: "http://127.0.0.1:1/dead".into(),
                    headers: BTreeMap::new(),
                },
            },
        )
        .unwrap();

        // Tool search (summary mode) lazy-probes both; broken fails cleanly while github succeeds.
        let summary = hub
            .search_tools(CallerScope::LocalOwner, Some("issue"), None, false, 10)
            .await;
        let tools = summary["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "{summary}");
        assert_eq!(tools[0]["server"], "github");
        assert_eq!(tools[0]["name"], "create_issue");
        assert!(tools[0].get("inputSchema").is_none());

        // Tool search (schema mode) includes inputSchema.
        let with_schema = hub
            .search_tools(
                CallerScope::LocalOwner,
                Some("issue"),
                Some("github"),
                true,
                10,
            )
            .await;
        assert_eq!(
            with_schema["tools"][0]["inputSchema"]["required"],
            json!(["title"])
        );

        // Upstream skills were discovered and can be read via resources/read (redacted).
        let skills = hub.cached_skills(CallerScope::LocalOwner);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "triage");
        let skill_doc = hub
            .read_upstream_skill(CallerScope::LocalOwner, "github", "triage")
            .await
            .unwrap();
        assert_eq!(skill_doc["uri"], "skill://hub/github/triage/SKILL.md");
        let skill_text = skill_doc["content"].as_str().unwrap();
        assert!(skill_text.contains("# Triage Playbook"), "{skill_doc}");
        assert!(!skill_text.contains("ghp_1234567890"), "{skill_doc}");
        assert!(skill_text.contains("[REDACTED]"), "{skill_doc}");
        assert!(matches!(
            hub.read_upstream_skill(CallerScope::LocalTask, "github", "triage")
                .await,
            Err(HubCallError::ScopeDenied { .. })
        ));

        // LocalOwner can call create_issue.
        let called = hub
            .call_tool(
                CallerScope::LocalOwner,
                "github",
                "create_issue",
                json!({ "title": "Fix bug" }),
            )
            .await
            .unwrap();
        assert_eq!(called["result"]["content"][0]["text"], "created:Fix bug");

        // Secrets returned by upstream tool are redacted!
        let leaked = hub
            .call_tool(CallerScope::LocalOwner, "github", "leak_secret", json!({}))
            .await
            .unwrap();
        let text = leaked["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains("ghp_1234567890"), "{leaked}");
        assert!(text.contains("[REDACTED]"), "{leaked}");
        assert_eq!(leaked["redacted"], true);

        // LocalTask is blocked from LocalOnly upstream (scope_denied) and cannot even see it in search!
        let task_search = hub
            .search_tools(CallerScope::LocalTask, Some("issue"), None, false, 10)
            .await;
        assert!(task_search["tools"].as_array().unwrap().is_empty());
        let denied = hub
            .call_tool(
                CallerScope::LocalTask,
                "github",
                "create_issue",
                json!({ "title": "Sneaky" }),
            )
            .await;
        assert!(matches!(denied, Err(HubCallError::ScopeDenied { .. })));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_upstream_roundtrip_and_isolation() {
        let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}}}}\n' "$id"
      ;;
    *'"tools/list"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"ping_stdio","description":"Echo from stdio","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"tools/call"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong-from-stdio"}],"isError":false}}\n' "$id"
      ;;
    *'"skills/list"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"not supported"}}\n' "$id"
      ;;
  esac
done
"#;
        let hub = McpHub::new(Vec::new(), None);
        hub.upsert(
            "local_stdio".into(),
            UpstreamUpsertBody {
                label: Some("Local Stdio".into()),
                enabled: true,
                scope: UpstreamScope::Tasks,
                isolation: UpstreamIsolation::Shared,
                timeout_s: Some(10),
                allowed_tools: Vec::new(),
                denied_tools: Vec::new(),
                transport: UpstreamTransport::Stdio {
                    command: "sh".into(),
                    args: vec!["-c".into(), script.into()],
                    env: BTreeMap::new(),
                },
            },
        )
        .unwrap();

        let probed = hub.probe("local_stdio").await.unwrap();
        assert!(probed.healthy, "{:?}", probed.last_error);
        assert_eq!(probed.tool_count, 1);
        assert_eq!(probed.tools[0].name, "ping_stdio");

        // Tasks scope permits LocalTask, denies RemoteOrRestricted.
        let res = hub
            .call_tool(
                CallerScope::LocalTask,
                "local_stdio",
                "ping_stdio",
                json!({}),
            )
            .await
            .unwrap();
        assert_eq!(res["result"]["content"][0]["text"], "pong-from-stdio");

        let denied = hub
            .call_tool(
                CallerScope::RemoteOrRestricted,
                "local_stdio",
                "ping_stdio",
                json!({}),
            )
            .await;
        assert!(matches!(denied, Err(HubCallError::ScopeDenied { .. })));
    }

    #[tokio::test]
    async fn rest_api_crud_probe_and_secret_preservation() {
        let url = start_mock_http_mcp("Bearer api-secret-99", false).await;
        let state = test_state("admin", vec![test_device("d1", "device-tok")]);
        let app = mount(Router::new()).with_state(state.clone());

        // PUT creates upstream
        let req = Request::builder()
            .method("PUT")
            .uri("/api/mcp/upstreams/my_hub")
            .header("content-type", "application/json")
            .header("authorization", "Bearer device-tok")
            .body(Body::from(
                json!({
                    "label": "My Hub",
                    "enabled": true,
                    "scope": "local_only",
                    "isolation": "shared",
                    "transport": {
                        "kind": "http",
                        "url": url,
                        "headers": { "Authorization": "Bearer api-secret-99" }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["upstream"]["transport"]["headers"]["Authorization"],
            MASKED_SECRET
        );

        // PUT again with MASKED_SECRET preserves the real token so probe still authenticates!
        let req = Request::builder()
            .method("PUT")
            .uri("/api/mcp/upstreams/my_hub")
            .header("content-type", "application/json")
            .header("authorization", "Bearer device-tok")
            .body(Body::from(
                json!({
                    "label": "My Hub Renamed",
                    "enabled": true,
                    "scope": "tasks",
                    "isolation": "shared",
                    "transport": {
                        "kind": "http",
                        "url": url,
                        "headers": { "Authorization": MASKED_SECRET }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // POST probe succeeds with the preserved real token!
        let req = Request::builder()
            .method("POST")
            .uri("/api/mcp/upstreams/my_hub/probe")
            .header("authorization", "Bearer device-tok")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let probed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(probed["upstream"]["healthy"], true, "{probed}");
        assert_eq!(probed["upstream"]["tool_count"], 2, "{probed}");

        // GET lists upstream with capability
        let mut get_req = Request::builder()
            .method("GET")
            .uri("/api/mcp/upstreams")
            .body(Body::empty())
            .unwrap();
        get_req.headers_mut().extend(bearer_headers("device-tok"));
        let resp = app.clone().oneshot(get_req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let listed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(listed["capability"], CAPABILITY);
        assert_eq!(listed["upstreams"].as_array().unwrap().len(), 1);

        // DELETE removes upstream
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/mcp/upstreams/my_hub")
            .header("authorization", "Bearer device-tok")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(state.mcp_hub.list_views().is_empty());
    }

    #[tokio::test]
    async fn security_blocks_redirects_secret_replay_oversized_bodies_and_metadata_urls() {
        // 1. Metadata IP, embedded credentials, and reserved headers are rejected.
        let hub = McpHub::new(Vec::new(), None);
        for bad_url in [
            "http://169.254.169.254/latest/meta-data",
            "http://metadata.google.internal/computeMetadata/v1",
            "https://user:pass@example.com/mcp",
        ] {
            let err = hub
                .upsert(
                    "bad_url".into(),
                    UpstreamUpsertBody {
                        label: None,
                        enabled: true,
                        scope: UpstreamScope::LocalOnly,
                        isolation: UpstreamIsolation::Shared,
                        timeout_s: Some(5),
                        allowed_tools: Vec::new(),
                        denied_tools: Vec::new(),
                        transport: UpstreamTransport::Http {
                            url: bad_url.into(),
                            headers: BTreeMap::new(),
                        },
                    },
                )
                .unwrap_err();
            assert!(!err.is_empty(), "expected error for {bad_url}");
        }

        let mut bad_headers = BTreeMap::new();
        bad_headers.insert("Host".into(), "evil.example".into());
        assert!(hub
            .upsert(
                "bad_hdr".into(),
                UpstreamUpsertBody {
                    label: None,
                    enabled: true,
                    scope: UpstreamScope::LocalOnly,
                    isolation: UpstreamIsolation::Shared,
                    timeout_s: Some(5),
                    allowed_tools: Vec::new(),
                    denied_tools: Vec::new(),
                    transport: UpstreamTransport::Http {
                        url: "https://example.com/mcp".into(),
                        headers: bad_headers,
                    },
                },
            )
            .is_err());

        // 2. Masked secret cannot be replayed onto a different HTTP origin or Stdio command.
        let mut h = BTreeMap::new();
        h.insert("Authorization".into(), "Bearer real-token-123".into());
        hub.upsert(
            "sec".into(),
            UpstreamUpsertBody {
                label: None,
                enabled: true,
                scope: UpstreamScope::LocalOnly,
                isolation: UpstreamIsolation::Shared,
                timeout_s: Some(5),
                allowed_tools: Vec::new(),
                denied_tools: Vec::new(),
                transport: UpstreamTransport::Http {
                    url: "https://trusted.example.com/mcp".into(),
                    headers: h,
                },
            },
        )
        .unwrap();
        let mut replay_h = BTreeMap::new();
        replay_h.insert("Authorization".into(), MASKED_SECRET.into());
        let replay_err = hub
            .upsert(
                "sec".into(),
                UpstreamUpsertBody {
                    label: None,
                    enabled: true,
                    scope: UpstreamScope::LocalOnly,
                    isolation: UpstreamIsolation::Shared,
                    timeout_s: Some(5),
                    allowed_tools: Vec::new(),
                    denied_tools: Vec::new(),
                    transport: UpstreamTransport::Http {
                        url: "https://attacker.example.com/mcp".into(),
                        headers: replay_h,
                    },
                },
            )
            .unwrap_err();
        assert!(replay_err.contains("origin"), "{replay_err}");

        // 3. HTTP redirect (302) is refused and never forwards Authorization to redirect target.
        let leaked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let leaked_flag = leaked.clone();
        let sink_app = Router::new().route(
            "/steal",
            axum_post(move || {
                let flag = leaked_flag.clone();
                async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );
        let sink_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sink_addr = sink_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(sink_listener, sink_app).await.unwrap();
        });
        let redirect_target = format!("http://{sink_addr}/steal");
        let redir_app = Router::new().route(
            "/mcp",
            axum_post(move || {
                let loc = redirect_target.clone();
                async move { (StatusCode::FOUND, [("location", loc)]).into_response() }
            }),
        );
        let redir_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redir_addr = redir_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(redir_listener, redir_app).await.unwrap();
        });

        let mut redir_h = BTreeMap::new();
        redir_h.insert("Authorization".into(), "Bearer do-not-leak".into());
        hub.upsert(
            "redir".into(),
            UpstreamUpsertBody {
                label: None,
                enabled: true,
                scope: UpstreamScope::LocalOnly,
                isolation: UpstreamIsolation::Shared,
                timeout_s: Some(5),
                allowed_tools: Vec::new(),
                denied_tools: Vec::new(),
                transport: UpstreamTransport::Http {
                    url: format!("http://{redir_addr}/mcp"),
                    headers: redir_h,
                },
            },
        )
        .unwrap();
        let probed = hub.probe("redir").await.unwrap();
        assert!(!probed.healthy);
        assert!(
            probed
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("redirect"),
            "{:?}",
            probed.last_error
        );
        assert!(
            !leaked.load(std::sync::atomic::Ordering::SeqCst),
            "redirect target must never be contacted"
        );

        // 4. Oversized HTTP response (>4 MiB) is rejected cleanly without OOM.
        let big_app = Router::new().route(
            "/mcp",
            axum_post(|| async { "x".repeat(MAX_HTTP_RESPONSE_BYTES + 1024) }),
        );
        let big_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let big_addr = big_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(big_listener, big_app).await.unwrap();
        });
        hub.upsert(
            "big".into(),
            UpstreamUpsertBody {
                label: None,
                enabled: true,
                scope: UpstreamScope::LocalOnly,
                isolation: UpstreamIsolation::Shared,
                timeout_s: Some(5),
                allowed_tools: Vec::new(),
                denied_tools: Vec::new(),
                transport: UpstreamTransport::Http {
                    url: format!("http://{big_addr}/mcp"),
                    headers: BTreeMap::new(),
                },
            },
        )
        .unwrap();
        let big_probe = hub.probe("big").await.unwrap();
        assert!(!big_probe.healthy);
        assert!(
            big_probe
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("limit"),
            "{:?}",
            big_probe.last_error
        );
    }

    #[tokio::test]
    async fn shared_agent_fails_closed_when_peer_task_is_active_on_that_agent() {
        use crate::tasks::domain::{
            PrincipalKind, Task, TaskBinding, TaskId, TaskKind, TaskOrigin, TaskPrincipal,
            TaskState,
        };
        let state = test_state("admin", Vec::new());
        let now = now_ms();
        state
            .tasks
            .insert_for_test(&Task {
                id: TaskId::new(),
                kind: TaskKind::Delegation,
                goal: "remote peer work".into(),
                cwd: None,
                binding: TaskBinding::AgentSession {
                    agent_id: "claude".into(),
                    asid: "ses_peer_1".into(),
                },
                state: TaskState::Working,
                summary: None,
                exit_code: None,
                created_by: TaskPrincipal {
                    kind: PrincipalKind::Gateway,
                    id: "peer-gw-1".into(),
                },
                created_ms: now,
                updated_ms: now,
                seq: 0,
                parent: None,
                origin: TaskOrigin::Peer,
                label: None,
                requester_name: Some("mini".into()),
                session_title: None,
                mentions: Vec::new(),
                client_ref: None,
                requester_server_id: Some("srv-mini".into()),
                reopened_ms: None,
                reopen_count: 0,
            })
            .await;

        let shared_claude = AssistantSession {
            token_id: "tok-claude".into(),
            binding: TokenBinding::Agent {
                agent_id: "claude".into(),
            },
            label: "claude".into(),
            expires_ms: None,
        };
        let scope = resolve_caller_scope(&state, &shared_claude, None).await;
        assert_eq!(scope, CallerScope::RemoteOrRestricted);
        assert!(!scope.permits(UpstreamScope::LocalOnly));
        assert!(!scope.permits(UpstreamScope::Tasks));
        assert!(scope.permits(UpstreamScope::All));
    }

    #[tokio::test]
    async fn tool_level_acl_hides_and_blocks_denied_or_non_allowed_tools() {
        let url = start_mock_http_mcp("Bearer acl-tok", false).await;
        let hub = McpHub::new(Vec::new(), None);
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".into(), "Bearer acl-tok".into());
        hub.upsert(
            "gh_acl".into(),
            UpstreamUpsertBody {
                label: Some("GitHub ACL".into()),
                enabled: true,
                scope: UpstreamScope::LocalOnly,
                isolation: UpstreamIsolation::Shared,
                timeout_s: Some(10),
                allowed_tools: vec!["create_*".into(), "leak_*".into()],
                denied_tools: vec!["leak_*".into()],
                transport: UpstreamTransport::Http { url, headers },
            },
        )
        .unwrap();

        let search = hub
            .search_tools(CallerScope::LocalOwner, None, Some("gh_acl"), false, 20)
            .await;
        let names: Vec<&str> = search["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["create_issue"]);

        // Denied tool cannot be called directly even though allowed_tools also matched it.
        let blocked = hub
            .call_tool(CallerScope::LocalOwner, "gh_acl", "leak_secret", json!({}))
            .await;
        assert!(matches!(blocked, Err(HubCallError::ToolDenied { .. })));

        // Allowed non-denied tool succeeds.
        let ok = hub
            .call_tool(
                CallerScope::LocalOwner,
                "gh_acl",
                "create_issue",
                json!({ "title": "ACL ok" }),
            )
            .await
            .unwrap();
        assert_eq!(ok["result"]["content"][0]["text"], "created:ACL ok");
    }
}
