use axum::{
    extract::{Extension, Path, Query, State},
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

use super::domain::{AgentSessionId, ModelRef, PermissionDecision, SessionQuery};
use super::ports::mirror::SessionMirrorPort;
use crate::{
    api_error, content_envelope, require_device, still_paired, stream_event, validate_text,
    ApiResult, AppState, EncryptedStreamContext, EventStreamSealer, STREAM_DEVICE_RECHECK_INTERVAL,
};

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
    /// Restrict the list (or the catalog) to one agent. Absent, the list
    /// merges every attached agent.
    pub agent_id: Option<String>,
}

/// `?agent_id=` alone, on the routes that read one agent.
#[derive(Debug, Deserialize)]
pub struct AgentQuery {
    pub agent_id: Option<String>,
}

/// What a session list answers when the caller does not say.
///
/// The app already asks for fifty; a caller that asks for nothing used to get
/// every session the agent had ever seen. A page is the right default for a
/// list a phone scrolls.
const DEFAULT_SESSION_LIMIT: usize = 50;

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
            limit: Some(self.limit.unwrap_or(DEFAULT_SESSION_LIMIT)),
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
pub enum SwitchModeBody {
    Wrapped { mode: String },
    Bare(String),
}

impl SwitchModeBody {
    pub fn into_mode(self) -> String {
        match self {
            Self::Wrapped { mode } => mode,
            Self::Bare(mode) => mode,
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
    pub mode: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SendAgentPromptBody {
    pub text: String,
    pub attachments: Option<Vec<String>>,
    pub delivery: Option<String>,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
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

/// `?directory=` on the worktree routes: the **project** directory whose
/// inventory is being read or changed, not a worktree's own.
#[derive(Debug, Deserialize)]
pub struct AgentWorktreesQuery {
    pub directory: Option<String>,
}

/// `POST /api/agent-worktrees`. `directory` is the project; the rest is
/// `Worktree.CreateInput`, every field of which 2.0.1 makes optional.
#[derive(Debug, Deserialize)]
pub struct CreateWorktreeBody {
    /// The project directory to create the worktree under.
    pub directory: Option<String>,
    /// The worktree directory's name. Omitted, OpenCode names it.
    pub name: Option<String>,
    /// An **existing** ref to branch from -- not a name to create.
    pub branch: Option<String>,
    pub from: Option<String>,
    pub strategy: Option<String>,
}

/// `DELETE /api/agent-worktrees`. Two directories, and they are not the same
/// one: `directory` is the project, `worktree` is the thing being removed.
#[derive(Debug, Deserialize)]
pub struct RemoveWorktreeBody {
    pub directory: Option<String>,
    pub worktree: String,
    /// `Worktree.RemoveInput.force` is required by OpenCode; the gateway
    /// defaults it to false rather than making every caller spell it.
    #[serde(default)]
    pub force: bool,
}

/// `POST /api/agent-worktrees/refresh`.
#[derive(Debug, Deserialize)]
pub struct RefreshWorktreesBody {
    pub directory: Option<String>,
}

/// `POST /api/agent-sessions/{asid}/move`.
#[derive(Debug, Deserialize)]
pub struct MoveSessionBody {
    pub directory: String,
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
    super::ws_routes::mount(router)
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
            "/api/agent-sessions/{asid}/revert/stage",
            post(stage_agent_session_revert),
        )
        .route(
            "/api/agent-sessions/{asid}/revert/commit",
            post(commit_agent_session_revert),
        )
        .route(
            "/api/agent-sessions/{asid}/skill",
            post(activate_agent_skill),
        )
        .route(
            "/api/agent-worktrees",
            get(list_agent_worktrees)
                .post(create_agent_worktree)
                .delete(remove_agent_worktree),
        )
        .route(
            "/api/agent-worktrees/refresh",
            post(refresh_agent_worktrees),
        )
        .route("/api/agent-sessions/{asid}/move", post(move_agent_session))
        .route(
            "/api/agent-sessions/{asid}/permissions/saved",
            get(list_saved_agent_permissions),
        )
        .route(
            "/api/agent-sessions/{asid}/permissions/saved/{saved_id}",
            delete(forget_saved_agent_permission),
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
        .route("/api/agent-status", get(get_agent_status))
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
        .route("/api/agent-sessions/{asid}/mode", post(switch_mode_global))
        .route("/api/agent-files", get(find_agent_files_root_global))
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
        .route("/api/agent-catalog", get(get_global_agent_catalog))
        .route("/api/agent-projects", get(list_agent_projects_global))
        .route("/api/agent-directories", get(list_agent_directories_global))
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
            "/api/sessions/{session_id}/agent-sessions/{asid}/mode",
            post(switch_mode_legacy),
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

/// The manager of one named agent. An id this gateway has never heard of is
/// `400 invalid_agent`; a known one that is not attached is
/// `503 agent_unavailable`, naming it.
async fn agent_manager_or_err(
    state: &AppState,
    agent_id: &str,
) -> ApiResult<std::sync::Arc<super::manager::AgentManager>> {
    if !state.agent_runtime.is_known_agent(agent_id) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_agent",
            &format!("Unknown agent '{agent_id}'"),
        ));
    }
    state
        .agent_runtime
        .manager_for_agent(agent_id)
        .await
        .ok_or_else(|| {
            api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent_unavailable",
                &format!("Agent '{agent_id}' is not available"),
            )
        })
}

/// The managers a read covers: the named agent alone, or every attached one.
async fn managers_for_read(
    state: &AppState,
    agent_id: Option<&str>,
) -> ApiResult<Vec<std::sync::Arc<super::manager::AgentManager>>> {
    if let Some(agent_id) = agent_id {
        return Ok(vec![agent_manager_or_err(state, agent_id).await?]);
    }
    let all_managers = state.agent_runtime.all_managers().await;
    if all_managers.is_empty() {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "No agent is available",
        ));
    }
    Ok(all_managers)
}

async fn do_list_agent_sessions(
    state: &AppState,
    query: &SessionQuery,
    agent_id: Option<&str>,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    require_device(state, headers)?;

    let all_managers = managers_for_read(state, agent_id).await?;

    let mut sessions = Vec::new();
    let mut any_success = false;
    let mut last_err = None;

    for manager in &all_managers {
        match manager.sessions().list_sessions(query).await {
            Ok(list) => {
                any_success = true;
                for s in &list {
                    state
                        .agent_runtime
                        .record_session_route(&s.asid.0, manager.agent().kind())
                        .await;
                }
                sessions.extend(list);
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = manager.agent().kind(),
                    error = %e,
                    "agent session list failed; skipping this agent"
                );
                last_err = Some(e);
            }
        }
    }

    // An agent that errors is skipped while another answers. When every one
    // asked failed, or the caller named the one that failed, say so.
    if !any_success || (agent_id.is_some() && last_err.is_some()) {
        if let Some(err) = last_err {
            return Err(agent_error(err));
        }
    }

    // Sort newest first
    sessions.sort_by_key(|b| std::cmp::Reverse(b.updated_ms));

    Ok(json_etag_response(
        headers,
        content_envelope(json!(sessions)),
    ))
}

async fn do_create_agent_session(
    state: &AppState,
    body: CreateAgentSessionBody,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    // An explicit `agent_id` is the only selector. The model's provider says
    // nothing about the agent: an OpenCode provider may be named `deepseek`.
    let manager = match body.agent_id.as_deref() {
        Some(h) => agent_manager_or_err(state, h).await?,
        None => state.agent_runtime.manager().await.ok_or_else(|| {
            api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent_unavailable",
                "No agent is available",
            )
        })?,
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
        // The App shows `~/` for a session it opens without naming a folder,
        // so that is the folder it gets. Left as `None`, OpenCode used its own
        // cwd and DeepSeek Harness refused the relative "." outright.
        _ => dirs::home_dir().map(|home| home.to_string_lossy().to_string()),
    };

    let session = manager
        .sessions()
        .create_session(
            validated_dir.as_deref(),
            body.model.as_ref(),
            body.mode.as_deref(),
        )
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()))?;

    state
        .agent_runtime
        .record_session_route(&session.asid.0, manager.agent().kind())
        .await;

    Ok(Json(content_envelope(json!(session))))
}

async fn do_get_agent_session(
    state: &AppState,
    asid: &str,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    require_device(state, headers)?;

    let manager = session_manager_or_err(state, asid).await?;

    let snapshot = manager
        .sessions()
        .get_snapshot(&AgentSessionId(asid.to_string()))
        .await
        .map_err(|e| api_error(StatusCode::NOT_FOUND, "session_not_found", &e.to_string()))?;

    // A session the reader is sitting on is polled and mostly unchanged; the
    // whole snapshot is the expensive thing to send twice.
    Ok(json_etag_response(
        headers,
        content_envelope(json!(snapshot)),
    ))
}

async fn do_get_agent_session_events(
    state: &AppState,
    asid: &str,
    after_seq: u64,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let manager = session_manager_or_err(state, asid).await?;

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

    let manager = session_manager_or_err(state, asid).await?;

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

async fn do_switch_mode(
    state: &AppState,
    asid: &str,
    mode: &str,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let manager = session_manager_or_err(state, asid).await?;

    manager
        .agent()
        .switch_mode(asid, mode)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()))?;

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
            "No agent is available",
        ));
    };

    require_directory(directory)?;

    let files = manager
        .agent()
        .find_files(query, limit, directory)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "fs_error", &e.to_string()))?;

    let hits: Vec<Value> = files
        .iter()
        .filter_map(|f| {
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
        })
        .collect();

    Ok(Json(content_envelope(json!(hits))))
}

async fn do_send_agent_prompt(
    state: &AppState,
    asid: &str,
    body: SendAgentPromptBody,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let manager = session_manager_or_err(state, asid).await?;

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
        .map_err(|e| match e {
            super::ports::agent::AgentError::InvalidRequest(_) => agent_error(e),
            _ => api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()),
        })?;

    Ok(Json(content_envelope(json!({ "submitted": true }))))
}

async fn do_revert_agent_session(
    state: &AppState,
    asid: &str,
    body: RevertSessionBody,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let manager = session_manager_or_err(state, asid).await?;

    manager
        .sessions()
        .revert_session(&AgentSessionId(asid.to_string()), &body.message_id)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()))?;

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

    let manager = session_manager_or_err(state, asid).await?;

    manager
        .prompts()
        .interrupt(&AgentSessionId(asid.to_string()))
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "interrupted": true }))))
}

async fn do_switch_agent_model(
    state: &AppState,
    asid: &str,
    model: ModelRef,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let manager = session_manager_or_err(state, asid).await?;

    manager
        .sessions()
        .switch_model(&AgentSessionId(asid.to_string()), &model)
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()))?;

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

    let manager = session_manager_or_err(state, asid).await?;

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
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()))?;

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

    let manager = session_manager_or_err(state, asid).await?;

    manager
        .interactions()
        .reply_form(
            &AgentSessionId(asid.to_string()),
            form_id,
            serde_json::Value::Object(body.answers),
        )
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()))?;

    Ok(Json(content_envelope(json!({ "replied": true }))))
}

async fn do_get_agent_vcs_diff(
    state: &AppState,
    asid: &str,
    mode: Option<&str>,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let manager = session_manager_or_err(state, asid).await?;

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

    // The session's own directory, checked before the read: a session whose
    // folder has been deleted answered a blank 502, and the useful thing to
    // say is which folder went.
    let directory = manager
        .agent()
        .get_session(asid)
        .await
        .ok()
        .and_then(|info| info.directory);
    require_directory(directory.as_deref())?;

    let diffs = manager
        .sessions()
        .get_vcs_diff(&AgentSessionId(asid.to_string()), mode)
        .await
        .map_err(agent_error)?;

    // OpenCode answers `200` with an empty list both for a clean repository
    // and for a directory that is not a repository at all, so on its own the
    // app cannot tell "nothing has changed" from "there is nothing here to
    // change". `vcs` and `reason` are the gateway's answer to that.
    let is_repo = directory
        .as_deref()
        .map(super::adapters::opencode::driver::is_git_worktree)
        .unwrap_or(true);
    Ok(Json(content_envelope(vcs_diff_body(json!(diffs), is_repo))))
}

/// Whether a catalog is one a client should be allowed to keep.
///
/// Models, providers and modes all come back empty from a directory OpenCode
/// has not loaded yet, and any one of them empty makes the catalog useless: a
/// model picker with no models is as broken as a mode picker with no modes.
/// The driver waits for all three, and this is the last guard behind it.
///
/// Skills and commands are deliberately not in this list -- a project really
/// can have none of either, and refusing to cache on that would mean never
/// caching for such a project.
pub(crate) fn catalog_is_incomplete(catalog: &crate::agents::domain::AgentCatalog) -> bool {
    catalog.models.is_empty() || catalog.providers.is_empty() || catalog.modes.is_empty()
}

/// The catalog's answer, and whether the app may cache it.
///
/// An incomplete catalog is not a catalog. Answering one with an ETag would
/// let the app hold on to an empty picker until the bytes changed, which for
/// an empty list may be never; so it is answered `no-store` and tagless, and
/// the next request asks again.
///
/// A complete catalog is tagged as usual. The tag is a hash of the whole body,
/// so two directories that resolve to different catalogs get different tags on
/// their own, and two that resolve to the same catalog share one.
pub(crate) fn catalog_response(
    headers: &HeaderMap,
    catalog: crate::agents::domain::AgentCatalog,
) -> Response {
    let payload = content_envelope(json!(catalog));
    if catalog_is_incomplete(&catalog) {
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json".to_string()),
                (header::CACHE_CONTROL, "private, no-store".to_string()),
            ],
            serde_json::to_vec(&payload).unwrap_or_default(),
        )
            .into_response();
    }
    json_etag_response(headers, payload)
}

/// What a validated answer depends on.
///
/// `accept-encoding` because the same payload is served gzipped or not, and
/// `accept-language` because every request from the app carries one and the
/// bodies these routes return are not all locale-free. Without it a cache --
/// the app's own included -- could hand one client the answer built for
/// another.
pub(crate) const VALIDATED_VARY: &str = "accept-encoding, accept-language";

pub(crate) fn json_etag_response(headers: &HeaderMap, payload: Value) -> Response {
    let body_bytes = serde_json::to_vec(&payload).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&body_bytes);
    let hash = hasher.finalize();
    let etag = format!("\"{}\"", crate::hex(&hash));

    if let Some(if_none_match) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
    {
        let trimmed = if_none_match.trim();
        if trimmed == etag || trimmed == "*" || trimmed == format!("W/{}", etag) {
            return (
                StatusCode::NOT_MODIFIED,
                [
                    (header::ETAG, etag),
                    (
                        header::CACHE_CONTROL,
                        "private, must-revalidate".to_string(),
                    ),
                    (header::VARY, VALIDATED_VARY.to_string()),
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
            (
                header::CACHE_CONTROL,
                "private, must-revalidate".to_string(),
            ),
            (header::VARY, VALIDATED_VARY.to_string()),
        ],
        body_bytes,
    )
        .into_response()
}

async fn do_list_agent_projects(
    state: &AppState,
    agent_id: Option<&str>,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    require_device(state, headers)?;

    let manager = match agent_id {
        Some(h) => agent_manager_or_err(state, h).await?,
        None => state.agent_runtime.manager().await.ok_or_else(|| {
            api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent_unavailable",
                "No agent is available",
            )
        })?,
    };

    let projects = manager
        .agent()
        .list_projects()
        .await
        .map_err(|e| api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()))?;

    Ok(json_etag_response(
        headers,
        content_envelope(json!(projects)),
    ))
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
        expanded
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("/"))
    };

    if let Ok(mut entries) = tokio::fs::read_dir(&target).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(file_type) = entry.file_type().await {
                if file_type.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.')
                        || query
                            .query
                            .as_deref()
                            .map(|q| q.starts_with('.'))
                            .unwrap_or(false)
                    {
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
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    if dirs_list.len() > 50 {
        dirs_list.truncate(50);
    }

    Ok(Json(content_envelope(json!(dirs_list))))
}

/// The `event:` name and `data:` string one domain event is published under.
///
/// One function for both transports -- this SSE stream and the `GET /api/ws`
/// socket -- so a client switching between them reads the same bytes.
pub(crate) fn agent_event_record(
    event: &super::domain::AgentDomainEvent,
) -> (&'static str, String) {
    (
        event.event_name(),
        serde_json::to_string(event).unwrap_or_default(),
    )
}

/// The agent's event stream, sealed exactly like the terminal's.
///
/// This stream used to go out in the clear on every deployment, including one
/// configured `transport_encryption: required` -- the device token in the
/// header and every agent event in the body. The terminal stream
/// (`GET /api/sessions/{id}/events`) has always sealed each event
/// individually, because a response that never ends cannot be authenticated
/// as a whole; this now does the same, with the same sealer, the same record
/// shape and the same `ENCRYPTED_SSE_EVENT` name, so a client that can already
/// read one can read the other.
///
/// `stream_crypto` is present exactly when the request itself arrived through
/// the encrypted transport, so a device paired without a transport key keeps
/// the plaintext stream byte for byte.
async fn do_stream_agent_session(
    state: &AppState,
    asid: &str,
    stream_crypto: Option<Extension<EncryptedStreamContext>>,
    headers: &HeaderMap,
) -> Result<
    Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>,
    (StatusCode, Json<Value>),
> {
    let device_id = require_device(state, headers)?;

    let mut sealer = match stream_crypto {
        Some(Extension(context)) => Some(EventStreamSealer::new(&context).map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "transport_key_unavailable",
                "encrypted transport is unavailable",
            )
        })?),
        None => None,
    };

    let manager = session_manager_or_err(state, asid).await?;

    let target_asid = AgentSessionId(asid.to_string());
    let mut rx = state.agent_runtime.subscribe_events();
    drop(manager);
    let devices = state.clone();

    let stream = async_stream::stream! {
        let hello = serde_json::to_string(&json!({ "asid": target_asid.0 })).unwrap_or_default();
        if let Some(event) = stream_event(&mut sealer, "connected", &hello) {
            yield Ok(event);
        }

        // The authorisation this stream was opened on is rechecked for as
        // long as it is open, as the terminal event stream and `/api/ws` do.
        // See `STREAM_DEVICE_RECHECK_INTERVAL`.
        let mut device_recheck = tokio::time::interval(STREAM_DEVICE_RECHECK_INTERVAL);
        device_recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick is immediate, and `require_device` just ran.
        device_recheck.tick().await;

        loop {
            let next = tokio::select! {
                _ = device_recheck.tick() => {
                    if !still_paired(&devices, &device_id) {
                        // Closed without an event, like the terminal stream: a
                        // revoked device is not owed an explanation, and a
                        // legitimate client learns it from its reconnect's 403.
                        break;
                    }
                    continue;
                }
                next = rx.recv() => next,
            };
            match next {
                Ok(ev) => {
                    // An event with no session -- a global resync, or a
                    // worktree change -- reaches every stream.
                    if !ev.asid().0.is_empty() && ev.asid() != &target_asid {
                        continue;
                    }
                    let (ev_name, payload) = agent_event_record(&ev);
                    // `None` means the record could not be sealed. It is
                    // dropped rather than ever leaving in the clear.
                    if let Some(event) = stream_event(&mut sealer, ev_name, &payload) {
                        yield Ok(event);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let resync = serde_json::to_string(&json!({ "asid": target_asid.0 })).unwrap_or_default();
                    if let Some(event) = stream_event(&mut sealer, "agent.resync", &resync) {
                        yield Ok(event);
                    }
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

    let all_managers = managers_for_read(&state, query.agent_id.as_deref()).await?;

    require_directory(query.directory.as_deref())?;

    let mut merged_models = Vec::new();
    let mut merged_modes = Vec::new();
    let mut merged_mcp = Vec::new();
    let mut merged_skills = Vec::new();
    let mut merged_providers = Vec::new();
    let mut merged_commands = Vec::new();
    let mut primary_defaults = None;

    for manager in &all_managers {
        if let Ok(cat) = manager
            .sessions()
            .get_catalog(query.directory.as_deref())
            .await
        {
            if primary_defaults.is_none() {
                primary_defaults = Some(cat.defaults);
            }
            for m in cat.models {
                if !merged_models
                    .iter()
                    .any(|existing: &crate::agents::domain::ModelInfo| {
                        existing.id == m.id && existing.provider_id == m.provider_id
                    })
                {
                    merged_models.push(m);
                }
            }
            for a in cat.modes {
                if !merged_modes
                    .iter()
                    .any(|existing: &crate::agents::domain::ModeInfo| existing.id == a.id)
                {
                    merged_modes.push(a);
                }
            }
            for p in cat.providers {
                if !merged_providers
                    .iter()
                    .any(|existing: &crate::agents::domain::ProviderInfo| existing.id == p.id)
                {
                    merged_providers.push(p);
                }
            }
            for s in cat.skills {
                if !merged_skills
                    .iter()
                    .any(|existing: &crate::agents::domain::SkillInfo| existing.id == s.id)
                {
                    merged_skills.push(s);
                }
            }
            for c in cat.commands {
                if !merged_commands
                    .iter()
                    .any(|existing: &crate::agents::domain::CommandInfo| existing.name == c.name)
                {
                    merged_commands.push(c);
                }
            }
            merged_mcp.extend(cat.mcp);
        }
    }

    let catalog = crate::agents::domain::AgentCatalog {
        models: merged_models,
        modes: merged_modes,
        mcp: merged_mcp,
        skills: merged_skills,
        providers: merged_providers,
        commands: merged_commands,
        defaults: primary_defaults.unwrap_or_default(),
    };

    if catalog_is_incomplete(&catalog) {
        tracing::warn!(
            directory = query.directory.as_deref().unwrap_or("<none>"),
            models = catalog.models.len(),
            providers = catalog.providers.len(),
            modes = catalog.modes.len(),
            "agent catalog is incomplete; answering without an ETag"
        );
    }
    Ok(catalog_response(&headers, catalog))
}

// ---------------------------------------------------------------------------
// Global route handlers
// ---------------------------------------------------------------------------

async fn list_agent_sessions_global(
    State(state): State<AppState>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    do_list_agent_sessions(
        &state,
        &query.to_session_query(),
        query.agent_id.as_deref(),
        &headers,
    )
    .await
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
) -> ApiResult<Response> {
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

async fn switch_mode_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SwitchModeBody>,
) -> ApiResult<Json<Value>> {
    do_switch_mode(&state, &asid, &body.into_mode(), &headers).await
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
) -> ApiResult<Response> {
    do_list_agent_sessions(
        &state,
        &query.to_session_query(),
        query.agent_id.as_deref(),
        &headers,
    )
    .await
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
) -> ApiResult<Response> {
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

async fn switch_mode_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<SwitchModeBody>,
) -> ApiResult<Json<Value>> {
    do_switch_mode(&state, &asid, &body.into_mode(), &headers).await
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
    Query(query): Query<AgentQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    do_list_agent_projects(&state, query.agent_id.as_deref(), &headers).await
}

async fn list_agent_projects_legacy(
    State(state): State<AppState>,
    Path(_session_id): Path<String>,
    Query(query): Query<AgentQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    do_list_agent_projects(&state, query.agent_id.as_deref(), &headers).await
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
    stream_crypto: Option<Extension<EncryptedStreamContext>>,
    headers: HeaderMap,
) -> Result<
    Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>,
    (StatusCode, Json<Value>),
> {
    do_stream_agent_session(&state, &asid, stream_crypto, &headers).await
}

async fn stream_agent_session_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    stream_crypto: Option<Extension<EncryptedStreamContext>>,
    headers: HeaderMap,
) -> Result<
    Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>> + Send>,
    (StatusCode, Json<Value>),
> {
    do_stream_agent_session(&state, &asid, stream_crypto, &headers).await
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
                    "No agent is available",
                ));
            }
        }
    }};
}

macro_rules! session_manager_or_err {
    ($state:expr, $headers:expr, $asid:expr) => {{
        require_device($state, $headers)?;
        session_manager_or_err($state, $asid).await?
    }};
}

/// The manager that owns a session. Nothing attached is 503; an agent is
/// attached but none of them knows the id is 404, so a session id from before
/// a restart is never handed to an agent that will not recognise it.
async fn session_manager_or_err(
    state: &AppState,
    asid: &str,
) -> ApiResult<std::sync::Arc<super::manager::AgentManager>> {
    if let Some(manager) = state.agent_runtime.manager_for_session(asid).await {
        return Ok(manager);
    }
    if state.agent_runtime.all_managers().await.is_empty() {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "No agent is available",
        ));
    }
    Err(api_error(
        StatusCode::NOT_FOUND,
        "session_not_found",
        "No attached agent owns this session",
    ))
}

fn agent_error(err: super::ports::agent::AgentError) -> (StatusCode, Json<Value>) {
    // A folder that is gone is not an agent fault, and answering 502 for it
    // told the user their agent had broken when their directory had simply
    // been deleted. It is a 404 that names the folder, and it carries the path
    // as its own field so the app can offer to forget the session rather than
    // parse a sentence.
    match &err {
        super::ports::agent::AgentError::WorkspaceMissing(directory) => {
            workspace_missing(directory)
        }
        super::ports::agent::AgentError::Unsupported(feature) => api_error(
            StatusCode::NOT_IMPLEMENTED,
            "feature_unsupported",
            &format!("This agent does not support: {feature}"),
        ),
        super::ports::agent::AgentError::InvalidRequest(message) => {
            api_error(StatusCode::BAD_REQUEST, "invalid_request", message)
        }
        _ => api_error(StatusCode::BAD_GATEWAY, "agent_error", &err.to_string()),
    }
}

/// The diff, and whether there was anywhere for one to come from.
///
/// OpenCode answers `200` with an empty list both for a clean repository and
/// for a directory that is not a repository at all, so on its own the app
/// cannot tell "nothing has changed" from "there is nothing here to change" --
/// and showed an empty diff screen for both. `vcs` and `reason` are the
/// gateway's answer to that; the files themselves are unchanged.
pub(crate) fn vcs_diff_body(files: Value, is_repo: bool) -> Value {
    json!({
        "files": files,
        "vcs": if is_repo { Some("git") } else { None },
        "reason": if is_repo { None } else { Some("not_a_repository") },
    })
}

/// `404 workspace_missing`, with the directory alongside the message.
pub(crate) fn workspace_missing(directory: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "code": "workspace_missing",
                "message": format!("The workspace folder is gone: {directory}"),
                "directory": directory,
            }
        })),
    )
}

/// The guard every directory-scoped route runs before proxying.
fn require_directory(directory: Option<&str>) -> Result<(), (StatusCode, Json<Value>)> {
    super::adapters::opencode::driver::check_directory(directory).map_err(agent_error)
}

async fn list_agent_session_children(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let mut session_query = query.to_session_query();
    session_query.parent_id = Some(asid);
    do_list_agent_sessions(&state, &session_query, query.agent_id.as_deref(), &headers).await
}

async fn delete_agent_session_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    manager
        .agent()
        .delete_session(&asid)
        .await
        .map_err(agent_error)?;
    // OpenCode deletes the children too, but it also announces each one on the
    // stream, so the mirror only has to forget this session here.
    manager
        .mirror()
        .remove_session(&AgentSessionId(asid.clone()))
        .await;
    state.agent_runtime.forget_session_route(&asid).await;
    Ok(Json(content_envelope(json!({ "deleted": true }))))
}

async fn rename_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RenameSessionBody>,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    validate_text(&body.title)?;
    manager
        .agent()
        .rename_session(&asid, &body.title)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(
        json!({ "renamed": true, "title": body.title }),
    )))
}

async fn clear_agent_session_revert(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    manager
        .agent()
        .clear_revert(&asid)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "cleared": true }))))
}

/// The project a session belongs to.
///
/// Saved permissions are a project-wide list and the session is the only thing
/// the app names, so this is the translation between the two. It is read from
/// OpenCode rather than the mirror: forgetting a permission is a destructive
/// act on the user's own configuration, and a stale project id would aim it at
/// the wrong list.
async fn session_project_id(
    agent: &dyn super::ports::agent::AgentPort,
    asid: &str,
) -> ApiResult<String> {
    let session = agent.get_session(asid).await.map_err(agent_error)?;
    session.project_id.filter(|p| !p.is_empty()).ok_or_else(|| {
        api_error(
            StatusCode::BAD_GATEWAY,
            "agent_error",
            "the session reported no project",
        )
    })
}

/// The decisions the user has answered "always allow" to, for this session's
/// project. `PermissionSaved.Info`, renamed into the gateway's snake_case.
async fn list_saved_agent_permissions(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    let agent = manager.agent();
    let project_id = session_project_id(agent.as_ref(), &asid).await?;
    let items = agent
        .list_saved_permissions(Some(&project_id))
        .await
        .map_err(agent_error)?;
    let items: Vec<Value> = items
        .iter()
        .filter_map(super::adapters::opencode::mapper::map_saved_permission)
        .collect();
    Ok(Json(content_envelope(json!({ "items": items }))))
}

/// Forget one remembered decision, so the next time the agent asks.
///
/// OpenCode's own delete is global -- an id and nothing else. This route is
/// scoped to a session, so the id is checked against that session's project
/// first: a device holding one session must not be able to reach into another
/// project's list through it. An id that is not in that list is `404`,
/// which is also the answer for an id that was already deleted.
async fn forget_saved_agent_permission(
    State(state): State<AppState>,
    Path((asid, saved_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    let agent = manager.agent();
    let project_id = session_project_id(agent.as_ref(), &asid).await?;
    let known = agent
        .list_saved_permissions(Some(&project_id))
        .await
        .map_err(agent_error)?
        .iter()
        .any(|item| item.get("id").and_then(Value::as_str) == Some(saved_id.as_str()));
    if !known {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "saved_permission_not_found",
            "no such saved permission in this session's project",
        ));
    }
    agent
        .delete_saved_permission(&saved_id)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "deleted": true }))))
}

/// A project's worktrees.
///
/// `Worktree.Directory` is already snake_case-clean -- `{directory,
/// strategy?}` -- so the entries pass through as they are. The project's own
/// root is in the list without a `strategy`: it is not a worktree OpenCode
/// made, and `DELETE` refuses it.
async fn list_agent_worktrees(
    State(state): State<AppState>,
    Query(query): Query<AgentWorktreesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    require_directory(query.directory.as_deref())?;
    let items = manager
        .agent()
        .list_worktrees(query.directory.as_deref())
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "items": items }))))
}

async fn create_agent_worktree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateWorktreeBody>,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    require_directory(body.directory.as_deref())?;
    // `Worktree.CreateInput` declares additionalProperties:false, so only the
    // fields the caller actually set are sent -- an explicit null is refused.
    let mut input = json!({});
    for (key, value) in [
        ("name", &body.name),
        ("branch", &body.branch),
        ("from", &body.from),
        ("strategy", &body.strategy),
    ] {
        if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            input[key] = json!(value);
        }
    }
    let created = manager
        .agent()
        .create_worktree(body.directory.as_deref(), &input)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "worktree": created }))))
}

async fn remove_agent_worktree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RemoveWorktreeBody>,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    require_directory(body.directory.as_deref())?;
    let worktree = body.worktree.trim();
    if worktree.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_worktree",
            "worktree must not be empty",
        ));
    }
    manager
        .agent()
        .remove_worktree(body.directory.as_deref(), worktree, Some(body.force))
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "deleted": true }))))
}

async fn refresh_agent_worktrees(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<RefreshWorktreesBody>>,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let directory = body.and_then(|Json(b)| b.directory);
    require_directory(directory.as_deref())?;
    manager
        .agent()
        .refresh_worktrees(directory.as_deref())
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "refreshed": true }))))
}

/// Move a session to another directory -- the point of a worktree.
///
/// The reply is the session as it is afterwards, read back rather than
/// assembled here, because the move can change more than the directory: a
/// target outside the current project moves the session into the project that
/// directory belongs to, and `project_id` changes with it.
///
/// There is no scope check, because 2.0.1 expresses no scope rule: the spec
/// requires only `directory`, and the live service accepts any directory that
/// exists -- including one in another project -- and refuses a missing one
/// with a 400. Inventing a narrower rule here would refuse moves OpenCode
/// itself allows, which is the gateway deciding policy it was not given.
async fn move_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<MoveSessionBody>,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    let directory = body.directory.trim();
    if directory.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_directory",
            "directory must not be empty",
        ));
    }
    require_directory(Some(directory))?;
    manager
        .agent()
        .move_session(&asid, directory)
        .await
        .map_err(agent_error)?;
    let info = manager
        .sessions()
        .get_session(&asid)
        .await
        .map_err(agent_error)?;
    manager.mirror().update_session(info.clone()).await;
    Ok(Json(content_envelope(
        serde_json::to_value(&info).unwrap_or(Value::Null),
    )))
}

/// Stage a rollback without applying it: the boundary moves, the files stay
/// as they are, and `info.revert` is set until this is committed or cleared.
/// The reply is `Session.Revert`, whose `files` is what the app draws in the
/// confirmation before the user commits.
async fn stage_agent_session_revert(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<StageRevertBody>,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    let message_id = body.message_id.trim();
    if message_id.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_message_id",
            "message_id must not be empty",
        ));
    }
    let res = manager
        .agent()
        .stage_revert(&asid, message_id, body.files)
        .await
        .map_err(agent_error)?;
    // `Session.Revert` comes back under `data`; it is mapped rather than
    // forwarded so the app reads the same snake_case shape it already reads
    // on `info.revert`.
    let revert = res
        .get("data")
        .and_then(super::adapters::opencode::mapper::map_revert)
        .or_else(|| super::adapters::opencode::mapper::map_revert(&res));
    Ok(Json(content_envelope(json!({ "revert": revert }))))
}

/// Apply the staged rollback. OpenCode answers `204`; the boundary message and
/// everything after it are gone once this returns, and the removal reaches the
/// app as `agent.timeline.removed` off `session.revert.committed`.
async fn commit_agent_session_revert(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    manager
        .agent()
        .commit_revert(&asid)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "committed": true }))))
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
    let manager = session_manager_or_err!(&state, &headers, &asid);
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
        .agent()
        .activate_skill(&asid, skill, body.resume)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "status": "ok" }))))
}

async fn compact_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<CompactBody>>,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    let delivery = body.and_then(|Json(b)| b.delivery);
    let res = manager
        .agent()
        .compact_session(&asid, delivery.as_deref())
        .await
        .map_err(agent_error)?;
    // The reply is the inbox item the request was admitted as.
    Ok(Json(content_envelope(json!({
        "requested": true,
        "item": res.get("data").cloned().unwrap_or(res),
    }))))
}

async fn get_agent_session_context(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    let messages = manager
        .agent()
        .get_context(&asid)
        .await
        .map_err(agent_error)?;

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
    let manager = session_manager_or_err!(&state, &headers, &asid);
    manager
        .agent()
        .background_session(&asid)
        .await
        .map_err(agent_error)?;
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
    let manager = session_manager_or_err!(&state, &headers, &asid);
    manager
        .agent()
        .wait_session(&asid)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "idle": true }))))
}

async fn view_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<ViewSessionBody>>,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    let idle = body.and_then(|Json(b)| b.idle).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    });
    manager
        .view_session(&asid, idle)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "viewed": idle }))))
}

async fn export_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<ExportQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    // Sanitized by default: an export leaves the device.
    let sanitize = query.sanitize.unwrap_or(true);
    let res = manager
        .agent()
        .export_session(&asid, Some(sanitize))
        .await
        .map_err(agent_error)?;
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
    let manager = session_manager_or_err!(&state, &headers, &asid);
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
        .agent()
        .run_command(&asid, name, &arguments, body.delivery.as_deref())
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!({ "submitted": true }))))
}

async fn get_agent_inbox(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    let items = manager
        .agent()
        .get_inbox(&asid)
        .await
        .map_err(agent_error)?;
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
    let manager = session_manager_or_err!(&state, &headers, &asid);
    manager
        .agent()
        .cancel_inbox_item(&asid, &inbox_id)
        .await
        .map_err(agent_error)?;
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
    let manager = session_manager_or_err!(&state, &headers, &asid);
    manager
        .agent()
        .set_inbox_delivery(&asid, &inbox_id, delivery)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(
        json!({ "delivery": delivery, "inbox_id": inbox_id }),
    )))
}

/// What agent, if any, the gateway currently has. Unlike every other agent
/// route this one answers 200 with `available: false` rather than 503 -- the
/// app asks it precisely to find out why the others are refusing.
async fn get_agent_status(
    State(state): State<AppState>,
    Query(query): Query<AgentQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let status = match query.agent_id.as_deref() {
        Some(h) => {
            // Validates the id (400) and that it is attached (503).
            agent_manager_or_err(&state, h).await?;
            state.agent_runtime.status_for(h).await.ok_or_else(|| {
                api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "agent_unavailable",
                    &format!("Agent '{h}' is not available"),
                )
            })?
        }
        None => state.agent_runtime.status().await,
    };
    Ok(Json(content_envelope(json!(status))))
}

async fn list_agent_shells(
    State(state): State<AppState>,
    Query(query): Query<AgentCatalogQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let shells = manager
        .agent()
        .list_shells(query.directory.as_deref())
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(json!(shells))))
}

async fn get_agent_shell(
    State(state): State<AppState>,
    Path(shell_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let shell = manager
        .agent()
        .get_shell(&shell_id)
        .await
        .map_err(agent_error)?;
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
        .agent()
        .get_shell_output(&shell_id, query.cursor, query.limit)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(output)))
}

async fn kill_agent_shell(
    State(state): State<AppState>,
    Path(shell_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    let shell = manager
        .agent()
        .kill_shell(&shell_id)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(
        json!({ "killed": true, "shell": shell }),
    )))
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

    /// `files` is tri-state on the way in: absent leaves OpenCode's own
    /// default alone, and `false` is a caller who does not want the diff
    /// computed -- not the same thing.
    fn catalog_with(
        modes: Vec<crate::agents::domain::ModeInfo>,
    ) -> crate::agents::domain::AgentCatalog {
        crate::agents::domain::AgentCatalog {
            models: Vec::new(),
            modes,
            mcp: Vec::new(),
            skills: Vec::new(),
            providers: Vec::new(),
            commands: Vec::new(),
            defaults: Default::default(),
        }
    }

    fn complete_catalog() -> crate::agents::domain::AgentCatalog {
        let mut catalog = catalog_with(vec![mode("build")]);
        catalog.models = vec![crate::agents::domain::ModelInfo {
            id: "union-alpha".into(),
            name: "Union Alpha".into(),
            provider_id: "opencode".into(),
            family: None,
            limit: None,
            variants: None,
            cost: None,
            enabled: true,
            status: None,
        }];
        catalog.providers = vec![crate::agents::domain::ProviderInfo {
            id: "opencode".into(),
            name: "OpenCode".into(),
            activation: None,
            available: true,
            models: Vec::new(),
        }];
        catalog
    }

    fn mode(id: &str) -> crate::agents::domain::ModeInfo {
        crate::agents::domain::ModeInfo {
            id: id.to_string(),
            name: id.to_string(),
            model: None,
            description: None,
            mode: Some("primary".to_string()),
            color: None,
            hidden: false,
        }
    }

    /// The validator the app has been sending all along.
    ///
    /// It puts `If-None-Match` on the sessions list and the gateway ignored
    /// it, so every poll after every turn paid for the whole list again. The
    /// same bytes must produce the same tag, a matching tag must answer 304
    /// with no body, and different bytes must produce a different tag.
    #[test]
    fn a_validated_answer_is_a_304_when_the_client_already_has_it() {
        let payload = json!({ "sessions": [{ "asid": "ses_1" }] });

        let fresh = json_etag_response(&HeaderMap::new(), payload.clone());
        assert_eq!(fresh.status(), StatusCode::OK);
        let tag = fresh
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .expect("a validated answer carries a tag")
            .to_string();
        assert_eq!(
            fresh
                .headers()
                .get(header::VARY)
                .and_then(|v| v.to_str().ok()),
            Some(VALIDATED_VARY),
            "and says what it varies by, so a cache cannot cross the wires"
        );

        // The same body, asked for again with the tag the client holds.
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, tag.parse().unwrap());
        let repeat = json_etag_response(&headers, payload.clone());
        assert_eq!(repeat.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            repeat
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok()),
            Some(tag.as_str())
        );
        assert_eq!(
            repeat
                .headers()
                .get(header::VARY)
                .and_then(|v| v.to_str().ok()),
            Some(VALIDATED_VARY),
            "a 304 has to carry it too, or the cache entry it refreshes loses it"
        );

        // A weak tag for the same bytes is still the same answer.
        let mut weak = HeaderMap::new();
        weak.insert(header::IF_NONE_MATCH, format!("W/{tag}").parse().unwrap());
        assert_eq!(
            json_etag_response(&weak, payload.clone()).status(),
            StatusCode::NOT_MODIFIED
        );

        // One more session is a different answer, and must not 304.
        let changed = json!({ "sessions": [{ "asid": "ses_1" }, { "asid": "ses_2" }] });
        let response = json_etag_response(&headers, changed);
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(
            response
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok()),
            Some(tag.as_str())
        );
    }

    /// A folder that has been deleted is a 404 that names it, never a 502 --
    /// and the path is its own field so the app can offer to forget the
    /// session rather than parse a sentence out of the message.
    #[test]
    fn a_missing_workspace_is_a_404_that_names_the_folder() {
        let (status, Json(body)) = workspace_missing("/tmp/muqun-c10/repo");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "workspace_missing");
        assert_eq!(
            body["error"]["message"],
            "The workspace folder is gone: /tmp/muqun-c10/repo"
        );
        assert_eq!(body["error"]["directory"], "/tmp/muqun-c10/repo");
    }

    /// And the agent error that stands for it maps to exactly that, rather
    /// than falling into the 502 every other agent failure takes.
    #[test]
    fn a_missing_workspace_never_becomes_a_bad_gateway() {
        let (status, Json(body)) = agent_error(
            super::super::ports::agent::AgentError::WorkspaceMissing("/gone".into()),
        );
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "workspace_missing");
        assert_eq!(body["error"]["directory"], "/gone");

        // Everything else still is one.
        let (status, Json(body)) = agent_error(
            super::super::ports::agent::AgentError::RequestFailed("nope".into()),
        );
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"]["code"], "agent_error");
    }

    /// Pointing the agent at a file nobody uploaded is the client's mistake.
    #[test]
    fn an_invalid_request_is_a_400() {
        let (status, Json(body)) = agent_error(
            super::super::ports::agent::AgentError::InvalidRequest("not an upload".into()),
        );
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    /// An empty diff from a repository and an empty diff from a directory that
    /// is not one look identical coming out of OpenCode. They must not look
    /// identical going into the app.
    #[test]
    fn an_empty_diff_says_whether_there_was_a_repository_at_all() {
        let in_repo = vcs_diff_body(json!([]), true);
        assert_eq!(in_repo["files"], json!([]));
        assert_eq!(in_repo["vcs"], "git");
        assert!(in_repo["reason"].is_null(), "nothing to explain");

        let no_repo = vcs_diff_body(json!([]), false);
        assert_eq!(no_repo["files"], json!([]));
        assert!(no_repo["vcs"].is_null());
        assert_eq!(no_repo["reason"], "not_a_repository");

        // A real diff is carried through untouched.
        let files = json!([{ "path": "a.rs", "patch": "@@", "additions": 1, "deletions": 0 }]);
        let real = vcs_diff_body(files.clone(), true);
        assert_eq!(real["files"], files);
        assert_eq!(real["vcs"], "git");
    }

    /// An empty catalog must not become a cached empty catalog. OpenCode
    /// answers a directory it has not loaded with `[]` on every arm, and an
    /// ETag on that would pin an empty picker until the bytes changed.
    #[test]
    fn an_empty_catalog_is_answered_without_an_etag() {
        let response = catalog_response(&HeaderMap::new(), catalog_with(Vec::new()));
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().get(header::ETAG).is_none(),
            "nothing to cache"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
    }

    /// Every arm that can come back empty from an unloaded directory, not just
    /// modes.
    ///
    /// This is the bug the app found: a catalog with seven modes and **zero
    /// models** was answered 200 with an ETag, so a model picker that happened
    /// to open first was pinned empty. A catalog is only cacheable when all
    /// three of models, providers and modes have something in them.
    #[test]
    fn a_catalog_missing_models_or_providers_is_not_cacheable_either() {
        let full = complete_catalog();
        assert!(
            !catalog_is_incomplete(&full),
            "the fixture is a real catalog"
        );
        assert!(
            catalog_response(&HeaderMap::new(), full)
                .headers()
                .get(header::ETAG)
                .is_some(),
            "and a real catalog is tagged"
        );

        for (what, catalog) in [
            ("models", {
                let mut c = complete_catalog();
                c.models.clear();
                c
            }),
            ("providers", {
                let mut c = complete_catalog();
                c.providers.clear();
                c
            }),
            ("modes", {
                let mut c = complete_catalog();
                c.modes.clear();
                c
            }),
        ] {
            assert!(
                catalog_is_incomplete(&catalog),
                "a catalog with no {what} is not a catalog"
            );
            let response = catalog_response(&HeaderMap::new(), catalog);
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response.headers().get(header::ETAG).is_none(),
                "no {what} means nothing worth keeping"
            );
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                "private, no-store",
                "and the client is told not to keep it ({what})"
            );
        }

        // Skills and commands are not in the rule: a project really can have
        // none of either, and refusing to cache on that would mean never
        // caching for such a project.
        let mut bare = complete_catalog();
        bare.skills.clear();
        bare.commands.clear();
        assert!(!catalog_is_incomplete(&bare));
        assert!(catalog_response(&HeaderMap::new(), bare)
            .headers()
            .get(header::ETAG)
            .is_some());
    }

    /// A real catalog is tagged as before, and the tag follows the body -- so
    /// two directories with different catalogs cannot share one.
    #[test]
    fn a_populated_catalog_is_tagged_per_body() {
        // Complete catalogs: a catalog missing models or providers is refused
        // a tag on purpose, which is a different test.
        let with_modes = |modes: Vec<crate::agents::domain::ModeInfo>| {
            let mut catalog = complete_catalog();
            catalog.modes = modes;
            catalog
        };
        let one = catalog_response(&HeaderMap::new(), with_modes(vec![mode("build")]));
        let two = catalog_response(
            &HeaderMap::new(),
            with_modes(vec![mode("build"), mode("osuki-coder")]),
        );
        let tag = |r: &Response| {
            r.headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let (one, two) = (tag(&one).expect("tagged"), tag(&two).expect("tagged"));
        assert_ne!(one, two, "a different catalog is a different tag");

        // And the same catalog is the same tag, which is what makes 304 work.
        let again = catalog_response(&HeaderMap::new(), with_modes(vec![mode("build")]));
        assert_eq!(tag(&again).expect("tagged"), one);
    }

    /// Two directories in one body, and confusing them would remove the
    /// wrong thing: `directory` is the project, `worktree` is what goes.
    #[test]
    fn a_worktree_removal_body_separates_the_project_from_the_worktree() {
        let body: RemoveWorktreeBody = serde_json::from_value(json!({
            "directory": "/tmp/muqun-gw-wt",
            "worktree": "/w/016d5f/probe"
        }))
        .expect("body parses");
        assert_eq!(body.directory.as_deref(), Some("/tmp/muqun-gw-wt"));
        assert_eq!(body.worktree, "/w/016d5f/probe");
        assert!(!body.force, "force defaults to off, not to on");

        let forced: RemoveWorktreeBody = serde_json::from_value(json!({
            "worktree": "/w/016d5f/probe", "force": true
        }))
        .expect("the project may be left to OpenCode's own default location");
        assert!(forced.force);

        let no_target: Result<RemoveWorktreeBody, _> =
            serde_json::from_value(json!({ "directory": "/tmp/muqun-gw-wt" }));
        assert!(
            no_target.is_err(),
            "there is nothing to remove without `worktree`"
        );
    }

    /// Every field of `Worktree.CreateInput` is optional, so an empty create
    /// is a real request: OpenCode names the worktree itself.
    #[test]
    fn a_worktree_create_body_is_optional_all_the_way_down() {
        let empty: CreateWorktreeBody = serde_json::from_value(json!({})).expect("empty parses");
        assert!(empty.directory.is_none());
        assert!(empty.name.is_none());
        assert!(empty.branch.is_none());

        let full: CreateWorktreeBody = serde_json::from_value(json!({
            "directory": "/tmp/muqun-gw-wt", "name": "probe", "branch": "main"
        }))
        .expect("body parses");
        assert_eq!(full.name.as_deref(), Some("probe"));
        assert_eq!(
            full.branch.as_deref(),
            Some("main"),
            "`branch` is the ref to branch from, not a name to create"
        );
    }

    #[test]
    fn a_move_body_requires_somewhere_to_move_to() {
        let body: MoveSessionBody =
            serde_json::from_value(json!({ "directory": "/w/probe" })).expect("body parses");
        assert_eq!(body.directory, "/w/probe");

        let empty: Result<MoveSessionBody, _> = serde_json::from_value(json!({}));
        assert!(empty.is_err(), "directory is required");
    }

    #[test]
    fn a_stage_body_keeps_files_optional() {
        let bare: StageRevertBody =
            serde_json::from_value(json!({ "message_id": "msg_1" })).expect("id alone parses");
        assert_eq!(bare.message_id, "msg_1");
        assert!(bare.files.is_none());

        let with_files: StageRevertBody =
            serde_json::from_value(json!({ "message_id": "msg_1", "files": false }))
                .expect("files parses");
        assert_eq!(with_files.files, Some(false));

        let no_id: Result<StageRevertBody, _> = serde_json::from_value(json!({ "files": true }));
        assert!(no_id.is_err(), "message_id is required");
    }

    #[test]
    fn a_skill_body_takes_an_id_and_an_optional_resume() {
        let bare: ActivateSkillBody =
            serde_json::from_value(json!({ "skill": "docs" })).expect("skill alone parses");
        assert_eq!(bare.skill, "docs");
        assert!(
            bare.resume.is_none(),
            "an absent resume is OpenCode's default"
        );

        let with_resume: ActivateSkillBody =
            serde_json::from_value(json!({ "skill": "docs", "resume": false }))
                .expect("resume parses");
        assert_eq!(with_resume.resume, Some(false));

        let no_skill: Result<ActivateSkillBody, _> = serde_json::from_value(json!({}));
        assert!(no_skill.is_err(), "skill is required");
    }

    #[test]
    fn switch_mode_body_accepts_object_and_bare_string() {
        let wrapped: SwitchModeBody =
            serde_json::from_value(json!({ "mode": "build" })).expect("object should parse");
        assert_eq!(wrapped.into_mode(), "build");

        let bare: SwitchModeBody =
            serde_json::from_value(json!("plan")).expect("bare string should parse");
        assert_eq!(bare.into_mode(), "plan");
    }

    async fn create_refusal(body: Value) -> (StatusCode, Value) {
        let state = crate::test_support::test_state(
            "admin",
            vec![crate::test_support::test_device("phone-1", "device-token")],
        );
        let headers = crate::test_support::bearer_headers("device-token");
        let body: CreateAgentSessionBody = serde_json::from_value(body).unwrap();
        let refusal = do_create_agent_session(&state, body, &headers)
            .await
            .expect_err("nothing is attached");
        let status = refusal.0;
        (status, crate::test_support::error_body(&refusal))
    }

    #[tokio::test]
    async fn create_session_rejects_an_unknown_agent() {
        let (status, body) = create_refusal(json!({ "agent_id": "claude" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_agent");
    }

    #[tokio::test]
    async fn create_session_names_a_known_agent_that_is_not_attached() {
        let (status, body) = create_refusal(json!({ "agent_id": "deepseek" })).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "agent_unavailable");
    }

    #[tokio::test]
    async fn a_provider_name_does_not_select_the_agent() {
        // Without `agent_id` the primary is used; a `deepseek` provider is just
        // a provider, so with nothing attached this is the generic 503.
        let (status, body) =
            create_refusal(json!({ "model": { "provider_id": "deepseek", "model_id": "m" } }))
                .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["message"], "No agent is available");
    }

    #[tokio::test]
    async fn a_session_with_no_agent_attached_is_unavailable_not_missing() {
        let state = crate::test_support::test_state("admin", Vec::new());
        let refusal = session_manager_or_err(&state, "ses_1")
            .await
            .err()
            .expect("nothing attached");
        assert_eq!(refusal.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    // -- multi-agent contract --------------------------------------------

    use crate::test_support::FakeAgent;

    fn device_headers() -> HeaderMap {
        crate::test_support::bearer_headers("device-token")
    }

    async fn state_with(agents: Vec<FakeAgent>) -> AppState {
        let state = crate::test_support::test_state(
            "admin",
            vec![crate::test_support::test_device("phone-1", "device-token")],
        );
        for agent in agents {
            state.agent_runtime.attach_for_test(agent.manager()).await;
        }
        state
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body reads");
        serde_json::from_slice(&bytes).expect("body is json")
    }

    fn ids(data: &Value) -> Vec<(String, String)> {
        data.as_array()
            .expect("a list")
            .iter()
            .map(|s| {
                (
                    s["asid"].as_str().unwrap().to_string(),
                    s["agent_id"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn every_session_object_carries_its_agent() {
        let state = state_with(vec![
            FakeAgent::new("opencode").with_sessions(&[("ses_oc", 10)]),
            FakeAgent::new("deepseek").with_sessions(&[("ses_ds", 20)]),
        ])
        .await;
        let headers = device_headers();

        // create, on each agent
        for (agent_id, expected) in [(None, "opencode"), (Some("deepseek"), "deepseek")] {
            let body = CreateAgentSessionBody {
                directory: None,
                model: None,
                mode: None,
                agent_id: agent_id.map(str::to_string),
            };
            let Json(created) = do_create_agent_session(&state, body, &headers)
                .await
                .expect("creates");
            assert_eq!(created["data"]["agent_id"], expected);
        }

        // get, from the agent and then again from the mirror
        for _ in 0..2 {
            let response = do_get_agent_session(&state, "ses_ds", &headers)
                .await
                .expect("gets");
            assert_eq!(
                body_json(response).await["data"]["info"]["agent_id"],
                "deepseek"
            );
        }

        // the events the stream replays
        let manager = state
            .agent_runtime
            .manager_for_agent("deepseek")
            .await
            .unwrap();
        let events = manager
            .sessions()
            .get_events_after(&AgentSessionId("ses_ds".into()), 0)
            .await
            .expect("events are held");
        let events = serde_json::to_string(&events).unwrap();
        assert!(events.contains("\"agent_id\":\"deepseek\""), "{events}");
    }

    #[tokio::test]
    async fn the_session_list_merges_every_agent_and_routes_each_session() {
        let state = state_with(vec![
            FakeAgent::new("opencode").with_sessions(&[("ses_oc1", 10), ("ses_oc2", 30)]),
            FakeAgent::new("deepseek").with_sessions(&[("ses_ds", 20)]),
        ])
        .await;
        let response =
            do_list_agent_sessions(&state, &SessionQuery::default(), None, &device_headers())
                .await
                .expect("lists");
        let data = body_json(response).await["data"].clone();
        assert_eq!(
            ids(&data),
            vec![
                ("ses_oc2".to_string(), "opencode".to_string()),
                ("ses_ds".to_string(), "deepseek".to_string()),
                ("ses_oc1".to_string(), "opencode".to_string()),
            ]
        );

        // Routes were recorded, so a per-session lookup needs no probing.
        for (asid, agent_id) in ids(&data) {
            let owner = state
                .agent_runtime
                .manager_for_session(&asid)
                .await
                .unwrap();
            assert_eq!(owner.agent().kind(), agent_id);
        }

        let response = do_list_agent_sessions(
            &state,
            &SessionQuery::default(),
            Some("deepseek"),
            &device_headers(),
        )
        .await
        .expect("filtered list");
        let data = body_json(response).await["data"].clone();
        assert_eq!(
            ids(&data),
            vec![("ses_ds".to_string(), "deepseek".to_string())]
        );
    }

    /// The agent-session twin of the terminal stream's
    /// `revoking_a_device_closes_the_event_stream_it_already_had`: a real
    /// stream, over an attached agent that owns the session so it cannot end
    /// for any other reason, closed by the revoke route itself.
    ///
    /// `to_bytes` finishes exactly when the body ends, so it is the assertion:
    /// before the recheck this ran until the timeout. It takes one recheck
    /// interval, about five seconds.
    #[tokio::test]
    async fn revoking_a_device_closes_the_agent_session_stream_it_already_had() {
        use tower::ServiceExt as _;

        let token = "device-token";
        let state = state_with(vec![
            FakeAgent::new("opencode").with_sessions(&[("ses_1", 10)])
        ])
        .await;
        let app = Router::new()
            .route(
                "/api/agent-sessions/{asid}/stream",
                get(stream_agent_session_global),
            )
            .with_state(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/agent-sessions/ses_1/stream")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let revoking = tokio::spawn({
            let state = state.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                crate::connectivity::routes::revoke_paired_device(
                    State(state),
                    Path("phone-1".to_owned()),
                    crate::test_support::bearer_headers(token),
                )
                .await
                .expect("the revoke route should accept a paired device")
            }
        });

        let ended = tokio::time::timeout(
            STREAM_DEVICE_RECHECK_INTERVAL * 3,
            axum::body::to_bytes(response.into_body(), 1 << 20),
        )
        .await;
        drop(revoking.await.unwrap());
        let body = ended.expect("the stream outlived the revocation of the device holding it");
        let body = String::from_utf8(body.unwrap().to_vec()).unwrap();
        assert!(body.contains("event: connected"), "{body}");
    }

    #[tokio::test]
    async fn a_failing_agent_is_skipped_not_a_502() {
        let state = state_with(vec![
            FakeAgent::new("opencode").failing(),
            FakeAgent::new("deepseek").with_sessions(&[("ses_ds", 20)]),
        ])
        .await;
        let response =
            do_list_agent_sessions(&state, &SessionQuery::default(), None, &device_headers())
                .await
                .expect("the healthy agent still answers");
        let data = body_json(response).await["data"].clone();
        assert_eq!(
            ids(&data),
            vec![("ses_ds".to_string(), "deepseek".to_string())]
        );

        // Naming the broken one is an explicit ask, and says it failed.
        let refusal = do_list_agent_sessions(
            &state,
            &SessionQuery::default(),
            Some("opencode"),
            &device_headers(),
        )
        .await
        .expect_err("the named agent is down");
        assert_eq!(refusal.0, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn the_agent_query_selects_one_catalog_and_project_list() {
        let mut opencode = FakeAgent::new("opencode");
        opencode.catalog = complete_catalog();
        let mut deepseek = FakeAgent::new("deepseek");
        deepseek.catalog = catalog_with(vec![mode("deepseek-agent")]);
        let state = state_with(vec![opencode, deepseek]).await;

        let query = |agent_id: Option<&str>| AgentSessionsQuery {
            directory: None,
            parent_id: None,
            roots: None,
            limit: None,
            order: None,
            search: None,
            cursor: None,
            agent_id: agent_id.map(str::to_string),
        };
        let modes_of = |body: &Value| -> Vec<String> {
            body["data"]["modes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["id"].as_str().unwrap().to_string())
                .collect()
        };

        let response = get_global_agent_catalog(
            State(state.clone()),
            Query(query(Some("deepseek"))),
            device_headers(),
        )
        .await
        .expect("deepseek catalog");
        assert_eq!(modes_of(&body_json(response).await), vec!["deepseek-agent"]);

        let response = get_global_agent_catalog(
            State(state.clone()),
            Query(query(Some("opencode"))),
            device_headers(),
        )
        .await
        .expect("opencode catalog");
        assert_eq!(modes_of(&body_json(response).await), vec!["build"]);

        let response = do_list_agent_projects(&state, Some("deepseek"), &device_headers())
            .await
            .expect("deepseek projects");
        assert_eq!(
            body_json(response).await["data"][0]["id"],
            "deepseek-project"
        );
        let response = do_list_agent_projects(&state, None, &device_headers())
            .await
            .expect("primary projects");
        assert_eq!(
            body_json(response).await["data"][0]["id"],
            "opencode-project"
        );

        let refusal = get_global_agent_catalog(
            State(state.clone()),
            Query(query(Some("claude"))),
            device_headers(),
        )
        .await
        .expect_err("unknown");
        assert_eq!(refusal.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            crate::test_support::error_body(&refusal)["error"]["code"],
            "invalid_agent"
        );
    }

    #[tokio::test]
    async fn agent_status_is_per_agent_with_the_two_refusals() {
        let state = state_with(vec![FakeAgent::new("opencode"), FakeAgent::new("deepseek")]).await;
        let status = |agent_id: Option<&str>| {
            get_agent_status(
                State(state.clone()),
                Query(AgentQuery {
                    agent_id: agent_id.map(str::to_string),
                }),
                device_headers(),
            )
        };

        let Json(primary) = status(None).await.expect("primary");
        assert_eq!(primary["data"]["agent_id"], "opencode");
        assert_eq!(primary["data"]["kind"], "opencode");
        assert_eq!(primary["data"]["available"], true);

        let Json(other) = status(Some("deepseek")).await.expect("deepseek");
        assert_eq!(other["data"]["agent_id"], "deepseek");
        assert_eq!(other["data"]["kind"], "deepseek");
        assert_eq!(other["data"]["available"], true);

        let refusal = status(Some("claude")).await.expect_err("unknown");
        assert_eq!(refusal.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            crate::test_support::error_body(&refusal)["error"]["code"],
            "invalid_agent"
        );

        let only_opencode = state_with(vec![FakeAgent::new("opencode")]).await;
        let refusal = get_agent_status(
            State(only_opencode),
            Query(AgentQuery {
                agent_id: Some("deepseek".into()),
            }),
            device_headers(),
        )
        .await
        .expect_err("not attached");
        assert_eq!(refusal.0, StatusCode::SERVICE_UNAVAILABLE);
        let body = crate::test_support::error_body(&refusal);
        assert_eq!(body["error"]["code"], "agent_unavailable");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("deepseek"));
    }
}
