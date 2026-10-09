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
use super::service::{valid_name, DelegateRequest, TaskError};
use crate::agents::directories::expand_directory_param;
use crate::{api_error, lock_devices, require_device, ApiResult, AppState};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
/// The same bound `tasks.delegate` puts on a goal.
const MAX_GOAL_BYTES: usize = 32 * 1024;
const MAX_LABEL_CHARS: usize = 200;
pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/tasks", get(list).post(create))
        .route("/api/tasks/{id}", get(read))
        .route("/api/tasks/{id}/cancel", post(cancel))
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
    let by = principal(require_device(&state, &headers)?);
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

#[derive(Debug, Deserialize)]
struct CreateBody {
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
}

fn refused(code: &str, message: &str) -> (StatusCode, Json<Value>) {
    api_error(StatusCode::BAD_REQUEST, code, message)
}

/// An absolute, existing directory, canonicalized; `~` expands. None: the
/// home directory, which is what the App shows for a session it opens
/// without naming a folder.
fn task_directory(cwd: Option<&str>) -> ApiResult<Option<String>> {
    let Some(dir) = expand_directory_param(cwd).filter(|dir| !dir.trim().is_empty()) else {
        return Ok(dirs::home_dir().map(|home| home.to_string_lossy().into_owned()));
    };
    let path = std::path::Path::new(dir.trim());
    if !path.is_absolute() {
        return Err(refused(
            "invalid_directory",
            "Working directory must be an absolute path",
        ));
    }
    let canonical = path.canonicalize().map_err(|_| {
        refused(
            "directory_not_found",
            "Directory does not exist or is inaccessible",
        )
    })?;
    if !canonical.is_dir() {
        return Err(refused(
            "not_a_directory",
            "Specified path is not a directory",
        ));
    }
    Ok(Some(canonical.to_string_lossy().into_owned()))
}

impl CreateBody {
    fn request(self) -> ApiResult<DelegateRequest> {
        if self.goal.trim().is_empty() || self.goal.len() > MAX_GOAL_BYTES {
            return Err(refused(
                "invalid_request",
                "goal must be non-empty and at most 32 KiB",
            ));
        }
        let names = [&self.agent_id, &self.mode, &self.model, &self.variant];
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
        Ok(DelegateRequest {
            goal: self.goal,
            cwd: task_directory(self.cwd.as_deref())?,
            agent_id: self.agent_id,
            mode: self.mode,
            model: self.model,
            variant: self.variant,
            machine: None,
            label,
        })
    }
}

/// `POST /api/tasks`: what `tasks.delegate` does -- a new agent session with
/// the goal as its first prompt, recorded as a delegation -- opened by the
/// device. Answers the task view plus the session's `asid`.
async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateBody>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    // Who is asking comes first: a stranger learns nothing about the body.
    let device_id = require_device(&state, &headers)?;
    let Json(body) = body.map_err(|_| invalid_request())?;
    let request = body.request()?;
    let name = lock_devices(&state)?
        .iter()
        .find(|device| device.id == device_id)
        .map(|device| device.name.clone())
        .unwrap_or_else(|| device_id.clone());
    let by = Requested {
        by: principal(device_id),
        origin: TaskOrigin::Device,
        parent: None,
        name: Some(name),
    };
    let task = state
        .tasks
        .delegate(&state, by, request)
        .await
        .map_err(create_failed)?;
    let asid = match &task.binding {
        TaskBinding::AgentSession { asid, .. } => Value::String(asid.clone()),
        _ => Value::Null,
    };
    let mut answer = json!(task);
    answer["asid"] = asid;
    Ok(Json(answer))
}

fn create_failed(error: TaskError) -> (StatusCode, Json<Value>) {
    match error {
        TaskError::AgentUnavailable(_) => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_unavailable",
            "That agent is not available",
        ),
        TaskError::UnknownModel(_) => refused(
            "unknown_model",
            "Unknown model for that agent. Use a model id from its catalog, or <provider_id>/<model_id>.",
        ),
        TaskError::Agent(error) => {
            tracing::warn!(%error, "device task: the agent refused");
            api_error(
                StatusCode::BAD_GATEWAY,
                "agent_error",
                "the agent could not start the task",
            )
        }
        TaskError::UnknownVariant(detail) => refused(
            "unknown_variant",
            &format!("Unknown variant {detail}."),
        ),
        error => failed(error),
    }
}

async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let by = principal(require_device(&state, &headers)?);
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
    let by = principal(require_device(&state, &headers)?);
    let task = state
        .tasks
        .cancel(&state, &by, &TaskId(id))
        .await
        .map_err(failed)?;
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
        let state = with_agent().await;
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let body = json!({
            "goal": "fix the build",
            "cwd": dir,
            "label": "  build  ",
            "agent_id": "opencode",
            "mode": "build"
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

        let (_, listed) = call(&state, "GET", "/api/tasks").await;
        assert_eq!(listed["tasks"][0]["origin"], "device");
    }

    #[tokio::test]
    async fn opening_a_task_checks_auth_then_the_body() {
        let state = with_agent().await;
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
}
