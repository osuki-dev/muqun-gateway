//! Authenticated HTTP boundary for durable work records. Native execution is separate.
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::work::model::*;
use crate::work::store::WorkStore;
use crate::{api_error, find_session, now_unix_ms, require_device, ApiResult, AppState};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateRequest {
    pub request_key: String,
    #[serde(default)]
    pub input_refs: Vec<InputRef>,
    #[serde(flatten)]
    pub input: CreateTask,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RevisionRequest<T> {
    pub request_key: String,
    pub expected_revision: u64,
    #[serde(flatten)]
    pub input: T,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListQuery {
    pub after_id: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SummaryQuery {
    pub after_id: Option<String>,
    pub snapshot_cursor: Option<u64>,
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChangesQuery {
    pub after_cursor: Option<u64>,
    pub limit: Option<u32>,
}

pub(super) fn work_error(error: WorkError) -> (StatusCode, Json<Value>) {
    let status = match error.0 {
        FailureCode::InvalidInput => StatusCode::BAD_REQUEST,
        FailureCode::NotFound => StatusCode::NOT_FOUND,
        FailureCode::ScopeMismatch => StatusCode::FORBIDDEN,
        FailureCode::StorageUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        FailureCode::CapabilityUnavailable => StatusCode::NOT_IMPLEMENTED,
        FailureCode::ResourceLimit => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::CONFLICT,
    };
    // Only stable domain codes cross the boundary; SQLite and filesystem details do not.
    let code = serde_json::to_value(&error.0)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "storage_unavailable".into());
    api_error(status, &code, "The work request could not be completed.")
}

pub(super) async fn with_store<T, F>(state: &AppState, action: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&mut WorkStore) -> WorkResult<T> + Send + 'static,
{
    let store = state
        .work
        .clone()
        .ok_or_else(|| work_error(WorkError(FailureCode::StorageUnavailable)))?;
    tokio::task::spawn_blocking(move || {
        let mut guard = store
            .lock()
            .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
        action(&mut guard)
    })
    .await
    .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?
    .map_err(work_error)
}

fn validate_id(id: &str) -> ApiResult<()> {
    if id.is_empty() || id.len() > 256 || id.contains('\0') {
        return Err(work_error(WorkError(FailureCode::InvalidInput)));
    }
    Ok(())
}

fn page_limit(limit: Option<u32>) -> ApiResult<u32> {
    let limit = limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(work_error(WorkError(FailureCode::InvalidInput)));
    }
    Ok(limit)
}

fn now() -> i64 {
    now_unix_ms().min(i64::MAX as u128) as i64
}

pub(super) async fn create_task(
    State(state): State<AppState>,
    Path(session): Path<String>,
    headers: HeaderMap,
    Json(mut body): Json<CreateRequest>,
) -> ApiResult<Json<Mutation<Task>>> {
    let actor = require_device(&state, &headers)?;
    let config = find_session(&state.config, &session)?;
    if state.work.is_none() {
        return Err(work_error(WorkError(FailureCode::StorageUnavailable)));
    }
    if body
        .input
        .policy
        .allowed_agents
        .iter()
        .any(|kind| !crate::tasks::is_known_agent_kind(kind, &state.config.agent_commands))
    {
        return Err(work_error(WorkError(FailureCode::InvalidInput)));
    }
    let roots = crate::task_repo_roots(&state, config).await;
    body.input.repo_path = crate::tasks::resolve_repo_path(&body.input.repo_path, &roots)
        .and_then(|path| path.to_str().map(str::to_owned))
        .ok_or_else(|| work_error(WorkError(FailureCode::ScopeMismatch)))?;
    let replay_actor = actor.clone();
    let replay_session = session.clone();
    let replay_key = body.request_key.clone();
    let replay_input = body.input.clone();
    let replay_refs = body.input_refs.clone();
    if let Some(replay) = with_store(&state, move |store| {
        store.replay_create_with_inputs(
            &replay_actor,
            &replay_session,
            &replay_key,
            &replay_input,
            &replay_refs,
        )
    })
    .await?
    {
        return Ok(Json(replay));
    }
    let preview_actor = actor.clone();
    let preview_session = session.clone();
    let repo = body.input.repo_path.clone();
    let refs = body.input_refs.clone();
    let frozen = with_store(&state, move |store| {
        store.resolve_inputs(&preview_actor, &preview_session, &repo, None, &refs, now())
    })
    .await?;
    let resolved = crate::work_prompt::resolve_inputs(state.work_artifacts.clone(), frozen)
        .await
        .map_err(work_error)?;
    let executable = std::env::current_exe()
        .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?;
    crate::work_prompt::render_with_inputs(
        &body.input.brief,
        &executable,
        Some(crate::work_prompt::INSTRUCTIONS_VERSION),
        &resolved,
    )
    .map_err(work_error)?;
    crate::work_inputs::authorized_store(
        state.clone(),
        headers.clone(),
        actor.clone(),
        move |store| {
            store.create_task_with_inputs(
                &actor,
                &session,
                &body.request_key,
                body.input,
                body.input_refs,
                now(),
            )
        },
    )
    .await
    .map(Json)
}

#[derive(Serialize)]
pub(super) struct TaskPage {
    tasks: Vec<Task>,
    next_after_id: Option<String>,
}

pub(super) async fn list_tasks(
    State(state): State<AppState>,
    Path(session): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<TaskPage>> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    let limit = page_limit(query.limit)?;
    if let Some(after) = &query.after_id {
        validate_id(after)?;
    }
    with_store(&state, move |store| {
        let tasks = store.list_tasks(&session, query.after_id.as_deref(), limit)?;
        // A full page may have a continuation; an empty final page terminates it.
        let next_after_id = if tasks.len() == limit as usize {
            tasks.last().map(|task| task.id.clone())
        } else {
            None
        };
        Ok(TaskPage {
            tasks,
            next_after_id,
        })
    })
    .await
    .map(Json)
}

pub(super) async fn task_summaries(
    State(state): State<AppState>,
    Path(session): Path<String>,
    headers: HeaderMap,
    Query(query): Query<SummaryQuery>,
) -> ApiResult<Json<TaskSummaryPage>> {
    let actor = require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    crate::work_inputs::authorized_store(state, headers, actor, move |store| {
        store.task_summaries(
            &session,
            query.limit.unwrap_or(20),
            query.after_id.as_deref(),
            query.snapshot_cursor,
        )
    })
    .await
    .map(Json)
}

pub(super) async fn task_detail(
    State(state): State<AppState>,
    Path((session, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<crate::work::store::pagination::PagedDetail>> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    validate_id(&id)?;
    with_store(&state, move |store| store.paged_detail(&session, &id))
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordsQuery {
    kind: String,
    snapshot_revision: u64,
    after_id: Option<String>,
    limit: Option<u32>,
}
pub(super) async fn task_records(
    State(state): State<AppState>,
    Path((session, id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<RecordsQuery>,
) -> ApiResult<Json<crate::work::store::pagination::RecordPage>> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    validate_id(&id)?;
    with_store(&state, move |store| {
        store.task_records(
            &session,
            &id,
            &query.kind,
            query.snapshot_revision,
            query.after_id.as_deref(),
            query.limit.unwrap_or(20),
        )
    })
    .await
    .map(Json)
}
async fn immutable_record(
    state: AppState,
    session: String,
    task: String,
    record: String,
    headers: HeaderMap,
    kind: &'static str,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    validate_id(&task)?;
    validate_id(&record)?;
    with_store(&state, move |store| {
        store.immutable_record(&session, &task, kind, &record)
    })
    .await
    .map(Json)
}
pub(super) async fn get_result(
    State(state): State<AppState>,
    Path((session, task, record)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    immutable_record(state, session, task, record, headers, "result").await
}
pub(super) async fn get_review(
    State(state): State<AppState>,
    Path((session, task, record)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    immutable_record(state, session, task, record, headers, "review").await
}

pub(super) async fn get_operation(
    State(state): State<AppState>,
    Path((session, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Operation>> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    validate_id(&id)?;
    with_store(&state, move |store| store.get_operation(&session, &id))
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReceiptQuery {
    pub kind: OperationKind,
    pub request_key: String,
}

pub(super) async fn receipt(
    State(state): State<AppState>,
    Path(session): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ReceiptQuery>,
) -> ApiResult<Json<RequestReceipt>> {
    let actor = require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    with_store(&state, move |store| {
        store.get_receipt(&actor, &session, query.kind, &query.request_key)
    })
    .await
    .map(Json)
}

pub(super) async fn changes(
    State(state): State<AppState>,
    Path(session): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ChangesQuery>,
) -> ApiResult<Json<ChangePage>> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    let limit = page_limit(query.limit)?;
    with_store(&state, move |store| {
        store.changes(&session, query.after_cursor.unwrap_or(0), limit)
    })
    .await
    .map(Json)
}

pub(super) async fn submit_result(
    State(state): State<AppState>,
    Path((session, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RevisionRequest<ResultInput>>,
) -> ApiResult<Json<Mutation<ResultSubmission>>> {
    let actor = require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    validate_id(&id)?;
    crate::work_results::submit(
        &state,
        crate::work_results::Actor {
            id: actor,
            local: None,
        },
        session,
        id,
        body.request_key,
        body.expected_revision,
        body.input,
    )
    .await
    .map(Json)
    .map_err(work_error)
}

pub(super) async fn revoke_local_authority(
    State(state): State<AppState>,
    Path((session_id, task_id, attempt_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session_id)?;
    let scope = crate::work_authority::Scope {
        session_id,
        task_id,
        attempt_id,
    };
    let check = scope.clone();
    with_store(&state, move |store| {
        let detail = store.detail(&check.session_id, &check.task_id)?;
        if !detail.attempts.iter().any(|a| a.id == check.attempt_id) {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        Ok(())
    })
    .await?;
    let local = state
        .work_local
        .as_ref()
        .ok_or_else(|| work_error(WorkError(FailureCode::CapabilityUnavailable)))?;
    let revoked = local
        .registry
        .lock()
        .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?
        .revoke(&scope);
    Ok(Json(serde_json::json!({"revoked":revoked})))
}

pub(super) async fn set_paused(
    State(state): State<AppState>,
    Path((session, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RevisionRequest<PauseInput>>,
) -> ApiResult<Json<Mutation<Task>>> {
    let actor = require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    validate_id(&id)?;
    with_store(&state, move |store| {
        store.set_paused(
            &actor,
            &session,
            &id,
            &body.request_key,
            body.expected_revision,
            body.input,
            now(),
        )
    })
    .await
    .map(Json)
}

pub(super) async fn review_result(
    State(state): State<AppState>,
    Path((session, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RevisionRequest<ReviewInput>>,
) -> ApiResult<Json<Mutation<Review>>> {
    let actor = require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    validate_id(&id)?;
    with_store(&state, move |store| {
        store.review_result(
            &actor,
            &session,
            &id,
            &body.request_key,
            body.expected_revision,
            body.input,
            now(),
        )
    })
    .await
    .map(Json)
}

/// A result authorizes one stored content digest. No URL parameter becomes a filesystem path.
pub(super) async fn artifact_content(
    State(state): State<AppState>,
    Path((session, task_id, submission_id, index)): Path<(String, String, String, usize)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    find_session(&state.config, &session)?;
    validate_id(&task_id)?;
    validate_id(&submission_id)?;
    let artifact = with_store(&state, move |store| {
        let detail = store.detail(&session, &task_id)?;
        detail
            .results
            .into_iter()
            .find(|result| result.id == submission_id)
            .and_then(|result| result.result.artifacts.get(index).cloned())
            .ok_or(WorkError(FailureCode::NotFound))
    })
    .await?;
    let root = state
        .work_artifacts
        .clone()
        .ok_or_else(|| work_error(WorkError(FailureCode::StorageUnavailable)))?;
    let bytes =
        tokio::task::spawn_blocking(move || crate::work_artifacts::retrieve(&root, &artifact))
            .await
            .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?
            .map_err(work_error)?;
    Ok((
        [
            ("content-type", "application/octet-stream"),
            ("content-disposition", "attachment; filename=artifact.bin"),
            ("x-content-type-options", "nosniff"),
            ("cache-control", "private, no-store"),
        ],
        bytes,
    )
        .into_response())
}

pub(super) async fn start_attempt(
    State(state): State<AppState>,
    Path((session_id, task_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<crate::work_execution::StartRequest>,
) -> ApiResult<Json<Mutation<Operation>>> {
    let actor = require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    if !crate::tasks::is_known_agent_kind(&body.agent_kind, &state.config.agent_commands) {
        return Err(work_error(WorkError(FailureCode::InvalidInput)));
    }
    if let Some(branch) = &body.branch_name {
        crate::tasks::validate_branch_name(branch)
            .map_err(|_| work_error(WorkError(FailureCode::InvalidInput)))?;
    }
    let tid = task_id.clone();
    let sid = session_id.clone();
    let task = with_store(&state, move |s| Ok(s.detail(&sid, &tid)?.task)).await?;
    let roots = crate::task_repo_roots(&state, session).await;
    if crate::tasks::resolve_repo_path(&task.repo_path, &roots)
        .as_ref()
        .is_none_or(|p| p.to_str() != Some(task.repo_path.as_str()))
    {
        return Err(work_error(WorkError(FailureCode::ScopeMismatch)));
    }
    let store = state
        .work
        .clone()
        .ok_or_else(|| work_error(WorkError(FailureCode::StorageUnavailable)))?;
    let port = crate::work_execution::SessionPort {
        authority: crate::work_actor::WorkActor::paired(state.clone(), headers.clone(), &actor)
            .map_err(work_error)?,
        state: &state,
        session,
    };
    crate::work_execution::Execution {
        store,
        session: session_id.clone(),
        port: &port,
    }
    .start(&actor, &task_id, body)
    .await
    .map(Json)
    .map_err(work_error)
}

pub(super) async fn reconcile_attempt(
    State(state): State<AppState>,
    Path((session_id, task_id, attempt_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<ReconcileInput>,
) -> ApiResult<Json<Mutation<ReconciliationReceipt>>> {
    let actor = require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    validate_id(&task_id)?;
    validate_id(&attempt_id)?;
    let store = state
        .work
        .clone()
        .ok_or_else(|| work_error(WorkError(FailureCode::StorageUnavailable)))?;
    let registry = state
        .work_local
        .as_ref()
        .ok_or_else(|| work_error(WorkError(FailureCode::CapabilityUnavailable)))?
        .registry
        .clone();
    let port = crate::work_execution::SessionPort {
        authority: crate::work_actor::WorkActor::paired(state.clone(), headers.clone(), &actor)
            .map_err(work_error)?,
        state: &state,
        session,
    };
    crate::work_execution::Execution {
        store,
        session: session_id,
        port: &port,
    }
    .reconcile(&actor, &task_id, &attempt_id, body, registry)
    .await
    .map(Json)
    .map_err(work_error)
}

pub(super) async fn deliver_prompt(
    State(state): State<AppState>,
    Path((session_id, task_id, attempt_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<crate::work_execution::DeliveryRequest>,
) -> ApiResult<Json<Mutation<Operation>>> {
    let actor = require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?;
    let store = state
        .work
        .clone()
        .ok_or_else(|| work_error(WorkError(FailureCode::StorageUnavailable)))?;
    let port = crate::work_execution::SessionPort {
        authority: crate::work_actor::WorkActor::paired(state.clone(), headers.clone(), &actor)
            .map_err(work_error)?,
        state: &state,
        session,
    };
    crate::work_execution::Execution {
        store,
        session: session_id.clone(),
        port: &port,
    }
    .deliver(&actor, &task_id, &attempt_id, body)
    .await
    .map(Json)
    .map_err(work_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flattened_request_preserves_strict_domain_fields() {
        let body = json!({"request_key":"one", "repo_path":"/repo", "title":"Task", "brief":"Work", "parent_task_id":null, "policy":{"allowed_agents":["codex"],"max_workers":1}});
        assert!(serde_json::from_value::<CreateRequest>(body.clone()).is_ok());
        let mut with_refs = body.clone();
        with_refs["input_refs"] = json!([{"input_id":"d4d30000-0000-4000-8000-000000000001","caption":"Keep as a reference","use":"reference-only"}]);
        assert_eq!(
            serde_json::from_value::<CreateRequest>(with_refs)
                .unwrap()
                .input_refs
                .len(),
            1
        );
        let mut extra = body;
        extra["unexpected"] = json!(true);
        assert!(serde_json::from_value::<CreateRequest>(extra).is_err());
    }

    #[test]
    fn revision_envelopes_decode_and_reject_unknown_fields() {
        let result = json!({"request_key":"result", "expected_revision":3, "attempt_id":"attempt", "summary":"Submitted", "artifacts":[], "evidence":[]});
        assert!(serde_json::from_value::<RevisionRequest<ResultInput>>(result.clone()).is_ok());
        let mut unknown = result;
        unknown["unexpected"] = json!(true);
        assert!(serde_json::from_value::<RevisionRequest<ResultInput>>(unknown).is_err());
        let review = json!({"request_key":"review", "expected_revision":4, "submission_id":"submission", "decision":"accepted", "message":null});
        assert!(serde_json::from_value::<RevisionRequest<ReviewInput>>(review.clone()).is_ok());
        let mut unknown = review;
        unknown["unexpected"] = json!(true);
        assert!(serde_json::from_value::<RevisionRequest<ReviewInput>>(unknown).is_err());
    }

    #[test]
    fn pagination_and_errors_are_bounded() {
        assert_eq!(page_limit(None).unwrap(), 50);
        assert!(page_limit(Some(0)).is_err());
        assert!(page_limit(Some(101)).is_err());
        assert!(validate_id(&"x".repeat(257)).is_err());
        assert_eq!(
            work_error(WorkError(FailureCode::StorageUnavailable)).0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            work_error(WorkError(FailureCode::RevisionConflict)).0,
            StatusCode::CONFLICT
        );
    }
    fn fixture() -> AppState {
        crate::tests::test_state("admin", vec![crate::tests::test_device("phone", "device")])
    }
    #[tokio::test]
    async fn summaries_are_scoped_compact_and_snapshot_pinned() {
        let state = fixture();
        let session = state.config.sessions[0].id.clone();
        let seed_session = session.clone();
        with_store(&state, move |store| {
            for (index, scope) in [seed_session.as_str(), seed_session.as_str(), "foreign"]
                .iter()
                .enumerate()
            {
                store.create_task(
                    "phone",
                    scope,
                    &format!("summary-{index}"),
                    CreateTask {
                        repo_path: "/repo".into(),
                        title: format!("Task {index}"),
                        brief: "Never expose this brief".into(),
                        parent_task_id: None,
                        policy: TaskPolicy {
                            allowed_agents: vec!["codex".into()],
                            max_workers: 1,
                        },
                    },
                    1,
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
        let first = task_summaries(
            State(state.clone()),
            Path(session.clone()),
            headers("device"),
            Query(SummaryQuery {
                after_id: None,
                snapshot_cursor: None,
                limit: Some(1),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0].session_id, session);
        let encoded = serde_json::to_string(&first).unwrap();
        assert!(!encoded.contains("brief"));
        assert!(encoded.len() <= 131072);
        let after = first.next_after_id.unwrap();
        let second = task_summaries(
            State(state.clone()),
            Path(session.clone()),
            headers("device"),
            Query(SummaryQuery {
                after_id: Some(after.clone()),
                snapshot_cursor: Some(first.snapshot_cursor),
                limit: Some(1),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(second.items.len(), 1);
        assert_ne!(second.items[0].task_id, first.items[0].task_id);
        let task = first.items[0].clone();
        let scope = session.clone();
        with_store(&state, move |store| {
            store.set_paused(
                "phone",
                &scope,
                &task.task_id,
                "pause-summary",
                task.task_revision,
                PauseInput { paused: true },
                2,
            )
        })
        .await
        .unwrap();
        let stale = task_summaries(
            State(state),
            Path(session),
            headers("device"),
            Query(SummaryQuery {
                after_id: Some(after),
                snapshot_cursor: Some(first.snapshot_cursor),
                limit: Some(1),
            }),
        )
        .await;
        assert!(matches!(stale, Err((StatusCode::CONFLICT, _))));
    }

    #[tokio::test]
    async fn summaries_validate_authority_scope_and_page_bounds() {
        let state = fixture();
        let session = state.config.sessions[0].id.clone();
        for token in ["admin", "unknown"] {
            let result = task_summaries(
                State(state.clone()),
                Path(session.clone()),
                headers(token),
                Query(SummaryQuery {
                    after_id: None,
                    snapshot_cursor: None,
                    limit: None,
                }),
            )
            .await;
            assert!(matches!(result, Err((StatusCode::FORBIDDEN, _))));
        }
        for query in [
            SummaryQuery {
                after_id: None,
                snapshot_cursor: None,
                limit: Some(21),
            },
            SummaryQuery {
                after_id: None,
                snapshot_cursor: Some(1),
                limit: None,
            },
        ] {
            assert!(matches!(
                task_summaries(
                    State(state.clone()),
                    Path(session.clone()),
                    headers("device"),
                    Query(query)
                )
                .await,
                Err((StatusCode::BAD_REQUEST, _))
            ));
        }
        assert!(serde_json::from_value::<SummaryQuery>(json!({"unknown":1})).is_err());
        let missing = task_summaries(
            State(state),
            Path("not-configured".into()),
            headers("device"),
            Query(SummaryQuery {
                after_id: None,
                snapshot_cursor: None,
                limit: None,
            }),
        )
        .await;
        assert!(matches!(missing, Err((StatusCode::NOT_FOUND, _))));
    }

    #[tokio::test]
    async fn reconciliation_refuses_admin_and_unpaired_authority_before_native_work() {
        let state = fixture();
        let session = state.config.sessions[0].id.clone();
        for token in ["admin", "unknown"] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
            let result = reconcile_attempt(
                State(state.clone()),
                Path((
                    session.clone(),
                    uuid::Uuid::new_v4().to_string(),
                    uuid::Uuid::new_v4().to_string(),
                )),
                headers,
                Json(ReconcileInput {
                    request_key: "check".into(),
                    expected_revision: 1,
                    expected_instance_id: None,
                    expected_native_owner_epoch: None,
                }),
            )
            .await;
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn delegation_pause_requires_paired_authority_and_exact_revision() {
        let state = fixture();
        let session = state.config.sessions[0].id.clone();
        let create: CreateTask = serde_json::from_value(json!({
            "repo_path":"/repo", "title":"Task", "brief":"Work", "parent_task_id":null,
            "policy":{"allowed_agents":["codex"],"max_workers":0}
        }))
        .unwrap();
        let task = state
            .work
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create_task("phone", &session, "create", create, 1)
            .unwrap()
            .value;
        let request = || {
            Json(RevisionRequest {
                request_key: "pause".into(),
                expected_revision: 1,
                input: PauseInput { paused: true },
            })
        };
        let denied = set_paused(
            State(state.clone()),
            Path((session.clone(), task.id.clone())),
            headers("admin"),
            request(),
        )
        .await;
        assert!(matches!(denied, Err((StatusCode::FORBIDDEN, _))));
        let updated = set_paused(
            State(state.clone()),
            Path((session.clone(), task.id.clone())),
            headers("device"),
            request(),
        )
        .await
        .unwrap()
        .0;
        assert!(updated.value.paused);
        assert_eq!(updated.value.revision, 2);
        let replay = set_paused(
            State(state.clone()),
            Path((session.clone(), task.id.clone())),
            headers("device"),
            request(),
        )
        .await
        .unwrap()
        .0;
        assert!(replay.replayed);
        let conflict = set_paused(
            State(state),
            Path((session, task.id)),
            headers("device"),
            Json(RevisionRequest {
                request_key: "stale-resume".into(),
                expected_revision: 1,
                input: PauseInput { paused: false },
            }),
        )
        .await;
        assert!(matches!(conflict, Err((StatusCode::CONFLICT, _))));
    }

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn paired_auth_is_required_before_storage_and_scope_access() {
        let mut state = fixture();
        let session = state.config.sessions[0].id.clone();
        state.work = None;
        let refusal = list_tasks(
            State(state.clone()),
            Path(session.clone()),
            headers("admin"),
            Query(ListQuery {
                after_id: None,
                limit: None,
            }),
        )
        .await;
        assert!(matches!(refusal, Err((StatusCode::FORBIDDEN, _))));
        let missing = list_tasks(
            State(state),
            Path(session),
            headers("device"),
            Query(ListQuery {
                after_id: None,
                limit: None,
            }),
        )
        .await;
        assert!(matches!(missing, Err((StatusCode::SERVICE_UNAVAILABLE, _))));
    }

    #[tokio::test]
    async fn session_reads_cannot_access_another_sessions_records() {
        let state = fixture();
        let session = state.config.sessions[0].id.clone();
        let task = with_store(&state, |store| {
            store.create_task(
                "phone",
                "another-session",
                "seed",
                CreateTask {
                    repo_path: "/repo".into(),
                    title: "Private task".into(),
                    brief: "Work".into(),
                    parent_task_id: None,
                    policy: TaskPolicy {
                        allowed_agents: vec!["codex".into()],
                        max_workers: 1,
                    },
                },
                1,
            )
        })
        .await
        .unwrap()
        .value;
        let missing = task_detail(
            State(state.clone()),
            Path((session.clone(), task.id)),
            headers("device"),
        )
        .await;
        assert!(matches!(missing, Err((StatusCode::NOT_FOUND, _))));
        let listed = list_tasks(
            State(state),
            Path(session),
            headers("device"),
            Query(ListQuery {
                after_id: None,
                limit: None,
            }),
        )
        .await
        .unwrap();
        assert!(listed.0.tasks.is_empty());
    }

    #[tokio::test]
    async fn missing_task_artifacts_are_refused_before_file_access() {
        let state = fixture();
        let session = state.config.sessions[0].id.clone();
        let result = submit_result(
            State(state.clone()),
            Path((session.clone(), "task".into())),
            headers("device"),
            Json(RevisionRequest {
                request_key: "result".into(),
                expected_revision: 1,
                input: ResultInput {
                    attempt_id: "attempt".into(),
                    summary: "Finished".into(),
                    evidence: vec![],
                    artifacts: vec![ArtifactRef {
                        path: "result.txt".into(),
                        sha256: "0".repeat(64),
                        size_bytes: 12,
                    }],
                },
            }),
        )
        .await;
        assert!(matches!(result, Err((StatusCode::NOT_FOUND, _))));
        let changes = with_store(&state, move |store| store.changes(&session, 0, 100))
            .await
            .unwrap();
        assert!(changes.changes.is_empty());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn artifact_submission_replays_after_source_deletion_and_retrieval_checks_membership() {
        use sha2::{Digest, Sha256};
        let raw =
            std::env::temp_dir().join(format!("work-http-artifacts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&raw).unwrap();
        let root = std::fs::canonicalize(raw).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("result.html"), b"<script>unsafe()</script>").unwrap();
        let mut state = fixture();
        state.work_artifacts = Some(root.join("blobs"));
        let session = state.config.sessions[0].id.clone();
        let setup_session = session.clone();
        let setup_repo = repo.clone();
        let (task_id, attempt_id, revision) = with_store(&state, move |store| {
            let task = store
                .create_task(
                    "phone",
                    &setup_session,
                    "create",
                    CreateTask {
                        repo_path: setup_repo.to_str().unwrap().into(),
                        title: "Task".into(),
                        brief: "Work".into(),
                        parent_task_id: None,
                        policy: TaskPolicy {
                            allowed_agents: vec!["codex".into()],
                            max_workers: 1,
                        },
                    },
                    1,
                )?
                .value;
            let operation = store
                .prepare_attempt(
                    "phone",
                    &setup_session,
                    &task.id,
                    "attempt",
                    task.revision,
                    NewAttempt {
                        agent_kind: "codex".into(),
                        role: AttemptRole::Lead,
                    },
                    "digest",
                    2,
                )?
                .value;
            let revision = store.detail(&setup_session, &task.id)?.task.revision;
            Ok((task.id, operation.attempt_id.unwrap(), revision))
        })
        .await
        .unwrap();
        let input = ResultInput {
            attempt_id,
            summary: "Submitted".into(),
            evidence: vec![],
            artifacts: vec![ArtifactRef {
                path: "result.html".into(),
                sha256: format!("{:x}", Sha256::digest(b"<script>unsafe()</script>")),
                size_bytes: 25,
            }],
        };
        let submission = submit_result(
            State(state.clone()),
            Path((session.clone(), task_id.clone())),
            headers("device"),
            Json(RevisionRequest {
                request_key: "result".into(),
                expected_revision: revision,
                input: input.clone(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(!submission.replayed);
        std::fs::remove_file(repo.join("result.html")).unwrap();
        let replay = submit_result(
            State(state.clone()),
            Path((session.clone(), task_id.clone())),
            headers("device"),
            Json(RevisionRequest {
                request_key: "result".into(),
                expected_revision: revision,
                input,
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(replay.replayed);
        assert_eq!(submission.value.id, replay.value.id);
        let response = artifact_content(
            State(state.clone()),
            Path((
                session.clone(),
                task_id.clone(),
                submission.value.id.clone(),
                0,
            )),
            headers("device"),
        )
        .await
        .unwrap();
        assert_eq!(
            response.headers()["content-type"],
            "application/octet-stream"
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert!(response.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment"));
        let content = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(content.as_ref(), b"<script>unsafe()</script>");
        let invalid = artifact_content(
            State(state.clone()),
            Path((
                session.clone(),
                task_id.clone(),
                "another-submission".into(),
                0,
            )),
            headers("device"),
        )
        .await;
        assert!(matches!(invalid, Err((StatusCode::NOT_FOUND, _))));
        let invalid = artifact_content(
            State(state),
            Path((session, task_id, submission.value.id, 1)),
            headers("device"),
        )
        .await;
        assert!(matches!(invalid, Err((StatusCode::NOT_FOUND, _))));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn receipt_lookup_is_paired_actor_scoped_and_read_only() {
        let mut state = fixture();
        state.work = Some(std::sync::Arc::new(std::sync::Mutex::new(
            WorkStore::in_memory().unwrap(),
        )));
        let session = state.config.sessions[0].id.clone();
        let task = state
            .work
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create_task(
                "phone",
                &session,
                "recover",
                CreateTask {
                    repo_path: "/tmp/project".into(),
                    title: "Recover".into(),
                    brief: "Lost reply".into(),
                    parent_task_id: None,
                    policy: TaskPolicy {
                        allowed_agents: vec!["codex".into()],
                        max_workers: 0,
                    },
                },
                1,
            )
            .unwrap()
            .value;
        let query = || {
            Query(ReceiptQuery {
                kind: OperationKind::CreateTask,
                request_key: "recover".into(),
            })
        };
        assert!(matches!(
            receipt(
                State(state.clone()),
                Path(session.clone()),
                headers("admin"),
                query()
            )
            .await,
            Err((StatusCode::FORBIDDEN, _))
        ));
        state
            .devices
            .lock()
            .unwrap()
            .push(crate::tests::test_device("other", "other-device"));
        assert!(matches!(
            receipt(
                State(state.clone()),
                Path(session.clone()),
                headers("other-device"),
                query()
            )
            .await,
            Err((StatusCode::NOT_FOUND, _))
        ));
        let Json(found) = receipt(
            State(state.clone()),
            Path(session.clone()),
            headers("device"),
            query(),
        )
        .await
        .unwrap();
        assert_eq!(found.value["id"], task.id);
        let absent = receipt(
            State(state.clone()),
            Path(session.clone()),
            headers("device"),
            Query(ReceiptQuery {
                kind: OperationKind::StartAttempt,
                request_key: "recover".into(),
            }),
        )
        .await;
        assert!(matches!(absent, Err((StatusCode::NOT_FOUND, _))));
        assert_eq!(
            state
                .work
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .detail(&session, &task.id)
                .unwrap()
                .task
                .revision,
            1
        );
    }
}
