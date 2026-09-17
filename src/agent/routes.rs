use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::domain::{AgentDomainEvent, AgentSessionId, ModelRef, PermissionDecision};
use crate::{api_error, content_envelope, require_device, validate_text, ApiResult, AppState};

#[derive(Debug, Deserialize)]
pub struct AgentDirectoriesQuery {
    pub prefix: Option<String>,
    pub query: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentSessionsQuery {
    pub directory: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentEventsQuery {
    pub after: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct AgentFilesQuery {
    pub query: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct SwitchAgentBody {
    pub agent: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateAgentSessionBody {
    pub directory: Option<String>,
    pub model: Option<ModelRef>,
    pub agent: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SendAgentPromptBody {
    pub text: String,
    pub attachments: Option<Vec<String>>,
    pub delivery: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RevertSessionBody {
    pub message_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ReplyPermissionBody {
    pub decision: String,
}

#[derive(Debug, Deserialize)]
pub struct ReplyFormBody {
    pub answers: serde_json::Map<String, Value>,
}

pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        // Standalone independent OpenCode agent routes (no tmux / herdr session required)
        .route(
            "/api/agent-sessions",
            get(list_agent_sessions_global).post(create_agent_session_global),
        )
        .route(
            "/api/agent-sessions/{asid}",
            get(get_agent_session_global),
        )
        .route(
            "/api/agent-sessions/{asid}/events",
            get(get_agent_session_events_global),
        )
        .route(
            "/api/agent-sessions/{asid}/timeline",
            get(get_agent_session_timeline_global),
        )
        .route(
            "/api/agent-sessions/{asid}/agent",
            post(switch_agent_mode_global),
        )
        .route(
            "/api/agent-files",
            get(find_agent_files_root_global),
        )
        .route(
            "/api/agent-sessions/{asid}/files",
            get(find_agent_files_global),
        )
        .route(
            "/api/agent-sessions/{asid}/prompt",
            post(send_agent_prompt_global),
        )
        .route(
            "/api/agent-sessions/{asid}/interrupt",
            post(interrupt_agent_session_global),
        )
        .route(
            "/api/agent-sessions/{asid}/model",
            post(switch_agent_model_global),
        )
        .route(
            "/api/agent-sessions/{asid}/permissions/{req_id}/reply",
            post(reply_agent_permission_global),
        )
        .route(
            "/api/agent-sessions/{asid}/forms/{form_id}/reply",
            post(reply_agent_form_global),
        )
        .route(
            "/api/agent-sessions/{asid}/vcs/diff",
            get(get_agent_vcs_diff_global),
        )
        .route(
            "/api/agent-sessions/{asid}/vcs-diff",
            get(get_agent_vcs_diff_global),
        )
        .route(
            "/api/agent-sessions/{asid}/abort",
            post(interrupt_agent_session_global),
        )
        .route(
            "/api/agent-sessions/{asid}/revert",
            post(revert_agent_session_global),
        )
        .route(
            "/api/agent-catalog",
            get(get_global_agent_catalog),
        )
        .route(
            "/api/agent-projects",
            get(list_agent_projects_global),
        )
        .route(
            "/api/agent-directories",
            get(list_agent_directories_global),
        )
        .route(
            "/api/agent-sessions/{asid}/stream",
            get(stream_agent_session_global),
        )
        // Backwards-compatible session-scoped routes (do NOT require session_id to exist in herdr/tmux)
        .route(
            "/api/sessions/{session_id}/agent-sessions",
            get(list_agent_sessions_legacy).post(create_agent_session_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}",
            get(get_agent_session_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/events",
            get(get_agent_session_events_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/timeline",
            get(get_agent_session_timeline_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/agent",
            post(switch_agent_mode_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-files",
            get(find_agent_files_root_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/files",
            get(find_agent_files_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/prompt",
            post(send_agent_prompt_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/interrupt",
            post(interrupt_agent_session_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/model",
            post(switch_agent_model_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/permissions/{req_id}/reply",
            post(reply_agent_permission_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/forms/{form_id}/reply",
            post(reply_agent_form_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/vcs/diff",
            get(get_agent_vcs_diff_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/vcs-diff",
            get(get_agent_vcs_diff_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/abort",
            post(interrupt_agent_session_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/revert",
            post(revert_agent_session_legacy),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/agent-catalog",
            get(get_pane_agent_catalog),
        )
        .route(
            "/api/sessions/{session_id}/agent-catalog",
            get(get_session_agent_catalog),
        )
        .route(
            "/api/sessions/{session_id}/agent-projects",
            get(list_agent_projects_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-directories",
            get(list_agent_directories_legacy),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/stream",
            get(stream_agent_session_legacy),
        )
}

// ---------------------------------------------------------------------------
// Core implementation functions (pure OpenCode, zero herdr/tmux dependency)
// ---------------------------------------------------------------------------

async fn do_list_agent_sessions(
    state: &AppState,
    directory: Option<&str>,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let sessions = manager
        .sessions()
        .list_sessions(directory)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!(sessions))))
}

async fn do_create_agent_session(
    state: &AppState,
    body: CreateAgentSessionBody,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let validated_dir = match body.directory.as_deref() {
        Some(dir) if !dir.trim().is_empty() => {
            let p = std::path::Path::new(dir.trim());
            if !p.is_absolute() {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_directory",
                    "Working directory must be an absolute path",
                ));
            }
            let canonical = std::fs::canonicalize(p).map_err(|e| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "directory_not_found",
                    &format!("Directory does not exist or is inaccessible: {e}"),
                )
            })?;
            if !canonical.is_dir() {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "not_a_directory",
                    "Specified path is not a directory",
                ));
            }
            Some(canonical.to_string_lossy().to_string())
        }
        _ => None,
    };

    let session = manager
        .sessions()
        .create_session(
            validated_dir.as_deref(),
            body.model.as_ref(),
            body.agent.as_deref(),
        )
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!(session))))
}

async fn do_get_agent_session(
    state: &AppState,
    asid: &str,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let snapshot = manager
        .sessions()
        .get_snapshot(&AgentSessionId(asid.to_string()))
        .await
        .map_err(|e| api_error(StatusCode::NOT_FOUND, "session_not_found", &e.to_string()))?;

    Ok(Json(content_envelope(json!(snapshot))))
}

async fn do_get_agent_session_events(
    state: &AppState,
    asid: &str,
    after_seq: u64,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    match manager
        .sessions()
        .get_events_after(&AgentSessionId(asid.to_string()), after_seq)
        .await
    {
        Some(events) => Ok(Json(content_envelope(json!(events)))),
        None => Err(api_error(
            StatusCode::GONE,
            "resync_required",
            "Requested events are no longer in the ring buffer; re-fetch the snapshot",
        )),
    }
}

async fn do_get_agent_session_timeline(
    state: &AppState,
    asid: &str,
    after_seq: u64,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let (items, status, resync, latest_seq) = manager
        .mirror()
        .get_timeline_delta(&AgentSessionId(asid.to_string()), after_seq)
        .await;

    let payload = json!({
        "items": items,
        "status": status,
        "resync": resync,
        "latest_seq": latest_seq,
    });

    Ok(Json(content_envelope(payload)))
}

async fn do_switch_agent_mode(
    state: &AppState,
    asid: &str,
    agent: &str,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    manager
        .engine()
        .switch_agent(asid, agent)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "ok": true }))))
}

async fn do_find_agent_files(
    state: &AppState,
    query: &str,
    limit: usize,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let files = manager
        .engine()
        .find_files(query, limit)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "fs_error", &e.to_string()))?;

    let hits: Vec<Value> = files.iter().filter_map(|f| {
        let path = f.get("path").and_then(Value::as_str)?;
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);
        let kind = f.get("type").and_then(Value::as_str).unwrap_or("file");
        Some(json!({
            "path": path,
            "name": name,
            "kind": kind,
        }))
    }).collect();

    Ok(Json(content_envelope(json!(hits))))
}

async fn do_send_agent_prompt(
    state: &AppState,
    asid: &str,
    body: SendAgentPromptBody,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    validate_text(&body.text)?;

    let attachments = body.attachments.as_deref().unwrap_or(&[]);
    manager
        .prompts()
        .send_prompt(
            &AgentSessionId(asid.to_string()),
            &body.text,
            attachments,
            body.delivery.as_deref(),
        )
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "submitted": true }))))
}

async fn do_revert_agent_session(
    state: &AppState,
    asid: &str,
    body: RevertSessionBody,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    manager
        .sessions()
        .revert_session(&AgentSessionId(asid.to_string()), &body.message_id)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({
        "status": "ok",
        "reverted_to": body.message_id,
    }))))
}

async fn do_interrupt_agent_session(
    state: &AppState,
    asid: &str,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    manager
        .prompts()
        .interrupt(&AgentSessionId(asid.to_string()))
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "interrupted": true }))))
}

async fn do_switch_agent_model(
    state: &AppState,
    asid: &str,
    model: ModelRef,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    manager
        .sessions()
        .switch_model(&AgentSessionId(asid.to_string()), &model)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "switched": true }))))
}

async fn do_reply_agent_permission(
    state: &AppState,
    asid: &str,
    req_id: &str,
    body: ReplyPermissionBody,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let decision = match body.decision.as_str() {
        "allow" | "once" => PermissionDecision::Allow,
        "allow_always" | "always" => PermissionDecision::AllowAlways,
        "deny" | "reject" => PermissionDecision::Deny,
        _ => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_decision",
                "decision must be 'allow', 'allow_always', or 'deny'",
            ));
        }
    };

    manager
        .interactions()
        .reply_permission(&AgentSessionId(asid.to_string()), req_id, decision)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "replied": true }))))
}

async fn do_reply_agent_form(
    state: &AppState,
    asid: &str,
    form_id: &str,
    body: ReplyFormBody,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    manager
        .interactions()
        .reply_form(
            &AgentSessionId(asid.to_string()),
            form_id,
            serde_json::Value::Object(body.answers),
        )
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "replied": true }))))
}

async fn do_get_agent_vcs_diff(
    state: &AppState,
    asid: &str,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let diffs = manager
        .sessions()
        .get_vcs_diff(&AgentSessionId(asid.to_string()))
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!(diffs))))
}

async fn do_list_agent_projects(
    state: &AppState,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let projects = manager
        .engine()
        .list_projects()
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!(projects))))
}

async fn do_list_agent_directories(
    state: &AppState,
    query: AgentDirectoriesQuery,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let search_dir = query.prefix.as_deref().unwrap_or("~");
    let expanded = if search_dir.starts_with("~/") || search_dir == "~" {
        if let Some(home) = dirs::home_dir() {
            if search_dir == "~" {
                home
            } else {
                home.join(&search_dir[2..])
            }
        } else {
            std::path::PathBuf::from(search_dir)
        }
    } else {
        std::path::PathBuf::from(search_dir)
    };

    let mut dirs_list = Vec::new();
    let target = if expanded.is_dir() {
        expanded
    } else {
        expanded.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| std::path::PathBuf::from("/"))
    };

    if let Ok(mut entries) = tokio::fs::read_dir(&target).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(file_type) = entry.file_type().await {
                if file_type.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.') || query.query.as_deref().map(|q| q.starts_with('.')).unwrap_or(false) {
                        let full_path = entry.path().to_string_lossy().to_string();
                        dirs_list.push(json!({
                            "name": name,
                            "path": full_path,
                        }));
                    }
                }
            }
        }
    }
    dirs_list.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
    });
    if dirs_list.len() > 50 {
        dirs_list.truncate(50);
    }

    Ok(Json(content_envelope(json!(dirs_list))))
}

async fn do_stream_agent_session(
    state: &AppState,
    asid: &str,
    headers: &HeaderMap,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>, (StatusCode, Json<Value>)> {
    require_device(state, headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let target_asid = AgentSessionId(asid.to_string());
    let mut rx = manager.subscribe_events();

    let stream = async_stream::stream! {
        yield Ok(Event::default().event("connected").data(serde_json::to_string(&json!({ "asid": target_asid.0 })).unwrap_or_default()));

        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let matches = match &ev {
                        AgentDomainEvent::TimelineUpsert { asid, .. } => asid == &target_asid,
                        AgentDomainEvent::TimelineRemoved { asid, .. } => asid == &target_asid,
                        AgentDomainEvent::StatusChanged { asid, .. } => asid == &target_asid,
                        AgentDomainEvent::SessionUpdated { asid, .. } => asid == &target_asid,
                        AgentDomainEvent::PermissionPending { asid, .. } => asid == &target_asid,
                        AgentDomainEvent::PermissionResolved { asid, .. } => asid == &target_asid,
                        AgentDomainEvent::FormPending { asid, .. } => asid == &target_asid,
                        AgentDomainEvent::FormResolved { asid, .. } => asid == &target_asid,
                        AgentDomainEvent::Resync { asid, .. } => asid == &target_asid,
                    };
                    if !matches {
                        continue;
                    }

                    let ev_name = match &ev {
                        AgentDomainEvent::TimelineUpsert { .. } => "agent.timeline.upsert",
                        AgentDomainEvent::TimelineRemoved { .. } => "agent.timeline.removed",
                        AgentDomainEvent::StatusChanged { .. } => "agent.status.changed",
                        AgentDomainEvent::SessionUpdated { .. } => "agent.session.updated",
                        AgentDomainEvent::PermissionPending { .. } => "agent.permission.pending",
                        AgentDomainEvent::PermissionResolved { .. } => "agent.permission.resolved",
                        AgentDomainEvent::FormPending { .. } => "agent.form.pending",
                        AgentDomainEvent::FormResolved { .. } => "agent.form.resolved",
                        AgentDomainEvent::Resync { .. } => "agent.resync",
                    };

                    let payload = serde_json::to_string(&ev).unwrap_or_default();
                    yield Ok(Event::default().event(ev_name).data(payload));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    yield Ok(Event::default().event("agent.resync").data(serde_json::to_string(&json!({ "asid": target_asid.0 })).unwrap_or_default()));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15))))
}

pub async fn get_global_agent_catalog(
    State(state): State<AppState>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let catalog = manager
        .sessions()
        .get_catalog(query.directory.as_deref())
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!(catalog))))
}

// ---------------------------------------------------------------------------
// Global route handlers
// ---------------------------------------------------------------------------

async fn list_agent_sessions_global(
    State(state): State<AppState>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_list_agent_sessions(&state, query.directory.as_deref(), &headers).await
}

async fn create_agent_session_global(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateAgentSessionBody>,
) -> ApiResult<Json<Value>> {
    do_create_agent_session(&state, body, &headers).await
}

async fn get_agent_session_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_get_agent_session(&state, &asid, &headers).await
}

async fn get_agent_session_events_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<AgentEventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_get_agent_session_events(&state, &asid, query.after.unwrap_or(0), &headers).await
}

async fn get_agent_session_timeline_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<AgentEventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_get_agent_session_timeline(&state, &asid, query.after.unwrap_or(0), &headers).await
}

async fn switch_agent_mode_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SwitchAgentBody>,
) -> ApiResult<Json<Value>> {
    do_switch_agent_mode(&state, &asid, &body.agent, &headers).await
}

async fn find_agent_files_root_global(
    State(state): State<AppState>,
    Query(query): Query<AgentFilesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_find_agent_files(&state, query.query.as_deref().unwrap_or(""), query.limit.unwrap_or(20), &headers).await
}

async fn find_agent_files_global(
    State(state): State<AppState>,
    Path(_asid): Path<String>,
    Query(query): Query<AgentFilesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_find_agent_files(&state, query.query.as_deref().unwrap_or(""), query.limit.unwrap_or(20), &headers).await
}

async fn send_agent_prompt_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SendAgentPromptBody>,
) -> ApiResult<Json<Value>> {
    do_send_agent_prompt(&state, &asid, body, &headers).await
}

async fn interrupt_agent_session_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_interrupt_agent_session(&state, &asid, &headers).await
}

async fn revert_agent_session_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RevertSessionBody>,
) -> ApiResult<Json<Value>> {
    do_revert_agent_session(&state, &asid, body, &headers).await
}

async fn switch_agent_model_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(model): Json<ModelRef>,
) -> ApiResult<Json<Value>> {
    do_switch_agent_model(&state, &asid, model, &headers).await
}

async fn reply_agent_permission_global(
    State(state): State<AppState>,
    Path((asid, req_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ReplyPermissionBody>,
) -> ApiResult<Json<Value>> {
    do_reply_agent_permission(&state, &asid, &req_id, body, &headers).await
}

async fn reply_agent_form_global(
    State(state): State<AppState>,
    Path((asid, form_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ReplyFormBody>,
) -> ApiResult<Json<Value>> {
    do_reply_agent_form(&state, &asid, &form_id, body, &headers).await
}

async fn get_agent_vcs_diff_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_get_agent_vcs_diff(&state, &asid, &headers).await
}

// ---------------------------------------------------------------------------
// Legacy route handlers (session_id is ignored)
// ---------------------------------------------------------------------------

async fn list_agent_sessions_legacy(
    State(state): State<AppState>,
    Path(_session_id): Path<String>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_list_agent_sessions(&state, query.directory.as_deref(), &headers).await
}

async fn create_agent_session_legacy(
    State(state): State<AppState>,
    Path(_session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateAgentSessionBody>,
) -> ApiResult<Json<Value>> {
    do_create_agent_session(&state, body, &headers).await
}

async fn get_agent_session_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_get_agent_session(&state, &asid, &headers).await
}

async fn get_agent_session_events_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    Query(query): Query<AgentEventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_get_agent_session_events(&state, &asid, query.after.unwrap_or(0), &headers).await
}

async fn get_agent_session_timeline_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    Query(query): Query<AgentEventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_get_agent_session_timeline(&state, &asid, query.after.unwrap_or(0), &headers).await
}

async fn switch_agent_mode_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SwitchAgentBody>,
) -> ApiResult<Json<Value>> {
    do_switch_agent_mode(&state, &asid, &body.agent, &headers).await
}

async fn find_agent_files_root_legacy(
    State(state): State<AppState>,
    Path(_session_id): Path<String>,
    Query(query): Query<AgentFilesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_find_agent_files(&state, query.query.as_deref().unwrap_or(""), query.limit.unwrap_or(20), &headers).await
}

async fn find_agent_files_legacy(
    State(state): State<AppState>,
    Path((_session_id, _asid)): Path<(String, String)>,
    Query(query): Query<AgentFilesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_find_agent_files(&state, query.query.as_deref().unwrap_or(""), query.limit.unwrap_or(20), &headers).await
}

async fn send_agent_prompt_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SendAgentPromptBody>,
) -> ApiResult<Json<Value>> {
    do_send_agent_prompt(&state, &asid, body, &headers).await
}

async fn interrupt_agent_session_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_interrupt_agent_session(&state, &asid, &headers).await
}

async fn revert_agent_session_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RevertSessionBody>,
) -> ApiResult<Json<Value>> {
    do_revert_agent_session(&state, &asid, body, &headers).await
}

async fn switch_agent_model_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
    Json(model): Json<ModelRef>,
) -> ApiResult<Json<Value>> {
    do_switch_agent_model(&state, &asid, model, &headers).await
}

async fn reply_agent_permission_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid, req_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<ReplyPermissionBody>,
) -> ApiResult<Json<Value>> {
    do_reply_agent_permission(&state, &asid, &req_id, body, &headers).await
}

async fn reply_agent_form_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid, form_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<ReplyFormBody>,
) -> ApiResult<Json<Value>> {
    do_reply_agent_form(&state, &asid, &form_id, body, &headers).await
}

async fn get_agent_vcs_diff_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_get_agent_vcs_diff(&state, &asid, &headers).await
}

async fn get_pane_agent_catalog(
    State(state): State<AppState>,
    Path((_session_id, _pane_id)): Path<(String, String)>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    get_global_agent_catalog(State(state), Query(query), headers).await
}

async fn get_session_agent_catalog(
    State(state): State<AppState>,
    Path(_session_id): Path<String>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    get_global_agent_catalog(State(state), Query(query), headers).await
}

async fn list_agent_projects_global(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_list_agent_projects(&state, &headers).await
}

async fn list_agent_projects_legacy(
    State(state): State<AppState>,
    Path(_session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_list_agent_projects(&state, &headers).await
}

async fn list_agent_directories_global(
    State(state): State<AppState>,
    Query(query): Query<AgentDirectoriesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_list_agent_directories(&state, query, &headers).await
}

async fn list_agent_directories_legacy(
    State(state): State<AppState>,
    Path(_session_id): Path<String>,
    Query(query): Query<AgentDirectoriesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_list_agent_directories(&state, query, &headers).await
}

async fn stream_agent_session_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>, (StatusCode, Json<Value>)> {
    do_stream_agent_session(&state, &asid, &headers).await
}

async fn stream_agent_session_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>, (StatusCode, Json<Value>)> {
    do_stream_agent_session(&state, &asid, &headers).await
}

