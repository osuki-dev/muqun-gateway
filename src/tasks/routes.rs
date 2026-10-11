//! `/api/tasks`: the paired phone's view of the gateway's tasks, and the
//! way it opens one.

use std::collections::HashMap;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use serde::Deserialize;

use super::domain::{
    PrincipalKind, Requested, TaskBinding, TaskId, TaskOrigin, TaskPrincipal, TaskState,
};
use super::service::{
    task_directory, valid_goal, valid_name, BadDirectory, DelegateRequest, TaskError,
};
use crate::agents::directories::expand_directory_param;
use crate::connectivity::authority::DeviceKind;
use crate::fleet::peers::same_name;
use crate::platform::http::{peer_forbidden, require_device_or_peer, Caller};
use crate::{api_error, require_device, ApiResult, AppState};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const MAX_LABEL_CHARS: usize = 200;
/// The most computers one task may name (`mentions`).
const MAX_MENTIONS: usize = 8;
pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/tasks", get(list).post(create))
        .route("/api/tasks/defaults", get(defaults))
        .route("/api/tasks/{id}", get(read))
        .route("/api/tasks/{id}/cancel", post(cancel))
        .route("/api/tasks/{id}/complete", post(complete))
}

fn invalid_request() -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "invalid request",
    )
}

fn principal(device_id: String) -> TaskPrincipal {
    TaskPrincipal {
        kind: PrincipalKind::Device,
        id: device_id,
    }
}

/// The routes another gateway may use answer it as itself: a `Gateway`
/// principal, which sees and cancels only the tasks it opened.
fn caller_principal(caller: &Caller) -> TaskPrincipal {
    TaskPrincipal {
        kind: match caller.kind {
            DeviceKind::App => PrincipalKind::Device,
            DeviceKind::Gateway => PrincipalKind::Gateway,
        },
        id: caller.device_id.clone(),
    }
}

fn failed(error: TaskError) -> (StatusCode, Json<Value>) {
    match error {
        TaskError::NotFound | TaskError::NotYours => {
            api_error(StatusCode::NOT_FOUND, "task_not_found", "Task not found")
        }
        error => {
            tracing::warn!(%error, "task request failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "task_failed",
                "the task request failed",
            )
        }
    }
}

/// `a,b` as states; an empty value means no filter.
fn parse_states(text: &str) -> Option<Vec<TaskState>> {
    text.split(',')
        .map(|name| serde_json::from_value(Value::String(name.trim().to_owned())).ok())
        .collect()
}

async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let caller = require_device_or_peer(&state, &headers)?;
    // A task tree is not narrowed to its asker; another computer reads only
    // its own tasks, one at a time or listed.
    if caller.kind == DeviceKind::Gateway && query.contains_key("root") {
        return Err(peer_forbidden());
    }
    let by = caller_principal(&caller);
    let states = match query.get("state").filter(|text| !text.is_empty()) {
        Some(text) => Some(parse_states(text).ok_or_else(invalid_request)?),
        None => None,
    };
    let limit = match query.get("limit") {
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or_else(invalid_request)?
            .min(MAX_LIMIT),
        None => DEFAULT_LIMIT,
    };
    let tasks = match query.get("root") {
        Some(root) if root.is_empty() => return Err(invalid_request()),
        Some(root) => state.tasks.tree(&TaskId(root.clone()), states, limit).await,
        None => state.tasks.list_for(Some(&by), states, limit).await,
    };
    let tasks = tasks.map_err(|error| failed(error.into()))?;
    Ok(Json(json!({ "tasks": tasks })))
}

/// `GET /api/tasks/defaults`: what a task the device opens without `cwd`
/// runs in (see [`TaskService::default_cwd`]); `null` when unknown. With
/// `agent_id` naming an agent that runs in a terminal pane (and optionally
/// `model`, `variant`), also the `model` and `variant` such a task would
/// run: the ones named where its launch can pass them, else the agent's
/// own (Codex's config), else `default`.
///
/// [`TaskService::default_cwd`]: super::service::TaskService::default_cwd
async fn defaults(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let by = principal(require_device(&state, &headers)?);
    let mut answer = json!({ "default_cwd": state.tasks.default_cwd(&by).await });
    if let Some(kind) = query.get("agent_id") {
        if super::service::runs_in_pane(&state, kind).await {
            let (_, model, variant) = super::pane_agent::launch_model(
                &state,
                kind,
                query.get("model").map(String::as_str),
                query.get("variant").map(String::as_str),
            )
            .await
            .map_err(|_| invalid_request())?;
            let model = model.unwrap_or_else(|| "default".to_owned());
            answer["model"] = json!(model);
            answer["variant"] = json!(variant);
        }
    }
    Ok(Json(answer))
}

#[derive(Debug, Deserialize)]
struct CreateBody {
    /// Required for a delegation (checked there); a command has `command`.
    #[serde(default)]
    goal: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    label: Option<String>,
    /// The computers the user `@`-named: linked peers' names (any case) or `self`.
    #[serde(default)]
    mentions: Vec<String>,
    /// The requester's own reference for the task, echoed in its view.
    #[serde(default)]
    client_ref: Option<String>,
    /// The terminal session a terminal agent starts in, by its config id;
    /// absent: the first that answers.
    #[serde(default)]
    session_id: Option<String>,
    /// Files sent with the goal, as `POST /api/agent-sessions/{asid}/prompt`
    /// takes them.
    #[serde(default)]
    attachments: Vec<String>,
    /// `command`: run `command` as a command task (another computer's
    /// `exec.run` with `machine`) instead of delegating `goal`.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    command: Option<String>,
    /// How long the create waits for the command before answering with its
    /// handle (1..=45, default 30).
    #[serde(default)]
    timeout_s: Option<u64>,
}

/// How many attachments one task's goal may carry.
const MAX_ATTACHMENTS: usize = 16;

fn refused(code: &str, message: &str) -> (StatusCode, Json<Value>) {
    api_error(StatusCode::BAD_REQUEST, code, message)
}

/// `mentions` as stored: each the name this gateway gives a linked peer, or
/// `self`, once each, in the order given. At most [`MAX_MENTIONS`]; a name no
/// peer has is refused, so the agent is never told of a computer it cannot reach.
fn known_mentions(mentions: Vec<String>, linked: &[String]) -> ApiResult<Vec<String>> {
    let refusal = || {
        refused(
            "invalid_mentions",
            "mentions must be at most 8 names of linked computers (or self)",
        )
    };
    if mentions.len() > MAX_MENTIONS {
        return Err(refusal());
    }
    let mut known: Vec<String> = Vec::new();
    for wanted in mentions {
        let wanted = wanted.trim();
        let name = if crate::fleet::peers::reserved_name(wanted) {
            "self".to_owned()
        } else {
            let peer = linked.iter().find(|name| same_name(name, wanted));
            peer.ok_or_else(refusal)?.clone()
        };
        if !known.contains(&name) {
            known.push(name);
        }
    }
    Ok(known)
}

/// An absolute, existing directory, canonicalized; `~` expands. None uses
/// the home directory after the device's task history has been checked.
fn requested_directory(cwd: Option<&str>) -> ApiResult<Option<String>> {
    let Some(dir) = expand_directory_param(cwd).filter(|dir| !dir.trim().is_empty()) else {
        return Ok(dirs::home_dir().map(|home| home.to_string_lossy().into_owned()));
    };
    let canonical = task_directory(dir.trim()).map_err(|bad| match bad {
        BadDirectory::NotAbsolute => refused(
            "invalid_directory",
            "Working directory must be an absolute path",
        ),
        BadDirectory::Missing => refused(
            "directory_not_found",
            "Directory does not exist or is inaccessible",
        ),
        BadDirectory::NotADirectory => {
            refused("not_a_directory", "Specified path is not a directory")
        }
    })?;
    Ok(Some(canonical.to_string_lossy().into_owned()))
}

impl CreateBody {
    /// `linked`: the linked computers' names, which `mentions` must be among.
    fn request(self, default_cwd: Option<&str>, linked: &[String]) -> ApiResult<DelegateRequest> {
        if !valid_goal(&self.goal) {
            return Err(refused(
                "invalid_request",
                "goal must be non-empty and at most 32 KiB",
            ));
        }
        let names = [
            &self.agent_id,
            &self.mode,
            &self.model,
            &self.variant,
            &self.session_id,
        ];
        if names
            .iter()
            .filter_map(|name| name.as_deref())
            .any(|name| !valid_name(name))
        {
            return Err(invalid_request());
        }
        let label = self
            .label
            .map(|label| label.trim().to_owned())
            .filter(|label| !label.is_empty());
        if label
            .as_ref()
            .is_some_and(|label| label.chars().count() > MAX_LABEL_CHARS)
        {
            return Err(refused(
                "invalid_request",
                "label must be at most 200 characters",
            ));
        }
        if self.attachments.len() > MAX_ATTACHMENTS
            || self
                .attachments
                .iter()
                .any(|reference| reference.trim().is_empty() || reference.len() > 4096)
        {
            return Err(refused(
                "invalid_request",
                "attachments must be at most 16 non-empty references",
            ));
        }
        let mentions = known_mentions(self.mentions, linked)?;
        if self
            .client_ref
            .as_deref()
            .is_some_and(|reference| !crate::valid_install_id(reference))
        {
            return Err(refused(
                "invalid_request",
                "client_ref must be 1 to 128 of A-Z, a-z, 0-9, - and _",
            ));
        }
        // A remembered directory that has since gone away is no error: the
        // task opens at home, as if nothing were remembered. Only a directory
        // the device asked for by name is refused when it is missing.
        let remembered = default_cwd.filter(|dir| std::path::Path::new(dir).is_dir());
        Ok(DelegateRequest {
            goal: self.goal,
            cwd: requested_directory(self.cwd.as_deref().or(remembered))?,
            agent_id: self.agent_id,
            mode: self.mode,
            model: self.model,
            variant: self.variant,
            machine: None,
            label,
            mentions,
            client_ref: self.client_ref,
            session_id: self.session_id,
            attachments: self.attachments,
        })
    }
}

/// `POST /api/tasks`: what `tasks.delegate` does -- a new agent session with
/// the goal as its first prompt, recorded as a delegation -- opened by the
/// device. Answers the task view plus the session's `asid`; for a terminal
/// agent (`codex`, `claude`, ...) the pane it was started in, `pane_id`.
async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateBody>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    // Who is asking comes first: a stranger learns nothing about the body.
    let caller = require_device_or_peer(&state, &headers)?;
    // Another of the owner's computers, while the owner switched that off.
    if caller.kind == DeviceKind::Gateway && !state.peers.accepts_delegations() {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "delegations_disabled",
            "This computer does not take work from other computers",
        ));
    }
    let Json(mut body) = body.map_err(|_| invalid_request())?;
    if body.kind.as_deref() == Some("command") {
        return create_command(&state, caller, body).await;
    }
    if body
        .kind
        .as_deref()
        .is_some_and(|kind| kind != "delegation")
    {
        return Err(invalid_request());
    }
    body.kind = None;
    // References are files on this computer, uploaded from a device: another
    // computer has none to send.
    if caller.kind == DeviceKind::Gateway && !body.attachments.is_empty() {
        return Err(refused(
            "attachments_unsupported",
            "Attachments go only to a session agent on this computer",
        ));
    }
    let by_caller = caller_principal(&caller);
    let previous_cwd = if body.cwd.is_none() {
        state
            .tasks
            .latest_device_cwd(&by_caller)
            .await
            .map_err(|error| failed(error.into()))?
    } else {
        None
    };
    let request = body.request(previous_cwd.as_deref(), &state.peers.names())?;
    // Named from the device record the token belongs to, never from the
    // body: another computer cannot pass itself off as a third (§10.5).
    let by = Requested {
        forwarding: crate::tasks::domain::ForwardingContext::Unknown,
        by: by_caller,
        origin: match caller.kind {
            DeviceKind::App => TaskOrigin::Device,
            DeviceKind::Gateway => TaskOrigin::Peer,
        },
        parent: None,
        name: Some(caller.name),
        server_id: caller.server_id.clone(),
    };
    let task = state
        .tasks
        .delegate(&state, by, request)
        .await
        .map_err(create_failed)?;
    let mut answer = json!(task);
    // Where the app opens the task: the agent session, or the terminal
    // agent's pane.
    match &task.binding {
        TaskBinding::Pane { pane_id, .. } => answer["pane_id"] = json!(pane_id),
        TaskBinding::AgentSession { asid, .. } => answer["asid"] = json!(asid),
        _ => answer["asid"] = Value::Null,
    }
    Ok(Json(answer))
}

/// `POST /api/tasks` with `kind: "command"`: run `command` in `cwd` (home
/// when absent; `~` expands) as a command task, exactly as `exec.run` does
/// here -- a visible pane, a row in this computer's ledger -- and answer
/// with the task view, its exit code and output tail once it ended within
/// `timeout_s`, else its handle. Another computer's command (`exec.run`
/// with `machine`) runs only in a directory inside this computer's home.
async fn create_command(
    state: &AppState,
    caller: Caller,
    body: CreateBody,
) -> ApiResult<Json<Value>> {
    let command = body.command.unwrap_or_default();
    if command.trim().is_empty() || command.len() > MAX_COMMAND_BYTES {
        return Err(refused(
            "invalid_request",
            "command must be non-empty and at most 16 KiB",
        ));
    }
    if body
        .client_ref
        .as_deref()
        .is_some_and(|reference| !crate::valid_install_id(reference))
    {
        return Err(invalid_request());
    }
    let cwd = requested_directory(Some(body.cwd.as_deref().unwrap_or("~")))?
        .ok_or_else(invalid_request)?;
    let peer = caller.kind == DeviceKind::Gateway;
    if peer {
        let home = dirs::home_dir().and_then(|home| home.canonicalize().ok());
        if !home.is_some_and(|home| std::path::Path::new(&cwd).starts_with(home)) {
            return Err(refused(
                "invalid_directory",
                "Another computer's command runs only inside this computer's home",
            ));
        }
    }
    let request = crate::tasks::run_command::RunRequest {
        cwd: cwd.into(),
        command,
        timeout: crate::mcp::tools::wait_slice(body.timeout_s),
        client_ref: body.client_ref,
    };
    let by = Requested {
        forwarding: crate::tasks::domain::ForwardingContext::Unknown,
        by: caller_principal(&caller),
        origin: if peer {
            TaskOrigin::Peer
        } else {
            TaskOrigin::Device
        },
        parent: None,
        name: Some(caller.name),
        server_id: caller.server_id.clone(),
    };
    let handle = state
        .tasks
        .run(state, by, request)
        .await
        .map_err(create_failed)?;
    let mut answer = json!(state.tasks.present(&handle.task).await);
    answer["exit_code"] = json!(handle.exit_code);
    answer["stdout_tail"] = json!(handle.stdout_tail);
    answer["duration_ms"] = json!(handle.duration_ms);
    if let TaskBinding::Pane { pane_id, .. } = &handle.task.binding {
        answer["pane_id"] = json!(pane_id);
    }
    Ok(Json(answer))
}

/// The longest command a create runs, as `exec.run` takes it.
const MAX_COMMAND_BYTES: usize = 16 * 1024;

fn create_failed(error: TaskError) -> (StatusCode, Json<Value>) {
    match error {
        TaskError::NoTerminal => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_terminal",
            "No terminal session is reachable on this computer",
        ),
        TaskError::AgentUnavailable(_) => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "That agent is not available",
        ),
        TaskError::InvalidAttachment(_) => refused(
            "invalid_attachment",
            "An attachment is not a file uploaded to this computer",
        ),
        TaskError::AttachmentsUnsupported => refused(
            "attachments_unsupported",
            "Attachments go only to a session agent on this computer",
        ),
        TaskError::TerminalUnknown(_) => refused(
            "terminal_unknown",
            "No terminal session of that id on this computer",
        ),
        TaskError::UnknownModel(_) => refused(
            "unknown_model",
            "Unknown model for that agent. Use a model id from its catalog, or <provider_id>/<model_id>.",
        ),
        TaskError::Agent(_) | TaskError::Backend(_) => {
            tracing::warn!(%error, "device task: the agent refused");
            api_error(
                StatusCode::BAD_GATEWAY,
                "agent_error",
                "the agent could not start the task",
            )
        }
        TaskError::UnknownVariant { wanted, offered } => refused(
            "unknown_variant",
            &super::service::unknown_variant_message(crate::i18n::current(), &wanted, &offered),
        ),
        error => failed(error),
    }
}

async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let by = caller_principal(&require_device_or_peer(&state, &headers)?);
    let (task, events) = state
        .tasks
        .history(&by, &TaskId(id))
        .await
        .map_err(failed)?;
    Ok(Json(json!({ "task": task, "events": events })))
}

async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let by = caller_principal(&require_device_or_peer(&state, &headers)?);
    let task = state
        .tasks
        .cancel(&state, &by, &TaskId(id))
        .await
        .map_err(failed)?;
    Ok(Json(json!(task)))
}

#[derive(Debug, Default, Deserialize)]
struct CompleteBody {
    #[serde(default)]
    summary: Option<String>,
}

/// `POST /api/tasks/{id}/complete`: the user confirms that a delegation the
/// phone opened, paused (`idle_unconfirmed`) or waiting on them, is finished.
/// The body (`{"summary"?}`) may be left out.
async fn complete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let body: CompleteBody = if body.iter().all(u8::is_ascii_whitespace) {
        CompleteBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|_| invalid_request())?
    };
    let task = state
        .tasks
        .confirm(&TaskId(id), body.summary.as_deref())
        .await
        .map_err(|error| match error {
            TaskError::NotConfirmable => api_error(
                StatusCode::CONFLICT,
                "task_not_confirmable",
                "Only a paused or waiting task opened on a device can be confirmed",
            ),
            TaskError::InvalidSummary => {
                refused("invalid_request", "summary must be at most 32 KiB")
            }
            error => failed(error),
        })?;
    Ok(Json(json!(task)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::domain::{now_ms, Task, TaskBinding, TaskKind};
    use crate::tasks::ports::TaskStore as _;
    use crate::tasks::store_sqlite::SqliteTaskStore;
    use crate::{bearer_headers, test_device, test_state};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    async fn insert(state: &AppState, goal: &str, to: Option<TaskState>) -> String {
        let store = SqliteTaskStore::new(state.db.clone());
        let now = now_ms();
        let task = store
            .insert(&Task {
                id: TaskId::new(),
                kind: TaskKind::Delegation,
                goal: goal.into(),
                cwd: None,
                binding: TaskBinding::None,
                state: TaskState::Queued,
                summary: None,
                exit_code: None,
                created_by: TaskPrincipal {
                    kind: PrincipalKind::Assistant,
                    id: "a".into(),
                },
                created_ms: now,
                updated_ms: now,
                seq: 0,
                parent: None,
                origin: Default::default(),
                label: None,
                requester_name: None,
                session_title: None,
                mentions: Vec::new(),
                client_ref: None,
                requester_server_id: None,
                reopened_ms: None,
                reopen_count: 0,
            })
            .await
            .unwrap();
        if let Some(to) = to {
            store
                .transition(
                    &task.id,
                    crate::tasks::domain::TaskChange {
                        to,
                        source: "test".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        task.id.0
    }

    async fn call(state: &AppState, method: &str, path: &str) -> (StatusCode, Value) {
        send(state, method, path, None, true).await
    }

    async fn send(
        state: &AppState,
        method: &str,
        path: &str,
        body: Option<Value>,
        authorized: bool,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if authorized {
            for (name, value) in bearer_headers("device") {
                request = request.header(name.unwrap(), value);
            }
        }
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = mount(Router::new())
            .with_state(state.clone())
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn state() -> AppState {
        test_state("admin", vec![test_device("d1", "device")])
    }

    #[tokio::test]
    async fn list_filters_by_state_and_bounds_the_limit() {
        let state = state();
        let queued = insert(&state, "one", None).await;
        let cancelled = insert(&state, "two", Some(TaskState::Cancelled)).await;

        let (status, body) = call(&state, "GET", "/api/tasks").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tasks"].as_array().unwrap().len(), 2);

        let (_, body) = call(&state, "GET", "/api/tasks?state=cancelled,done").await;
        let tasks = body["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["task_id"], cancelled);
        assert_eq!(tasks[0]["state"], "cancelled");

        let (_, body) = call(&state, "GET", "/api/tasks?state=queued&limit=1").await;
        assert_eq!(body["tasks"][0]["task_id"], queued);

        let (status, body) = call(&state, "GET", "/api/tasks?state=nope").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");
        let (status, _) = call(&state, "GET", "/api/tasks?limit=0").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(&state, "GET", "/api/tasks?limit=9999").await;
        assert_eq!(status, StatusCode::OK, "an oversized limit is clamped");
    }

    #[tokio::test]
    async fn read_returns_the_task_and_its_history() {
        let state = state();
        let id = insert(&state, "one", Some(TaskState::Dispatching)).await;
        let (status, body) = call(&state, "GET", &format!("/api/tasks/{id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["task"]["task_id"], id);
        assert_eq!(body["task"]["state"], "dispatching");
        assert_eq!(
            body["task"]["goal"], "one",
            "the phone's task card shows it"
        );
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 2, "creation and the move");
        assert_eq!(events[1]["to"], "dispatching");
    }

    #[tokio::test]
    async fn cancel_moves_the_task_and_is_idempotent() {
        let state = state();
        let id = insert(&state, "one", None).await;
        let (status, body) = call(&state, "POST", &format!("/api/tasks/{id}/cancel")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "cancelled");
        let (status, body) = call(&state, "POST", &format!("/api/tasks/{id}/cancel")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "cancelled");
    }

    #[tokio::test]
    async fn an_unknown_task_is_not_found_and_auth_comes_first() {
        let state = state();
        for (method, path) in [
            ("GET", "/api/tasks/nope"),
            ("POST", "/api/tasks/nope/cancel"),
        ] {
            let (status, body) = call(&state, method, path).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(body["error"]["code"], "task_not_found");
        }
        let response = mount(Router::new())
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/api/tasks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// The phone confirms a paused task it opened; nothing else is confirmable.
    #[tokio::test]
    async fn a_device_confirms_a_paused_task_it_opened() {
        let state = state();
        let opened = |origin| {
            let now = now_ms();
            Task {
                id: TaskId::new(),
                kind: TaskKind::Delegation,
                goal: "g".into(),
                cwd: None,
                binding: TaskBinding::None,
                state: TaskState::Queued,
                summary: Some("Shall I push?".into()),
                exit_code: None,
                created_by: principal("d1".into()),
                created_ms: now,
                updated_ms: now,
                seq: 0,
                parent: None,
                origin,
                label: None,
                requester_name: Some("phone".into()),
                session_title: None,
                mentions: Vec::new(),
                client_ref: None,
                requester_server_id: None,
                reopened_ms: None,
                reopen_count: 0,
            }
        };
        let mut ids = Vec::new();
        for (origin, to) in [
            (TaskOrigin::Device, TaskState::IdleUnconfirmed),
            (TaskOrigin::Device, TaskState::Waiting),
            (TaskOrigin::Device, TaskState::Working),
            (TaskOrigin::Mcp, TaskState::IdleUnconfirmed),
            // Another of the owner's computers opened it: the phone owns both.
            (TaskOrigin::Peer, TaskState::IdleUnconfirmed),
        ] {
            let id = state.tasks.insert_for_test(&opened(origin)).await;
            let to = crate::tasks::domain::TaskChange {
                to,
                source: "test".into(),
                ..Default::default()
            };
            state.tasks.move_to(&id, to).await.unwrap();
            ids.push(id.0);
        }
        let mut updates = state.task_events.subscribe();
        let path = |id: &str| format!("/api/tasks/{id}/complete");

        // No body: the hint stays the summary.
        let (status, body) = call(&state, "POST", &path(&ids[0])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "done");
        assert_eq!(body["summary"], "Shall I push?");
        assert!(updates.try_recv().is_ok(), "task.updated is published");
        let again = send(&state, "POST", &path(&ids[0]), Some(json!({})), true).await;
        assert_eq!(again.0, StatusCode::OK, "confirming twice answers the task");

        let summary = Some(json!({ "summary": "pushed\nand tagged" }));
        let (status, body) = send(&state, "POST", &path(&ids[1]), summary, true).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            (&body["state"], &body["summary"]),
            (&json!("done"), &json!("pushed and tagged"))
        );

        let (status, body) = call(&state, "POST", &path(&ids[4])).await;
        assert_eq!(
            (status, &body["state"]),
            (StatusCode::OK, &json!("done")),
            "{body}"
        );

        for (id, why) in [
            (&ids[2], "still working"),
            (&ids[3], "not opened on a device"),
        ] {
            let (status, body) = call(&state, "POST", &path(id)).await;
            assert_eq!(status, StatusCode::CONFLICT, "{why}");
            assert_eq!(body["error"]["code"], "task_not_confirmable");
        }
        let (status, _) = call(&state, "POST", &path("nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(&state, "POST", &path(&ids[2]), None, false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let bad = send(&state, "POST", &path(&ids[2]), Some(json!("x")), true).await;
        assert_eq!(bad.0, StatusCode::BAD_REQUEST);
    }

    /// A state whose OpenCode fake answers a new session `opencode_new`.
    async fn with_agent() -> AppState {
        let state = state();
        let agent =
            crate::test_support::FakeAgent::new("opencode").with_sessions(&[("opencode_new", 1)]);
        state.agent_runtime.attach_for_test(agent.manager()).await;
        state
    }

    #[tokio::test]
    async fn a_device_opens_a_delegation_and_is_told_its_session() {
        let state = state();
        let agent =
            crate::test_support::FakeAgent::new("opencode").with_sessions(&[("opencode_new", 1)]);
        let (prompts, attached, calls) = (
            agent.prompts.clone(),
            agent.attached.clone(),
            agent.calls.clone(),
        );
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let dir = std::env::temp_dir().canonicalize().unwrap();
        // Two files uploaded through the gateway, as the dock sends them.
        let uploads = crate::ensure_uploads_dir().unwrap();
        let files: Vec<String> = ["shot.png", "notes.txt"]
            .iter()
            .map(|name| {
                let path = uploads.join(format!("{}-{name}", uuid::Uuid::new_v4()));
                std::fs::write(&path, b"x").unwrap();
                path.canonicalize().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        // A file that was never uploaded is refused before anything is made:
        // no task, no session.
        let body = json!({ "goal": "g", "cwd": dir, "agent_id": "opencode",
                           "attachments": [files[0], "/etc/hosts"] });
        let (status, refusal) = send(&state, "POST", "/api/tasks", Some(body), true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refusal}");
        assert_eq!(refusal["error"]["code"], "invalid_attachment");
        assert!(
            calls.lock().unwrap().is_empty(),
            "{:?}",
            calls.lock().unwrap()
        );
        let (_, listed) = call(&state, "GET", "/api/tasks").await;
        assert_eq!(listed["tasks"], json!([]));

        let body = json!({
            "goal": "fix the build",
            "cwd": dir,
            "label": "  build  ",
            "agent_id": "opencode",
            "mode": "build",
            "attachments": files
        });
        let (status, task) = send(&state, "POST", "/api/tasks", Some(body), true).await;
        assert_eq!(status, StatusCode::OK, "{task}");
        assert_eq!(task["asid"], "opencode_new");
        assert_eq!(task["kind"], "delegation");
        assert_eq!(task["state"], "working");
        assert_eq!(task["goal"], "fix the build");
        assert_eq!(task["label"], "build");
        assert_eq!(task["origin"], "device");
        assert_eq!(task["parent_task_id"], Value::Null);
        assert_eq!(
            task["requested_by"],
            json!({ "kind": "device", "id": "d1", "name": "device d1" })
        );
        assert_eq!(
            task["children"],
            json!({ "total": 0, "active": 0, "waiting": 0, "failed": 0 })
        );
        assert_eq!(
            task["binding"],
            json!({ "kind": "agent_session", "agent_id": "opencode", "asid": "opencode_new" })
        );
        let stored = state
            .tasks
            .get_for_test(&TaskId(task["task_id"].as_str().unwrap().into()))
            .await;
        assert_eq!(stored.cwd.as_deref(), Some(dir.to_str().unwrap()));
        assert_eq!(stored.created_by, principal("d1".into()));

        // The goal and both attachments, as the session prompt sends them.
        assert_eq!(*prompts.lock().unwrap(), ["fix the build"]);
        assert_eq!(*attached.lock().unwrap(), [files]);

        let (_, listed) = call(&state, "GET", "/api/tasks").await;
        assert_eq!(listed["tasks"][0]["origin"], "device");
    }

    #[tokio::test]
    async fn device_task_directory_defaults() {
        let home = dirs::home_dir().unwrap();
        let temp = |name: &str| {
            let dir =
                crate::test_state_dir().join(format!("task-cwd-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            dir.canonicalize().unwrap()
        };
        let previous = temp("previous");
        let explicit = temp("explicit");
        let deleted = temp("deleted");
        std::fs::remove_dir(&deleted).unwrap();

        // Each case: the device's earlier tasks as (cwd, created_ms,
        // updated_ms), the cwd it asks for now, and where the task opens.
        let cases = [
            (
                "previous task",
                vec![(Some(&explicit), 5, 100), (Some(&previous), 10, 10)],
                None,
                previous.as_path(),
            ),
            ("no previous task", vec![], None, home.as_path()),
            (
                "explicit cwd",
                vec![(Some(&explicit), 5, 100), (Some(&previous), 10, 10)],
                Some(explicit.as_path()),
                explicit.as_path(),
            ),
            (
                "deleted directory",
                vec![(Some(&previous), 5, 5), (Some(&deleted), 10, 10)],
                None,
                home.as_path(),
            ),
            (
                "newest task without a directory",
                vec![(Some(&previous), 10, 10), (None, 15, 15)],
                None,
                previous.as_path(),
            ),
        ];

        for (name, earlier, cwd, expected) in cases {
            let state = with_agent().await;
            let store = SqliteTaskStore::new(state.db.clone());
            // Creation order wins over updates, and another device is ignored.
            let other = ("d2", Some(&explicit), 20, 20);
            for (device_id, directory, created_ms, updated_ms) in earlier
                .into_iter()
                .map(|(directory, created_ms, updated_ms)| {
                    ("d1", directory, created_ms, updated_ms)
                })
                .chain([other])
            {
                store
                    .insert(&Task {
                        id: TaskId::new(),
                        kind: TaskKind::Delegation,
                        goal: "earlier task".into(),
                        cwd: directory.map(|dir| dir.to_string_lossy().into_owned()),
                        binding: TaskBinding::None,
                        state: TaskState::Queued,
                        summary: None,
                        exit_code: None,
                        created_by: principal(device_id.into()),
                        created_ms,
                        updated_ms,
                        seq: 0,
                        parent: None,
                        origin: TaskOrigin::Device,
                        label: None,
                        requester_name: None,
                        session_title: None,
                        mentions: Vec::new(),
                        client_ref: None,
                        requester_server_id: None,
                        reopened_ms: None,
                        reopen_count: 0,
                    })
                    .await
                    .unwrap();
            }
            // What the device is told before it asks: home as `~`, no user
            // name (folders below home: `service::home_relative`'s table).
            if expected == home {
                let (_, defaults) = send(&state, "GET", "/api/tasks/defaults", None, true).await;
                assert_eq!(defaults, json!({ "default_cwd": "~" }), "{name}");
            }
            let mut body = json!({ "goal": name, "agent_id": "opencode" });
            if let Some(cwd) = cwd {
                body["cwd"] = json!(cwd);
            }
            let (status, view) = send(&state, "POST", "/api/tasks", Some(body), true).await;
            assert_eq!(status, StatusCode::OK, "{name}: {view}");
            let task = state
                .tasks
                .get_for_test(&TaskId(view["task_id"].as_str().unwrap().into()))
                .await;
            assert_eq!(task.cwd.as_deref(), expected.to_str(), "{name}");
        }
    }

    #[tokio::test]
    async fn a_terminal_agent_opens_in_a_pane_bound_to_its_task() {
        let herdr = crate::FakeHerdr::start(vec!["$ "], None);
        herdr.set_panes(json!([
            { "pane_id": "w1:p9", "workspace_id": "w1", "tab_id": "w1:t9", "shell_pid": 4242 }
        ]));
        let mut state = state();
        state.config.sessions[0].socket_path = herdr.session().socket_path;
        // The catalog's `codex`, run as a program that is on every PATH.
        state
            .config
            .agent_commands
            .insert("codex".into(), "sh".into());
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let body = json!({ "goal": "fix the build", "cwd": dir, "agent_id": "codex",
                           "model": "gpt-6-sol", "variant": "low", "label": "build" });
        let (status, task) = send(&state, "POST", "/api/tasks", Some(body), true).await;
        assert_eq!(status, StatusCode::OK, "{task}");
        // Answered once the pane exists; the agent starts in the background.
        assert_eq!(task["pane_id"], "w1:p9");
        assert!(task.get("asid").is_none(), "{task}");
        assert_eq!(task["state"], "dispatching");
        assert_eq!(task["origin"], "device");
        assert_eq!(
            task["binding"],
            json!({ "kind": "pane", "session_id": "default", "pane_id": "w1:p9",
                    "tab_id": "w1:t9", "shell_pid": 4242, "agent_id": "codex",
                    "model": "gpt-6-sol", "variant": "low" })
        );
        assert_eq!(
            (&task["model"], &task["variant"]),
            (&json!("gpt-6-sol"), &json!("low"))
        );
        let id = TaskId(task["task_id"].as_str().unwrap().into());
        for _ in 0..400 {
            if state.tasks.get_for_test(&id).await.state != TaskState::Dispatching {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            state.tasks.get_for_test(&id).await.state,
            TaskState::Working
        );
        // In the asked directory, named for the task, started the spawn
        // route's way: bypass flags, then the goal as its prompt.
        let calls = herdr.calls.lock().unwrap().clone();
        let tab = calls.iter().find(|c| c["method"] == "tab.create").unwrap();
        assert_eq!(tab["params"]["cwd"], json!(dir));
        assert!(calls
            .iter()
            .any(|c| c["method"] == "tab.rename" && c["params"]["label"] == "build"));
        let start = calls.iter().find(|c| c["method"] == "agent.start").unwrap();
        assert_eq!(start["params"]["kind"], "codex");
        assert_eq!(
            start["params"]["args"][0],
            crate::agents::tasks::CODEX_BYPASS_FLAG
        );
        // The model and effort asked for, for this run only, before the goal.
        let args: Vec<&str> = start["params"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let at = args
            .iter()
            .position(|arg| *arg == "model=\"gpt-6-sol\"")
            .unwrap();
        assert_eq!(
            &args[at - 1..at + 3],
            [
                "-c",
                "model=\"gpt-6-sol\"",
                "-c",
                "model_reasoning_effort=\"low\""
            ]
        );
        let prompt = calls
            .iter()
            .find(|c| c["method"] == "agent.prompt")
            .unwrap();
        // The goal alone: how to finish is the MCP server's to say.
        assert_eq!(prompt["params"]["text"], "fix the build");
        assert_eq!(task["goal"], "fix the build");

        // The terminal session the request names, not the first that
        // answers; one this computer does not have is refused.
        let mut other = state.config.sessions[0].clone();
        other.id = "hd".into();
        state.config.sessions.push(other);
        // Files cannot go to a terminal agent. Refusals make nothing.
        for (name, extra, status, answer) in [
            (
                "session hd",
                json!({ "session_id": "hd" }),
                StatusCode::OK,
                json!("hd"),
            ),
            (
                "session nope",
                json!({ "session_id": "nope" }),
                StatusCode::BAD_REQUEST,
                json!("terminal_unknown"),
            ),
            (
                "attachments",
                json!({ "attachments": ["/tmp/x.png"] }),
                StatusCode::BAD_REQUEST,
                json!("attachments_unsupported"),
            ),
        ] {
            let (_, before) = call(&state, "GET", "/api/tasks").await;
            let mut body = json!({ "goal": "g", "cwd": dir, "agent_id": "codex" });
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let (status_got, view) = send(&state, "POST", "/api/tasks", Some(body), true).await;
            assert_eq!(status_got, status, "{name}: {view}");
            let (_, after) = call(&state, "GET", "/api/tasks").await;
            let made = after["tasks"].as_array().unwrap().len()
                - before["tasks"].as_array().unwrap().len();
            if status == StatusCode::OK {
                assert_eq!(view["binding"]["session_id"], answer, "{name}: {view}");
                assert_eq!(made, 1, "{name}");
            } else {
                assert_eq!(view["error"]["code"], answer, "{name}: {view}");
                assert_eq!(made, 0, "{name}");
            }
        }

        // What such a task would run, before it is made: the named model and
        // effort where the launch passes them; no plain word is refused.
        for (query, status, model) in [
            (
                "agent_id=codex&model=gpt-6-sol&variant=low",
                StatusCode::OK,
                json!("gpt-6-sol"),
            ),
            (
                "agent_id=codex&model=-x",
                StatusCode::BAD_REQUEST,
                Value::Null,
            ),
        ] {
            let (got, defaults) = send(
                &state,
                "GET",
                &format!("/api/tasks/defaults?{query}"),
                None,
                true,
            )
            .await;
            assert_eq!(got, status, "{query}: {defaults}");
            if status == StatusCode::OK {
                assert_eq!(
                    (&defaults["model"], &defaults["variant"]),
                    (&model, &json!("low")),
                    "{query}"
                );
            }
        }

        // A task cancelled before its agent starts gets no agent and no goal.
        let session = state.config.sessions[0].clone();
        let placed =
            crate::agents::session_routes::place_agent(&state, &session, "codex", None, None)
                .await
                .unwrap();
        let cancelled = insert(&state, "late", Some(TaskState::Cancelled)).await;
        let cancelled = state.tasks.get_for_test(&TaskId(cancelled)).await;
        let prompts = || {
            let calls = herdr.calls.lock().unwrap();
            calls
                .iter()
                .filter(|c| c["method"] == "agent.prompt")
                .count()
        };
        let before = prompts();
        let starts = || {
            let calls = herdr.calls.lock().unwrap();
            calls
                .iter()
                .filter(|c| c["method"] == "agent.start")
                .count()
        };
        let started = starts();
        crate::tasks::service::start_terminal_agent(
            state.clone(),
            session,
            "codex".into(),
            placed,
            crate::tasks::service::TerminalStart {
                cwd: None,
                goal: "late".into(),
                model_args: Vec::new(),
            },
            cancelled,
        )
        .await;
        assert_eq!(prompts(), before);
        assert_eq!(starts(), started, "no agent started either");

        // Neither an unknown agent nor an unattached session agent (OpenCode
        // is in the terminal catalog too) is started in a pane.
        for agent_id in ["no-such-agent", "opencode"] {
            let body = json!({ "goal": "x", "agent_id": agent_id });
            let (status, body) = send(&state, "POST", "/api/tasks", Some(body), true).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{agent_id}");
            assert_eq!(body["error"]["code"], "agent_unavailable");
        }
    }

    #[tokio::test]
    async fn opening_a_task_checks_auth_then_the_body() {
        let state = with_agent().await;
        state
            .peers
            .store_for_test(crate::fleet::peers::Peer {
                peer_id: "peer-mini".into(),
                server_id: "srv-mini".into(),
                name: "mini".into(),
                url: "http://127.0.0.1:9".into(),
                token: "t".into(),
                device_id: "d".into(),
                transport_key: None,
                transport_min_version: 1,
                added_ms: 0,
                last_seen_ms: None,
                capabilities: Vec::new(),
                manifest: None,
                unauthorized: false,
            })
            .await;
        let (status, _) = send(
            &state,
            "POST",
            "/api/tasks",
            Some(json!({ "goal": "x" })),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(&state, "POST", "/api/tasks", Some(json!([1])), false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "auth before the body");
        let (status, answer) = send(&state, "POST", "/api/tasks", Some(json!([1])), true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(answer["error"]["code"], "invalid_request");
        let (status, _) = send(&state, "POST", "/api/tasks", None, true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "no body, no content type");
        for (body, code) in [
            (json!({ "goal": "  " }), "invalid_request"),
            (
                json!({ "goal": "x".repeat(32 * 1024 + 1) }),
                "invalid_request",
            ),
            (
                json!({ "goal": "x", "cwd": "relative/dir" }),
                "invalid_directory",
            ),
            (
                json!({ "goal": "x", "cwd": "/no/such/dir/anywhere" }),
                "directory_not_found",
            ),
            (
                json!({ "goal": "x", "label": "l".repeat(201) }),
                "invalid_request",
            ),
            (json!({ "goal": "x", "mode": "" }), "invalid_request"),
            (
                json!({ "goal": "x", "mentions": "mini" }),
                "invalid_request",
            ),
            (
                json!({ "goal": "x", "mentions": vec!["mini"; 9] }),
                "invalid_mentions",
            ),
            (
                json!({ "goal": "x", "mentions": ["mini", "printer"] }),
                "invalid_mentions",
            ),
        ] {
            let (status, answer) =
                send(&state, "POST", "/api/tasks", Some(body.clone()), true).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(answer["error"]["code"], code, "{body}");
        }
        let (status, answer) = send(
            &state,
            "POST",
            "/api/tasks",
            Some(json!({ "goal": "x", "agent_id": "deepseek" })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(answer["error"]["code"], "agent_unavailable");
        let (status, answer) = send(
            &state,
            "POST",
            "/api/tasks",
            Some(json!({ "goal": "x", "model": "gpt-9" })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(answer["error"]["code"], "unknown_model");
        assert!(state
            .tasks
            .list_for(None, None, 10)
            .await
            .unwrap()
            .is_empty());

        // Named in any case, once each, in the user's order; kept on the task.
        let (status, answer) = send(
            &state,
            "POST",
            "/api/tasks",
            Some(json!({
                "goal": "x", "cwd": std::env::temp_dir().canonicalize().unwrap(),
                "mentions": ["MINI", "self", "mini"],
            })),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["mentions"], json!(["mini", "self"]));
    }

    #[tokio::test]
    async fn a_root_lists_its_tree() {
        let state = state();
        let store = SqliteTaskStore::new(state.db.clone());
        let root = insert(&state, "root", None).await;
        let other = insert(&state, "other", None).await;
        let mut child = store.get(&TaskId(other.clone())).await.unwrap().unwrap();
        child.id = TaskId::new();
        child.parent = Some(TaskId(root.clone()));
        let child = store.insert(&child).await.unwrap();

        let (status, body) = call(&state, "GET", &format!("/api/tasks?root={root}")).await;
        assert_eq!(status, StatusCode::OK);
        let tasks = body["tasks"].as_array().unwrap();
        let ids: Vec<&str> = tasks
            .iter()
            .map(|t| t["task_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&root.as_str()) && ids.contains(&child.id.0.as_str()));
        let root_view = tasks.iter().find(|t| t["task_id"] == root).unwrap();
        assert_eq!(root_view["children"]["total"], 1);
        assert_eq!(root_view["children"]["active"], 1);
        let child_view = tasks.iter().find(|t| t["task_id"] == child.id.0).unwrap();
        assert_eq!(child_view["parent_task_id"], root);

        let (_, all) = call(&state, "GET", "/api/tasks").await;
        assert_eq!(
            all["tasks"].as_array().unwrap().len(),
            3,
            "the default list is unchanged"
        );
        let (_, none) = call(&state, "GET", "/api/tasks?root=t_missing").await;
        assert_eq!(none["tasks"], json!([]));
        let (status, _) = call(&state, "GET", "/api/tasks?root=").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_device_may_name_the_model_variant_and_unknown_fields_are_tolerated() {
        let state = state();
        let mut agent =
            crate::test_support::FakeAgent::new("opencode").with_sessions(&[("opencode_new", 1)]);
        agent.catalog.models = vec![serde_json::from_value(json!({
            "id": "gpt-6", "name": "GPT-6", "provider_id": "openai",
            "variants": [{ "id": "low" }, { "id": "high" }]
        }))
        .unwrap()];
        let created = agent.created_with.clone();
        state.agent_runtime.attach_for_test(agent.manager()).await;

        let open = json!({
            "goal": "x", "cwd": std::env::temp_dir().canonicalize().unwrap(),
            "model": "openai/gpt-6", "variant": "high", "from_a_newer_app": [1, 2]
        });
        let (status, task) = send(&state, "POST", "/api/tasks", Some(open), true).await;
        assert_eq!(status, StatusCode::OK, "{task}");
        let asked = created.lock().unwrap()[0].clone().unwrap();
        assert_eq!(asked.variant.as_deref(), Some("high"));
        assert_eq!(task.get("variant"), None, "the task view does not carry it");

        for (body, code) in [
            (
                json!({ "goal": "x", "model": "gpt-6", "variant": "max" }),
                "unknown_variant",
            ),
            (
                json!({ "goal": "x", "model": "gpt-6", "variant": " " }),
                "invalid_request",
            ),
        ] {
            let (status, answer) =
                send(&state, "POST", "/api/tasks", Some(body.clone()), true).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(answer["error"]["code"], code, "{body}");
        }
        assert_eq!(created.lock().unwrap().len(), 1, "nothing else was created");
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let body = json!({ "goal": "x", "cwd": cwd, "variant": "high" });
        let (status, task) = send(&state, "POST", "/api/tasks", Some(body), true).await;
        assert_eq!(status, StatusCode::OK, "a variant alone is dropped: {task}");
        assert_eq!(created.lock().unwrap()[1], None);
    }

    /// Another of the owner's computers opens a task here like a phone, is
    /// named on it as a gateway, and is refused while the switch is off.
    #[tokio::test]
    async fn another_computer_opens_a_task_unless_the_switch_is_off() {
        let state = with_agent().await;
        state
            .devices
            .lock()
            .unwrap()
            .push(crate::connectivity::authority::DeviceRecord {
                server_id: Some("srv-coding".into()),
                ..crate::test_support::test_peer_device("coding-1", "peer-token")
            });
        for (accept, status, check) in [
            (
                true,
                StatusCode::OK,
                json!({ "origin": "peer", "requested_by": { "kind": "gateway", "id": "coding-1", "name": "gateway coding-1", "server_id": "srv-coding" } }),
            ),
            (
                false,
                StatusCode::FORBIDDEN,
                json!({ "error": { "code": "delegations_disabled" } }),
            ),
        ] {
            state.peers.set_accepts_delegations(accept).unwrap();
            let response = mount(Router::new())
                .with_state(state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/tasks")
                        .header("authorization", "Bearer peer-token")
                        .header("content-type", "application/json")
                        .body(Body::from(json!({ "goal": "screenshot Home" }).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status, "accept={accept}");
            let body: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap(),
            )
            .unwrap();
            for (key, expected) in check.as_object().unwrap() {
                if key == "error" {
                    assert_eq!(body["error"]["code"], expected["code"], "accept={accept}");
                } else {
                    assert_eq!(&body[key], expected, "accept={accept}: {key}");
                }
            }
        }
    }
}
