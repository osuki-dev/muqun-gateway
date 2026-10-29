//! `AgentPort` over a T3 Code server.
//!
//! A T3 thread is a session and its id is the session id. Reads come from
//! the HTTP read-model snapshots, writes are orchestration commands over the
//! RPC socket. Everything T3 does not offer keeps the trait default and
//! answers `Unsupported`; nothing here returns `Ok(())` for work it did not
//! do.

use std::sync::Arc;

use serde_json::Value;

use super::client::{
    bounded_prompt, new_id, validate_id, ModelSelection, T3Client, DEFAULT_INTERACTION_MODE,
    DEFAULT_RUNTIME_MODE, RUNTIME_MODES,
};
use super::endpoint::T3Endpoint;
use super::mapper;
use super::stream::ThreadWatcher;
use super::v2;
use crate::agents::domain::{
    AgentCatalog, AgentProject, AgentSessionInfo, FormRequest, ModelRef, PermissionDecision,
    PermissionRequest, SessionQuery, TimelineItem,
};
use crate::agents::ports::agent::{
    append_attachment_paths, AgentError, AgentFuture, AgentPort, AttachmentMode, PreparedTimeline,
};

/// The title a thread is created with; read back, it means "untitled".
pub(crate) const NEW_THREAD_TITLE: &str = "New thread";

pub struct T3Driver {
    client: Arc<T3Client>,
    /// The T3 runtime mode new threads get: the permission policy
    /// (`full-access`, `approval-required`, `auto-accept-edits`, `auto`).
    /// A gateway setting, not a mode the app picks: T3 has no persona
    /// inside an agent, so the catalog's modes are empty.
    runtime_mode: String,
    /// Follows the threads a client looks at, so their turns stream. A
    /// driver built without a listener gets a detached one.
    watcher: ThreadWatcher,
}

impl T3Driver {
    async fn mutate_queued(
        &self,
        session: &str,
        item: &str,
        kind: &str,
        text: Option<&str>,
    ) -> Result<(), AgentError> {
        let session = validate_id(session, "session id")?;
        let item = validate_id(item, "queued run id")?;
        let snapshot = self.client.thread_snapshot(&session, None).await?;
        let projection = &snapshot["projection"];
        v2::inbox::queued_run(projection, &item)?;
        let mut command =
            serde_json::json!({"type":kind,"commandId":new_id(),"threadId":session,"runId":item});
        if kind == "queued-message.promote-to-steer" {
            if v2::projection::has_active_run(projection) {
                command.as_object_mut().unwrap().remove("runId");
                command["queuedRunId"] = serde_json::json!(item);
                command["targetRunId"] = serde_json::json!(v2::inbox::steer_target(projection)?);
            } else {
                command["type"] = serde_json::json!("queued-run.start");
            }
        }
        if let Some(text) = text {
            command["text"] = serde_json::json!(text);
        }
        // The server arbitrates a claim or active-run race. Never retry or
        // re-create the message when the reply is lost or the command refused.
        self.client.dispatch(command).await?;
        self.watcher.watch(&session);
        Ok(())
    }

    pub fn new(endpoint: T3Endpoint) -> Self {
        Self::from_client(Arc::new(T3Client::new(endpoint)))
    }

    pub fn from_client(client: Arc<T3Client>) -> Self {
        Self {
            client,
            runtime_mode: DEFAULT_RUNTIME_MODE.to_string(),
            watcher: ThreadWatcher::detached(),
        }
    }

    /// Follow the threads this driver opens, creates or lists as live with
    /// `watcher`, and stop following the ones it deletes.
    pub fn with_watcher(mut self, watcher: ThreadWatcher) -> Self {
        self.watcher = watcher;
        self
    }

    /// Use another T3 runtime mode for new threads. Rejects names T3 does
    /// not know rather than letting the server reject every thread later.
    pub fn with_runtime_mode(mut self, runtime_mode: &str) -> Result<Self, AgentError> {
        let name = runtime_mode.trim();
        if !RUNTIME_MODES.contains(&name) {
            return Err(AgentError::RequestFailed("unknown T3 runtime mode".into()));
        }
        self.runtime_mode = name.to_string();
        Ok(self)
    }

    pub fn client(&self) -> &Arc<T3Client> {
        &self.client
    }

    /// The thread snapshot, mapped. Also the directory lookup, which needs
    /// the project list.
    async fn session_from_snapshot(
        &self,
        thread_id: &str,
    ) -> Result<(AgentSessionInfo, Value), AgentError> {
        let detail = self.client.thread_snapshot(thread_id, None).await?;
        {
            let projection = detail["projection"].clone();
            let roots = mapper::project_roots(&self.client.shell_snapshot().await?);
            let info = v2::mapper::session(&projection["thread"], Some(&projection), &roots)
                .ok_or_else(|| AgentError::Protocol("invalid V2 thread".into()))?;
            Ok((info, projection))
        }
    }

    /// Find the project whose workspace root is `directory`, or create one.
    async fn project_for(&self, directory: &str) -> Result<String, AgentError> {
        let dir = directory.trim().trim_end_matches('/');
        if dir.is_empty() {
            return Err(AgentError::RequestFailed("a directory is required".into()));
        }
        if !std::path::Path::new(dir).is_dir() {
            return Err(AgentError::WorkspaceMissing(dir.to_string()));
        }
        let shell = self.client.shell_snapshot().await?;
        if let Some(existing) = shell
            .get("projects")
            .and_then(Value::as_array)
            .and_then(|items| {
                items.iter().find(|p| {
                    p.get("workspaceRoot")
                        .and_then(Value::as_str)
                        .map(|r| r.trim_end_matches('/') == dir)
                        .unwrap_or(false)
                })
            })
            .and_then(|p| p.get("id").and_then(Value::as_str))
        {
            return Ok(existing.to_string());
        }
        let project_id = new_id();
        let title = std::path::Path::new(dir)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| dir.to_string());
        self.client.create_project(&project_id, &title, dir).await?;
        tracing::info!(project_id = %project_id, "t3 project created");
        Ok(project_id)
    }

    async fn default_model(&self) -> Result<ModelSelection, AgentError> {
        let config = self.client.get_config().await?;
        let catalog = mapper::map_catalog(&config);
        catalog
            .defaults
            .model
            .as_ref()
            .map(mapper::model_selection_from_ref)
            .ok_or_else(|| {
                AgentError::NotAvailable("no provider is ready on the T3 Code host".into())
            })
    }
}

impl AgentPort for T3Driver {
    fn revert_session<'a>(
        &'a self,
        _session_id: &'a str,
        _message_id: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async {
            Err(AgentError::Unsupported(
                "V2 message-targeted conversation and file revert".into(),
            ))
        })
    }
    fn get_vcs_diff<'a>(
        &'a self,
        _session_id: &'a str,
        _mode: &'a str,
    ) -> AgentFuture<'a, Vec<crate::agents::ports::agent::FileDiffItem>> {
        Box::pin(async { Err(AgentError::Unsupported("V2 cumulative file diff".into())) })
    }

    fn get_inbox<'a>(&'a self, session: &'a str) -> AgentFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let id = validate_id(session, "session id")?;
            let snapshot = self.client.thread_snapshot(&id, None).await?;
            self.watcher.watch(&id);
            v2::inbox::items(&snapshot["projection"])
        })
    }

    fn cancel_inbox_item<'a>(&'a self, session: &'a str, item: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            self.mutate_queued(session, item, "queued-run.cancel", None)
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
            let text = bounded_prompt(text)?;
            if text.trim().is_empty() {
                return Err(AgentError::InvalidRequest(
                    "queue text must not be empty".into(),
                ));
            }
            self.mutate_queued(session, item, "queued-run.edit", Some(text))
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
            match delivery {
                "steer" => {
                    self.mutate_queued(session, item, "queued-message.promote-to-steer", None)
                        .await
                }
                "queue" => {
                    let snapshot = self.client.thread_snapshot(session, None).await?;
                    v2::inbox::queued_run(&snapshot["projection"], item)?;
                    Ok(())
                }
                _ => Err(AgentError::InvalidRequest("unknown delivery mode".into())),
            }
        })
    }

    fn kind(&self) -> &'static str {
        super::KIND
    }

    fn attachment_mode(&self) -> AttachmentMode {
        AttachmentMode::ByPath
    }

    fn probe(&self) -> AgentFuture<'_, bool> {
        Box::pin(async move {
            Ok(self
                .client
                .endpoint
                .describe(self.client.http())
                .await
                .is_ok_and(|descriptor| {
                    descriptor.protocol().ok() == Some(self.client.endpoint.protocol)
                        && self
                            .client
                            .endpoint
                            .environment_id
                            .as_ref()
                            .is_none_or(|id| id == &descriptor.environment_id)
                }))
        })
    }

    fn list_projects(&self) -> AgentFuture<'_, Vec<AgentProject>> {
        Box::pin(async move {
            let shell = self.client.shell_snapshot().await?;
            Ok(shell
                .get("projects")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(mapper::map_project).collect())
                .unwrap_or_default())
        })
    }

    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> AgentFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            let shell = self.client.shell_snapshot().await?;
            let mut sessions = { v2::mapper::shell_sessions(&shell) };
            sessions.retain(|s| query.keeps_parent(s.parent_id.as_deref()));
            if let Some(dir) = query.directory.as_deref().map(|d| d.trim_end_matches('/')) {
                sessions.retain(|s| {
                    s.directory
                        .as_deref()
                        .map(|d| d.trim_end_matches('/') == dir)
                        .unwrap_or(false)
                });
            }
            if let Some(search) = query
                .search
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let needle = search.to_lowercase();
                let matched: std::collections::HashSet<String> =
                    match self.client.search_threads(search, 50).await {
                        Ok(result) => result
                            .get("matches")
                            .and_then(Value::as_array)
                            .map(|m| {
                                m.iter()
                                    .filter_map(|x| x.get("threadId").and_then(Value::as_str))
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        Err(e) => {
                            tracing::debug!(error = %e, "t3 search unavailable; title match only");
                            Default::default()
                        }
                    };
                sessions.retain(|s| {
                    matched.contains(&s.asid.0) || s.title.to_lowercase().contains(&needle)
                });
            }
            if query.order.as_deref() == Some("asc") {
                sessions.reverse();
            }
            if let Some(limit) = query.limit {
                sessions.truncate(limit);
            }
            // A thread with a turn in flight is worth following before it is
            // opened: its approvals should reach the phone.
            for session in sessions
                .iter()
                .filter(|s| s.status == crate::agents::domain::AgentSessionStatus::Busy)
            {
                self.watcher.watch(&session.asid.0);
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
            let directory = directory.ok_or_else(|| {
                AgentError::RequestFailed("a directory is required to open a T3 thread".into())
            })?;
            if mode.map(str::trim).filter(|a| !a.is_empty()).is_some() {
                return Err(AgentError::Unsupported("mode".into()));
            }
            let runtime_mode = self.runtime_mode.as_str();
            let interaction_mode = DEFAULT_INTERACTION_MODE;
            let project_id = self.project_for(directory).await?;
            let selection = match model {
                Some(m) => mapper::model_selection_from_ref(m),
                None => self.default_model().await?,
            };
            let thread_id = new_id();
            let title = NEW_THREAD_TITLE;
            self.client
                .dispatch({
                    v2::commands::thread_create(
                        &thread_id,
                        &project_id,
                        title,
                        &selection,
                        runtime_mode,
                        interaction_mode,
                    )
                })
                .await?;
            self.watcher.watch(&thread_id);
            match self.session_from_snapshot(&thread_id).await {
                Ok((info, _)) => Ok(info),
                Err(e) => {
                    // The command committed; the read model is a moment behind.
                    tracing::debug!(error = %e, "t3 thread created but not yet readable");
                    Ok(AgentSessionInfo {
                        asid: thread_id.clone().into(),
                        backend_session_id: String::new(),
                        runtime: None,
                        context_usage: None,
                        git_info: None,
                        agent_name: None,
                        agent_role: None,
                        agent_id: String::new(),
                        title: title.into(),
                        mode: None,
                        model: mapper::map_model_ref(&selection.to_value()),
                        status: crate::agents::domain::AgentSessionStatus::Idle,
                        directory: Some(directory.trim_end_matches('/').to_string()),
                        cost: None,
                        tokens: None,
                        limit: None,
                        parent_id: None,
                        project_id: Some(project_id),
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
                }
            }
        })
    }

    fn get_session<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let info = self.session_from_snapshot(&id).await?.0;
            self.watcher.watch(&id);
            Ok(info)
        })
    }

    fn send_prompt<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            // T3's own attachment upload is unverified; its agents read
            // files with their tools, so paths travel in the text.
            let text = append_attachment_paths(text, attachments);
            let text = bounded_prompt(&text)?;
            let (info, thread) = self.session_from_snapshot(&id).await?;
            {
                // Default delivery never interrupts or restarts a root run.
                // A concurrent upstream rejection remains an error, not an
                // excuse to submit the prompt again with another mode.
                let mode = v2::inbox::dispatch_mode(&thread, delivery)?;
                let model = info.model.as_ref().map(mapper::model_selection_from_ref);
                let mut command =
                    v2::commands::message_dispatch(&id, &new_id(), text, model.as_ref(), false);
                command["dispatchMode"] = mode;
                self.client.dispatch(command).await?;
                self.watcher.watch(&id);
                Ok(())
            }
        })
    }

    fn interrupt<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            {
                let snapshot = self.client.thread_snapshot(&id, None).await?;
                let run = v2::mapper::interrupt_run(&snapshot["projection"])?;
                self.client
                    .dispatch(v2::commands::interrupt(&id, run))
                    .await?;
                Ok(())
            }
        })
    }

    fn switch_model<'a>(&'a self, session_id: &'a str, model: &'a ModelRef) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let config = self.client.get_config().await?;
            if !v2::projection::rows(&config, "providers")
                .iter()
                .any(|provider| {
                    v2::projection::string(provider, "instanceId") == Some(&model.provider_id)
                        && provider.get("enabled").and_then(Value::as_bool) == Some(true)
                        && v2::projection::rows(provider, "models")
                            .iter()
                            .any(|m| v2::projection::string(m, "slug") == Some(&model.model_id))
                })
            {
                return Err(AgentError::RequestFailed(
                    "selected model is not in the enabled T3 catalog".into(),
                ));
            }
            let selection = mapper::model_selection_from_ref(model);
            self.client
                .dispatch(v2::commands::set_model(&id, &selection))
                .await?;
            Ok(())
        })
    }

    /// T3 has no persona to switch between. Its runtime and interaction
    /// modes are permission policies (`commands::runtime_mode_set`,
    /// `commands::interaction_mode_set`), which are not the app's modes.
    fn switch_mode<'a>(&'a self, _session_id: &'a str, _mode: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("switch_mode".into())) })
    }

    fn find_files<'a>(
        &'a self,
        _query: &'a str,
        _limit: usize,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<Value>> {
        Box::pin(async { Err(AgentError::Unsupported("find_files".into())) })
    }

    fn reply_permission<'a>(
        &'a self,
        session_id: &'a str,
        request_id: &'a str,
        decision: PermissionDecision,
        message: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let request = validate_id(request_id, "request id")?;
            // Neither protocol's approval reply carries text: a reason the
            // user wrote would be dropped on the floor, so it is refused.
            if message.is_some_and(|value| !value.trim().is_empty()) {
                return Err(AgentError::Unsupported("approval reason".into()));
            }
            {
                let (_, projection) = self.session_from_snapshot(&id).await?;
                let choice = v2::mapper::approval_decision(&projection, &request, decision)?;
                self.client
                    .dispatch(v2::commands::respond(&id, &request, Some(choice), None))
                    .await?;
                Ok(())
            }
        })
    }

    fn reply_form<'a>(
        &'a self,
        session_id: &'a str,
        form_id: &'a str,
        answers: Value,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let request = validate_id(form_id, "form id")?;
            {
                let (_, projection) = self.session_from_snapshot(&id).await?;
                v2::mapper::validate_answers(&projection, &request, &answers)?;
                self.client
                    .dispatch(v2::commands::respond(&id, &request, None, Some(answers)))
                    .await?;
                Ok(())
            }
        })
    }

    fn get_catalog<'a>(&'a self, _directory: Option<&'a str>) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move {
            let config = self.client.get_config().await?;
            Ok(mapper::map_catalog(&config))
        })
    }

    fn get_pending_permissions<'a>(
        &'a self,
        session_id: &'a str,
    ) -> AgentFuture<'a, Vec<PermissionRequest>> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let (_, thread) = self.session_from_snapshot(&id).await?;
            Ok(v2::mapper::permissions(&id, &thread))
        })
    }

    fn get_pending_forms<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, Vec<FormRequest>> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let (_, thread) = self.session_from_snapshot(&id).await?;
            Ok(v2::mapper::forms(&id, &thread))
        })
    }

    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            {
                let prepared = self.prepare_timeline(session_id, limit).await?;
                if let Some(commit) = prepared.commit {
                    commit()?;
                }
                Ok(prepared.items)
            }
        })
    }

    fn prepare_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, PreparedTimeline<'a>> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let prepared = self.client.prepare_timeline_snapshot(&id, limit).await?;
            let detail = &prepared.detail;
            let mut items = v2::mapper::timeline(
                &id,
                &detail["projection"],
                detail["snapshotSequence"].as_u64().unwrap_or(0),
            );
            if limit > 0 && items.len() > limit {
                items.drain(..items.len() - limit);
            }
            Ok(PreparedTimeline {
                items,
                commit: Some(Box::new(move || {
                    let commit = self.client.commit_timeline(&id, prepared, true)?;
                    self.watcher.watch(&id);
                    Ok(commit)
                })),
            })
        })
    }

    fn load_earlier_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let prepared = self.prepare_earlier_timeline(session_id, limit).await?;
            if let Some(commit) = prepared.commit {
                commit()?;
            }
            Ok(prepared.items)
        })
    }

    fn prepare_earlier_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, PreparedTimeline<'a>> {
        Box::pin(async move {
            if limit == 0 || limit > 2000 {
                return Err(AgentError::InvalidRequest(
                    "T3 history page limit must be between 1 and 2000".into(),
                ));
            }
            let id = validate_id(session_id, "session id")?;
            let Some(prepared) = self.client.prepare_earlier_snapshot(&id, limit).await? else {
                return Ok(PreparedTimeline::ready(Vec::new()));
            };
            let detail = &prepared.detail;
            let anchor = detail["gatewayHistoryAnchorId"].as_str();
            let mut items = v2::mapper::timeline(
                &id,
                &detail["projection"],
                detail["snapshotSequence"].as_u64().unwrap_or(0),
            );
            items.retain(|row| Some(row.id.as_str()) != anchor);
            Ok(PreparedTimeline {
                items,
                commit: Some(Box::new(move || {
                    self.client.commit_timeline(&id, prepared, true)
                })),
            })
        })
    }

    fn delete_session<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            self.client
                .dispatch(v2::commands::thread_delete(&id))
                .await?;
            self.client.forget_timeline(&id).await;
            self.watcher.unwatch(&id);
            Ok(())
        })
    }

    fn rename_session<'a>(&'a self, session_id: &'a str, title: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let title = title.trim();
            if title.is_empty() || title.chars().count() > 200 {
                return Err(AgentError::RequestFailed("invalid title".into()));
            }
            self.client
                .dispatch(v2::commands::rename(&id, title))
                .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `tokio::test`: constructing a driver spawns the RPC actor.
    #[tokio::test]
    async fn runtime_mode_is_validated_against_what_t3_accepts() {
        let endpoint = || {
            T3Endpoint::new(
                "http://127.0.0.1:1",
                super::super::endpoint::T3Credential::None,
            )
        };
        let driver = T3Driver::new(endpoint());
        assert_eq!(driver.runtime_mode, "full-access");
        driver.client().shutdown();
        let driver = T3Driver::new(endpoint())
            .with_runtime_mode("approval-required")
            .unwrap();
        assert_eq!(driver.runtime_mode, "approval-required");
        driver.client().shutdown();
        for bad in ["plan", "build"] {
            let attempt = T3Driver::new(endpoint());
            let client = attempt.client().clone();
            assert!(attempt.with_runtime_mode(bad).is_err(), "{bad}");
            client.shutdown();
        }
    }

    #[tokio::test]
    async fn list_sessions_honours_the_parent_filter() {
        use axum::{routing::get, Json, Router};
        let shell: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/shell_snapshot.json")).unwrap();
        let app = Router::new().route(
            "/api/orchestration/shell",
            get(move || {
                let shell = shell.clone();
                async move { Json(shell) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let driver = T3Driver::new(T3Endpoint::new(
            url,
            super::super::endpoint::T3Credential::Bearer("test-token".into()),
        ));
        let query = |parent: Option<&str>| SessionQuery {
            parent_id: parent.map(str::to_string),
            ..Default::default()
        };
        let all = driver.list_sessions(&query(None)).await.unwrap();
        assert!(!all.is_empty());
        assert_eq!(
            driver
                .list_sessions(&query(Some("null")))
                .await
                .unwrap()
                .len(),
            all.len(),
            "roots only keeps top-level threads"
        );
        assert!(driver
            .list_sessions(&query(Some("thread-x")))
            .await
            .unwrap()
            .is_empty());
        driver.client().shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn a_mode_on_create_and_switch_mode_are_unsupported() {
        let driver = T3Driver::new(T3Endpoint::new(
            "http://127.0.0.1:1",
            super::super::endpoint::T3Credential::None,
        ));
        assert!(matches!(
            driver.create_session(Some("/"), None, Some("build")).await,
            Err(AgentError::Unsupported(_))
        ));
        assert!(matches!(
            driver.switch_mode("t1", "plan").await,
            Err(AgentError::Unsupported(_))
        ));
        driver.client().shutdown();
    }

    /// Read-only, against the owner's own server: with `MUQUN_T3_PROBE=1`,
    /// reads the descriptor and lists sessions with the bearer the gateway
    /// stored (`MUQUN_T3_STATE_DIR`, default `~/.local/share/muqun-gateway`),
    /// or `T3_TOKEN`, at `T3_URL` (default loopback). Never creates a
    /// session or sends a prompt.
    #[tokio::test]
    #[ignore = "requires a running T3 Code server and MUQUN_T3_PROBE=1; read-only"]
    async fn live_read_only_probe() {
        if std::env::var("MUQUN_T3_PROBE").as_deref() != Ok("1") {
            eprintln!("MUQUN_T3_PROBE is not 1; skipping");
            return;
        }
        let url = std::env::var("T3_URL").unwrap_or_else(|_| "http://127.0.0.1:3773".into());
        let token = std::env::var("T3_TOKEN").ok().or_else(|| {
            // `state_dir()` is a scratch directory under `cfg(test)`.
            let dir = std::env::var_os("MUQUN_T3_STATE_DIR")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| {
                        std::path::PathBuf::from(home).join(".local/share/muqun-gateway")
                    })
                })?;
            crate::platform::store::read_t3_credential_at(&dir)
                .ok()
                .flatten()
                .filter(|stored| stored.url == url)
                .map(|stored| stored.token)
        });
        let token = token.expect("T3_TOKEN or a stored gateway credential for T3_URL");
        let endpoint = T3Endpoint::new(url, super::super::endpoint::T3Credential::Bearer(token));

        let descriptor = endpoint
            .describe(&reqwest::Client::new())
            .await
            .expect("descriptor");
        eprintln!(
            "server {} protocol {}",
            descriptor.server_version,
            descriptor.protocol_version()
        );
        assert_eq!(
            descriptor.unsupported_reason(),
            None,
            "a protocol this gateway speaks"
        );

        let driver = Arc::new(T3Driver::new(endpoint));
        assert!(driver.probe().await.unwrap(), "healthy");
        let sessions = driver
            .list_sessions(&SessionQuery::default())
            .await
            .expect("list sessions");
        let projects = driver.list_projects().await.expect("list projects");
        eprintln!("{} sessions in {} projects", sessions.len(), projects.len());
        for session in sessions.iter().take(3) {
            assert!(!session.asid.0.is_empty());
        }
        if let Ok(id) = std::env::var("T3_PROBE_SESSION") {
            let info = driver
                .get_session(&id)
                .await
                .expect("read selected session");
            assert_eq!(info.asid.0, id);
            let timeline = driver
                .prepare_timeline(&id, 100)
                .await
                .expect("read selected timeline");
            eprintln!(
                "selected session timeline prepared: {:?}",
                timeline.items.len()
            );
            drop(timeline);
            let mirror = Arc::new(crate::agents::adapters::memory_mirror::MemoryMirror::new());
            let service =
                crate::agents::use_cases::session_service::SessionService::with_memory_mirror(
                    driver.clone(),
                    mirror,
                );
            let window = service
                .history_window(&crate::agents::domain::AgentSessionId(id), None, true)
                .await
                .expect("selected child full history window");
            eprintln!("selected window rows: {}", window.snapshot.timeline.len());
        }
        driver.client().shutdown();
    }

    /// Against a real server: `T3_URL` plus `T3_TOKEN` (bearer) or
    /// `T3_PAIRING_TOKEN` (from `t3 pair`), and optionally `T3_MODEL` as
    /// `instance/model` (default `claudeAgent/claude-haiku-4-5`). Creates its
    /// own project in a temp dir and one thread; sends one tiny prompt.
    #[tokio::test]
    #[ignore = "requires a running T3 Code server"]
    async fn live_thread_lifecycle() {
        let endpoint = T3Endpoint::from_env().expect("T3_URL and a credential");
        let driver = T3Driver::new(endpoint);
        assert!(driver.probe().await.unwrap(), "descriptor answers");

        let catalog = driver.get_catalog(None).await.expect("catalog");
        assert!(!catalog.models.is_empty(), "a ready provider with models");
        let model = std::env::var("T3_MODEL")
            .ok()
            .and_then(|m| {
                m.split_once('/').map(|(p, m)| ModelRef {
                    provider_id: p.into(),
                    model_id: m.into(),
                    variant: None,
                })
            })
            .unwrap_or(ModelRef {
                provider_id: "claudeAgent".into(),
                model_id: "claude-haiku-4-5".into(),
                variant: None,
            });

        let dir = std::env::temp_dir().join(format!("t3-live-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_s = dir.to_string_lossy().to_string();

        let session = driver
            .create_session(Some(&dir_s), Some(&model), None)
            .await
            .expect("create");
        assert_eq!(session.directory.as_deref(), Some(dir_s.as_str()));
        let id = session.asid.0.clone();
        assert!(driver
            .list_projects()
            .await
            .unwrap()
            .iter()
            .any(|p| p.canonical == dir_s));
        assert!(driver
            .list_sessions(&SessionQuery {
                directory: Some(dir_s.clone()),
                ..Default::default()
            })
            .await
            .unwrap()
            .iter()
            .any(|s| s.asid.0 == id));

        driver
            .rename_session(&id, "gateway live test")
            .await
            .expect("rename");
        driver
            .send_prompt(&id, "Reply with exactly the word: pong", &[], None)
            .await
            .expect("prompt");
        let mut done = false;
        for _ in 0..120 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let info = driver.get_session(&id).await.expect("session");
            if info.outcome.is_some() {
                done = true;
                break;
            }
        }
        assert!(done, "the turn settled");
        let timeline = driver.get_timeline(&id, 100).await.expect("timeline");
        assert!(timeline
            .iter()
            .any(|i| i.role == crate::agents::domain::TimelineRole::User));
        assert!(timeline.iter().any(|i| matches!(&i.part, crate::agents::domain::AgentPart::Text { text } if text.to_lowercase().contains("pong"))));
        assert!(matches!(
            driver.get_vcs_diff(&id, "working").await,
            Err(AgentError::Unsupported(_))
        ));
        assert!(driver
            .get_pending_permissions(&id)
            .await
            .unwrap()
            .is_empty());
        driver
            .send_prompt(&id, "Write a detailed 2000-word explanation of sorting algorithms. Do not use tools or change files.", &[], None)
            .await
            .expect("start isolated stop check");
        let mut active = false;
        for _ in 0..30 {
            let snapshot = driver.client.thread_snapshot(&id, None).await.unwrap();
            if v2::mapper::interrupt_run(&snapshot["projection"]).is_ok() {
                active = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert!(active, "isolated run became interruptible");
        driver.interrupt(&id).await.expect("interrupt live V2 run");
        let mut interrupted = false;
        for _ in 0..30 {
            let info = driver.get_session(&id).await.unwrap();
            if info.outcome.as_deref() == Some("interrupted") {
                interrupted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert!(interrupted, "server confirmed interruption");
        driver.delete_session(&id).await.expect("delete");
        let mut removed = false;
        for _ in 0..30 {
            if !driver
                .list_sessions(&SessionQuery::default())
                .await
                .unwrap()
                .iter()
                .any(|session| session.asid.0 == id)
            {
                removed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert!(
            removed,
            "deleted test thread disappeared from the live list"
        );
        driver.client().shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
