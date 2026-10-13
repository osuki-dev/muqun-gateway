use serde_json::Value;
use std::sync::Arc;

use super::client::DeepseekClient;
use super::endpoint::DeepseekEndpoint;
use super::mapper;
use super::stream::DeepseekInteractions;
use crate::agents::domain::{
    AgentCatalog, AgentProject, AgentSessionId, AgentSessionInfo, AgentSessionStatus, FormRequest,
    ModelRef, PermissionDecision, PermissionRequest, SessionQuery, TimelineItem,
};
use crate::agents::ports::agent::{
    AgentError, AgentFuture, AgentPort, AttachmentMode, FileDiffItem,
};

pub struct DeepseekDriver {
    pub client: Arc<DeepseekClient>,
}

impl DeepseekDriver {
    pub fn new(endpoint: DeepseekEndpoint) -> Self {
        Self {
            client: Arc::new(DeepseekClient::new(endpoint)),
        }
    }

    pub fn client(&self) -> &DeepseekClient {
        &self.client
    }
}

impl AgentPort for DeepseekDriver {
    fn kind(&self) -> &'static str {
        "deepseek"
    }

    fn probe(&self) -> AgentFuture<'_, bool> {
        Box::pin(async move {
            let req_client = reqwest::Client::new();
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
            let sessions: Vec<AgentSessionInfo> = raw_sessions
                .iter()
                .filter_map(mapper::map_session)
                .filter(|s| query.keeps_parent(s.parent_id.as_deref()))
                .collect();
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
        _message: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
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

    fn get_vcs_diff<'a>(
        &'a self,
        _session_id: &'a str,
        _mode: &'a str,
    ) -> AgentFuture<'a, Vec<FileDiffItem>> {
        Box::pin(async move { Ok(vec![]) })
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
            let res = self.client.get_page(session_id, limit).await?;
            Ok(mapper::map_timeline_records(&res))
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
