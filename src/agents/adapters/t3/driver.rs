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
    bounded_prompt, commands, new_id, validate_id, ModelSelection, T3Client,
    DEFAULT_INTERACTION_MODE, DEFAULT_RUNTIME_MODE, RUNTIME_MODES,
};
use super::endpoint::T3Endpoint;
use super::mapper;
use super::stream::ThreadWatcher;
use crate::agents::domain::{
    AgentCatalog, AgentProject, AgentSessionInfo, FormRequest, ModelRef, PermissionDecision,
    PermissionRequest, SessionQuery, TimelineItem,
};
use crate::agents::ports::agent::{AgentError, AgentFuture, AgentPort, FileDiffItem};

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
        let thread = detail
            .get("thread")
            .cloned()
            .ok_or_else(|| AgentError::Protocol("thread snapshot without thread".into()))?;
        let roots = match self.client.shell_snapshot().await {
            Ok(shell) => mapper::project_roots(&shell),
            Err(e) => {
                tracing::debug!(error = %e, "t3 shell snapshot unavailable; session without directory");
                Default::default()
            }
        };
        let info = mapper::map_session(&thread, &roots)
            .ok_or_else(|| AgentError::Protocol("thread snapshot missing id".into()))?;
        Ok((info, thread))
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
        self.client
            .dispatch(commands::project_create(&project_id, &title, dir))
            .await?;
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
    fn kind(&self) -> &'static str {
        super::KIND
    }

    fn probe(&self) -> AgentFuture<'_, bool> {
        Box::pin(async move { Ok(self.client.endpoint.probe_healthy(self.client.http()).await) })
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
            let mut sessions = mapper::map_shell_sessions(&shell);
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
            let title = "New thread";
            self.client
                .dispatch(commands::thread_create(
                    &thread_id,
                    &project_id,
                    title,
                    &selection,
                    runtime_mode,
                    interaction_mode,
                ))
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
        _delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            if !attachments.is_empty() {
                return Err(AgentError::Unsupported("attachments".into()));
            }
            let text = bounded_prompt(text)?;
            let (info, thread) = self.session_from_snapshot(&id).await?;
            let runtime_mode = thread
                .get("runtimeMode")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_RUNTIME_MODE)
                .to_string();
            let interaction_mode = thread
                .get("interactionMode")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_INTERACTION_MODE)
                .to_string();
            let model = info.model.as_ref().map(mapper::model_selection_from_ref);
            self.client
                .dispatch(commands::turn_start(
                    &id,
                    &new_id(),
                    text,
                    model.as_ref(),
                    &runtime_mode,
                    &interaction_mode,
                ))
                .await?;
            Ok(())
        })
    }

    /// `message_id` is a T3 checkpoint turn count (`0` = before the first
    /// turn), a timeline message id, or the synthetic `turn:<turnId>` message
    /// id activities are filed under. T3 rolls back whole turns, so a message
    /// resolves to the turn count before the turn it belongs to.
    fn revert_session<'a>(
        &'a self,
        session_id: &'a str,
        message_id: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let target = mapper::t3_message_id(message_id.trim());
            let turn_count = if let Ok(n) = target.parse::<u64>() {
                n
            } else {
                let (_, thread) = self.session_from_snapshot(&id).await?;
                mapper::revert_turn_count(&thread, target).ok_or_else(|| {
                    AgentError::RequestFailed("no checkpoint for that message".into())
                })?
            };
            self.client
                .dispatch(commands::checkpoint_revert(&id, turn_count))
                .await?;
            Ok(())
        })
    }

    fn interrupt<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            self.client.dispatch(commands::turn_interrupt(&id)).await?;
            Ok(())
        })
    }

    fn switch_model<'a>(&'a self, session_id: &'a str, model: &'a ModelRef) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let selection = mapper::model_selection_from_ref(model);
            self.client
                .dispatch(commands::thread_set_model(&id, &selection))
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
        _message: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let request = validate_id(request_id, "request id")?;
            // Prefer the decisions the request itself offered; a label is
            // not necessarily a valid reply, but the decision ids are.
            let offered: Vec<Value> = match self.session_from_snapshot(&id).await {
                Ok((_, thread)) => thread
                    .get("activities")
                    .and_then(Value::as_array)
                    .and_then(|items| {
                        items.iter().find(|a| {
                            a.get("kind").and_then(Value::as_str) == Some("approval.requested")
                                && a.get("payload")
                                    .and_then(|p| p.get("requestId"))
                                    .and_then(Value::as_str)
                                    == Some(request.as_str())
                        })
                    })
                    .and_then(|a| {
                        a.get("payload")
                            .and_then(|p| p.get("options"))
                            .and_then(Value::as_array)
                    })
                    .cloned()
                    .unwrap_or_default(),
                Err(e) => {
                    tracing::debug!(error = %e, "t3 approval options unavailable; using defaults");
                    vec![]
                }
            };
            let t3_decision = mapper::decision_to_t3(decision, &offered);
            self.client
                .dispatch(commands::approval_respond(&id, &request, t3_decision))
                .await?;
            Ok(())
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
            if !answers.is_object() {
                return Err(AgentError::RequestFailed(
                    "answers must be an object".into(),
                ));
            }
            self.client
                .dispatch(commands::user_input_respond(&id, &request, answers))
                .await?;
            Ok(())
        })
    }

    fn get_catalog<'a>(&'a self, _directory: Option<&'a str>) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move {
            let config = self.client.get_config().await?;
            Ok(mapper::map_catalog(&config))
        })
    }

    /// `working` and `branch` both answer the thread's cumulative diff up to
    /// its latest checkpoint, which is what T3 keeps; `committed` has no
    /// T3 counterpart.
    fn get_vcs_diff<'a>(
        &'a self,
        session_id: &'a str,
        mode: &'a str,
    ) -> AgentFuture<'a, Vec<FileDiffItem>> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            if !matches!(mode, "working" | "branch") {
                return Err(AgentError::Unsupported(format!("vcs diff mode {mode}")));
            }
            let (_, thread) = self.session_from_snapshot(&id).await?;
            let Some(turn) = mapper::latest_checkpoint_turn(&thread) else {
                return Ok(vec![]);
            };
            let result = self.client.get_full_thread_diff(&id, turn).await?;
            let diff = result.get("diff").and_then(Value::as_str).unwrap_or("");
            Ok(mapper::map_unified_diff(diff))
        })
    }

    fn get_pending_permissions<'a>(
        &'a self,
        session_id: &'a str,
    ) -> AgentFuture<'a, Vec<PermissionRequest>> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let (_, thread) = self.session_from_snapshot(&id).await?;
            Ok(mapper::map_pending_permissions(&id, &thread))
        })
    }

    fn get_pending_forms<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, Vec<FormRequest>> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let (_, thread) = self.session_from_snapshot(&id).await?;
            Ok(mapper::map_pending_forms(&id, &thread))
        })
    }

    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            let detail = self.client.thread_snapshot(&id, None).await?;
            let thread = detail
                .get("thread")
                .ok_or_else(|| AgentError::Protocol("thread snapshot without thread".into()))?;
            self.watcher.watch(&id);
            let mut items = mapper::map_thread_timeline(&id, thread);
            if limit > 0 && items.len() > limit {
                items.drain(..items.len() - limit);
            }
            Ok(items)
        })
    }

    fn delete_session<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let id = validate_id(session_id, "session id")?;
            self.client.dispatch(commands::thread_delete(&id)).await?;
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
                .dispatch(commands::thread_rename(&id, title))
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
        let diff = driver.get_vcs_diff(&id, "working").await.expect("diff");
        assert!(diff.is_empty(), "nothing was written");
        assert!(driver
            .get_pending_permissions(&id)
            .await
            .unwrap()
            .is_empty());
        driver.delete_session(&id).await.expect("delete");
        assert!(matches!(
            driver.get_session(&id).await,
            Err(AgentError::SessionNotFound(_))
        ));
        driver.client().shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
