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

use super::directories::expand_directory_param;
use super::domain::{AgentSessionId, ModelRef, PermissionDecision, SessionQuery};
use super::message_images;
use super::ports::mirror::SessionMirrorPort;
use crate::platform::git;
use crate::platform::vcs_routes::{self, VcsDiscardBody, VcsFileQuery};
use crate::tasks::transcript::{for_session, hide_in_event, hide_in_inbox, hide_in_items};
use crate::terminal::routes::git_error;
use crate::{
    api_error, content_envelope, require_device, still_paired, stream_event, validate_text,
    ApiResult, AppState, EncryptedStreamContext, EventStreamSealer, STREAM_DEVICE_RECHECK_INTERVAL,
};

#[derive(Debug, Deserialize)]
pub struct AgentDirectoriesQuery {
    /// What the user has typed: `~/Work/mu`, `/ho`, `~/.co`.
    pub prefix: Option<String>,
    /// Case-insensitive folder-name keyword search beneath the user's home.
    pub query: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentSessionsQuery {
    pub directory: Option<String>,
    /// Exact native session context for plugin catalogs; unused by list reads.
    pub asid: Option<String>,
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

#[derive(Debug, Default, Deserialize)]
pub struct AgentSnapshotQuery {
    #[serde(default)]
    pub refresh: bool,
}

/// What a session list answers when the caller does not say.
///
/// The app already asks for fifty; a caller that asks for nothing used to get
/// every session the agent had ever seen. A page is the right default for a
/// list a phone scrolls.
const DEFAULT_SESSION_LIMIT: usize = 50;
const MAX_SESSION_PAGE_LIMIT: usize = 100;
const MAX_SESSION_CURSOR_BYTES: usize = 16 * 1024;

impl AgentSessionsQuery {
    fn to_page_query(&self) -> ApiResult<SessionQuery> {
        let invalid = |message| api_error(StatusCode::BAD_REQUEST, "invalid_query", message);
        if self
            .agent_id
            .as_deref()
            .is_none_or(|id| id.trim().is_empty())
        {
            return Err(invalid("agent_id is required for session pagination"));
        }
        if self
            .limit
            .is_some_and(|limit| !(1..=MAX_SESSION_PAGE_LIMIT).contains(&limit))
        {
            return Err(invalid("limit must be between 1 and 100"));
        }
        if self
            .order
            .as_deref()
            .is_some_and(|order| !matches!(order, "asc" | "desc"))
        {
            return Err(invalid("order must be asc or desc"));
        }
        for (value, max, nonempty) in [
            (self.agent_id.as_deref(), 256, true),
            (self.directory.as_deref(), 4096, true),
            (self.parent_id.as_deref(), 256, true),
            (self.search.as_deref(), 4096, false),
            (self.cursor.as_deref(), MAX_SESSION_CURSOR_BYTES, true),
        ] {
            if value.is_some_and(|v| {
                v.len() > max
                    || v.chars().any(char::is_control)
                    || (nonempty && v.trim().is_empty())
            }) {
                return Err(invalid(
                    "Session query contains an invalid or oversized value",
                ));
            }
        }
        if self.directory.as_deref().is_some_and(|directory| {
            super::directories::expand_home(directory.trim()).is_none_or(|path| !path.is_absolute())
        }) {
            return Err(invalid("directory must be absolute or home-relative"));
        }
        let mut query = self.to_session_query();
        query.order = Some(self.order.clone().unwrap_or_else(|| "desc".into()));
        Ok(query)
    }

    fn to_session_query(&self) -> SessionQuery {
        SessionQuery {
            directory: expand_directory_param(self.directory.as_deref()),
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
    pub agent_id: Option<String>,
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkSessionBody {
    pub message_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SendAgentPromptBody {
    pub text: String,
    /// Correlation only. A successful or lost reply never licenses a repeated send.
    #[serde(default)]
    pub client_ref: Option<String>,
    #[serde(default)]
    pub computer_ids: Vec<String>,
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
        .route(
            "/api/agents/{agent_id}/account-limits",
            get(get_account_limits),
        )
        .route(
            "/api/agent-sessions/{asid}/audio-asset",
            get(message_images::audio_asset),
        )
        .route(
            "/api/agent-sessions/{asid}/file-asset",
            get(message_images::file_asset),
        )
        .route(
            "/api/agent-sessions/{asid}/file-source",
            get(message_images::file_source),
        )
        .route(
            "/api/agent-images/{id}/{variant}",
            get(message_images::image_content),
        )
        .route(
            "/api/agent-images/{id}/{variant}/{version}",
            get(message_images::image_version_content),
        )
        // Standalone independent OpenCode agent routes (no tmux / herdr session required)
        .route(
            "/api/agent-sessions",
            get(list_agent_sessions_global).post(create_agent_session_global),
        )
        .route("/api/agent-sessions/page", get(list_agent_sessions_page))
        .route(
            "/api/agent-sessions/{asid}",
            get(get_agent_session_global).delete(delete_agent_session_global),
        )
        .route(
            "/api/agents/{agent_id}/sessions/{asid}",
            delete(delete_agent_session_scoped),
        )
        .route(
            "/api/agent-sessions/{asid}/prepare",
            post(prepare_agent_session_global),
        )
        .route(
            "/api/agents/{agent_id}/sessions/{asid}/prepare",
            post(prepare_agent_session_scoped),
        )
        .route("/api/agent-sessions/{asid}/fork", post(fork_agent_session))
        .route(
            "/api/agents/{agent_id}/sessions/{asid}/fork",
            post(fork_agent_session_scoped),
        )
        .route(
            "/api/agent-sessions/{asid}/children",
            get(list_agent_session_children),
        )
        .route(
            "/api/agent-sessions/{asid}/descendants",
            get(list_agent_session_descendants),
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
            post(run_agent_session_command).layer(large_body()),
        )
        .route("/api/agent-sessions/{asid}/inbox", get(get_agent_inbox))
        .route(
            "/api/agent-sessions/{asid}/inbox/{inbox_id}",
            delete(cancel_agent_inbox_item).patch(edit_agent_inbox_item),
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
        .route(
            "/api/agent-sessions/{asid}/timeline/earlier",
            post(load_earlier_timeline_global),
        )
        .route(
            "/api/agent-sessions/{asid}/timeline/window",
            get(history_window_global),
        )
        .route(
            "/api/agent-sessions/{asid}/timeline/page",
            post(history_page_global),
        )
        .route("/api/agent-sessions/{asid}/mode", post(switch_mode_global))
        .route("/api/agent-files", get(find_agent_files_root_global))
        .route(
            "/api/agent-sessions/{asid}/files",
            get(find_agent_files_global),
        )
        .route(
            "/api/agent-sessions/{asid}/prompt",
            post(send_agent_prompt_global).layer(large_body()),
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
            "/api/agent-sessions/{asid}/vcs/files",
            get(get_agent_vcs_files),
        )
        .route(
            "/api/agent-sessions/{asid}/vcs/file",
            get(get_agent_vcs_file),
        )
        .route(
            "/api/agent-sessions/{asid}/vcs/discard",
            post(discard_agent_vcs_file),
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
            "/api/sessions/{session_id}/agent-sessions/{asid}/timeline/earlier",
            post(load_earlier_timeline_legacy),
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
            post(send_agent_prompt_legacy).layer(large_body()),
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

async fn get_account_limits(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    let manager = agent_manager_or_err(&state, &agent_id).await?;
    let snapshot = manager
        .agent()
        .account_limits()
        .await
        .map_err(agent_error)?;
    let mut response = Json(snapshot).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

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

    if query.cursor.is_some() && agent_id.is_none_or(|id| id.trim().is_empty()) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "A provider cursor requires an explicit agent_id",
        ));
    }
    let all_managers = managers_for_read(state, agent_id).await?;

    let mut sessions = Vec::new();
    let mut any_success = false;
    let mut last_err = None;

    // Asked at once: one slow agent must not hold the others' lists back.
    let lists = futures::future::join_all(
        all_managers
            .iter()
            .map(|manager| manager.sessions().list_sessions(query)),
    )
    .await;
    for (manager, list) in all_managers.iter().zip(lists) {
        match list {
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

    let directory = expand_directory_param(body.directory.as_deref());
    let validated_dir = match directory.as_deref() {
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
    do_read_agent_session(state, asid, false, headers).await
}

async fn do_read_agent_session(
    state: &AppState,
    asid: &str,
    refresh: bool,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    require_device(state, headers)?;
    Ok(json_etag_response(
        headers,
        socket_read_agent_session(state, asid, refresh).await?.0,
    ))
}

async fn socket_read_agent_session(
    state: &AppState,
    asid: &str,
    refresh: bool,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err(state, asid).await?;
    let id = AgentSessionId(asid.to_string());
    let mut snapshot = if refresh {
        manager
            .sessions()
            .refresh_snapshot(&id)
            .await
            .map_err(agent_error)?
    } else {
        manager
            .sessions()
            .get_snapshot(&id)
            .await
            .map_err(agent_error)?
    };
    let directory = snapshot.info.directory.clone();
    let plumbing = for_session(state, asid).await;
    hide_in_items(&mut snapshot.timeline, plumbing);
    if plumbing.task {
        hide_in_inbox(&mut snapshot.inbox);
    }
    message_images::attach_to_items(state, asid, directory.as_deref(), &mut snapshot.timeline)
        .await;
    message_images::attach_to_inbox(state, asid, &mut snapshot.inbox).await;

    // A session the reader is sitting on is polled and mostly unchanged; the
    // whole snapshot is the expensive thing to send twice.
    Ok(Json(content_envelope(json!(snapshot))))
}

/// Called only after the socket has authenticated an App principal. Reuses the
/// exact HTTP projection, including hidden internal rows and image attachment.
pub(crate) async fn socket_history(
    state: &AppState,
    asid: &str,
    operation: &str,
    refresh: bool,
    after: u64,
    cursor: Option<&str>,
) -> ApiResult<Json<Value>> {
    match operation {
        "window" => socket_history_window(state, asid, cursor, refresh).await,
        "page" => socket_history_page(state, asid, cursor).await,
        "snapshot" => socket_read_agent_session(state, asid, refresh).await,
        "earlier" => socket_load_earlier_timeline(state, asid).await,
        "delta" => socket_get_agent_session_timeline(state, asid, after).await,
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_operation",
            "Unknown history operation",
        )),
    }
}

#[derive(Default, Deserialize)]
struct HistoryWindowQuery {
    cursor: Option<String>,
    #[serde(default)]
    refresh: bool,
}

async fn history_window_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<HistoryWindowQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    socket_history_window(&state, &asid, query.cursor.as_deref(), query.refresh).await
}

async fn history_page_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(query): Json<HistoryWindowQuery>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    socket_history_page(&state, &asid, query.cursor.as_deref()).await
}

fn validate_history_cursor(cursor: Option<&str>) -> ApiResult<()> {
    if cursor.is_some_and(|cursor| cursor.is_empty() || cursor.len() > 4096) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "Invalid history cursor",
        ));
    }
    Ok(())
}

async fn socket_history_window(
    state: &AppState,
    asid: &str,
    cursor: Option<&str>,
    refresh: bool,
) -> ApiResult<Json<Value>> {
    validate_history_cursor(cursor)?;
    let manager = session_manager_or_err(state, asid).await?;
    if !manager.agent().supports_earlier_timeline() {
        // Providers without older paging must retain the complete cached
        // snapshot; otherwise opening a bounded window would hide history.
        return socket_read_agent_session(state, asid, refresh).await;
    }
    let mut window = manager
        .sessions()
        .history_window(&AgentSessionId(asid.to_owned()), cursor, refresh)
        .await
        .map_err(agent_error)?;
    let snapshot = &mut window.snapshot;
    let plumbing = for_session(state, asid).await;
    hide_in_items(&mut snapshot.timeline, plumbing);
    if plumbing.task {
        hide_in_inbox(&mut snapshot.inbox);
    }
    message_images::attach_to_items(
        state,
        asid,
        snapshot.info.directory.as_deref(),
        &mut snapshot.timeline,
    )
    .await;
    message_images::attach_to_inbox(state, asid, &mut snapshot.inbox).await;
    Ok(Json(content_envelope(json!(window))))
}

async fn socket_history_page(
    state: &AppState,
    asid: &str,
    cursor: Option<&str>,
) -> ApiResult<Json<Value>> {
    validate_history_cursor(cursor)?;
    let cursor = cursor.ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "A history cursor is required",
        )
    })?;
    let manager = session_manager_or_err(state, asid).await?;
    let mut page = manager
        .sessions()
        .history_page(&AgentSessionId(asid.to_owned()), cursor)
        .await
        .map_err(agent_error)?;
    hide_in_items(&mut page.items, for_session(state, asid).await);
    message_images::attach_to_items(state, asid, None, &mut page.items).await;
    Ok(Json(content_envelope(json!(page))))
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
        Some(mut events) => {
            let plumbing = for_session(state, asid).await;
            for event in &mut events {
                hide_in_event(event, plumbing);
                message_images::attach_to_event(state, event).await;
            }
            Ok(Json(content_envelope(json!(events))))
        }
        None => Err(api_error(
            StatusCode::GONE,
            "resync_required",
            "Requested events are no longer in the ring buffer; re-fetch the snapshot",
        )),
    }
}

async fn load_earlier_timeline_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_load_earlier_timeline(&state, &asid, &headers).await
}

async fn load_earlier_timeline_legacy(
    State(state): State<AppState>,
    Path((_, asid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_load_earlier_timeline(&state, &asid, &headers).await
}

async fn do_load_earlier_timeline(
    state: &AppState,
    asid: &str,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;
    socket_load_earlier_timeline(state, asid).await
}

async fn socket_load_earlier_timeline(state: &AppState, asid: &str) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err(state, asid).await?;
    let mut items = manager
        .sessions()
        .load_earlier_timeline(&AgentSessionId(asid.to_string()))
        .await
        .map_err(agent_error)?;
    // Filtering internal plumbing must not turn a consumed page into a false
    // end-of-history signal. Only the adapter knows whether it returned rows.
    let exhausted = items.is_empty();
    hide_in_items(&mut items, for_session(state, asid).await);
    message_images::attach_to_items(state, asid, None, &mut items).await;
    Ok(Json(content_envelope(json!({
        "items": items,
        "exhausted": exhausted,
    }))))
}

async fn do_get_agent_session_timeline(
    state: &AppState,
    asid: &str,
    after_seq: u64,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;
    socket_get_agent_session_timeline(state, asid, after_seq).await
}

async fn socket_get_agent_session_timeline(
    state: &AppState,
    asid: &str,
    after_seq: u64,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err(state, asid).await?;

    let (mut items, status, resync, latest_seq) = manager
        .mirror()
        .get_timeline_delta(&AgentSessionId(asid.to_string()), after_seq)
        .await;
    hide_in_items(&mut items, for_session(state, asid).await);
    message_images::attach_to_items(state, asid, None, &mut items).await;

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

enum FileSearchScope<'a> {
    Session(&'a str),
    Agent(Option<&'a str>),
}

async fn do_find_agent_files(
    state: &AppState,
    scope: FileSearchScope<'_>,
    query: &str,
    limit: usize,
    directory: Option<&str>,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let (manager, directory) = match scope {
        FileSearchScope::Session(asid) => {
            let manager = session_manager_or_err(state, asid).await?;
            let info = manager
                .agent()
                .get_session(asid)
                .await
                .map_err(agent_error)?;
            let directory = info
                .directory
                .filter(|path| std::path::Path::new(path).is_absolute())
                .ok_or_else(|| {
                    api_error(
                        StatusCode::BAD_REQUEST,
                        "workspace_unknown",
                        "The selected session has no workspace directory",
                    )
                })?;
            (manager, Some(directory))
        }
        FileSearchScope::Agent(agent_id) => {
            let manager = match agent_id {
                Some(id) => agent_manager_or_err(state, id).await?,
                None => state.agent_runtime.manager().await.ok_or_else(|| {
                    api_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "agent_unavailable",
                        "No agent is available",
                    )
                })?,
            };
            (manager, expand_directory_param(directory))
        }
    };
    let directory = directory.as_deref();
    require_directory(directory)?;

    let files = manager
        .agent()
        .find_files(query, limit, directory)
        .await
        .map_err(agent_error)?;

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

    if body.client_ref.as_ref().is_some_and(|value| {
        value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {
                "code": "invalid_client_ref", "message": "client_ref must be 1 to 128 of A-Z, a-z, 0-9, - and _", "delivery": "not_sent"
            }})),
        ));
    }

    let context = super::computer_context::context(state, &body.computer_ids)
        .await
        .map_err(super::computer_context::ComputerContextError::before_dispatch)?;
    let text = if let Some(context) = context {
        let info = manager
            .sessions()
            .get_session(asid)
            .await
            .map_err(agent_error)?;
        super::computer_context::prepare(
            state,
            manager.agent().as_ref(),
            asid,
            info.directory.as_deref(),
        )
        .await
        .map_err(super::computer_context::ComputerContextError::before_dispatch)?;
        format!("{}\n\n{}", body.text, context)
    } else {
        body.text.clone()
    };
    let attachments = body.attachments.as_deref().unwrap_or(&[]);
    manager
        .prompts()
        .send_prompt_with_display(
            &AgentSessionId(asid.to_string()),
            &text,
            &body.text,
            attachments,
            body.delivery.as_deref(),
        )
        .await
        .map_err(|e| match e {
            super::ports::agent::AgentError::InvalidRequest(_)
            | super::ports::agent::AgentError::TerminalInteraction(_) => agent_error(e),
            _ => api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()),
        })?;

    let mut accepted = json!({ "submitted": true });
    if let Some(client_ref) = body.client_ref {
        accepted["client_ref"] = json!(client_ref);
    }
    Ok(Json(content_envelope(accepted)))
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
        .map_err(|e| match e {
            super::ports::agent::AgentError::TerminalInteraction(_) => agent_error(e),
            _ => api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()),
        })?;

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
        .map_err(|e| match e {
            super::ports::agent::AgentError::TerminalInteraction(_) => agent_error(e),
            _ => api_error(StatusCode::BAD_GATEWAY, "agent_error", &e.to_string()),
        })?;

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
    let mut body = vcs_diff_body(json!(diffs), is_repo);
    // The same `repo` the Changes list carries. Best effort: a git that
    // cannot answer leaves it out rather than failing the diff.
    if let Some(directory) = directory.as_deref().filter(|_| is_repo) {
        if let Ok(line) = Box::pin(git::branch_line(std::path::Path::new(directory))).await {
            body["repo"] = line.branch_json();
        }
    }
    Ok(Json(content_envelope(body)))
}

// ---------------------------------------------------------------------------
// Changes (git): the agent-independent list, one file, and discard
// ---------------------------------------------------------------------------
//
// `vcs/diff` asks the agent and answers every patch at once; these ask git,
// in the session's directory, so they mean the same thing for every agent.

/// The checkout an agent session works in: `None` when the session has no
/// directory or its directory is in no checkout. A directory that has been
/// deleted is `404 workspace_missing`, as on every directory-scoped route; an
/// agent that cannot say where the session is, or a git that cannot answer,
/// is an error rather than "not a repository".
async fn agent_session_checkout(
    state: &AppState,
    asid: &str,
) -> ApiResult<Option<std::path::PathBuf>> {
    let manager = session_manager_or_err(state, asid).await?;
    let directory = manager
        .agent()
        .get_session(asid)
        .await
        .map_err(agent_error)?
        .directory
        .filter(|directory| !directory.trim().is_empty());
    require_directory(directory.as_deref())?;
    let Some(directory) = directory else {
        return Ok(None);
    };
    git::repository_root(std::path::Path::new(&directory))
        .await
        .map_err(git_error)
}

async fn get_agent_vcs_files(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<AgentVcsDiffQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let mode = vcs_routes::vcs_mode(query.mode.as_deref())?;
    let toplevel = agent_session_checkout(&state, &asid).await?;
    vcs_routes::files(toplevel.as_deref(), mode).await
}

async fn get_agent_vcs_file(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<VcsFileQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let mode = vcs_routes::vcs_mode(query.mode.as_deref())?;
    let toplevel = agent_session_checkout(&state, &asid).await?;
    vcs_routes::file(toplevel.as_deref(), mode, &query).await
}

async fn discard_agent_vcs_file(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<VcsDiscardBody>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let toplevel = agent_session_checkout(&state, &asid).await?;
    let (action, answer) = vcs_routes::discard(toplevel.as_deref(), &body.path).await?;
    tracing::info!(
        asid,
        path = ?body.path,
        action = action.as_str(),
        "discarded an agent session change"
    );
    Ok(answer)
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

/// `GET /api/agent-directories`: a completer for the folder a session opens
/// in. `data` stays the bare `[{name, path}]` list older Apps read; `home` and
/// `truncated` sit beside it in the envelope.
async fn do_list_agent_directories(
    state: &AppState,
    query: AgentDirectoriesQuery,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;

    let home = dirs::home_dir();
    let completion = if let Some(keyword) = query.query.as_deref().filter(|q| !q.trim().is_empty())
    {
        super::directories::search(keyword, home.as_deref()).await
    } else {
        super::directories::complete(query.prefix.as_deref().unwrap_or(""), home.as_deref()).await
    };
    let mut body = content_envelope(json!(completion.directories));
    body["home"] = json!(home.map(|h| h.to_string_lossy().into_owned()));
    body["truncated"] = json!(completion.truncated);
    Ok(Json(body))
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
/// the encrypted transport, so on a `disabled` deployment the stream stays
/// plaintext byte for byte (`required` serves no device without a transport
/// key -- `require_device` below refuses it).
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
                Ok(mut ev) => {
                    // An event with no session -- a global resync, or a
                    // worktree change -- reaches every stream.
                    if !ev.asid().0.is_empty() && ev.asid() != &target_asid {
                        continue;
                    }
                    hide_in_event(&mut ev, for_session(&devices, &target_asid.0).await);
                    message_images::attach_to_event(&devices, &mut ev).await;
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
    Query(mut query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    query.directory = expand_directory_param(query.directory.as_deref());

    let all_managers = if let Some(asid) = query.asid.as_deref() {
        validate_text(asid)?;
        if asid.trim().is_empty() {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_asid",
                "asid must not be empty",
            ));
        }
        let manager = session_manager_or_err(&state, asid).await?;
        if query
            .agent_id
            .as_deref()
            .is_some_and(|agent| agent != manager.agent().kind())
        {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "agent_session_mismatch",
                "asid belongs to another agent",
            ));
        }
        vec![manager]
    } else {
        managers_for_read(&state, query.agent_id.as_deref()).await?
    };

    require_directory(query.directory.as_deref())?;

    let mut merged_models = Vec::new();
    let mut merged_modes = Vec::new();
    let mut merged_mcp = Vec::new();
    let mut merged_skills = Vec::new();
    let mut merged_providers = Vec::new();
    let mut merged_commands = Vec::new();
    let mut primary_defaults = None;

    let catalogs = futures::future::join_all(all_managers.iter().map(|manager| {
        manager
            .sessions()
            .get_catalog_for_session(query.asid.as_deref(), query.directory.as_deref())
    }))
    .await;
    for result in catalogs {
        let cat = match result {
            Ok(cat) => cat,
            Err(error) if query.asid.is_some() => return Err(agent_error(error)),
            Err(_) => continue,
        };
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

async fn list_agent_sessions_page(
    State(state): State<AppState>,
    query: Result<Query<AgentSessionsQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    // Capture extraction failures so malformed queries cannot bypass auth.
    let Query(query) = query.map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "Invalid session query",
        )
    })?;
    let session_query = query.to_page_query()?;
    let manager = agent_manager_or_err(&state, query.agent_id.as_deref().unwrap()).await?;
    let page = manager
        .sessions()
        .list_sessions_page(&session_query)
        .await
        .map_err(agent_error)?;
    for session in &page.data {
        state
            .agent_runtime
            .record_session_route(&session.asid.0, manager.agent().kind())
            .await;
    }
    Ok(json_etag_response(&headers, content_envelope(json!(page))))
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
    Query(query): Query<AgentSnapshotQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    do_read_agent_session(&state, &asid, query.refresh, &headers).await
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
        FileSearchScope::Agent(query.agent_id.as_deref()),
        query.query.as_deref().unwrap_or(""),
        query.limit.unwrap_or(20),
        query.directory.as_deref(),
        &headers,
    )
    .await
}

async fn find_agent_files_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<AgentFilesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_find_agent_files(
        &state,
        FileSearchScope::Session(&asid),
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
    Query(query): Query<AgentSnapshotQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    do_read_agent_session(&state, &asid, query.refresh, &headers).await
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
        FileSearchScope::Agent(query.agent_id.as_deref()),
        query.query.as_deref().unwrap_or(""),
        query.limit.unwrap_or(20),
        query.directory.as_deref(),
        &headers,
    )
    .await
}

async fn find_agent_files_legacy(
    State(state): State<AppState>,
    Path((_session_id, asid)): Path<(String, String)>,
    Query(query): Query<AgentFilesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_find_agent_files(
        &state,
        FileSearchScope::Session(&asid),
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
        super::ports::agent::AgentError::SessionNotFound(_) => {
            api_error(StatusCode::NOT_FOUND, "session_not_found", &err.to_string())
        }
        super::ports::agent::AgentError::TerminalInteraction(runtime) => (
            StatusCode::CONFLICT,
            Json(
                json!({ "error": { "code": "agent_terminal_interaction_required",
                "message": "Native interaction is unavailable; use the current terminal binding when provided",
                "runtime": runtime } }),
            ),
        ),
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
        super::ports::agent::AgentError::InboxItemNotFound(message) => {
            api_error(StatusCode::NOT_FOUND, "inbox_item_not_found", message)
        }
        super::ports::agent::AgentError::HistoryCursorLost => api_error(
            StatusCode::CONFLICT,
            "history_cursor_lost",
            &err.to_string(),
        ),
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

/// The body limit of the routes that take a prompt (`config::takes_large_body`).
/// The transport middleware authenticates such a request before its body
/// is read (`server::large_body_after_auth`); this is the limit it then gets.
pub(crate) fn large_body() -> axum::extract::DefaultBodyLimit {
    axum::extract::DefaultBodyLimit::max(crate::MAX_PROMPT_BODY_BYTES)
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

/// How deep `/descendants` walks below the session asked about.
const MAX_DESCENDANT_DEPTH: usize = 8;
/// The most sessions `/descendants` answers; past it the answer is `truncated`.
const MAX_DESCENDANTS: usize = 500;

/// `GET /api/agent-sessions/{asid}/descendants`: `/children` over every
/// depth at once. The app used to walk `/children` once per descendant to
/// open a workbench (fifty-odd requests for one tree); this is one.
async fn list_agent_session_descendants(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    Query(query): Query<AgentSessionsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    do_list_agent_session_descendants(&state, &asid, &query, &headers).await
}

/// Breadth-first over the adapters' children listings, the same query
/// per parent as `/children` takes (less the cursor, which pages one
/// listing and cannot page a tree), at most [`MAX_DESCENDANT_DEPTH`] deep
/// and [`MAX_DESCENDANTS`] sessions; `truncated: true` beside the list
/// when either bound cut the walk short. Error semantics are `/children`'s:
/// an agent that fails is skipped while another answers, unless it was the
/// one named.
async fn do_list_agent_session_descendants(
    state: &AppState,
    asid: &str,
    query: &AgentSessionsQuery,
    headers: &HeaderMap,
) -> ApiResult<Response> {
    require_device(state, headers)?;
    if query.cursor.is_some() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "descendants take no cursor",
        ));
    }
    let agent_id = query.agent_id.as_deref().filter(|id| !id.trim().is_empty());
    let all_managers = managers_for_read(state, agent_id).await?;
    let mut session_query = query.to_session_query();

    let mut seen: std::collections::HashSet<String> = [asid.to_owned()].into_iter().collect();
    let mut sessions = Vec::new();
    let mut frontier = vec![asid.to_owned()];
    let mut truncated = false;
    let mut any_success = false;
    let mut last_err = None;
    'walk: for _ in 0..MAX_DESCENDANT_DEPTH {
        if frontier.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for parent in &frontier {
            session_query.parent_id = Some(parent.clone());
            for manager in &all_managers {
                let list = match manager.sessions().list_sessions(&session_query).await {
                    Ok(list) => list,
                    Err(e) => {
                        tracing::warn!(
                            agent_id = manager.agent().kind(),
                            error = %e,
                            "agent children list failed; skipping this agent"
                        );
                        last_err = Some(e);
                        continue;
                    }
                };
                any_success = true;
                for session in list {
                    if !seen.insert(session.asid.0.clone()) {
                        continue;
                    }
                    if sessions.len() >= MAX_DESCENDANTS {
                        truncated = true;
                        break 'walk;
                    }
                    state
                        .agent_runtime
                        .record_session_route(&session.asid.0, manager.agent().kind())
                        .await;
                    next.push(session.asid.0.clone());
                    sessions.push(session);
                }
            }
        }
        frontier = next;
    }
    if !frontier.is_empty() {
        // Depth exhausted with children still to ask about.
        truncated = true;
    }
    if !any_success || (agent_id.is_some() && last_err.is_some()) {
        if let Some(err) = last_err {
            return Err(agent_error(err));
        }
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_ms));
    let mut data = json!({ "data": sessions });
    if truncated {
        data["truncated"] = Value::Bool(true);
    }
    Ok(json_etag_response(headers, content_envelope(data)))
}

async fn prepare_agent_session_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_prepare_agent_session(&state, &asid, None, &headers).await
}

async fn prepare_agent_session_scoped(
    State(state): State<AppState>,
    Path((agent_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_prepare_agent_session(&state, &asid, Some(&agent_id), &headers).await
}

async fn do_prepare_agent_session(
    state: &AppState,
    asid: &str,
    agent_id: Option<&str>,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;
    if asid.is_empty()
        || asid.len() > 4096
        || matches!(asid, "." | "..")
        || asid.chars().any(char::is_control)
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_session",
            "Invalid session identifier",
        ));
    }
    let manager = match agent_id {
        Some(agent_id) => agent_manager_or_err(state, agent_id).await?,
        None => session_manager_or_err(state, asid).await?,
    };
    manager
        .sessions()
        .prepare_session(&AgentSessionId(asid.to_owned()))
        .await
        .map_err(agent_error)?;
    state
        .agent_runtime
        .record_session_route(asid, manager.agent().kind())
        .await;
    Ok(Json(content_envelope(json!({"prepared":true}))))
}

async fn delete_agent_session_global(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_delete_agent_session(&state, &asid, None, &headers).await
}

async fn delete_agent_session_scoped(
    State(state): State<AppState>,
    Path((agent_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    do_delete_agent_session(&state, &asid, Some(&agent_id), &headers).await
}

async fn do_delete_agent_session(
    state: &AppState,
    asid: &str,
    agent_id: Option<&str>,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;
    let manager = if let Some(agent_id) = agent_id {
        if asid.is_empty()
            || asid.len() > 4096
            || matches!(asid, "." | "..")
            || !asid.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
        {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_session",
                "Invalid session identifier",
            ));
        }
        let manager = agent_manager_or_err(state, agent_id).await?;
        let info = manager.agent().get_session(asid).await.map_err(|error| {
            use super::ports::agent::AgentError;
            let missing = matches!(&error, AgentError::SessionNotFound(_))
                || matches!(&error, AgentError::RequestFailed(message) if message.starts_with("HTTP 404"));
            if missing {
                api_error(
                    StatusCode::NOT_FOUND,
                    "session_not_found",
                    "The selected agent does not own this session",
                )
            } else {
                api_error(
                    StatusCode::BAD_GATEWAY,
                    "agent_error",
                    "The selected agent could not verify this session",
                )
            }
        })?;
        if info.asid.0 != asid {
            return Err(api_error(
                StatusCode::CONFLICT,
                "session_changed",
                "The selected agent returned a different session",
            ));
        }
        manager
    } else {
        session_manager_or_err(state, asid).await?
    };
    manager
        .agent()
        .delete_session(asid)
        .await
        .map_err(agent_error)?;
    // OpenCode deletes the children too, but it also announces each one on the
    // stream, so the mirror only has to forget this session here.
    manager
        .mirror()
        .remove_session(&AgentSessionId(asid.to_owned()))
        .await;
    if let Some(agent_id) = agent_id {
        state
            .agent_runtime
            .forget_session_route_for_agent(asid, agent_id)
            .await;
    } else {
        state.agent_runtime.forget_session_route(asid).await;
    }
    Ok(Json(content_envelope(json!({ "deleted": true }))))
}

async fn fork_agent_session(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<ForkSessionBody>>,
) -> ApiResult<Json<Value>> {
    do_fork_agent_session(
        &state,
        &asid,
        None,
        body.map(|Json(body)| body).unwrap_or_default(),
        &headers,
    )
    .await
}

async fn fork_agent_session_scoped(
    State(state): State<AppState>,
    Path((agent_id, asid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<ForkSessionBody>>,
) -> ApiResult<Json<Value>> {
    do_fork_agent_session(
        &state,
        &asid,
        Some(&agent_id),
        body.map(|Json(body)| body).unwrap_or_default(),
        &headers,
    )
    .await
}

async fn do_fork_agent_session(
    state: &AppState,
    asid: &str,
    agent_id: Option<&str>,
    body: ForkSessionBody,
    headers: &HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(state, headers)?;
    validate_text(asid)?;
    if asid.len() > 4096
        || matches!(asid, "." | "..")
        || !asid
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        || asid.trim().is_empty()
        || body
            .message_id
            .as_ref()
            .is_some_and(|id| id.trim().is_empty())
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_session",
            "Session and message identities must not be empty",
        ));
    }
    if let Some(id) = &body.message_id {
        validate_text(id)?;
    }
    let manager = match agent_id {
        Some(agent_id) => agent_manager_or_err(state, agent_id).await?,
        None => session_manager_or_err(state, asid).await?,
    };
    let source = manager
        .agent()
        .get_session(asid)
        .await
        .map_err(agent_error)?;
    if source.asid.0 != asid {
        return Err(api_error(
            StatusCode::CONFLICT,
            "session_changed",
            "The selected agent returned a different session",
        ));
    }
    let session = manager
        .sessions()
        .fork_session(asid, body.message_id.as_deref())
        .await
        .map_err(agent_error)?;
    state
        .agent_runtime
        .record_session_route(&session.asid.0, manager.agent().kind())
        .await;
    Ok(Json(content_envelope(json!(session))))
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
    Query(mut query): Query<AgentWorktreesQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    query.directory = expand_directory_param(query.directory.as_deref());
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
    Json(mut body): Json<CreateWorktreeBody>,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    body.directory = expand_directory_param(body.directory.as_deref());
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
    Json(mut body): Json<RemoveWorktreeBody>,
) -> ApiResult<Json<Value>> {
    let manager = manager_or_unavailable!(&state, &headers);
    body.directory = expand_directory_param(body.directory.as_deref());
    require_directory(body.directory.as_deref())?;
    let worktree = expand_directory_param(Some(&body.worktree)).unwrap_or_default();
    let worktree = worktree.trim();
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
    let directory = expand_directory_param(directory.as_deref());
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
    let directory = expand_directory_param(Some(&body.directory)).unwrap_or_default();
    let directory = directory.trim();
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
    let result = manager
        .agent()
        .execute_command(&asid, name, &arguments, body.delivery.as_deref())
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(result)))
}

async fn get_agent_inbox(
    State(state): State<AppState>,
    Path(asid): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    let mut items = manager
        .agent()
        .get_inbox(&asid)
        .await
        .map_err(agent_error)?;
    // Refresh the mirror so a later snapshot and the stream agree.
    manager
        .mirror()
        .set_inbox(&AgentSessionId(asid.clone()), items.clone())
        .await;
    if for_session(&state, &asid).await.task {
        hide_in_inbox(&mut items);
    }
    message_images::attach_to_inbox(&state, &asid, &mut items).await;
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInboxBody {
    text: String,
}

async fn edit_agent_inbox_item(
    State(state): State<AppState>,
    Path((asid, inbox_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<EditInboxBody>,
) -> ApiResult<Json<Value>> {
    let manager = session_manager_or_err!(&state, &headers, &asid);
    validate_text(&body.text)?;
    if body.text.trim().is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "text must not be empty",
        ));
    }
    manager
        .agent()
        .edit_inbox_item(&asid, &inbox_id, &body.text)
        .await
        .map_err(agent_error)?;
    Ok(Json(content_envelope(
        json!({"edited":true,"inbox_id":inbox_id}),
    )))
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
        .list_shells(expand_directory_param(query.directory.as_deref()).as_deref())
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

    #[test]
    fn a_rejected_session_resume_is_not_a_missing_session() {
        use super::super::ports::agent::AgentError;
        let (status, Json(body)) = agent_error(AgentError::RequestFailed(
            "thread already has an active writer".into(),
        ));
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"]["code"], "agent_error");
        let (status, Json(body)) = agent_error(AgentError::SessionNotFound("gone".into()));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "session_not_found");
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

    #[tokio::test]
    async fn explicit_snapshot_refresh_requires_app_device_before_provider_lookup() {
        let state = crate::test_support::required_test_state(
            "admin",
            vec![
                crate::test_support::sealed(crate::test_support::test_peer_device(
                    "peer",
                    "peer-token",
                )),
                crate::test_support::test_peer_device("old-peer", "old-peer-token"),
            ],
        );
        for (headers, status, code) in [
            (
                HeaderMap::new(),
                StatusCode::UNAUTHORIZED,
                "missing_authorization",
            ),
            (
                crate::test_support::bearer_headers("admin"),
                StatusCode::FORBIDDEN,
                "invalid_token",
            ),
            (
                crate::test_support::sealed_bearer_headers("peer-token"),
                StatusCode::FORBIDDEN,
                "peer_forbidden",
            ),
            (
                crate::test_support::bearer_headers("old-peer-token"),
                StatusCode::UPGRADE_REQUIRED,
                "device_repair_required",
            ),
        ] {
            let edit_refusal = edit_agent_inbox_item(
                State(state.clone()),
                Path(("fixture".into(), "queued".into())),
                headers.clone(),
                Json(EditInboxBody {
                    text: "changed".into(),
                }),
            )
            .await
            .unwrap_err();
            assert_eq!(edit_refusal.0, status);
            assert_eq!(
                crate::test_support::error_body(&edit_refusal)["error"]["code"],
                code
            );
            let limits_refusal =
                get_account_limits(State(state.clone()), Path("codex".into()), headers.clone())
                    .await
                    .unwrap_err();
            assert_eq!(limits_refusal.0, status);
            for selected in [None, Some("fx")] {
                let refusal = do_prepare_agent_session(&state, "fixture", selected, &headers)
                    .await
                    .unwrap_err();
                assert_eq!(refusal.0, status);
                assert_eq!(
                    crate::test_support::error_body(&refusal)["error"]["code"],
                    code
                );
            }
            let refusal = do_read_agent_session(&state, "fixture", true, &headers)
                .await
                .unwrap_err();
            assert_eq!(refusal.0, status);
            assert_eq!(
                crate::test_support::error_body(&refusal)["error"]["code"],
                code
            );
        }
    }

    #[tokio::test]
    async fn foreground_prepare_is_separate_from_snapshot_and_automatic_refresh() {
        let mut agent = FakeAgent::new("opencode").with_sessions(&[("conversation", 1)]);
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        agent.preparation_log = Some(log.clone());
        let prompts = agent.prompts.clone();
        let state = state_with(vec![agent]).await;
        do_read_agent_session(&state, "conversation", false, &device_headers())
            .await
            .unwrap();
        do_read_agent_session(&state, "conversation", true, &device_headers())
            .await
            .unwrap();
        assert!(log.lock().unwrap().iter().all(|(selected, _)| !selected));
        let observed = log.lock().unwrap().len();
        assert!(observed >= 2);
        let Json(response) =
            do_prepare_agent_session(&state, "conversation", Some("opencode"), &device_headers())
                .await
                .unwrap();
        assert_eq!(response["data"]["prepared"], true);
        assert_eq!(
            &log.lock().unwrap()[observed..],
            &[(true, "conversation".to_owned())]
        );
        assert!(prompts.lock().unwrap().is_empty());
        assert!(
            do_prepare_agent_session(&state, "missing", Some("opencode"), &device_headers())
                .await
                .is_err()
        );
        assert_eq!(log.lock().unwrap().len(), observed + 1);
    }

    #[tokio::test]
    async fn computer_reference_preflight_never_submits_and_keeps_provider_details_private() {
        use crate::test_support::FakeMcp;
        for (mcp, code) in [
            (None, "computer_tools_unavailable"),
            (
                Some(std::sync::Arc::new(std::sync::Mutex::new(FakeMcp {
                    refuse: true,
                    ..Default::default()
                }))),
                "computer_tools_not_ready",
            ),
        ] {
            let mut agent = FakeAgent::new("opencode").with_sessions(&[("conversation", 1)]);
            agent.mcp = mcp;
            let prompts = agent.prompts.clone();
            let state = state_with(vec![agent]).await;
            let body = serde_json::from_value(json!({
                "text": "Inspect", "computer_ids": [state.config.server_id], "client_ref": "prompt-one"
            })).unwrap();
            let (status, Json(refusal)) =
                do_send_agent_prompt(&state, "conversation", body, &device_headers())
                    .await
                    .unwrap_err();
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(refusal["error"]["code"], code);
            assert_eq!(refusal["error"]["delivery"], "not_sent");
            assert!(prompts.lock().unwrap().is_empty());
            assert!(!refusal.to_string().contains("Bearer"));
        }
    }

    #[tokio::test]
    async fn prompt_correlation_is_optional_bounded_and_does_not_claim_idempotency() {
        let agent = FakeAgent::new("opencode").with_sessions(&[("conversation", 1)]);
        let prompts = agent.prompts.clone();
        let state = state_with(vec![agent]).await;
        for client_ref in ["", "bad ref", "../bad", &"x".repeat(129)] {
            let body = serde_json::from_value(json!({"text": "Inspect", "client_ref": client_ref}))
                .unwrap();
            let (_, Json(refusal)) =
                do_send_agent_prompt(&state, "conversation", body, &device_headers())
                    .await
                    .unwrap_err();
            assert_eq!(refusal["error"]["code"], "invalid_client_ref");
            assert_eq!(refusal["error"]["delivery"], "not_sent");
        }
        assert!(prompts.lock().unwrap().is_empty());
        for value in [
            json!({"text": "Inspect"}),
            json!({"text": "Inspect", "client_ref": "prompt-one"}),
        ] {
            let body = serde_json::from_value(value.clone()).unwrap();
            let Json(accepted) =
                do_send_agent_prompt(&state, "conversation", body, &device_headers())
                    .await
                    .unwrap();
            assert_eq!(accepted["data"]["submitted"], true);
            assert_eq!(accepted["data"]["client_ref"], value["client_ref"]);
        }
        assert_eq!(prompts.lock().unwrap().len(), 2);
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

    #[tokio::test]
    async fn account_limits_refuses_unsupported_and_never_falls_back() {
        let state = state_with(vec![FakeAgent::new("opencode")]).await;
        for (agent, status) in [
            ("opencode", StatusCode::NOT_IMPLEMENTED),
            ("codex", StatusCode::SERVICE_UNAVAILABLE),
            ("unknown", StatusCode::BAD_REQUEST),
        ] {
            let response = get_response(
                state.clone(),
                &format!("/api/agents/{agent}/account-limits"),
                crate::test_support::bearer_headers("device-token"),
            )
            .await;
            assert_eq!(response.status(), status);
        }
    }

    /// One answer for the whole tree: every depth below the session, each
    /// row shaped as `/children` shapes it, newest first, merged over the
    /// attached agents unless one is named; nothing from another tree, and
    /// the session itself is not its own descendant.
    #[tokio::test]
    async fn descendants_route_flattens_the_tree_under_one_session() {
        let opencode = FakeAgent::new("opencode").with_session_tree(&[
            ("root", None, 1),
            ("a", Some("root"), 5),
            ("b", Some("root"), 3),
            ("a1", Some("a"), 7),
            ("a2", Some("a"), 2),
            ("a1x", Some("a1"), 9),
            ("b1", Some("b"), 4),
            ("other", None, 8),
            ("other-child", Some("other"), 6),
        ]);
        let oc_calls = opencode.calls.clone();
        let t3 = FakeAgent::new("t3").with_session_tree(&[("t3-child", Some("root"), 10)]);
        let state = state_with(vec![opencode, t3]).await;
        for (headers, expected) in [
            (HeaderMap::new(), StatusCode::UNAUTHORIZED),
            (
                crate::test_support::bearer_headers("wrong"),
                StatusCode::FORBIDDEN,
            ),
        ] {
            let response = get_response(
                state.clone(),
                "/api/agent-sessions/root/descendants",
                headers,
            )
            .await;
            assert_eq!(response.status(), expected);
        }

        let response = get_response(
            state.clone(),
            "/api/agent-sessions/root/descendants",
            device_headers(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["capabilities"]["session_descendants"], true);
        assert_eq!(
            ids(&body["data"]["data"]),
            [
                ("t3-child".to_string(), "t3".to_string()),
                ("a1x".to_string(), "opencode".to_string()),
                ("a1".to_string(), "opencode".to_string()),
                ("a".to_string(), "opencode".to_string()),
                ("b1".to_string(), "opencode".to_string()),
                ("b".to_string(), "opencode".to_string()),
                ("a2".to_string(), "opencode".to_string()),
            ]
        );
        assert!(body["data"].get("truncated").is_none(), "{body}");
        // Asked per parent, breadth first, as `/children` asks -- every
        // agent about every session found, whichever agent listed it.
        let asked: Vec<String> = oc_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.starts_with("list_sessions"))
            .cloned()
            .collect();
        assert_eq!(
            asked,
            [
                "list_sessions parent=root",
                "list_sessions parent=a",
                "list_sessions parent=b",
                "list_sessions parent=t3-child",
                "list_sessions parent=a1",
                "list_sessions parent=a2",
                "list_sessions parent=b1",
                "list_sessions parent=a1x",
            ]
        );
        assert_eq!(
            state
                .agent_runtime
                .manager_for_session("a1x")
                .await
                .unwrap()
                .agent()
                .kind(),
            "opencode",
            "every descendant is routed to the agent that listed it"
        );

        // One agent named: its tree only. A cursor cannot page a tree.
        let response = get_response(
            state.clone(),
            "/api/agent-sessions/root/descendants?agent_id=t3",
            device_headers(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(
            ids(&body["data"]["data"]),
            [("t3-child".to_string(), "t3".to_string())]
        );
        let response = get_response(
            state.clone(),
            "/api/agent-sessions/root/descendants?agent_id=opencode&cursor=x",
            device_headers(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["error"]["code"], "invalid_query");

        // A leaf has none, and that is an empty list, not an error.
        let response = get_response(
            state.clone(),
            "/api/agent-sessions/a1x/descendants",
            device_headers(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["data"]["data"], json!([]));
    }

    /// The walk is bounded: eight levels deep and five hundred sessions.
    /// Past either, what was gathered is answered with `truncated: true`,
    /// never an error and never a list that quietly stops.
    #[tokio::test]
    async fn descendants_route_truncates_past_its_depth_and_size_bounds() {
        // A chain twelve deep under `root`: eight levels are answered.
        let chain: Vec<(String, Option<String>, u64)> = (1..=12)
            .map(|depth| {
                let parent = if depth == 1 {
                    "root".to_string()
                } else {
                    format!("d{}", depth - 1)
                };
                (format!("d{depth}"), Some(parent), depth as u64)
            })
            .collect();
        let chain_refs: Vec<(&str, Option<&str>, u64)> = chain
            .iter()
            .map(|(asid, parent, updated)| (asid.as_str(), parent.as_deref(), *updated))
            .collect();
        let state = state_with(vec![
            FakeAgent::new("opencode").with_session_tree(&chain_refs)
        ])
        .await;
        let response = get_response(
            state,
            "/api/agent-sessions/root/descendants",
            device_headers(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let listed = ids(&body["data"]["data"]);
        assert_eq!(listed.len(), MAX_DESCENDANT_DEPTH);
        assert_eq!(
            listed[0].0, "d8",
            "newest first: the deepest level answered"
        );
        assert_eq!(body["data"]["truncated"], true);

        // Six hundred children under one session: five hundred are answered.
        let wide: Vec<(String, Option<String>, u64)> = (0..600)
            .map(|n| (format!("c{n}"), Some("root".to_string()), n as u64))
            .collect();
        let wide_refs: Vec<(&str, Option<&str>, u64)> = wide
            .iter()
            .map(|(asid, parent, updated)| (asid.as_str(), parent.as_deref(), *updated))
            .collect();
        let state = state_with(vec![
            FakeAgent::new("opencode").with_session_tree(&wide_refs)
        ])
        .await;
        let response = get_response(
            state,
            "/api/agent-sessions/root/descendants?limit=100",
            device_headers(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(ids(&body["data"]["data"]).len(), MAX_DESCENDANTS);
        assert_eq!(body["data"]["truncated"], true);
    }

    /// `/children`'s error semantics carry over: an agent that fails is
    /// skipped while another answers; the one named failing is a 502.
    #[tokio::test]
    async fn descendants_route_skips_a_failing_agent_unless_it_was_named() {
        let state = state_with(vec![
            FakeAgent::new("opencode").with_session_tree(&[("kid", Some("root"), 1)]),
            FakeAgent::new("t3").failing(),
        ])
        .await;
        let response = get_response(
            state.clone(),
            "/api/agent-sessions/root/descendants",
            device_headers(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            ids(&body_json(response).await["data"]["data"]),
            [("kid".to_string(), "opencode".to_string())]
        );
        let response = get_response(
            state,
            "/api/agent-sessions/root/descendants?agent_id=t3",
            device_headers(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn timeline_earlier_route_authenticates_and_refuses_unsupported_adapters() {
        let state = state_with(vec![
            FakeAgent::new("opencode").with_sessions(&[("owned", 1)])
        ])
        .await;
        for (headers, expected) in [
            (HeaderMap::new(), StatusCode::UNAUTHORIZED),
            (
                crate::test_support::bearer_headers("wrong"),
                StatusCode::FORBIDDEN,
            ),
        ] {
            assert_eq!(
                do_load_earlier_timeline(&state, "owned", &headers)
                    .await
                    .unwrap_err()
                    .0,
                expected
            );
        }
        let refusal = do_load_earlier_timeline(&state, "owned", &device_headers())
            .await
            .unwrap_err();
        assert_eq!(refusal.0, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(
            crate::test_support::error_body(&refusal)["error"]["code"],
            "feature_unsupported"
        );
    }

    #[tokio::test]
    async fn timeline_earlier_route_uses_codex_owner_and_retains_bounded_rows_in_snapshot() {
        use crate::agents::adapters::codex::{
            tests::{golden, Fixture},
            CodexDriver,
        };
        use crate::agents::ports::agent::AgentPort;
        use std::sync::Arc;
        let mut fixture = Fixture::new(|r| match r["method"].as_str().unwrap() {
            "thread/read" => json!({"thread":golden()["thread"]}),
            "thread/items/list" => {
                let offset = r["params"]["cursor"].as_str().map(|s|s.parse::<usize>().unwrap()).unwrap_or(0);
                let end = (offset+r["params"]["limit"].as_u64().unwrap() as usize).min(181);
                let rows: Vec<_> = (offset..end).map(|n| {
                    let item = if n == 180 {
                        json!({"type":"dynamicToolCall","id":"muqun-tool","tool":"muqun.tasks.list","arguments":{},"status":"completed"})
                    } else {
                        json!({"type":"agentMessage","id":format!("item-{}",181-n),"text":"synthetic"})
                    };
                    json!({"turnId":"turn","item":item})
                }).collect();
                json!({"data":rows,"nextCursor":if end<181 {json!(end.to_string())} else {Value::Null}})
            },
            _ => panic!("unexpected route fixture RPC"),
        }).await;
        fixture.ready().await;
        let driver = Arc::new(CodexDriver::new(fixture.client.clone()));
        let id = golden()["thread"]["id"].as_str().unwrap().to_owned();
        let tail = driver.get_timeline(&id, 100).await.unwrap();
        let state = state_with(vec![FakeAgent::new("opencode")]).await;
        let manager = Arc::new(crate::agents::manager::AgentManager::for_test(driver));
        manager
            .mirror()
            .update_session(
                crate::agents::adapters::codex::mapper::session(&golden()["thread"]).unwrap(),
            )
            .await;
        manager
            .mirror()
            .upsert_timeline_items(&AgentSessionId(id.clone()), tail.clone())
            .await;
        let held_tail = manager
            .mirror()
            .get_snapshot(&AgentSessionId(id.clone()))
            .await
            .unwrap()
            .timeline;
        state.agent_runtime.attach_for_test(manager.clone()).await;
        let Json(first) = do_load_earlier_timeline(&state, &id, &device_headers())
            .await
            .unwrap();
        assert_eq!(first["data"]["items"].as_array().unwrap().len(), 80);
        assert_eq!(first["data"]["exhausted"], false);
        let Json(last) = do_load_earlier_timeline(&state, &id, &device_headers())
            .await
            .unwrap();
        let last_items = last["data"]["items"].as_array().unwrap();
        assert_eq!(last_items.len(), 1);
        assert_eq!(last_items[0]["part"]["name"], "muqun.tasks.list");
        assert_eq!(last_items[0]["part"]["status"], "completed");
        assert_eq!(last["data"]["exhausted"], false);
        let Json(end) = do_load_earlier_timeline(&state, &id, &device_headers())
            .await
            .unwrap();
        assert_eq!(end["data"]["items"], json!([]));
        assert_eq!(end["data"]["exhausted"], true);
        let snapshot = manager
            .mirror()
            .get_snapshot(&AgentSessionId(id.clone()))
            .await
            .unwrap();
        assert_eq!(snapshot.timeline.len(), 181);
        assert_eq!(&snapshot.timeline[81..], &held_tail);
        manager
            .mirror()
            .mark_history_initialized(&AgentSessionId(id.clone()))
            .await;
        let Json(window) = socket_history_window(&state, &id, None, false)
            .await
            .unwrap();
        assert_eq!(window["data"]["timeline"].as_array().unwrap().len(), 100);
        let cursor = window["data"]["history_cursor"].as_str().unwrap();
        let Json(page) = socket_history_page(&state, &id, Some(cursor))
            .await
            .unwrap();
        assert_eq!(page["data"]["items"].as_array().unwrap().len(), 80);
        let Json(retry) = socket_history_page(&state, &id, Some(cursor))
            .await
            .unwrap();
        assert_eq!(retry["data"], page["data"]);
        let cursor = page["data"]["history_cursor"].as_str().unwrap();
        let Json(last) = socket_history_page(&state, &id, Some(cursor))
            .await
            .unwrap();
        assert_eq!(last["data"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(last["data"]["items"][0]["part"]["name"], "muqun.tasks.list");
        let cursor = last["data"]["history_cursor"].as_str().unwrap();
        let Json(end) = socket_history_page(&state, &id, Some(cursor))
            .await
            .unwrap();
        assert_eq!(end["data"]["exhausted"], true);
        let Json(reopen) = socket_history_window(&state, &id, None, false)
            .await
            .unwrap();
        assert_eq!(reopen["data"]["timeline"].as_array().unwrap().len(), 100);
    }

    /// The DeepSeek owner pages like the Codex one: each pull is one older
    /// batch under the frozen cut, retained in the snapshot, and `exhausted`
    /// only once the adapter has nothing below.
    #[tokio::test]
    async fn timeline_earlier_route_uses_deepseek_owner_and_retains_rows_in_snapshot() {
        use crate::agents::adapters::deepseek::{
            client::tests::{paged_history, rpc_server},
            DeepseekDriver,
        };
        use crate::agents::ports::agent::AgentPort;
        use std::sync::Arc;
        let (app, _calls) = paged_history(0);
        let (client, server) = rpc_server(app).await;
        let driver = Arc::new(DeepseekDriver::with_client(client));
        let tail = driver.get_timeline("s", 30).await.unwrap();
        assert_eq!(tail[0].seq, 270, "thirty messages: the newest page");
        let state = state_with(vec![FakeAgent::new("opencode")]).await;
        let manager = Arc::new(crate::agents::manager::AgentManager::for_test(driver));
        manager
            .mirror()
            .update_session(
                crate::agents::adapters::deepseek::mapper::map_session(&json!({
                    "sessionId": "s", "cwd": "/qa", "title": "paged"
                }))
                .unwrap(),
            )
            .await;
        manager
            .mirror()
            .upsert_timeline_items(&AgentSessionId("s".into()), tail.clone())
            .await;
        state.agent_runtime.attach_for_test(manager.clone()).await;
        // The fixture serves no session list, so the route is recorded by hand.
        state
            .agent_runtime
            .record_session_route("s", "deepseek")
            .await;
        let mut lowest = 270;
        let mut batches = 0;
        let mut ids: Vec<String> = tail.iter().map(|item| item.id.clone()).collect();
        loop {
            let Json(batch) = do_load_earlier_timeline(&state, "s", &device_headers())
                .await
                .unwrap();
            let items = batch["data"]["items"].as_array().unwrap();
            if batch["data"]["exhausted"] == true {
                assert!(items.is_empty());
                break;
            }
            batches += 1;
            assert!(!items.is_empty(), "a page under the cut is never empty");
            // Contiguous below the rows held: the record at the cut may be a
            // tool result, joined onto the call just before it, so the last
            // row sits at most two seqs under the cut.
            let producer_index = |item: &Value| -> u64 {
                item["message_id"]
                    .as_str()
                    .unwrap()
                    .strip_prefix('s')
                    .unwrap()
                    .parse()
                    .unwrap()
            };
            let last = producer_index(items.last().unwrap());
            assert!(last < lowest && lowest - last <= 2, "{last} under {lowest}");
            lowest = producer_index(&items[0]);
            // Public seq is the accepted mirror revision, not the producer's
            // chronological record number. Responses and retained rows agree.
            let retained = manager
                .mirror()
                .get_snapshot(&AgentSessionId("s".into()))
                .await
                .unwrap();
            for item in items {
                let row = retained
                    .timeline
                    .iter()
                    .find(|row| row.id == item["id"])
                    .unwrap();
                assert_eq!(row.seq, item["seq"].as_u64().unwrap());
                assert!(row.seq > 0 && row.seq <= retained.seq);
            }
            let mut older: Vec<String> = items
                .iter()
                .map(|item| item["id"].as_str().unwrap().to_owned())
                .collect();
            older.append(&mut ids);
            ids = older;
        }
        assert_eq!(lowest, 0);
        assert!(batches >= 2, "{batches} batches: the walk was paged");
        // Every batch is retained in the snapshot, oldest first, before the
        // tail it was read under (the mirror numbers the rows itself).
        let snapshot = manager
            .mirror()
            .get_snapshot(&AgentSessionId("s".into()))
            .await
            .unwrap();
        assert_eq!(
            snapshot
                .timeline
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            ids
        );
        server.abort();
    }

    #[tokio::test]
    async fn fork_route_authenticates_selects_exact_provider_and_registers_new_session() {
        let codex = FakeAgent::new("codex").with_sessions(&[("source", 1)]);
        let codex_calls = codex.calls.clone();
        let opencode = FakeAgent::new("opencode").with_sessions(&[("source", 1)]);
        let calls = opencode.calls.clone();
        let state = state_with(vec![codex, opencode]).await;
        for (headers, expected) in [
            (HeaderMap::new(), StatusCode::UNAUTHORIZED),
            (
                crate::test_support::bearer_headers("wrong"),
                StatusCode::FORBIDDEN,
            ),
        ] {
            assert_eq!(
                do_fork_agent_session(
                    &state,
                    "source",
                    Some("opencode"),
                    ForkSessionBody::default(),
                    &headers
                )
                .await
                .unwrap_err()
                .0,
                expected
            );
        }
        assert!(calls.lock().unwrap().is_empty() && codex_calls.lock().unwrap().is_empty());
        let Json(body) = do_fork_agent_session(
            &state,
            "source",
            Some("opencode"),
            ForkSessionBody {
                message_id: Some("boundary".into()),
            },
            &device_headers(),
        )
        .await
        .unwrap();
        assert_eq!(body["data"]["asid"], "opencode_fork");
        assert_eq!(body["data"]["agent_id"], "opencode");
        assert_eq!(*calls.lock().unwrap(), ["fork_session source boundary"]);
        assert!(codex_calls.lock().unwrap().is_empty());
        let manager = state
            .agent_runtime
            .manager_for_session("opencode_fork")
            .await
            .unwrap();
        assert_eq!(manager.agent().kind(), "opencode");
        assert!(manager
            .mirror()
            .get_snapshot(&AgentSessionId("opencode_fork".into()))
            .await
            .is_some());
        assert!(do_fork_agent_session(
            &state,
            "missing",
            Some("codex"),
            ForkSessionBody::default(),
            &device_headers()
        )
        .await
        .is_err());
        assert!(codex_calls.lock().unwrap().is_empty());
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body reads");
        serde_json::from_slice(&bytes).expect("body is json")
    }

    async fn get_response(state: AppState, uri: &str, headers: HeaderMap) -> Response {
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder().uri(uri);
        *request.headers_mut().unwrap() = headers;
        mount(Router::new())
            .with_state(state)
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn session_page_dispatches_once_to_the_selected_provider_and_retains_order_and_cursors() {
        use crate::agents::use_cases::session_service::tests::PageAgent;
        let state = state_with(vec![FakeAgent::new("deepseek").failing()]).await;
        let agent = PageAgent::new("opencode");
        let queries = agent.queries.clone();
        state
            .agent_runtime
            .attach_for_test(std::sync::Arc::new(
                super::super::manager::AgentManager::for_test(std::sync::Arc::new(agent)),
            ))
            .await;
        let uri = "/api/agent-sessions/page?agent_id=opencode&directory=~&roots=true&order=asc&search=work&cursor=opaque%2B%2F%3D";
        let response = get_response(state.clone(), uri, device_headers()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let tag = response.headers()[header::ETAG].clone();
        let body = body_json(response).await;
        assert_eq!(
            ids(&body["data"]["data"]),
            [
                ("older".to_string(), "opencode".to_string()),
                ("newer".to_string(), "opencode".to_string()),
            ]
        );
        assert_eq!(
            body["data"]["cursor"],
            json!({"next": "next+/=", "previous": "previous-/="})
        );
        assert_eq!(
            *queries.lock().unwrap(),
            [(
                true,
                SessionQuery {
                    directory: expand_directory_param(Some("~")),
                    parent_id: Some("null".into()),
                    limit: Some(50),
                    order: Some("asc".into()),
                    search: Some("work".into()),
                    cursor: Some("opaque+/=".into()),
                }
            )]
        );
        let owner = state
            .agent_runtime
            .manager_for_session("older")
            .await
            .unwrap();
        assert_eq!(owner.agent().kind(), "opencode");
        let mut headers = device_headers();
        headers.insert(header::IF_NONE_MATCH, tag);
        assert_eq!(
            get_response(state, uri, headers).await.status(),
            StatusCode::NOT_MODIFIED
        );
    }

    #[tokio::test]
    async fn session_page_authentication_precedes_malformed_query_rejection() {
        use crate::agents::use_cases::session_service::tests::PageAgent;
        let state = state_with(vec![]).await;
        let agent = PageAgent::new("opencode");
        let queries = agent.queries.clone();
        state
            .agent_runtime
            .attach_for_test(std::sync::Arc::new(
                super::super::manager::AgentManager::for_test(std::sync::Arc::new(agent)),
            ))
            .await;
        for query in ["limit=no", "roots=maybe"] {
            let uri = format!("/api/agent-sessions/page?agent_id=opencode&{query}");
            for (headers, status, code) in [
                (
                    HeaderMap::new(),
                    StatusCode::UNAUTHORIZED,
                    "missing_authorization",
                ),
                (
                    crate::test_support::bearer_headers("wrong-token"),
                    StatusCode::FORBIDDEN,
                    "invalid_token",
                ),
                (device_headers(), StatusCode::BAD_REQUEST, "invalid_query"),
            ] {
                let response = get_response(state.clone(), &uri, headers).await;
                assert_eq!(response.status(), status, "{query}: {code}");
                assert_eq!(body_json(response).await["error"]["code"], code);
                assert!(queries.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn session_page_rejects_invalid_queries_and_missing_auth_before_upstream() {
        use crate::agents::use_cases::session_service::tests::PageAgent;
        let state = state_with(vec![]).await;
        let agent = PageAgent::new("opencode");
        let queries = agent.queries.clone();
        state
            .agent_runtime
            .attach_for_test(std::sync::Arc::new(
                super::super::manager::AgentManager::for_test(std::sync::Arc::new(agent)),
            ))
            .await;
        assert_eq!(
            get_response(
                state.clone(),
                "/api/agent-sessions/page?agent_id=opencode",
                HeaderMap::new()
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
        let oversized = format!(
            "agent_id=opencode&cursor={}",
            "x".repeat(MAX_SESSION_CURSOR_BYTES + 1)
        );
        for query in [
            "",
            "agent_id=",
            "agent_id=%20",
            "agent_id=opencode&limit=0",
            "agent_id=opencode&limit=101",
            "agent_id=opencode&limit=-1",
            "agent_id=opencode&limit=no",
            "agent_id=opencode&order=newest",
            "agent_id=opencode&cursor=",
            "agent_id=opencode&cursor=%0A",
            "agent_id=opencode&roots=maybe",
            "agent_id=opencode&directory=relative",
            "agent_id=opencode&directory=~other",
            "agent_id=opencode&parent_id=%20",
            &oversized,
        ] {
            let response = get_response(
                state.clone(),
                &format!("/api/agent-sessions/page?{query}"),
                device_headers(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        }
        assert!(queries.lock().unwrap().is_empty());
        for limit in [1, 100] {
            let response = get_response(
                state.clone(),
                &format!("/api/agent-sessions/page?agent_id=opencode&limit={limit}&order=desc"),
                device_headers(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(queries.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn session_page_maps_unsupported_unknown_and_unavailable_agents() {
        let state = state_with(vec![FakeAgent::new("deepseek")]).await;
        for (agent_id, status, code) in [
            (
                "deepseek",
                StatusCode::NOT_IMPLEMENTED,
                "feature_unsupported",
            ),
            ("unknown", StatusCode::BAD_REQUEST, "invalid_agent"),
            (
                "opencode",
                StatusCode::SERVICE_UNAVAILABLE,
                "agent_unavailable",
            ),
        ] {
            let response = get_response(
                state.clone(),
                &format!("/api/agent-sessions/page?agent_id={agent_id}"),
                device_headers(),
            )
            .await;
            assert_eq!(response.status(), status);
            assert_eq!(body_json(response).await["error"]["code"], code);
        }
    }

    #[tokio::test]
    async fn unscoped_legacy_cursor_is_rejected_before_any_provider_fanout() {
        use crate::agents::use_cases::session_service::tests::PageAgent;
        let state = state_with(vec![]).await;
        let mut query_logs = vec![];
        for kind in ["opencode", "deepseek"] {
            let agent = PageAgent::new(kind);
            query_logs.push(agent.queries.clone());
            state
                .agent_runtime
                .attach_for_test(std::sync::Arc::new(
                    super::super::manager::AgentManager::for_test(std::sync::Arc::new(agent)),
                ))
                .await;
        }
        for path in [
            "/api/agent-sessions",
            "/api/sessions/ignored/agent-sessions",
        ] {
            let response = get_response(
                state.clone(),
                &format!("{path}?cursor=opaque"),
                device_headers(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        assert!(query_logs.iter().all(|log| log.lock().unwrap().is_empty()));
        let response = get_response(state, "/api/agent-sessions", device_headers()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["data"].as_array().unwrap().len(), 4);
        assert_eq!(body["data"][0]["updated_ms"], 30);
        assert_eq!(body["data"][3]["updated_ms"], 10);
        for log in query_logs {
            assert_eq!(
                log.lock().unwrap().as_slice(),
                [(
                    false,
                    SessionQuery {
                        limit: Some(50),
                        ..Default::default()
                    }
                )]
            );
        }
    }

    async fn delete_response(state: AppState, uri: &str, token: Option<&str>) -> Response {
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder().method("DELETE").uri(uri);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        mount(Router::new())
            .with_state(state)
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn scoped_session_delete_selects_the_named_owner_even_for_colliding_ids() {
        let opencode = FakeAgent::new("opencode")
            .with_sessions(&[("shared", 1)])
            .with_session_deletion();
        let t3 = FakeAgent::new("t3")
            .with_sessions(&[("shared", 1)])
            .with_session_deletion();
        let oc_calls = opencode.calls.clone();
        let t3_calls = t3.calls.clone();
        let state = state_with(vec![opencode, t3]).await;
        state
            .agent_runtime
            .record_session_route("shared", "t3")
            .await;

        let response = delete_response(
            state.clone(),
            "/api/agents/opencode/sessions/shared",
            Some("device-token"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["data"]["deleted"], true);
        assert_eq!(*oc_calls.lock().unwrap(), ["delete_session shared"]);
        assert!(t3_calls.lock().unwrap().is_empty());
        assert_eq!(
            state
                .agent_runtime
                .manager_for_session("shared")
                .await
                .unwrap()
                .agent()
                .kind(),
            "t3",
            "scoped deletion must keep another provider's cached route"
        );
    }

    #[tokio::test]
    async fn scoped_session_delete_never_falls_back_to_another_provider() {
        let opencode = FakeAgent::new("opencode")
            .with_sessions(&[("owned", 1)])
            .with_session_deletion();
        let oc_calls = opencode.calls.clone();
        let t3 = FakeAgent::new("t3").with_session_deletion();
        let t3_calls = t3.calls.clone();
        let state = state_with(vec![opencode, t3]).await;
        state
            .agent_runtime
            .record_session_route("owned", "opencode")
            .await;
        let response =
            delete_response(state, "/api/agents/t3/sessions/owned", Some("device-token")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await["error"]["code"],
            "session_not_found"
        );
        assert!(oc_calls.lock().unwrap().is_empty());
        assert!(t3_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn scoped_session_delete_refuses_unknown_unavailable_and_untrusted_callers() {
        let state = state_with(Vec::new()).await;
        // The admin token is a control-route refusal of `required` mode (see
        // `control_routes_reject_the_admin_token`); the state here is
        // token-only, where it acts as the owner's app.
        for (agent_id, token, status, code) in [
            (
                "unknown",
                None,
                StatusCode::UNAUTHORIZED,
                "missing_authorization",
            ),
            (
                "unknown",
                Some("wrong"),
                StatusCode::FORBIDDEN,
                "invalid_token",
            ),
            (
                "unknown",
                Some("device-token"),
                StatusCode::BAD_REQUEST,
                "invalid_agent",
            ),
            (
                "t3",
                Some("device-token"),
                StatusCode::SERVICE_UNAVAILABLE,
                "agent_unavailable",
            ),
        ] {
            let response = delete_response(
                state.clone(),
                &format!("/api/agents/{agent_id}/sessions/owned"),
                token,
            )
            .await;
            assert_eq!(response.status(), status);
            assert_eq!(body_json(response).await["error"]["code"], code);
        }
        let peer_state = crate::test_support::test_state(
            "admin",
            vec![crate::test_support::test_peer_device("peer", "peer-token")],
        );
        let response = delete_response(
            peer_state,
            "/api/agents/unknown/sessions/owned",
            Some("peer-token"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(response).await["error"]["code"], "peer_forbidden");
    }

    #[tokio::test]
    async fn scoped_session_delete_propagates_unsupported_without_forgetting_the_session() {
        let deepseek = FakeAgent::new("deepseek").with_sessions(&[("owned", 1)]);
        let calls = deepseek.calls.clone();
        let state = state_with(vec![deepseek]).await;
        state
            .agent_runtime
            .record_session_route("owned", "deepseek")
            .await;
        let response = delete_response(
            state.clone(),
            "/api/agents/deepseek/sessions/owned",
            Some("device-token"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(
            body_json(response).await["error"]["code"],
            "feature_unsupported"
        );
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(
            state
                .agent_runtime
                .manager_for_session("owned")
                .await
                .unwrap()
                .agent()
                .kind(),
            "deepseek"
        );
    }

    #[tokio::test]
    async fn scoped_session_delete_refuses_the_providers_http_missing_session_response() {
        let mut opencode = FakeAgent::new("opencode")
            .with_sessions(&[("owned", 1)])
            .with_session_deletion();
        opencode.http_404.push("owned".into());
        let calls = opencode.calls.clone();
        let response = delete_response(
            state_with(vec![opencode]).await,
            "/api/agents/opencode/sessions/owned",
            Some("device-token"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await["error"]["code"],
            "session_not_found"
        );
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn legacy_session_delete_keeps_the_recorded_owner_lookup() {
        let opencode = FakeAgent::new("opencode")
            .with_sessions(&[("shared", 1)])
            .with_session_deletion();
        let t3 = FakeAgent::new("t3")
            .with_sessions(&[("shared", 1)])
            .with_session_deletion();
        let oc_calls = opencode.calls.clone();
        let t3_calls = t3.calls.clone();
        let state = state_with(vec![opencode, t3]).await;
        state
            .agent_runtime
            .record_session_route("shared", "t3")
            .await;
        let response =
            delete_response(state, "/api/agent-sessions/shared", Some("device-token")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(oc_calls.lock().unwrap().is_empty());
        assert_eq!(*t3_calls.lock().unwrap(), ["delete_session shared"]);
    }

    #[tokio::test]
    async fn scoped_session_delete_rejects_ids_that_could_change_the_provider_request_path() {
        let state = state_with(Vec::new()).await;
        for id in [
            "..",
            "owned%2Fchild",
            "owned%3Fquery",
            "owned%23fragment",
            "owned%252fchild",
        ] {
            let response = delete_response(
                state.clone(),
                &format!("/api/agents/opencode/sessions/{id}"),
                Some("device-token"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                body_json(response).await["error"]["code"],
                "invalid_session"
            );
        }
    }

    #[tokio::test]
    async fn scoped_session_delete_binds_encrypted_provider_identity_and_refuses_replay() {
        use base64::Engine as _;
        use tower::ServiceExt;

        let opencode = FakeAgent::new("opencode")
            .with_sessions(&[("shared", 1)])
            .with_session_deletion();
        let t3 = FakeAgent::new("t3")
            .with_sessions(&[("shared", 1)])
            .with_session_deletion();
        let oc_calls = opencode.calls.clone();
        let t3_calls = t3.calls.clone();
        let state = state_with(vec![opencode, t3]).await;
        let key = [13; 32];
        state.devices.lock().unwrap()[0].transport_key =
            Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key));
        let path = "/api/agents/opencode/sessions/shared";
        let payload = json!({ "token": "device-token", "body": "" });
        let envelope = crate::connectivity::transport::seal(
            &key,
            crate::connectivity::transport::Direction::Request,
            format!("DELETE {path}").as_bytes(),
            &serde_json::to_vec(&payload).unwrap(),
            crate::now_unix_ms(),
        )
        .unwrap();
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let request = |path: &str| {
            axum::http::Request::builder()
                .method("DELETE")
                .uri(path)
                .header("x-muqun-transport", "1")
                .header("x-muqun-device", "phone-1")
                .body(axum::body::Body::from(bytes.clone()))
                .unwrap()
        };
        let app = mount(Router::new())
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::platform::server::encrypted_transport,
            ))
            .with_state(state);
        let tampered = app
            .clone()
            .oneshot(request("/api/agents/t3/sessions/shared"))
            .await
            .unwrap();
        assert_eq!(tampered.status(), StatusCode::FORBIDDEN);
        assert!(oc_calls.lock().unwrap().is_empty());
        assert!(t3_calls.lock().unwrap().is_empty());
        assert_eq!(
            app.clone().oneshot(request(path)).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            app.oneshot(request(path)).await.unwrap().status(),
            StatusCode::CONFLICT
        );
        assert_eq!(*oc_calls.lock().unwrap(), ["delete_session shared"]);
        assert!(t3_calls.lock().unwrap().is_empty());
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

    /// `~` and `~/…` are the home directory, and the session records the
    /// canonical absolute path so the App groups it with the same folder typed
    /// in full. `~user`, a relative path and a missing folder are refused as
    /// they always were.
    #[tokio::test]
    async fn create_session_expands_home_and_answers_the_canonical_path() {
        let state = state_with(vec![FakeAgent::new("opencode")]).await;
        let headers = device_headers();
        let home = std::fs::canonicalize(dirs::home_dir().unwrap()).unwrap();
        let create = |directory: &str| CreateAgentSessionBody {
            directory: Some(directory.to_string()),
            model: None,
            mode: None,
            agent_id: None,
        };

        // A real subdirectory of home, whatever this machine has.
        let sub = std::fs::read_dir(&home)
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| {
                e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && !e.file_name().to_string_lossy().starts_with('.')
            })
            .map(|e| e.file_name().to_string_lossy().into_owned());

        let mut cases = vec![
            ("~".to_string(), home.clone()),
            ("~/".to_string(), home.clone()),
        ];
        if let Some(sub) = sub {
            cases.push((
                format!("~/{sub}"),
                std::fs::canonicalize(home.join(&sub)).unwrap(),
            ));
        }
        for (typed, expected) in cases {
            let Json(created) = do_create_agent_session(&state, create(&typed), &headers)
                .await
                .unwrap_or_else(|e| panic!("{typed} creates: {:?}", e.1));
            assert_eq!(
                created["data"]["directory"],
                expected.to_string_lossy().as_ref(),
                "{typed}"
            );
        }

        for (typed, code) in [
            ("~nobody", "invalid_directory"),
            ("~nobody/x", "invalid_directory"),
            ("relative/dir", "invalid_directory"),
            ("~/muqun-no-such-dir-for-tests", "directory_not_found"),
        ] {
            let refusal = do_create_agent_session(&state, create(typed), &headers)
                .await
                .err()
                .unwrap_or_else(|| panic!("{typed} is refused"));
            assert_eq!(refusal.0, StatusCode::BAD_REQUEST, "{typed}");
            assert_eq!(
                crate::test_support::error_body(&refusal)["error"]["code"],
                code,
                "{typed}"
            );
        }
    }

    #[tokio::test]
    async fn file_mentions_use_the_session_owner_and_directory_not_the_primary_provider() {
        let primary = FakeAgent::new("opencode");
        let primary_calls = primary.calls.clone();
        let mut selected = FakeAgent::new("codex").with_sessions(&[("selected", 1)]);
        let directory = crate::test_support::asset_test_dir("agent-file-scope");
        selected.sessions[0].directory = Some(directory.as_path().to_string_lossy().into_owned());
        let selected_calls = selected.calls.clone();
        let state = state_with(vec![primary, selected]).await;
        let _ = do_find_agent_files(
            &state,
            FileSearchScope::Session("selected"),
            "name",
            10,
            Some("/wrong-project"),
            &device_headers(),
        )
        .await
        .unwrap();
        assert!(!primary_calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.starts_with("find_files:")));
        assert!(selected_calls
            .lock()
            .unwrap()
            .contains(&format!("find_files:{}", directory.as_path().display())));
        let _ = do_find_agent_files(
            &state,
            FileSearchScope::Agent(Some("codex")),
            "name",
            10,
            Some(directory.as_path().to_str().unwrap()),
            &device_headers(),
        )
        .await
        .unwrap();
        assert_eq!(
            selected_calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.starts_with("find_files:"))
                .count(),
            2
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The directory-scoped routes expand `~` before the missing-folder check,
    /// so a `~/…` that is gone is the usual 404 naming the absolute path.
    #[tokio::test]
    async fn a_missing_home_relative_workspace_is_a_404_with_the_expanded_path() {
        let state = state_with(vec![FakeAgent::new("opencode")]).await;
        let refusal = do_find_agent_files(
            &state,
            FileSearchScope::Agent(None),
            "x",
            10,
            Some("~/muqun-no-such-dir-for-tests"),
            &device_headers(),
        )
        .await
        .expect_err("a missing folder is refused");
        assert_eq!(refusal.0, StatusCode::NOT_FOUND);
        let body = crate::test_support::error_body(&refusal);
        assert_eq!(body["error"]["code"], "workspace_missing");
        let expected = dirs::home_dir()
            .unwrap()
            .join("muqun-no-such-dir-for-tests");
        assert_eq!(
            body["error"]["directory"],
            expected.to_string_lossy().as_ref()
        );

        // And one that exists is let through.
        let Json(_) = do_find_agent_files(
            &state,
            FileSearchScope::Agent(None),
            "x",
            10,
            Some("~"),
            &device_headers(),
        )
        .await
        .expect("home is a workspace the agent can search");
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
            asid: None,
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
    async fn session_catalog_selects_only_its_owner_and_rejects_mismatched_providers() {
        let mut opencode = FakeAgent::new("opencode").with_sessions(&[("oc-session", 1)]);
        opencode.catalog = catalog_with(vec![mode("opencode-mode")]);
        let mut deepseek = FakeAgent::new("deepseek").with_sessions(&[("ds-session", 1)]);
        deepseek.catalog = catalog_with(vec![mode("deepseek-mode")]);
        let state = state_with(vec![opencode, deepseek]).await;
        let query = |agent: Option<&str>| {
            serde_json::from_value::<AgentSessionsQuery>(
                json!({"asid":"ds-session","agent_id":agent}),
            )
            .unwrap()
        };
        let response =
            get_global_agent_catalog(State(state.clone()), Query(query(None)), device_headers())
                .await
                .unwrap();
        let data = body_json(response).await;
        assert_eq!(data["data"]["modes"].as_array().unwrap().len(), 1);
        assert_eq!(data["data"]["modes"][0]["id"], "deepseek-mode");
        let denied = get_global_agent_catalog(
            State(state.clone()),
            Query(query(Some("opencode"))),
            device_headers(),
        )
        .await
        .unwrap_err();
        assert_eq!(denied.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            crate::test_support::error_body(&denied)["error"]["code"],
            "agent_session_mismatch"
        );
        let denied = get_global_agent_catalog(State(state), Query(query(None)), HeaderMap::new())
            .await
            .unwrap_err();
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
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

    // -- changes (git) ----------------------------------------------------

    /// A DeepSeek session -- an agent whose own `vcs/diff` answers nothing --
    /// whose directory is a subdirectory of a real checkout, and one with no
    /// checkout at all.
    async fn vcs_state(name: &str) -> (AppState, std::path::PathBuf, std::path::PathBuf) {
        let (root, repo) = crate::test_support::git_test_repo(name);
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let mut agent =
            FakeAgent::new("deepseek").with_sessions(&[("ses_git", 10), ("ses_plain", 20)]);
        agent.sessions[0].directory = Some(repo.join("src").to_string_lossy().into_owned());
        agent.sessions[1].directory = Some(plain.to_string_lossy().into_owned());
        (state_with(vec![agent]).await, root, repo)
    }

    fn file_query(mode: Option<&str>, path: &str) -> VcsFileQuery {
        VcsFileQuery {
            mode: mode.map(str::to_string),
            path: Some(path.to_string()),
            context: None,
        }
    }

    #[tokio::test]
    async fn vcs_files_and_file_answer_from_git_for_any_agent() {
        // Keep the fixture and route futures off the default test worker stack.
        let (state, root, _repo) = Box::pin(vcs_state("agent-vcs-files")).await;
        // Each call boxed too: `AppState` is held by value and grows with
        // every agent's config block.
        let files = |asid: &str, mode: Option<&str>| {
            Box::pin(get_agent_vcs_files(
                State(state.clone()),
                Path(asid.to_string()),
                Query(AgentVcsDiffQuery {
                    mode: mode.map(str::to_string),
                }),
                device_headers(),
            ))
        };

        let Json(answer) = files("ses_git", None).await.expect("lists");
        let data = &answer["data"];
        assert_eq!(data["vcs"], "git");
        assert!(data["reason"].is_null());
        assert_eq!(data["mode"], "working");
        assert_eq!(data["truncated"], false);
        assert_eq!(data["repo"]["branch"], "main");
        assert_eq!(data["repo"]["head"].as_str().map(str::len), Some(7));
        let list = data["files"].as_array().unwrap();
        assert_eq!(list.len(), 2, "{list:?}");
        let modified = list.iter().find(|f| f["path"] == "src/a.ts").unwrap();
        assert_eq!(modified["status"], "modified");
        assert_eq!(modified["additions"], 1);
        assert_eq!(modified["deletions"], 1);
        assert_eq!(modified["binary"], false);
        let untracked = list.iter().find(|f| f["path"] == "notes.md").unwrap();
        assert_eq!(untracked["status"], "untracked");

        let Json(branch) = files("ses_git", Some("branch")).await.expect("lists");
        assert_eq!(branch["data"]["mode"], "branch");
        assert_eq!(branch["data"]["base"], "main");
        assert_eq!(branch["data"]["repo"]["branch"], "main");

        let refusal = files("ses_git", Some("committed"))
            .await
            .expect_err("committed is not a mode here");
        assert_eq!(refusal.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            crate::test_support::error_body(&refusal)["error"]["code"],
            "invalid_mode"
        );

        let Json(plain) = files("ses_plain", None).await.expect("answers");
        assert!(plain["data"]["vcs"].is_null());
        assert_eq!(plain["data"]["reason"], "not_a_repository");
        assert_eq!(plain["data"]["files"], json!([]));
        assert!(plain["data"].get("repo").is_none());

        // The agent's own diff carries the same branch line.
        let Json(diff) = Box::pin(do_get_agent_vcs_diff(
            &state,
            "ses_git",
            None,
            &device_headers(),
        ))
        .await
        .expect("diff");
        assert_eq!(diff["data"]["vcs"], "git");
        assert_eq!(diff["data"]["repo"]["branch"], "main");
        assert_eq!(diff["data"]["repo"]["detached"], false);
        let Json(diff) = Box::pin(do_get_agent_vcs_diff(
            &state,
            "ses_plain",
            None,
            &device_headers(),
        ))
        .await
        .expect("diff");
        assert!(diff["data"].get("repo").is_none());

        let file = |query: VcsFileQuery| {
            Box::pin(get_agent_vcs_file(
                State(state.clone()),
                Path("ses_git".to_string()),
                Query(query),
                device_headers(),
            ))
        };
        let Json(one) = file(file_query(None, "src/a.ts")).await.expect("patch");
        assert_eq!(one["data"]["path"], "src/a.ts");
        assert_eq!(one["data"]["status"], "modified");
        assert_eq!(one["data"]["truncated"], false);
        assert!(one["data"]["patch"]
            .as_str()
            .unwrap()
            .contains("\n-const b = 2;\n+const B = 2;\n"));

        for unknown in ["nope.txt", "../x", "/etc/passwd", "*", "src"] {
            let refusal = file(file_query(None, unknown))
                .await
                .expect_err("not a change");
            assert_eq!(refusal.0, StatusCode::NOT_FOUND, "{unknown:?}");
            assert_eq!(
                crate::test_support::error_body(&refusal)["error"]["code"],
                "unknown_path"
            );
        }
        let refusal = file(file_query(Some("staged"), "src/a.ts"))
            .await
            .expect_err("bad mode");
        assert_eq!(
            crate::test_support::error_body(&refusal)["error"]["code"],
            "invalid_mode"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn vcs_discard_restores_and_deletes_and_refuses_unknown_paths() {
        let (state, root, repo) = vcs_state("agent-vcs-discard").await;
        let discard = |path: &str, headers: HeaderMap| {
            discard_agent_vcs_file(
                State(state.clone()),
                Path("ses_git".to_string()),
                headers,
                Json(VcsDiscardBody {
                    path: path.to_string(),
                }),
            )
        };

        let refusal = discard("src/a.ts", crate::test_support::bearer_headers("wrong"))
            .await
            .expect_err("needs a device");
        assert_eq!(refusal.0, StatusCode::FORBIDDEN);
        assert!(std::fs::read_to_string(repo.join("src/a.ts"))
            .unwrap()
            .contains("const B"));

        let Json(restored) = discard("src/a.ts", device_headers())
            .await
            .expect("restores");
        assert_eq!(restored["data"]["path"], "src/a.ts");
        assert_eq!(restored["data"]["action"], "restored");
        assert!(std::fs::read_to_string(repo.join("src/a.ts"))
            .unwrap()
            .contains("const b"));

        let Json(deleted) = discard("notes.md", device_headers())
            .await
            .expect("deletes");
        assert_eq!(deleted["data"]["action"], "deleted");
        assert!(!repo.join("notes.md").exists());

        // Already restored, a glob, a directory: none is a row of the list.
        for unknown in ["nope.txt", "../x", "src/a.ts", "*", "src"] {
            let refusal = discard(unknown, device_headers())
                .await
                .expect_err("unknown");
            assert_eq!(refusal.0, StatusCode::NOT_FOUND);
            assert_eq!(
                crate::test_support::error_body(&refusal)["error"]["code"],
                "unknown_path"
            );
        }

        std::fs::remove_dir_all(&root).ok();
    }

    /// An image the agent's markdown names by path reaches the phone as a map
    /// on its text part -- on the snapshot, the timeline, the replayed events
    /// and the live stream alike -- and the asset URL in the map serves it,
    /// for exactly as long as the file is inside the session's directory.
    #[tokio::test]
    async fn text_parts_carry_servable_image_assets_and_keep_their_text() {
        let root = std::env::temp_dir().join(format!("muqun-msg-img-{}", std::process::id()));
        let work = root.join("work");
        std::fs::create_dir_all(work.join("out")).unwrap();
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x02\x00\x00\x00\x03\x08\x06\x00\x00\x00";
        std::fs::write(work.join("out/flow.png"), png).unwrap();
        std::fs::write(root.join("secret.png"), png).unwrap();
        let work = std::fs::canonicalize(&work).unwrap();

        let mut agent = FakeAgent::new("opencode").with_sessions(&[("ses_img", 10)]);
        agent.sessions[0].directory = Some(work.to_string_lossy().into_owned());
        let state = state_with(vec![agent]).await;
        let headers = device_headers();
        let text = format!(
            "![flow](./out/flow.png \"Flow\")\n![abs](file://{})\n![no](../secret.png)",
            work.join("out/flow.png").display()
        );
        let manager = state
            .agent_runtime
            .manager_for_agent("opencode")
            .await
            .unwrap();
        // The snapshot read seeds the mirror from the agent first.
        do_get_agent_session(&state, "ses_img", &headers)
            .await
            .expect("gets");
        let asid = AgentSessionId("ses_img".into());
        let item = crate::agents::domain::TimelineItem {
            id: "m1:t0".into(),
            message_id: "m1".into(),
            role: crate::agents::domain::TimelineRole::Assistant,
            part: crate::agents::domain::AgentPart::Text { text: text.clone() },
            seq: 0,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
        };
        manager
            .mirror()
            .upsert_timeline_items(&asid, vec![item.clone()])
            .await;

        let snapshot = body_json(
            do_get_agent_session(&state, "ses_img", &headers)
                .await
                .unwrap(),
        )
        .await;
        let row = &snapshot["data"]["timeline"][0];
        assert_eq!(row["part"]["text"], text, "the text is the agent's own");
        let assets = row["image_assets"].as_array().expect("a map");
        assert_eq!(assets.len(), 2, "{assets:?}");
        assert_eq!(assets[0]["src"], "./out/flow.png");
        assert_eq!(assets[0]["mime"], "image/png");
        assert_eq!(
            (assets[0]["width"].as_u64(), assets[0]["height"].as_u64()),
            (Some(2), Some(3))
        );

        let Json(timeline) = do_get_agent_session_timeline(&state, "ses_img", 0, &headers)
            .await
            .unwrap();
        assert_eq!(
            timeline["data"]["items"][0]["image_assets"],
            row["image_assets"]
        );

        // A streamed upsert gets the same map on its way out.
        let mut event = crate::agents::domain::AgentDomainEvent::TimelineUpsert {
            asid: asid.clone(),
            items: vec![item],
            seq: 9,
        };
        message_images::attach_to_event(&state, &mut event).await;
        let (_, payload) = agent_event_record(&event);
        assert!(payload.contains("\"image_assets\""), "{payload}");

        // The URL serves the bytes.
        let id = assets[0]["asset_id"].as_str().unwrap().to_owned();
        assert_eq!(assets[0]["url"], format!("/api/assets/{id}/content"));
        let response =
            crate::asset_content(State(state.clone()), Path(id.clone()), headers.clone())
                .await
                .expect("serves");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], &png[..]);

        // Swapped for a symlink out of the directory, it is a 404.
        #[cfg(unix)]
        {
            std::fs::remove_file(work.join("out/flow.png")).unwrap();
            std::os::unix::fs::symlink(root.join("secret.png"), work.join("out/flow.png")).unwrap();
            let refused =
                crate::asset_content(State(state.clone()), Path(id), headers.clone()).await;
            assert_eq!(
                refused.err().map(|(status, _)| status),
                Some(StatusCode::NOT_FOUND)
            );
        }

        std::fs::remove_dir_all(&root).ok();
    }
}
