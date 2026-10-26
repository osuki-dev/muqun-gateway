use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::client::{
    DeepseekClient, HistoryWalk, INITIAL_HISTORY_RETAINED_BYTES, MAX_HISTORY_RETAINED_BYTES,
};
use super::endpoint::DeepseekEndpoint;
use super::mapper::{self, ToolResult};
use super::stream::DeepseekInteractions;
use crate::agents::domain::{
    AgentCatalog, AgentProject, AgentSessionId, AgentSessionInfo, AgentSessionStatus, FormRequest,
    ModelRef, PermissionDecision, PermissionRequest, SessionQuery, TimelineItem,
};
use crate::agents::ports::agent::{
    AgentError, AgentFuture, AgentPort, AttachmentMode, FileDiffItem,
};

/// The most `load_earlier_timeline` maps in one batch, as the route documents.
const MAX_EARLIER_ITEMS: usize = 2000;
/// How many sessions keep a history cursor; the oldest is forgotten past it.
const MAX_HISTORY_CURSORS: usize = 256;

/// Where a session's history reads stopped: `load_earlier_timeline` reads
/// the page just below `before_seq`, under the same frozen `through_seq`
/// the snapshot was read under (`DeepseekClient::read_history`).
#[derive(Clone, Debug)]
struct HistoryCursor {
    through_seq: i64,
    /// The oldest seq delivered so far; the next walk's exclusive bound.
    before_seq: i64,
    has_more: bool,
    /// `tool/result` records whose calls lie below `before_seq`
    /// (`mapper::map_timeline_records_carrying`).
    carried: Vec<(ToolResult, u64)>,
}

#[derive(Default)]
struct HistoryCursors {
    by_session: HashMap<String, HistoryCursor>,
    order: VecDeque<String>,
}

impl HistoryCursors {
    fn get(&self, session_id: &str) -> Option<HistoryCursor> {
        self.by_session.get(session_id).cloned()
    }

    /// Keep `cursor` for `session_id`. A snapshot read that stops above a
    /// cursor already held (the end-of-run refetch of the newest twenty
    /// messages, a refresh after earlier pages were loaded) does not move
    /// that cursor back up: the mirror holds the rows between, and
    /// `load_earlier_timeline` continues below what was delivered.
    fn keep(&mut self, session_id: &str, cursor: HistoryCursor, deeper_only: bool) {
        if deeper_only
            && self
                .by_session
                .get(session_id)
                .is_some_and(|held| held.before_seq <= cursor.before_seq)
        {
            return;
        }
        if !self.by_session.contains_key(session_id) {
            self.order.push_back(session_id.to_owned());
            while self.order.len() > MAX_HISTORY_CURSORS {
                if let Some(oldest) = self.order.pop_front() {
                    self.by_session.remove(&oldest);
                }
            }
        }
        self.by_session.insert(session_id.to_owned(), cursor);
    }
}

pub struct DeepseekDriver {
    pub client: Arc<DeepseekClient>,
    /// Serializes history walks, so concurrent `load_earlier_timeline`
    /// calls consume successive disjoint pages.
    history: tokio::sync::Mutex<()>,
    cursors: std::sync::Mutex<HistoryCursors>,
    skills_available: AtomicBool,
    commands_available: AtomicBool,
}

impl DeepseekDriver {
    pub fn new(endpoint: DeepseekEndpoint) -> Self {
        Self {
            client: Arc::new(DeepseekClient::new(endpoint)),
            history: tokio::sync::Mutex::new(()),
            cursors: std::sync::Mutex::new(HistoryCursors::default()),
            skills_available: AtomicBool::new(false),
            commands_available: AtomicBool::new(false),
        }
    }

    pub fn client(&self) -> &DeepseekClient {
        &self.client
    }

    #[cfg(test)]
    pub(crate) fn with_client(client: DeepseekClient) -> Self {
        Self {
            client: Arc::new(client),
            history: tokio::sync::Mutex::new(()),
            cursors: std::sync::Mutex::new(HistoryCursors::default()),
            skills_available: AtomicBool::new(false),
            commands_available: AtomicBool::new(false),
        }
    }

    /// Map `walk` for `session_id` and remember where it stopped.
    fn keep_history(
        &self,
        session_id: &str,
        walk: &HistoryWalk,
        mut carried: Vec<(ToolResult, u64)>,
        deeper_only: bool,
    ) -> Vec<TimelineItem> {
        let items = mapper::map_timeline_records_carrying(&walk.page, &mut carried);
        let mut cursors = self
            .cursors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before_seq = walk.earliest_seq.unwrap_or(walk.through_seq + 1);
        cursors.keep(
            session_id,
            HistoryCursor {
                through_seq: walk.through_seq,
                before_seq,
                has_more: walk.has_more,
                carried,
            },
            deeper_only,
        );
        items
    }
}

impl AgentPort for DeepseekDriver {
    fn skills_listed(&self) -> bool {
        self.skills_available.load(Ordering::Relaxed)
    }
    fn slash_commands_listed(&self) -> bool {
        self.commands_available.load(Ordering::Relaxed)
    }

    fn get_catalog_for_session<'a>(
        &'a self,
        session: Option<&'a str>,
        directory: Option<&'a str>,
    ) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move {
            let mut catalog = self.get_catalog(directory).await?;
            let Some(session) = session else {
                return Ok(catalog);
            };
            let info = self.get_session(session).await?;
            if directory.is_some_and(|directory| info.directory.as_deref() != Some(directory)) {
                return Err(AgentError::InvalidRequest(
                    "catalog directory differs from the DeepSeek session".into(),
                ));
            }
            let (skills, commands) =
                tokio::join!(self.client.skills(session), self.client.commands(session));
            // A preset-specific failure empties this catalog; it cannot revoke
            // an instance method already observed in another session.
            if let Ok(skills) = skills.and_then(|raw| super::catalog::skills(&raw)) {
                catalog.skills = skills;
                self.skills_available.store(true, Ordering::Relaxed);
            }
            if let Ok(commands) = commands.and_then(|raw| super::catalog::commands(&raw)) {
                catalog.commands = commands;
                self.commands_available.store(true, Ordering::Relaxed);
            }
            Ok(catalog)
        })
    }

    fn activate_skill<'a>(
        &'a self,
        session: &'a str,
        name: &'a str,
        resume: Option<bool>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if resume == Some(false) {
                return Err(AgentError::Unsupported(
                    "DeepSeek deferred skill activation".into(),
                ));
            }
            let skills = super::catalog::skills(&self.client.skills(session).await?)?;
            if !skills.iter().any(|skill| skill.id == name) {
                return Err(AgentError::InvalidRequest(
                    "DeepSeek skill is no longer available in this session".into(),
                ));
            }
            self.client
                .send_prompt(session, &format!("/{name}"), &[], Some("queue"))
                .await?;
            Ok(())
        })
    }

    fn execute_command<'a>(
        &'a self,
        session: &'a str,
        name: &'a str,
        arguments: &'a str,
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, Value> {
        Box::pin(async move {
            if !super::catalog::command_name(name)
                || delivery.is_some_and(|mode| !matches!(mode, "queue" | "steer"))
            {
                return Err(AgentError::InvalidRequest(
                    "invalid DeepSeek command invocation".into(),
                ));
            }
            let line = if arguments.is_empty() {
                format!("/{name}")
            } else {
                format!("/{name} {arguments}")
            };
            super::catalog::execution(&self.client.execute_command(session, &line).await?)
        })
    }

    fn run_command<'a>(
        &'a self,
        session: &'a str,
        name: &'a str,
        arguments: &'a str,
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let result = self
                .execute_command(session, name, arguments, delivery)
                .await?;
            if result["command_result"]["kind"] == "error" {
                return Err(AgentError::RequestFailed(
                    result["command_result"]["text"]
                        .as_str()
                        .unwrap_or("command failed")
                        .into(),
                ));
            }
            Ok(())
        })
    }

    fn get_inbox<'a>(&'a self, session: &'a str) -> AgentFuture<'a, Vec<Value>> {
        Box::pin(async move {
            super::inbox::items(session, &self.client.get_projections(session).await?)
        })
    }

    fn cancel_inbox_item<'a>(&'a self, session: &'a str, item: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            self.client
                .update_queue(session, item, serde_json::json!({"kind":"remove"}))
                .await
        })
    }

    fn edit_inbox_item<'a>(
        &'a self,
        session: &'a str,
        item: &'a str,
        text: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if text.trim().is_empty() {
                return Err(AgentError::InvalidRequest(
                    "queue text must not be empty".into(),
                ));
            }
            self.client
                .update_queue(
                    session,
                    item,
                    serde_json::json!({"kind":"edit","content":[{"type":"text","text":text}]}),
                )
                .await
        })
    }

    fn set_inbox_delivery<'a>(
        &'a self,
        session: &'a str,
        item: &'a str,
        delivery: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            // The native mutation is a strict promotion. There is no demotion API.
            if delivery != "steer" {
                return Err(AgentError::Unsupported("DeepSeek inbox demotion".into()));
            }
            self.client
                .update_queue(session, item, serde_json::json!({"kind":"steer"}))
                .await
        })
    }

    fn adds_mcp_servers(&self) -> bool {
        true
    }

    fn mcp_server_connected<'a>(
        &'a self,
        name: &'a str,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, bool> {
        Box::pin(async move {
            if name != "muqun" {
                return Ok(false);
            }
            super::mcp::connected(&self.client).await
        })
    }

    fn supports_earlier_timeline(&self) -> bool {
        true
    }

    fn kind(&self) -> &'static str {
        "deepseek"
    }

    fn probe(&self) -> AgentFuture<'_, bool> {
        Box::pin(async move {
            let Ok(req_client) = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(60))
                .build()
            else {
                return Ok(false);
            };
            Ok(self.client.endpoint.probe_healthy(&req_client).await)
        })
    }

    fn list_projects(&self) -> AgentFuture<'_, Vec<AgentProject>> {
        Box::pin(async move {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string());

            let name = std::path::Path::new(&cwd)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "workspace".to_string());

            Ok(vec![AgentProject {
                id: "default".to_string(),
                canonical: cwd,
                name,
                vcs: Some("git".to_string()),
                sandboxes: vec![],
                missing: false,
            }])
        })
    }

    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> AgentFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            let raw_sessions = self.client.list_sessions(query).await?;
            // The query is applied here, as the T3 adapter applies it:
            // `session/list` takes none of it.
            let mut sessions: Vec<AgentSessionInfo> = raw_sessions
                .iter()
                .filter_map(mapper::map_session)
                .filter(|s| query.keeps_parent(s.parent_id.as_deref()))
                .collect();
            if let Some(dir) = query.directory.as_deref().map(|d| d.trim_end_matches('/')) {
                sessions.retain(|s| {
                    s.directory
                        .as_deref()
                        .is_some_and(|d| d.trim_end_matches('/') == dir)
                });
            }
            if let Some(needle) = query
                .search
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_lowercase)
            {
                sessions.retain(|s| s.title.to_lowercase().contains(&needle));
            }
            sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
            if query.order.as_deref() == Some("asc") {
                sessions.reverse();
            }
            if let Some(limit) = query.limit {
                sessions.truncate(limit);
            }
            Ok(sessions)
        })
    }

    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        model: Option<&'a ModelRef>,
        mode: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let session_id = self.client.create_session(directory, model, mode).await?;
            let session_id = session_id.as_str();

            let selected_model = if let Some(m) = model {
                Some(self
                    .client
                    .select_model(
                        session_id,
                        &m.provider_id,
                        &m.model_id,
                        m.variant.as_deref(),
                    )
                    .await
                    .map_err(|e| AgentError::RequestFailed(format!(
                        "session {} was created, but model selection failed: {e}; do not retry creation",
                        session_id.chars().take(128).collect::<String>()
                    )))?)
            } else {
                None
            };

            Ok(AgentSessionInfo {
                asid: AgentSessionId(session_id.to_string()),
                agent_id: String::new(),
                backend_session_id: session_id.to_string(),
                runtime: None,
                context_usage: None,
                git_info: None,
                agent_name: None,
                agent_role: None,
                title: "New DeepSeek Session".to_string(),
                mode: mode.map(str::to_string),
                model: selected_model,
                status: AgentSessionStatus::Idle,
                directory: directory.map(str::to_string),
                cost: None,
                tokens: None,
                limit: None,
                parent_id: None,
                project_id: None,
                outcome: None,
                error: None,
                revert: None,
                fork: None,
                time_idle: None,
                time_viewed: None,
                deleted: false,
                updated_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            })
        })
    }

    fn get_session<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            // Projections are default-valued even for an unknown id. Only the
            // authoritative inventory can establish ownership across agents.
            let summaries = self.client.list_sessions(&SessionQuery::default()).await?;
            let mut summary = summaries
                .into_iter()
                .find(|item| {
                    item.get("sessionId").and_then(serde_json::Value::as_str) == Some(session_id)
                })
                .ok_or_else(|| AgentError::SessionNotFound(session_id.to_string()))?;
            match self.client.get_projections(session_id).await {
                Ok(projections) => summary["projections"] = projections,
                Err(e @ AgentError::SessionNotFound(_)) => return Err(e),
                Err(e) => {
                    tracing::debug!(target: "deepseek", "projections failed; using session summary: {e}");
                }
            }
            mapper::map_session(&summary)
                .ok_or_else(|| AgentError::Protocol("session summary is unreadable".to_string()))
        })
    }

    /// DeepSeek's prompt has text parts only, but its tools read files.
    fn attachment_mode(&self) -> AttachmentMode {
        AttachmentMode::ByPath
    }

    fn send_prompt<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            self.client
                .send_prompt(session_id, text, attachments, delivery)
                .await?;
            Ok(())
        })
    }

    fn revert_session<'a>(
        &'a self,
        _session_id: &'a str,
        _message_id: &'a str,
    ) -> AgentFuture<'a, ()> {
        // DeepSeek Harness has no revert RPC (it forks instead).
        Box::pin(async { Err(AgentError::Unsupported("revert_session".into())) })
    }

    fn interrupt<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            self.client.cancel(session_id).await?;
            Ok(())
        })
    }

    fn switch_model<'a>(&'a self, session_id: &'a str, model: &'a ModelRef) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            self.client
                .select_model(
                    session_id,
                    &model.provider_id,
                    &model.model_id,
                    model.variant.as_deref(),
                )
                .await?;
            Ok(())
        })
    }

    fn switch_mode<'a>(&'a self, _session_id: &'a str, _mode: &'a str) -> AgentFuture<'a, ()> {
        // Agent presets are fixed at session creation; no switch RPC exists.
        Box::pin(async { Err(AgentError::Unsupported("switch_mode".into())) })
    }

    fn find_files<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let directory = directory.filter(|cwd| !cwd.is_empty()).ok_or_else(|| {
                AgentError::RequestFailed("file search requires a session working directory".into())
            })?;
            let query_path = query.replace('\\', "/");
            if query_path.starts_with('/') || query_path.split('/').any(|part| part == "..") {
                return Err(AgentError::RequestFailed(
                    "file search query must stay inside the workspace".into(),
                ));
            }
            if limit == 0 {
                return Ok(Vec::new());
            }
            let sessions = self.client.list_sessions(&SessionQuery::default()).await?;
            let mut scoped = sessions.iter().filter(|session| {
                session.get("cwd").and_then(Value::as_str) == Some(directory)
                    && session.get("agentAvailable").and_then(Value::as_bool) == Some(true)
            });
            let session_id = scoped
                .next()
                .and_then(|session| session.get("sessionId"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    AgentError::NotAvailable("no live DeepSeek session owns this directory".into())
                })?;
            if scoped.next().is_some() {
                return Err(AgentError::RequestFailed(
                    "file search directory has multiple live DeepSeek sessions".into(),
                ));
            }
            let res = self.client.file_references(session_id, query).await?;
            let candidates = res.as_array().ok_or_else(|| {
                AgentError::Protocol("file references response lacks candidates".into())
            })?;
            candidates
                .iter()
                .take(limit)
                .map(|candidate| {
                    let path = candidate
                        .get("path")
                        .and_then(Value::as_str)
                        .filter(|path| {
                            !path.is_empty()
                                && !path.starts_with('/')
                                && !path.starts_with('\\')
                                && !path.replace('\\', "/").split('/').any(|part| part == "..")
                        })
                        .ok_or_else(|| {
                            AgentError::Protocol(
                                "file reference has an invalid workspace path".into(),
                            )
                        })?;
                    let kind = candidate
                        .get("kind")
                        .and_then(Value::as_str)
                        .filter(|kind| matches!(*kind, "file" | "directory"))
                        .ok_or_else(|| {
                            AgentError::Protocol("file reference has an invalid kind".into())
                        })?;
                    Ok(serde_json::json!({ "path": path, "type": kind }))
                })
                .collect()
        })
    }

    /// An approval is a forwarded `approval/request` waterfall, settled with
    /// the `$events/result` RPC against the `clientId` of the live `$events`
    /// stream (see `stream.rs`). The pending request and that id come from
    /// the per-endpoint registry the stream listener fills.
    fn reply_permission<'a>(
        &'a self,
        session_id: &'a str,
        request_id: &'a str,
        decision: PermissionDecision,
        message: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            // The approval reply carries no text: a reason the user wrote
            // would be dropped on the floor, so it is refused instead.
            if message.is_some_and(|value| !value.trim().is_empty()) {
                return Err(AgentError::Unsupported("approval reason".into()));
            }
            let interactions = DeepseekInteractions::for_endpoint(&self.client.endpoint.url);
            let Some(pending) = interactions.approval(request_id) else {
                return Err(AgentError::SessionNotFound(
                    "the approval is no longer pending".to_string(),
                ));
            };
            if pending.asid.0 != session_id {
                return Err(AgentError::SessionNotFound(
                    "the approval belongs to another session".to_string(),
                ));
            }
            let Some(client_id) = interactions.client_id() else {
                return Err(AgentError::NotAvailable(
                    "the DeepSeek event stream is not connected".to_string(),
                ));
            };
            let args = serde_json::json!({
                "clientId": client_id,
                "eventId": request_id,
                "outcome": { "kind": "result", "value": mapper::approval_outcome(decision) },
            });
            self.client.call_remote("$events", "result", args).await?;
            interactions.remove(request_id);
            Ok(())
        })
    }

    /// A form is a forwarded `user-questions/request`; the answer goes back
    /// the same way, rebuilt in DeepSeek Harness's `{answers: [...]}` shape.
    fn reply_form<'a>(
        &'a self,
        session_id: &'a str,
        form_id: &'a str,
        answers: Value,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let interactions = DeepseekInteractions::for_endpoint(&self.client.endpoint.url);
            let Some(pending) = interactions.question(form_id) else {
                return Err(AgentError::SessionNotFound(
                    "the question is no longer pending".to_string(),
                ));
            };
            if pending.asid.0 != session_id {
                return Err(AgentError::SessionNotFound(
                    "the question belongs to another session".to_string(),
                ));
            }
            let Some(client_id) = interactions.client_id() else {
                return Err(AgentError::NotAvailable(
                    "the DeepSeek event stream is not connected".to_string(),
                ));
            };
            let value = mapper::question_answers(&pending.questions, &answers);
            let args = serde_json::json!({
                "clientId": client_id,
                "eventId": form_id,
                "outcome": { "kind": "result", "value": value },
            });
            self.client.call_remote("$events", "result", args).await?;
            interactions.remove(form_id);
            Ok(())
        })
    }

    fn get_catalog<'a>(&'a self, _directory: Option<&'a str>) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move {
            let raw = self.client.model_catalog().await?;
            // Modes are optional: without a roster the catalog has none.
            let presets = match self.client.agent_presets().await {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(target: "deepseek", "agentPresets/list failed: {e}");
                    None
                }
            };
            Ok(mapper::map_catalog(&raw, presets.as_ref()))
        })
    }

    /// DeepSeek Harness has no diff query: said so, never an empty diff the
    /// app would show as "nothing changed".
    fn get_vcs_diff<'a>(
        &'a self,
        _session_id: &'a str,
        _mode: &'a str,
    ) -> AgentFuture<'a, Vec<FileDiffItem>> {
        Box::pin(async { Err(AgentError::Unsupported("vcs diff".into())) })
    }

    /// DeepSeek Harness has no query for pending approvals: they exist only as
    /// unsettled waterfalls, which the `$events` stream re-delivers on every
    /// open. What is pending is therefore what the stream has told us.
    fn get_pending_permissions<'a>(
        &'a self,
        session_id: &'a str,
    ) -> AgentFuture<'a, Vec<PermissionRequest>> {
        Box::pin(async move {
            Ok(
                DeepseekInteractions::for_endpoint(&self.client.endpoint.url)
                    .approvals_for(session_id),
            )
        })
    }

    fn get_pending_forms<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, Vec<FormRequest>> {
        Box::pin(async move {
            Ok(
                DeepseekInteractions::for_endpoint(&self.client.endpoint.url)
                    .questions_for(session_id),
            )
        })
    }

    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let _history_read = self.history.lock().await;
            let walk = self
                .client
                .read_history(session_id, None, limit, INITIAL_HISTORY_RETAINED_BYTES)
                .await?;
            Ok(self.keep_history(session_id, &walk, Vec::new(), true))
        })
    }

    /// The page below the oldest row delivered for this session, under the
    /// frozen cut the snapshot was read under. Empty once the walk reached
    /// seq 0; `HistoryCursorLost` before any snapshot was read here.
    fn load_earlier_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            if limit == 0 || limit > MAX_EARLIER_ITEMS {
                return Err(AgentError::InvalidRequest(
                    "DeepSeek history limit exceeds 2000 mapped rows".into(),
                ));
            }
            let _history_read = self.history.lock().await;
            let Some(cursor) = self
                .cursors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(session_id)
            else {
                return Err(AgentError::HistoryCursorLost);
            };
            if !cursor.has_more {
                return Ok(Vec::new());
            }
            let walk = self
                .client
                .read_history(
                    session_id,
                    Some((cursor.through_seq, cursor.before_seq)),
                    limit,
                    MAX_HISTORY_RETAINED_BYTES,
                )
                .await?;
            // A failed read above left the cursor where it was: the page is
            // consumed only once it is delivered.
            Ok(self.keep_history(session_id, &walk, cursor.carried, false))
        })
    }

    fn rename_session<'a>(&'a self, session_id: &'a str, title: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            self.client.rename(session_id, title).await?;
            Ok(())
        })
    }

    fn fork_session<'a>(
        &'a self,
        session_id: &'a str,
        message_id: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            if message_id.is_some() {
                return Err(AgentError::Unsupported(
                    "DeepSeek message-boundary fork".into(),
                ));
            }
            let res = self.client.fork(session_id).await?;
            let new_id = res
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| AgentError::Protocol("fork response lacks sessionId".to_string()))?;

            Ok(AgentSessionInfo {
                asid: AgentSessionId(new_id.to_string()),
                agent_id: String::new(),
                backend_session_id: new_id.to_string(),
                runtime: None,
                context_usage: None,
                git_info: None,
                agent_name: None,
                agent_role: None,
                title: "Forked Session".to_string(),
                mode: None,
                model: None,
                status: AgentSessionStatus::Idle,
                directory: None,
                cost: None,
                tokens: None,
                limit: None,
                parent_id: Some(session_id.to_string()),
                project_id: None,
                outcome: None,
                error: None,
                revert: None,
                fork: None,
                time_idle: None,
                time_viewed: None,
                deleted: false,
                updated_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn plugin_catalog_uses_exact_session_and_never_guesses_from_directory() {
        use axum::{routing::post, Json, Router};
        use serde_json::json;
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let sink = seen.clone();
        let command_seen = seen.clone();
        let phase = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let skill_phase = phase.clone();
        let command_phase = phase.clone();
        let app=Router::new()
            .route("/api/session/modelCatalog",post(||async{Json(json!({"result":{"ok":true,"value":{"groups":[]}}}))}))
            .route("/api/agentPresets/list",post(||async{Json(json!({"result":{"ok":true,"value":{"presets":[]}}}))}))
            .route("/api/session/list",post(||async{Json(json!({"result":{"ok":true,"value":[{"sessionId":"a","cwd":"/old"},{"sessionId":"b","cwd":"/qa"}]}}))}))
            .route("/api/session/projections",post(||async{Json(json!({"result":{"ok":true,"value":{"values":{"workingDirectory":"/qa"}}}}))}))
            .route("/api/skills/list",post(move|Json(body):Json<Value>|{
                sink.lock().unwrap().push(body.clone());
                let phase=skill_phase.load(Ordering::Relaxed);
                async move {let session=&body["payload"]["args"]["request"]["sessionId"];
                    if phase==1 { return Json(json!({"result":{"ok":true,"value":{"skills":[]}}})); }
                    if phase==2 { return Json(json!({"result":{"ok":false,"error":{"code":"gateway/internal","message":"preset registry unavailable"}}})); }
                    Json(json!({"result":{"ok":true,"value":{"skills":[{"name":format!("skill-{}",session.as_str().unwrap()),"description":"native","modelInvocable":false}]}}}))}
            }))
            .route("/api/commands/list",post(move|Json(body):Json<Value>|{
                let sink=command_seen.clone(); sink.lock().unwrap().push(body.clone());
                let phase=command_phase.load(Ordering::Relaxed);
                async move {let session=body["payload"]["args"]["agentId"].as_str().unwrap();
                    if phase==2 { return Json(json!({"result":{"ok":false,"error":{"code":"gateway/internal","message":"preset registry unavailable"}}})); }
                    Json(json!({"result":{"ok":true,"value":[{"name":format!("command-{session}"),"description":"native"}]}}))}
            }));
        let (driver, server) = rpc_server(app).await;
        assert!(driver
            .get_catalog(Some("/qa"))
            .await
            .unwrap()
            .skills
            .is_empty());
        assert!(!driver.skills_listed() && !driver.slash_commands_listed());
        assert!(
            seen.lock().unwrap().is_empty(),
            "directory-only reads never select a native session"
        );
        let a = driver
            .get_catalog_for_session(Some("a"), Some("/qa"))
            .await
            .unwrap();
        let b = driver
            .get_catalog_for_session(Some("b"), Some("/qa"))
            .await
            .unwrap();
        assert_eq!(a.skills[0].id, "skill-a");
        assert_eq!(b.skills[0].id, "skill-b");
        assert_eq!(a.commands[0].name, "command-a");
        assert_eq!(b.commands[0].name, "command-b");
        assert!(driver.skills_listed() && driver.slash_commands_listed());
        phase.store(1, Ordering::Relaxed);
        assert!(
            driver
                .get_catalog_for_session(Some("a"), Some("/qa"))
                .await
                .unwrap()
                .skills
                .is_empty(),
            "removing the last skill clears the catalog"
        );
        phase.store(2, Ordering::Relaxed);
        let unavailable = driver
            .get_catalog_for_session(Some("b"), Some("/qa"))
            .await
            .unwrap();
        assert!(unavailable.skills.is_empty() && unavailable.commands.is_empty());
        assert!(
            driver.skills_listed() && driver.slash_commands_listed(),
            "one preset failure cannot revoke instance capabilities"
        );
        assert!(matches!(
            driver
                .get_catalog_for_session(Some("a"), Some("/wrong"))
                .await,
            Err(AgentError::InvalidRequest(_))
        ));
        assert!(matches!(
            driver
                .get_catalog_for_session(Some("missing"), Some("/qa"))
                .await,
            Err(AgentError::SessionNotFound(_))
        ));
        server.abort();
    }

    #[tokio::test]
    async fn skills_use_verified_slash_prompts_and_commands_never_fall_back_to_prompts() {
        use axum::{routing::post, Json, Router};
        use serde_json::json;
        let calls = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let prompts = calls.clone();
        let commands = calls.clone();
        let app=Router::new()
            .route("/api/skills/list",post(||async{Json(json!({"result":{"ok":true,"value":{"skills":[{"name":"review","description":"Review","modelInvocable":false}]}}}))}))
            .route("/api/session/prompt",post(move|Json(body):Json<Value>|{prompts.lock().unwrap().push(body);async{Json(json!({"result":{"ok":true,"value":{"accepted":true}}}))}}))
            .route("/api/commands/execute",post(move|Json(body):Json<Value>|{
                commands.lock().unwrap().push(body.clone());async move {
                    let args=&body["payload"]["args"];
                    let value=match args["line"].as_str().unwrap(){
                        "/failed details"=>json!({"commandId":"failure","result":{"kind":"error","text":"permission refused"}}),
                        "/unknown"=>Value::Null,
                        "/ambiguous"=>json!({}),
                        _=>json!({"commandId":"success","result":{"kind":"success","text":"done","sourceEventSeq":12}}),
                    };Json(json!({"result":{"ok":true,"value":value}}))
                }
            }));
        let (driver, server) = rpc_server(app).await;
        driver
            .activate_skill("native", "review", None)
            .await
            .unwrap();
        assert!(driver
            .activate_skill("native", "unknown", None)
            .await
            .is_err());
        assert!(driver
            .activate_skill("native", "review", Some(false))
            .await
            .is_err());
        let success = driver
            .execute_command("native", "review", "raw  input", Some("queue"))
            .await
            .unwrap();
        assert_eq!(success["command_result"]["kind"], "success");
        assert_eq!(success["command_result"]["source_event_seq"], 12);
        let failed = driver
            .execute_command("native", "failed", "details", None)
            .await
            .unwrap();
        assert_eq!(failed["command_result"]["kind"], "error");
        assert!(driver
            .execute_command("native", "unknown", "", None)
            .await
            .is_err());
        assert!(driver
            .execute_command("native", "ambiguous", "", None)
            .await
            .is_err());
        assert!(driver
            .execute_command("native", "x\ninjected", "", None)
            .await
            .is_err());
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            5,
            "one skill prompt and four native command attempts; no retry or fallback"
        );
        assert_eq!(
            calls[0]["payload"]["args"]["request"]["content"],
            json!([{"type":"text","text":"/review"}])
        );
        assert_eq!(calls[0]["payload"]["args"]["request"]["mode"], "queue");
        assert_eq!(
            calls[1]["payload"]["args"],
            json!({"agentId":"native","line":"/review raw  input","submittedAttachments":[]})
        );
        server.abort();
    }

    #[tokio::test]
    async fn durable_inbox_mutations_use_native_identity_and_never_retry_failures() {
        use axum::{routing::post, Json, Router};
        use serde_json::json;
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let sent = seen.clone();
        let app = Router::new()
            .route("/api/session/projections", post(|| async {
                Json(json!({"result":{"ok":true,"value":{"values":{"inbox":{
                    "next-turn":[{"id":"queued","content":[{"type":"text","text":"later"}]}],"next-step":[]}}}}}))
            }))
            .route("/api/session/updateQueue", post(move |Json(body):Json<Value>| {
                sent.lock().unwrap().push(body.clone());
                async move {
                    let request=&body["payload"]["args"]["request"];
                    if request["itemId"]=="gone" {
                        Json(json!({"result":{"ok":false,"error":{"code":"session/queue-item-not-found","message":"claimed"}}}))
                    } else if request["itemId"]=="ambiguous" {
                        Json(json!({"result":{"ok":true,"value":{}}}))
                    } else { Json(json!({"result":{"ok":true,"value":{"accepted":true}}})) }
                }
            }));
        let (driver, server) = rpc_server(app).await;
        assert_eq!(driver.get_inbox("s").await.unwrap()[0]["id"], "queued");
        driver
            .edit_inbox_item("s", "queued", "changed")
            .await
            .unwrap();
        driver.cancel_inbox_item("s", "queued").await.unwrap();
        driver
            .set_inbox_delivery("s", "queued", "steer")
            .await
            .unwrap();
        assert!(matches!(
            driver.cancel_inbox_item("s", "gone").await,
            Err(AgentError::InboxItemNotFound(_))
        ));
        assert!(driver
            .set_inbox_delivery("s", "ambiguous", "steer")
            .await
            .is_err());
        assert!(matches!(
            driver.set_inbox_delivery("s", "queued", "queue").await,
            Err(AgentError::Unsupported(_))
        ));
        assert!(driver.edit_inbox_item("s", "queued", " ").await.is_err());
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            5,
            "each mutation has one attempt; unsupported modes send nothing"
        );
        let requests: Vec<_> = seen
            .iter()
            .map(|body| &body["payload"]["args"]["request"])
            .collect();
        assert_eq!(
            requests[0]["action"],
            json!({"kind":"edit","content":[{"type":"text","text":"changed"}]})
        );
        assert_eq!(requests[1]["action"]["kind"], "remove");
        assert_eq!(requests[2]["action"]["kind"], "steer");
        assert!(requests.iter().all(|request| request["sessionId"] == "s"));
        server.abort();
    }

    async fn rpc_server(app: axum::Router) -> (DeepseekDriver, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let driver = DeepseekDriver::new(DeepseekEndpoint::new(
            format!("http://{}", listener.local_addr().unwrap()),
            None,
            None,
        ));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (driver, server)
    }

    #[tokio::test]
    async fn message_boundary_fork_is_rejected_instead_of_silently_copying_whole_session() {
        let (driver, server) = rpc_server(axum::Router::new()).await;
        assert!(matches!(
            driver.fork_session("source", Some("message")).await,
            Err(AgentError::Unsupported(_))
        ));
        server.abort();
    }

    #[tokio::test]
    async fn create_reports_accepted_model_or_explicit_partial_failure_without_retry() {
        use axum::{routing::post, Json, Router};
        use serde_json::json;
        for selection in [
            json!({ "ok": true, "value": { "selected": { "provider": "normalized", "model": "accepted", "reasoningEffort": "low" } } }),
            json!({ "ok": true, "value": { "selected": { "provider": "normalized", "model": "accepted" } } }),
            json!({ "ok": false, "error": { "code": "session/model-unavailable", "message": "not available" } }),
            json!({ "ok": true, "value": {} }),
        ] {
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let created = seen.clone();
            let selected = seen.clone();
            let expected = selection.pointer("/value/selected").cloned();
            let app = Router::new()
                .route("/api/session/create", post(move |Json(body): Json<Value>| { let seen = created.clone(); async move {
                    seen.lock().unwrap().push("create");
                    assert_eq!(body.pointer("/payload/args/request/cwd"), Some(&json!("/qa")));
                    Json(json!({ "result": { "ok": true, "value": { "sessionId": "created-session" } } }))
                } }))
                .route("/api/session/selectModel", post(move |Json(body): Json<Value>| { let seen = selected.clone(); let result = selection.clone(); async move {
                    seen.lock().unwrap().push("select");
                    assert_eq!(body.pointer("/payload/args/request"), Some(&json!({
                        "sessionId": "created-session", "provider": "requested", "model": "alias", "reasoningEffort": "high"
                    })));
                    Json(json!({ "result": result }))
                } }));
            let (driver, server) = rpc_server(app).await;
            let requested = ModelRef {
                provider_id: "requested".into(),
                model_id: "alias".into(),
                variant: Some("high".into()),
            };
            let result = driver
                .create_session(Some("/qa"), Some(&requested), None)
                .await;
            if let Some(expected) = expected {
                let info = result.unwrap();
                let model = info.model.unwrap();
                assert_eq!(model.provider_id, "normalized");
                assert_eq!(model.model_id, "accepted");
                assert_eq!(
                    model.variant.as_deref(),
                    expected.get("reasoningEffort").and_then(Value::as_str)
                );
            } else {
                let error = result.unwrap_err();
                assert!(matches!(error, AgentError::RequestFailed(_)));
                assert!(error.to_string().contains("created-session was created"));
                assert!(error.to_string().contains("do not retry creation"));
            }
            assert_eq!(*seen.lock().unwrap(), vec!["create", "select"]);
            server.abort();
        }
    }

    #[tokio::test]
    async fn file_search_uses_live_session_scope_query_and_limit() {
        use axum::{routing::post, Json, Router};
        use serde_json::json;
        let app = Router::new()
            .route(
                "/api/session/list",
                post(|| async {
                    Json(json!({ "result": { "ok": true, "value": [
                    { "sessionId": "other", "cwd": "/other", "agentAvailable": true },
                    { "sessionId": "cold", "cwd": "/qa", "agentAvailable": false },
                    { "sessionId": "owned", "cwd": "/qa", "agentAvailable": true }
                ] } }))
                }),
            )
            .route(
                "/api/fileReferences/list",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(
                        body.pointer("/payload/args"),
                        Some(&json!({ "agentId": "owned", "query": "sr" }))
                    );
                    Json(json!({ "result": { "ok": true, "value": [
                    { "path": "src/", "kind": "directory" },
                    { "path": "src/lib.rs", "kind": "file" },
                    { "path": "src/main.rs", "kind": "file" }
                ] } }))
                }),
            );
        let (driver, server) = rpc_server(app).await;
        assert_eq!(
            driver.find_files("sr", 2, Some("/qa")).await.unwrap(),
            json!([
                { "path": "src/", "type": "directory" }, { "path": "src/lib.rs", "type": "file" }
            ])
            .as_array()
            .unwrap()
            .clone()
        );
        assert!(matches!(
            driver.find_files("sr", 2, Some("/missing")).await,
            Err(AgentError::NotAvailable(_))
        ));
        assert!(matches!(
            driver.find_files("sr", 2, None).await,
            Err(AgentError::RequestFailed(_))
        ));
        assert!(matches!(
            driver.find_files("../secret", 2, Some("/qa")).await,
            Err(AgentError::RequestFailed(_))
        ));
        assert!(driver
            .find_files("sr", 0, Some("/qa"))
            .await
            .unwrap()
            .is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn file_search_rejects_ambiguous_scopes_and_upstream_refusals() {
        use axum::{routing::post, Json, Router};
        use serde_json::json;
        for (sessions, references) in [
            (
                json!([
                    { "sessionId": "one", "cwd": "/qa", "agentAvailable": true },
                    { "sessionId": "two", "cwd": "/qa", "agentAvailable": true }
                ]),
                json!({ "ok": true, "value": [] }),
            ),
            (
                json!([{ "sessionId": "one", "cwd": "/qa", "agentAvailable": true }]),
                json!({ "ok": false, "error": { "code": "gateway/lookup-not-found" } }),
            ),
            (
                json!([{ "sessionId": "one", "cwd": "/qa", "agentAvailable": true }]),
                json!({ "ok": true, "value": [{ "path": "../secret", "kind": "file" }] }),
            ),
        ] {
            let app = Router::new()
                .route(
                    "/api/session/list",
                    post(move || {
                        let value = sessions.clone();
                        async move { Json(json!({ "result": { "ok": true, "value": value } })) }
                    }),
                )
                .route(
                    "/api/fileReferences/list",
                    post(move || {
                        let result = references.clone();
                        async move { Json(json!({ "result": result })) }
                    }),
                );
            let (driver, server) = rpc_server(app).await;
            assert!(driver.find_files("sr", 2, Some("/qa")).await.is_err());
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_prompt_with_attachments_carries_the_paths_in_its_text() {
        use axum::{routing::post, Json, Router};
        let seen = Arc::new(std::sync::Mutex::new(Value::Null));
        let sink = seen.clone();
        let app = Router::new().route(
            "/api/session/prompt",
            post(move |Json(body): Json<Value>| {
                let sink = sink.clone();
                async move {
                    *sink.lock().unwrap() = body;
                    Json(serde_json::json!({ "result": { "ok": true, "value": {} } }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let driver = DeepseekDriver::new(DeepseekEndpoint::new(url, None, None));
        assert_eq!(driver.attachment_mode(), AttachmentMode::ByPath);
        driver
            .send_prompt("s", "look", &["/up/a.png".to_string()], None)
            .await
            .unwrap();
        let body = seen.lock().unwrap().clone();
        let parts = body
            .pointer("/payload/args/request/content")
            .cloned()
            .unwrap_or_else(|| panic!("no content in {body}"));
        assert_eq!(
            parts,
            serde_json::json!([{
                "type": "text",
                "text": "look\n\nAttached files (on this host):\n- /up/a.png"
            }])
        );
        server.abort();
    }

    #[tokio::test]
    async fn default_projections_cannot_claim_another_agents_session() {
        use axum::{routing::post, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let projection_calls = Arc::new(AtomicUsize::new(0));
        let count = projection_calls.clone();
        let app = Router::new()
            .route("/api/session/list", post(|| async {
                Json(serde_json::json!({ "result": { "ok": true, "value": [
                    { "sessionId": "session-owned", "cwd": "/qa", "title": "Known session" }
                ] } }))
            }))
            .route("/api/session/projections", post(move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({ "result": { "ok": true, "value": { "values": {} } } }))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let driver = DeepseekDriver::new(DeepseekEndpoint::new(url, None, None));
        assert!(
            matches!(driver.get_session("t3-thread").await, Err(AgentError::SessionNotFound(id)) if id == "t3-thread")
        );
        assert_eq!(projection_calls.load(Ordering::SeqCst), 0);
        let known = driver.get_session("session-owned").await.unwrap();
        assert_eq!(known.directory.as_deref(), Some("/qa"));
        assert_eq!(known.title, "Known session");
        assert_eq!(projection_calls.load(Ordering::SeqCst), 1);
        server.abort();
    }

    /// Opening reads the newest pages within the opening budget and never
    /// fails for the size of the history; `load_earlier_timeline` then
    /// delivers the pages below, one batch at a time under the same frozen
    /// cut, completing calls whose results sat above a page cut, until seq
    /// 0, after which it is empty. The end-of-run refetch of the newest
    /// messages does not move the cursor back up.
    #[tokio::test]
    async fn history_opens_within_budget_and_pages_earlier_to_the_start() {
        use crate::agents::adapters::deepseek::client::tests::{paged_history, rpc_server};
        use crate::agents::domain::{AgentPart, ToolCallStatus};
        // Pages of 90 records at 7,000 bytes each are about 630 KiB: under
        // the 1 MiB response cap, and the opening budget of 2 MiB holds
        // three of them but not the 60-record fourth.
        let (app, calls) = paged_history(7_000);
        let (client, server) = rpc_server(app).await;
        let driver = DeepseekDriver {
            client: Arc::new(client),
            history: tokio::sync::Mutex::new(()),
            cursors: std::sync::Mutex::new(HistoryCursors::default()),
            skills_available: AtomicBool::new(false),
            commands_available: AtomicBool::new(false),
        };
        let seq_of = |item: &TimelineItem| item.seq;
        // A limit the message count never reaches: the bytes end the walk.
        let tail = driver.get_timeline("s", 500).await.unwrap();
        assert_eq!(seq_of(&tail[0]), 90);
        // Seq 359 is a tool result, joined onto the call at 358: no row of its own.
        assert_eq!(seq_of(tail.last().unwrap()), 358);
        // The opening walk asked for the fourth page and left it for later.
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                (359, 360, 50),
                (359, 270, 50),
                (359, 180, 50),
                (359, 90, 50)
            ]
        );
        let mut all = tail.clone();

        // Before anything else, the refetch of a finished run: the newest
        // twenty messages again, and the cursor stays where the walk got to.
        let refetched = driver.get_timeline("s", 20).await.unwrap();
        assert!(refetched.len() < tail.len());
        assert_eq!(
            driver.cursors.lock().unwrap().get("s").unwrap().before_seq,
            90
        );

        let earlier = driver.load_earlier_timeline("s", 80).await.unwrap();
        assert_eq!(seq_of(&earlier[0]), 0);
        assert_eq!(seq_of(earlier.last().unwrap()), 88);
        // Seq 89 is the result of the call at seq 88, cut from it by the
        // page boundary at 90: the row at 88 is complete all the same.
        let call_at_88 = earlier
            .iter()
            .find(|item| item.seq == 88)
            .expect("the call row");
        let AgentPart::Tool(ref call) = call_at_88.part else {
            panic!("a tool row");
        };
        assert_eq!(call.status, ToolCallStatus::Completed);
        let mut rest = earlier;
        rest.append(&mut all);
        all = rest;
        let seqs: Vec<u64> = all.iter().map(seq_of).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(seqs, sorted, "oldest first, no row twice");
        assert_eq!(seqs.first(), Some(&0));

        assert!(driver
            .load_earlier_timeline("s", 80)
            .await
            .unwrap()
            .is_empty());
        assert!(driver
            .load_earlier_timeline("s", 80)
            .await
            .unwrap()
            .is_empty());
        // The earlier batch read one page; the exhausted calls read nothing.
        assert_eq!(calls.lock().unwrap().len(), 6);
        // A session never opened here has no cursor: not exhaustion, a
        // refusal the app answers by reading the snapshot again.
        assert!(matches!(
            driver.load_earlier_timeline("never", 80).await,
            Err(AgentError::HistoryCursorLost)
        ));
        assert!(matches!(
            driver.load_earlier_timeline("s", 0).await,
            Err(AgentError::InvalidRequest(_))
        ));
        server.abort();
    }

    #[tokio::test]
    async fn list_sessions_honours_the_parent_filter() {
        use axum::{routing::post, Json, Router};
        let app = Router::new().route(
            "/api/session/list",
            post(|| async {
                Json(serde_json::json!({ "result": { "ok": true, "value": [
                    { "sessionId": "session-a", "cwd": "/qa", "title": "A" },
                    { "sessionId": "session-b", "cwd": "/qa", "title": "B" }
                ] } }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let driver = DeepseekDriver::new(DeepseekEndpoint::new(url, None, None));
        let query = |parent: Option<&str>| SessionQuery {
            parent_id: parent.map(str::to_string),
            ..Default::default()
        };
        assert_eq!(driver.list_sessions(&query(None)).await.unwrap().len(), 2);
        assert_eq!(
            driver
                .list_sessions(&query(Some("null")))
                .await
                .unwrap()
                .len(),
            2,
            "roots only keeps top-level sessions"
        );
        assert!(driver
            .list_sessions(&query(Some("session-x")))
            .await
            .unwrap()
            .is_empty());
        server.abort();
    }

    #[tokio::test]
    #[ignore = "requires a running DeepSeek Harness; creates a real session"]
    async fn test_live_deepseek_driver_session_lifecycle() {
        let Some(endpoint) = DeepseekEndpoint::discover().await else {
            eprintln!("DeepSeek Harness not discovered, skipping live test");
            return;
        };

        let driver = DeepseekDriver::new(endpoint);
        let catalog = driver
            .get_catalog(None)
            .await
            .expect("Catalog fetch failed");
        assert!(!catalog.models.is_empty());
        println!(
            "Live Catalog Models: {:?}",
            catalog.models.iter().map(|m| &m.name).collect::<Vec<_>>()
        );

        let temp_path =
            std::env::temp_dir().join(format!("muqun-gateway-dsh-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_path).expect("create temp dir");
        let temp_dir = temp_path.to_str().expect("utf-8 temp dir");

        let session = driver
            .create_session(Some(temp_dir), None, None)
            .await
            .expect("Session creation failed");
        let session_id = session.asid.0.clone();
        println!("Created Live DeepSeek Session: {session_id}");

        let fetched = driver
            .get_session(&session_id)
            .await
            .expect("Fetch session failed");
        assert_eq!(fetched.asid.0, session_id);
        println!("Fetched Session Info: {:?}", fetched);

        let prompt_res = driver
            .send_prompt(
                &session_id,
                "Ping from Rust DeepseekDriver live test!",
                &[],
                None,
            )
            .await;
        assert!(prompt_res.is_ok(), "Prompt send failed: {:?}", prompt_res);
        println!("Successfully sent prompt to live DeepSeek session!");

        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        let timeline = driver
            .get_timeline(&session_id, 10)
            .await
            .expect("Get timeline failed");
        println!("Timeline items count: {}", timeline.len());
        for item in &timeline {
            println!("  [Role {:?}] Part: {:?}", item.role, item.part);
        }

        // DeepSeek Harness exposes no session/delete RPC, so the best available
        // cleanup is to stop any running turn and remove our workspace.
        let _ = driver.interrupt(&session_id).await;
        let _ = std::fs::remove_dir_all(&temp_path);
    }
}
