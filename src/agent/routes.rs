use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::domain::{AgentSessionId, ModelRef, PermissionDecision};
use crate::{api_error, content_envelope, find_session, require_device, validate_text, ApiResult, AppState};

#[derive(Deserialize)]
pub struct AgentSessionsQuery {
    pub directory: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateAgentSessionBody {
    pub directory: Option<String>,
    pub model: Option<ModelRef>,
    pub agent: Option<String>,
}

#[derive(Deserialize)]
pub struct AgentEventsQuery {
    pub after: Option<u64>,
}

#[derive(Deserialize)]
pub struct SendAgentPromptBody {
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Deserialize)]
pub struct ReplyPermissionBody {
    pub decision: String,
}

#[derive(Deserialize)]
pub struct ReplyFormBody {
    pub answers: Value,
}

pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/api/sessions/{session_id}/agent-sessions",
            get(list_agent_sessions).post(create_agent_session),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}",
            get(get_agent_session),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/events",
            get(get_agent_session_events),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/prompt",
            post(send_agent_prompt),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/interrupt",
            post(interrupt_agent_session),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/model",
            post(switch_agent_model),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/permissions/{req_id}/reply",
            post(reply_agent_permission),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/forms/{form_id}/reply",
            post(reply_agent_form),
        )
        .route(
            "/api/sessions/{session_id}/agent-sessions/{asid}/vcs/diff",
            get(get_agent_vcs_diff),
        )
        .route(
            "/api/sessions/{session_id}/panes/{pane_id}/agent-catalog",
            get(get_pane_agent_catalog),
        )
        .route(
            "/api/sessions/{session_id}/agent-catalog",
            get(get_pane_agent_catalog),
        )
}

async fn list_agent_sessions(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let sessions = manager
        .sessions()
        .list_sessions(query.directory.as_deref())
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!(sessions))))
}

async fn create_agent_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateAgentSessionBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let session = manager
        .sessions()
        .create_session(
            body.directory.as_deref(),
            body.model.as_ref(),
            body.agent.as_deref(),
        )
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!(session))))
}

async fn get_agent_session(
    State(state): State<AppState>,
    Path((session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let snapshot = manager
        .sessions()
        .get_snapshot(&AgentSessionId(asid))
        .await
        .map_err(|e| api_error(StatusCode::NOT_FOUND, "session_not_found", &e.to_string()))?;

    Ok(Json(content_envelope(json!(snapshot))))
}

async fn get_agent_session_events(
    State(state): State<AppState>,
    Path((session_id, asid)): Path<(String, String)>,
    Query(query): Query<AgentEventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let after_seq = query.after.unwrap_or(0);
    match manager
        .sessions()
        .get_events_after(&AgentSessionId(asid), after_seq)
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

async fn send_agent_prompt(
    State(state): State<AppState>,
    Path((session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SendAgentPromptBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    validate_text(&body.text)?;

    manager
        .prompts()
        .send_prompt(&AgentSessionId(asid), &body.text, &body.attachments)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "submitted": true }))))
}

async fn interrupt_agent_session(
    State(state): State<AppState>,
    Path((session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    manager
        .prompts()
        .interrupt(&AgentSessionId(asid))
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "interrupted": true }))))
}

async fn switch_agent_model(
    State(state): State<AppState>,
    Path((session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
    Json(model): Json<ModelRef>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    manager
        .sessions()
        .switch_model(&AgentSessionId(asid), &model)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "switched": true }))))
}

async fn reply_agent_permission(
    State(state): State<AppState>,
    Path((session_id, asid, req_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<ReplyPermissionBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

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
        .reply_permission(&AgentSessionId(asid), &req_id, decision)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "replied": true }))))
}

async fn reply_agent_form(
    State(state): State<AppState>,
    Path((session_id, asid, form_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<ReplyFormBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    manager
        .interactions()
        .reply_form(&AgentSessionId(asid), &form_id, body.answers)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "replied": true }))))
}

async fn get_pane_agent_catalog(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

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

async fn get_agent_vcs_diff(
    State(state): State<AppState>,
    Path((session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let _session = find_session(&state.config, &session_id)?;

    let Some(ref manager) = state.agent_manager else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "Agent engine is not available",
        ));
    };

    let diffs = manager
        .sessions()
        .get_vcs_diff(&AgentSessionId(asid))
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_engine_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!(diffs))))
}
