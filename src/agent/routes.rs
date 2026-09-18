use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::domain::{
    AgentDomainEvent, AgentSessionId, ModelRef, PermissionDecision, SessionQuery,
};
use crate::{api_error, content_envelope, require_device, validate_text, ApiResult, AppState};

#[derive(Debug, Deserialize)]
pub struct AgentDirectoriesQuery {
    pub prefix: Option<String>,
    pub query: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentSessionsQuery {
    pub directory: Option<String>,
    /// List the children of one session.
    pub parent_id: Option<String>,
    /// `true` lists top-level sessions only, leaving out every subagent
    /// session the `subagent` tool created.
    pub roots: Option<bool>,
    pub limit: Option<usize>,
    /// `asc` or `desc`.
    pub order: Option<String>,
    pub search: Option<String>,
    pub cursor: Option<String>,
}

impl AgentSessionsQuery {
    fn to_session_query(&self) -> SessionQuery {
        SessionQuery {
            directory: self.directory.clone(),
            // OpenCode takes the literal string `null` for "roots only".
            parent_id: match (self.parent_id.as_deref(), self.roots) {
                (Some(parent), _) if !parent.trim().is_empty() => Some(parent.to_string()),
                (_, Some(true)) => Some("null".to_string()),
                _ => None,
            },
            limit: self.limit,
            order: self.order.clone(),
            search: self.search.clone(),
            cursor: self.cursor.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AgentCatalogQuery {
    pub directory: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ShellOutputQuery {
    pub cursor: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    /// Defaults to true: an exported transcript leaves the device.
    pub sanitize: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct CompactBody {
    /// `steer` (the default) or `queue`.
    pub delivery: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RenameSessionBody {
    pub title: String,
}

#[derive(Debug, Deserialize)]
pub struct RunCommandBody {
    pub name: String,
    /// The argument string that fills `$ARGUMENTS`.
    pub arguments: Option<String>,
    pub delivery: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ViewSessionBody {
    /// The idle timestamp being acknowledged; defaults to now.
    pub idle: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct AgentEventsQuery {
    pub after: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct AgentVcsDiffQuery {
    /// `working` (default), `branch` or `committed`.
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentFilesQuery {
    pub query: Option<String>,
    pub limit: Option<usize>,
    pub directory: Option<String>,
}

/// `POST .../agent` accepts the documented `{"agent": "build"}` object and, as
/// a convenience, a bare `"build"` string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SwitchAgentBody {
    Wrapped { agent: String },
    Bare(String),
}

impl SwitchAgentBody {
    pub fn into_agent(self) -> String {
        match self {
            Self::Wrapped { agent } => agent,
            Self::Bare(agent) => agent,
        }
    }
}

/// `POST .../model` accepts both `{"model": {...}}` (what the app sends) and a
/// bare `ModelRef` (what this route used to require). Accepting only the bare
/// form made every model switch fail with 422.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SwitchModelBody {
    Wrapped { model: ModelRef },
    Bare(ModelRef),
}

impl SwitchModelBody {
    pub fn into_model(self) -> ModelRef {
        match self {
            Self::Wrapped { model } => model,
            Self::Bare(model) => model,
        }
    }
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

/// `POST …/revert/stage`. `files` asks OpenCode to work out the file changes
/// the rollback would undo and return them with the staged boundary.
#[derive(Debug, Deserialize)]
pub struct StageRevertBody {
    pub message_id: String,
    pub files: Option<bool>,
}

/// `POST …/skill`. `skill` is a `Skill.Info.id` from the catalog.
#[derive(Debug, Deserialize)]
pub struct ActivateSkillBody {
    pub skill: String,
    /// Whether OpenCode resumes the agent loop after appending the skill
    /// message. Omitted leaves OpenCode's own default alone.
    pub resume: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ReplyPermissionBody {
    pub decision: String,
    /// An optional reason, forwarded to OpenCode with a rejection.
    pub message: Option<String>,
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
            get(get_agent_session_global).delete(delete_agent_session_global),
        )
        .route(
            "/api/agent-sessions/{asid}/children",
            get(list_agent_session_children),
        )
        .route(
            "/api/agent-sessions/{asid}/rename",
            post(rename_agent_session),
        )
        .route(
            "/api/agent-sessions/{asid}/revert/clear",
            post(clear_agent_session_revert),
        )
        .route(
            "/api/agent-sessions/{asid}/skill",
            post(activate_agent_skill),
        )
        .route(
            "/api/agent-sessions/{asid}/compact",
            post(compact_agent_session),
        )
        .route(
            "/api/agent-sessions/{asid}/context",
            get(get_agent_session_context),
        )
        .route(
            "/api/agent-sessions/{asid}/background",
            post(background_agent_session),
        )
        .route("/api/agent-sessions/{asid}/wait", post(wait_agent_session))
        .route("/api/agent-sessions/{asid}/view", post(view_agent_session))
        .route(
            "/api/agent-sessions/{asid}/export",
            get(export_agent_session),
        )
        .route(
            "/api/agent-sessions/{asid}/command",
            post(run_agent_session_command),
        )
        .route("/api/agent-sessions/{asid}/inbox", get(get_agent_inbox))
        .route(
            "/api/agent-sessions/{asid}/inbox/{inbox_id}",
            delete(cancel_agent_inbox_item),
        )
        .route(
            "/api/agent-sessions/{asid}/inbox/{inbox_id}/steer",
            post(steer_agent_inbox_item),
        )
        .route(
            "/api/agent-sessions/{asid}/inbox/{inbox_id}/queue",
            post(queue_agent_inbox_item),
        )
        .route("/api/agent-engine", get(get_agent_engine_status))
        .route("/api/agent-shells", get(list_agent_shells))
        .route(
            "/api/agent-shells/{shell_id}",
            get(get_agent_shell).delete(kill_agent_shell),
        )
        .route(
            "/api/agent-shells/{shell_id}/output",
            get(get_agent_shell_output),
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
    query: &SessionQuery,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(manager) = state.agent_runtime.manager().await else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let sessions = manager
        .sessions()
        .list_sessions(query)
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

    let Some(manager) = state.agent_runtime.manager().await else {
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

    let Some(manager) = state.agent_runtime.manager().await else {
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

    let Some(manager) = state.agent_runtime.manager().await else {
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

    let Some(manager) = state.agent_runtime.manager().await else {
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

    let Some(manager) = state.agent_runtime.manager().await else {
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
    directory: Option<&str>,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(manager) = state.agent_runtime.manager().await else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let files = manager
        .engine()
        .find_files(query, limit, directory)
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

    let Some(manager) = state.agent_runtime.manager().await else {
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

    let Some(manager) = state.agent_runtime.manager().await else {
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

    let Some(manager) = state.agent_runtime.manager().await else {
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

    let Some(manager) = state.agent_runtime.manager().await else {
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

    let Some(manager) = state.agent_runtime.manager().await else {
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
        .reply_permission(
            &AgentSessionId(asid.to_string()),
            req_id,
            decision,
            body.message.as_deref(),
        )
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

    let Some(manager) = state.agent_runtime.manager().await else {
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
    mode: Option<&str>,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let Some(manager) = state.agent_runtime.manager().await else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let mode = match mode.unwrap_or("working") {
        m @ ("working" | "branch" | "committed") => m,
        _ => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_mode",
                "mode must be 'working', 'branch' or 'committed'",
            ));
        }
    };

    let diffs = manager
        .sessions()
        .get_vcs_diff(&AgentSessionId(asid.to_string()), mode)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!(diffs))))
}

pub(crate) fn json_etag_response(headers: &HeaderMap, payload: Value) -> Response {
    let body_bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&body_bytes);
    let hash = hasher.finalize();
    let etag = format!("\"{:x}\"", hash);

    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH).and_then(|h| h.to_str().ok()) {
        let trimmed = if_none_match.trim();
        if trimmed == etag || trimmed == "*" || trimmed == format!("W/{}", etag) {
            return (
                StatusCode::NOT_MODIFIED,
                [
                    (header::ETAG, etag),
                    (header::CACHE_CONTROL, "private, must-revalidate".to_string()),
                ],
            )
                .into_response();
        }
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "private, must-revalidate".to_string()),
        ],
        body_bytes,
    )
        .into_response()
}

async fn do_list_agent_projects(
    state: &AppState,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    require_device(state, headers)?;

    let Some(manager) = state.agent_runtime.manager().await else {
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

    Ok(json_etag_response(headers, content_envelope(json!(projects))))
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

    let Some(manager) = state.agent_runtime.manager().await else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let target_asid = AgentSessionId(asid.to_string());
    let mut rx = state.agent_runtime.subscribe_events();
    drop(manager);

    let stream = async_stream::stream! {
        yield Ok(Event::default().event("connected").data(serde_json::to_string(&json!({ "asid": target_asid.0 })).unwrap_or_default()));

        loop {
            match rx.recv().await {
                Ok(ev) => {
                    // An event with no session -- a global resync -- reaches
                    // every stream.
                    if !ev.asid().0.is_empty() && ev.asid() != &target_asid {
                        continue;
                    }
                    let ev_name = ev.event_name();

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
) -> ApiResult<Response> {
    require_device(&state, &headers)?;

    let Some(manager) = state.agent_runtime.manager().await else {
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

    Ok(json_etag_response(&headers, content_envelope(json!(catalog))))
}

// ---------------------------------------------------------------------------
// Global route handlers
// ---------------------------------------------------------------------------

async fn list_agent_sessions_global(
    State(state): State<AppState>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_list_agent_sessions(&state, &query.to_session_query(), &headers).await
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
    do_switch_agent_mode(&state, &asid, &body.into_agent(), &headers).await
}

async fn find_agent_files_root_global(
    State(state): State<AppState>,
    Query(query): Query<AgentFilesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_find_agent_files(
        &state,
        query.query.as_deref().unwrap_or(""),
        query.limit.unwrap_or(20),
        query.directory.as_deref(),
        &headers,
    )
    .await
}

async fn find_agent_files_global(
    State(state): State<AppState>,
    Path(_asid): Path<String>,
    Query(query): Query<AgentFilesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_find_agent_files(
        &state,
        query.query.as_deref().unwrap_or(""),
        query.limit.unwrap_or(20),
        query.directory.as_deref(),
        &headers,
    )
    .await
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
    Json(body): Json<SwitchModelBody>,
) -> ApiResult<Json<Value>> {
    do_switch_agent_model(&state, &asid, body.into_model(), &headers).await
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
    Query(query): Query<AgentVcsDiffQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_get_agent_vcs_diff(&state, &asid, query.mode.as_deref(), &headers).await
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
    do_list_agent_sessions(&state, &query.to_session_query(), &headers).await
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
    do_switch_agent_mode(&state, &asid, &body.into_agent(), &headers).await
}

async fn find_agent_files_root_legacy(
    State(state): State<AppState>,
    Path(_session_id): Path<String>,
    Query(query): Query<AgentFilesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_find_agent_files(
        &state,
        query.query.as_deref().unwrap_or(""),
        query.limit.unwrap_or(20),
        query.directory.as_deref(),
        &headers,
    )
    .await
}

async fn find_agent_files_legacy(
    State(state): State<AppState>,
    Path((_session_id, _asid)): Path<(String, String)>,
    Query(query): Query<AgentFilesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_find_agent_files(
        &state,
        query.query.as_deref().unwrap_or(""),
        query.limit.unwrap_or(20),
        query.directory.as_deref(),
        &headers,
    )
    .await
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
    Json(body): Json<SwitchModelBody>,
) -> ApiResult<Json<Value>> {
    do_switch_agent_model(&state, &asid, body.into_model(), &headers).await
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
    Query(query): Query<AgentVcsDiffQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_get_agent_vcs_diff(&state, &asid, query.mode.as_deref(), &headers).await
}

async fn get_pane_agent_catalog(
    State(state): State<AppState>,
    Path((_session_id, _pane_id)): Path<(String, String)>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    get_global_agent_catalog(State(state), Query(query), headers).await
}

async fn get_session_agent_catalog(
    State(state): State<AppState>,
    Path(_session_id): Path<String>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    get_global_agent_catalog(State(state), Query(query), headers).await
}

async fn list_agent_projects_global(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    do_list_agent_projects(&state, &headers).await
}

async fn list_agent_projects_legacy(
    State(state): State<AppState>,
    Path(_session_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
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



// ---------------------------------------------------------------------------
// Session operations added for OpenCode v2 parity. These are additive: every
// route that existed before behaves exactly as it did.
// ---------------------------------------------------------------------------

/// The manager, or the 503 every agent route answers with when OpenCode is not
/// reachable.
macro_rules! manager_or_unavailable {
    ($state:expr, $headers:expr) => {{
        require_device($state, $headers)?;
        match $state.agent_runtime.manager().await {
            Some(manager) => manager,
            None => {
                return Err(api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "agent_unavailable",
                    "Agent engine is not available",
                ));
            }
        }
    }};
}

fn engine_error(err: super::ports::engine::AgentEngineError) -> (StatusCode, Json<Value>) {
    api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &err.to_string())
}

async fn list_agent_session_children(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let mut session_query = query.to_session_query();
    session_query.parent_id = Some(asid);
    do_list_agent_sessions(&state, &session_query, &headers).await
}

async fn delete_agent_session_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    manager
        .driver()
        .client()
        .delete_session(&asid)
        .await
        .map_err(engine_error)?;
    // OpenCode deletes the children too, but it also announces each one on the
    // stream, so the mirror only has to forget this session here.
    manager
        .mirror()
        .remove_session(&AgentSessionId(asid.clone()))
        .await;
    Ok(Json(content_envelope(json!({ "deleted": true }))))
}

async fn rename_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RenameSessionBody>,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    validate_text(&body.title)?;
    manager
        .driver()
        .client()
        .rename_session(&asid, &body.title)
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(
        json!({ "renamed": true, "title": body.title }),
    )))
}

async fn clear_agent_session_revert(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    manager
        .driver()
        .client()
        .clear_revert(&asid)
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(json!({ "cleared": true }))))
}

/// Activate a skill by id. The activation is a message OpenCode appends to
/// the session, so what the user sees is a timeline row, not a reply here --
/// this answers as soon as OpenCode has accepted it.
async fn activate_agent_skill(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ActivateSkillBody>,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let skill = body.skill.trim();
    if skill.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_skill",
            "skill must not be empty",
        ));
    }
    validate_text(skill)?;
    manager
        .driver()
        .client()
        .activate_skill(&asid, skill, body.resume)
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(json!({ "status": "ok" }))))
}

async fn compact_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<CompactBody>>,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let delivery = body.and_then(|Json(b)| b.delivery);
    let res = manager
        .driver()
        .client()
        .compact_session(&asid, delivery.as_deref())
        .await
        .map_err(engine_error)?;
    // The reply is the inbox item the request was admitted as.
    Ok(Json(content_envelope(json!({
        "requested": true,
        "item": res.get("data").cloned().unwrap_or(Value::Null),
    }))))
}

async fn get_agent_session_context(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let messages = manager
        .driver()
        .client()
        .get_context(&asid)
        .await
        .map_err(engine_error)?;

    // The context window's token totals are the last assistant message's, which
    // is what "context used" is measured against.
    let tokens = messages
        .iter()
        .rev()
        .find_map(|m| m.get("tokens").cloned())
        .filter(|t| !t.is_null());

    Ok(Json(content_envelope(json!({
        "messages": messages.len(),
        "tokens": tokens,
    }))))
}

async fn background_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    manager
        .driver()
        .client()
        .background_session(&asid)
        .await
        .map_err(engine_error)?;
    // OpenCode has no event for this, so the tool cards are marked here.
    manager
        .mirror()
        .mark_running_tools_backgrounded(&AgentSessionId(asid))
        .await;
    Ok(Json(content_envelope(json!({ "backgrounded": true }))))
}

async fn wait_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    manager
        .driver()
        .client()
        .wait_session(&asid)
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(json!({ "idle": true }))))
}

async fn view_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<ViewSessionBody>>,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let idle = body.and_then(|Json(b)| b.idle).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    });
    manager
        .driver()
        .client()
        .view_session(&asid, idle)
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(json!({ "viewed": idle }))))
}

async fn export_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<ExportQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    // Sanitized by default: an export leaves the device.
    let sanitize = query.sanitize.unwrap_or(true);
    let res = manager
        .driver()
        .client()
        .export_session(&asid, sanitize)
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(
        res.get("data").cloned().unwrap_or(res),
    )))
}

async fn run_agent_session_command(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RunCommandBody>,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let name = body.name.trim().trim_start_matches('/');
    if name.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_command",
            "name must not be empty",
        ));
    }
    let arguments = body.arguments.unwrap_or_default();
    if !arguments.is_empty() {
        validate_text(&arguments)?;
    }
    manager
        .driver()
        .client()
        .run_command(&asid, name, &arguments, body.delivery.as_deref())
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(json!({ "submitted": true }))))
}

async fn get_agent_inbox(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let items = manager
        .driver()
        .client()
        .get_inbox(&asid)
        .await
        .map_err(engine_error)?;
    // Refresh the mirror so a later snapshot and the stream agree.
    manager
        .mirror()
        .set_inbox(&AgentSessionId(asid), items.clone())
        .await;
    Ok(Json(content_envelope(json!({ "items": items }))))
}

async fn cancel_agent_inbox_item(
    State(state): State<AppState>,
    Path((asid, inbox_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    manager
        .driver()
        .client()
        .cancel_inbox_item(&asid, &inbox_id)
        .await
        .map_err(engine_error)?;
    manager
        .mirror()
        .upsert_inbox_item(&AgentSessionId(asid), &inbox_id, None)
        .await;
    Ok(Json(content_envelope(json!({ "cancelled": true }))))
}

async fn steer_agent_inbox_item(
    State(state): State<AppState>,
    Path((asid, inbox_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    set_inbox_delivery(state, asid, inbox_id, "steer", headers).await
}

async fn queue_agent_inbox_item(
    State(state): State<AppState>,
    Path((asid, inbox_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    set_inbox_delivery(state, asid, inbox_id, "queue", headers).await
}

async fn set_inbox_delivery(
    state: AppState,
    asid: String,
    inbox_id: String,
    delivery: &str,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    manager
        .driver()
        .client()
        .set_inbox_delivery(&asid, &inbox_id, delivery)
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(
        json!({ "delivery": delivery, "inbox_id": inbox_id }),
    )))
}

/// What engine, if any, the gateway currently has. Unlike every other agent
/// route this one answers 200 with `available: false` rather than 503 -- the
/// app asks it precisely to find out why the others are refusing.
async fn get_agent_engine_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let status = state.agent_runtime.status().await;
    Ok(Json(content_envelope(json!(status))))
}

async fn list_agent_shells(
    State(state): State<AppState>,
    Query(query): Query<AgentCatalogQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let shells = manager
        .driver()
        .client()
        .list_shells(query.directory.as_deref())
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(json!(shells))))
}

async fn get_agent_shell(
    State(state): State<AppState>,
    Path(shell_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let shell = manager
        .driver()
        .client()
        .get_shell(&shell_id)
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(shell)))
}

async fn get_agent_shell_output(
    State(state): State<AppState>,
    Path(shell_id): Path<String>,
    Query(query): Query<ShellOutputQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let output = manager
        .driver()
        .client()
        .get_shell_output(&shell_id, query.cursor, query.limit)
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(output)))
}

async fn kill_agent_shell(
    State(state): State<AppState>,
    Path(shell_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    manager
        .driver()
        .client()
        .kill_shell(&shell_id)
        .await
        .map_err(engine_error)?;
    Ok(Json(content_envelope(json!({ "killed": true }))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_model_body_accepts_the_wrapped_shape_the_app_sends() {
        let body: SwitchModelBody = serde_json::from_value(json!({
            "model": { "provider_id": "opencode", "model_id": "glm-5.3-flash", "variant": "default" }
        }))
        .expect("wrapped body should parse");
        let model = body.into_model();
        assert_eq!(model.provider_id, "opencode");
        assert_eq!(model.model_id, "glm-5.3-flash");
        assert_eq!(model.variant.as_deref(), Some("default"));
    }

    #[test]
    fn switch_model_body_still_accepts_a_bare_model_ref() {
        let body: SwitchModelBody = serde_json::from_value(json!({
            "provider_id": "opencode",
            "model_id": "glm-5.3-flash"
        }))
        .expect("bare body should parse");
        let model = body.into_model();
        assert_eq!(model.model_id, "glm-5.3-flash");
        assert!(model.variant.is_none());
    }

    #[test]
    fn switch_model_body_rejects_a_model_with_no_id() {
        let parsed: Result<SwitchModelBody, _> =
            serde_json::from_value(json!({ "model": { "provider_id": "opencode" } }));
        assert!(parsed.is_err(), "a model without an id is not a ModelRef");
    }

    /// Every route in one table, mounted once. `matchit` panics on a routing
    /// conflict at insertion, so building the router is the assertion: the
    /// saved-permission paths sit under the same prefix as the reply path
    /// (`permissions/saved/{id}` beside `permissions/{req_id}/reply`) and a
    /// conflict there would take the whole gateway down at startup.
    #[test]
    fn every_agent_route_mounts_without_a_conflict() {
        let _router: Router<AppState> = mount(Router::new());
    }

    #[test]
    fn a_skill_body_takes_an_id_and_an_optional_resume() {
        let bare: ActivateSkillBody =
            serde_json::from_value(json!({ "skill": "docs" })).expect("skill alone parses");
        assert_eq!(bare.skill, "docs");
        assert!(bare.resume.is_none(), "an absent resume is OpenCode's default");

        let with_resume: ActivateSkillBody =
            serde_json::from_value(json!({ "skill": "docs", "resume": false }))
                .expect("resume parses");
        assert_eq!(with_resume.resume, Some(false));

        let no_skill: Result<ActivateSkillBody, _> = serde_json::from_value(json!({}));
        assert!(no_skill.is_err(), "skill is required");
    }

    #[test]
    fn switch_agent_body_accepts_object_and_bare_string() {
        let wrapped: SwitchAgentBody =
            serde_json::from_value(json!({ "agent": "build" })).expect("object should parse");
        assert_eq!(wrapped.into_agent(), "build");

        let bare: SwitchAgentBody =
            serde_json::from_value(json!("plan")).expect("bare string should parse");
        assert_eq!(bare.into_agent(), "plan");
    }
}
