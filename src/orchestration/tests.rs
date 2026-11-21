use std::sync::{Arc, Mutex};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::{store, ReviewerSpec, Run, Start};
use crate::tasks::{
    domain::{
        PrincipalKind, Task, TaskBinding, TaskId, TaskKind, TaskOrigin, TaskPrincipal, TaskState,
    },
    ports::TaskStore,
    store_sqlite::SqliteTaskStore,
};
use crate::test_support::{FakeAgent, FakeMcp};
use crate::{test_device, test_state, AppState};

fn request() -> Start {
    Start {
        client_ref: "request-1".into(),
        goal: "Investigate terminal delay and verify the fix".into(),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        coordinator: "opencode".into(),
        model: None,
        variant: None,
        asid: None,
        attachments: vec![],
        mode: None,
        reviewer: None,
        max_review_rounds: None,
    }
}

fn state() -> AppState {
    test_state(
        "admin",
        vec![
            test_device("owner", "token"),
            test_device("other", "other-token"),
            crate::test_support::test_peer_device("peer", "peer-token"),
        ],
    )
}

async fn call(
    state: &AppState,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = super::mount(Router::new())
        .with_state(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn concurrent_reservations_have_one_dispatch_owner_and_survive_reopen() {
    let path =
        crate::test_state_dir().join(format!("orchestration-{}.sqlite3", uuid::Uuid::new_v4()));
    let db = crate::platform::store::sqlite::SqliteStore::open(&path, None, &[store::MIGRATION])
        .unwrap();
    let (a, b) = tokio::join!(
        store::reserve(&db, "owner".into(), request()),
        store::reserve(&db, "owner".into(), request())
    );
    let (a, first) = a.unwrap();
    let (b, second) = b.unwrap();
    assert_eq!(a.id, b.id);
    assert_ne!(first, second);
    drop(db);
    let db = crate::platform::store::sqlite::SqliteStore::open(&path, None, &[store::MIGRATION])
        .unwrap();
    let (replayed, fresh) = store::reserve(&db, "owner".into(), request())
        .await
        .unwrap();
    assert!(!fresh);
    assert_eq!(replayed.id, a.id);
    assert!(store::by_ref(&db, "other".into(), request().client_ref)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn create_dispatches_once_and_conflicting_replay_never_sends() {
    let state = state();
    let mut agent = FakeAgent::new("opencode").with_sessions(&[("opencode_new", 1)]);
    agent.mcp = Some(Arc::new(Mutex::new(FakeMcp::default())));
    let prompts = agent.prompts.clone();
    state.agent_runtime.attach_for_test(agent.manager()).await;
    let body = json!(request());
    let (status, first) = call(&state, "token", "POST", "/api/orchestrations", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert!(first["run"]["task_id"].is_string(), "{first}");
    assert_eq!(prompts.lock().unwrap().len(), 1);
    let (_, second) = call(&state, "token", "POST", "/api/orchestrations", body.clone()).await;
    assert_eq!(first["run"]["id"], second["run"]["id"]);
    let mut changed = body;
    changed["goal"] = json!("Do different work");
    assert_eq!(
        call(&state, "token", "POST", "/api/orchestrations", changed)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(prompts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn uncertain_dispatch_is_shared_with_paired_apps_and_refuses_unknown_tokens() {
    let state = state();
    let mut requested = request();
    requested.cwd = "/directory-no-longer-present".into();
    let (run, _) = store::reserve(&state.db, "owner".into(), requested.clone())
        .await
        .unwrap();
    let (status, replayed) = call(
        &state,
        "token",
        "POST",
        "/api/orchestrations",
        json!(requested),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replayed["phase"], "dispatch_unconfirmed");
    let path = format!("/api/orchestrations/{}", run.id);
    assert_eq!(
        call(&state, "other-token", "GET", &path, Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&state, "invalid", "GET", &path, Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    let (_, listed) = call(
        &state,
        "other-token",
        "GET",
        "/api/orchestrations",
        Value::Null,
    )
    .await;
    assert_eq!(listed["runs"].as_array().unwrap().len(), 1);
    for (method, path, body) in [
        ("GET", path.as_str(), Value::Null),
        ("POST", "/api/orchestrations", json!(request())),
    ] {
        assert_eq!(
            call(&state, "peer-token", method, path, body).await.0,
            StatusCode::FORBIDDEN
        );
    }
}

async fn insert_task(
    state: &AppState,
    reference: Option<String>,
    parent: Option<TaskId>,
    status: TaskState,
) -> Task {
    SqliteTaskStore::new(state.db.clone())
        .insert(&Task {
            id: TaskId::new(),
            kind: TaskKind::Delegation,
            goal: "bounded work".into(),
            cwd: None,
            binding: TaskBinding::None,
            state: status,
            summary: None,
            exit_code: None,
            created_by: TaskPrincipal {
                kind: PrincipalKind::Device,
                id: "owner".into(),
            },
            created_ms: 1,
            updated_ms: 1,
            seq: 0,
            parent,
            origin: TaskOrigin::Device,
            label: None,
            requester_name: None,
            session_title: None,
            mentions: vec![],
            client_ref: reference,
            requester_server_id: None,
            reopened_ms: None,
            reopen_count: 0,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn recovers_lost_binding_and_never_calls_an_idle_coordinator_verified() {
    let state = state();
    let (run, _) = store::reserve(&state.db, "owner".into(), request())
        .await
        .unwrap();
    let task = insert_task(
        &state,
        Some(run.task_ref()),
        None,
        TaskState::IdleUnconfirmed,
    )
    .await;
    let path = format!("/api/orchestrations/{}", run.id);
    let (_, idle) = call(&state, "token", "GET", &path, Value::Null).await;
    assert_eq!(idle["run"]["task_id"], task.id.0);
    assert_eq!(idle["phase"], "awaiting_review");
    assert_eq!(idle["accepted"], false);
    insert_task(&state, None, Some(task.id), TaskState::Working).await;
    let (_, working) = call(&state, "token", "GET", &path, Value::Null).await;
    assert_eq!(working["phase"], "working");
    assert_eq!(working["tasks"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn unavailable_coordinator_is_rejected_before_reserving_work() {
    let state = state();
    let (status, _) = call(
        &state,
        "token",
        "POST",
        "/api/orchestrations",
        json!(request()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(store::list(&state.db).await.unwrap().is_empty());
}

#[tokio::test]
async fn existing_composer_session_is_reused_and_busy_session_is_not_dispatched() {
    let state = state();
    let mut agent = FakeAgent::new("opencode").with_sessions(&[("existing", 1), ("busy", 2)]);
    agent.sessions[1].status = crate::agents::AgentSessionStatus::Busy;
    agent.mcp = Some(Arc::new(Mutex::new(FakeMcp::default())));
    let calls = agent.calls.clone();
    let prompts = agent.prompts.clone();
    state.agent_runtime.attach_for_test(agent.manager()).await;
    let mut body = json!(request());
    body["asid"] = json!("existing");
    let (status, result) = call(&state, "token", "POST", "/api/orchestrations", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["tasks"][0]["binding"]["asid"], "existing");
    assert!(!calls.lock().unwrap().iter().any(|v| v == "create_session"));
    assert_eq!(prompts.lock().unwrap().len(), 1);
    body["client_ref"] = json!("busy-request");
    body["asid"] = json!("busy");
    let (status, _) = call(&state, "token", "POST", "/api/orchestrations", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(prompts.lock().unwrap().len(), 1);
    assert_eq!(store::list(&state.db).await.unwrap().len(), 1);
}

#[test]
fn reviewer_spec_included_in_prompt_and_round_bounds_validated() {
    let mut req = request();
    req.reviewer = Some(ReviewerSpec {
        agent: "claude".into(),
        model: Some("anthropic/opus-4.6".into()),
        variant: Some("high".into()),
    });
    req.max_review_rounds = Some(3);
    let run = Run {
        id: "o_test".into(),
        owner: "owner".into(),
        request: req.clone(),
        created_ms: 1,
        task_id: None,
        state: super::RunState::for_request(&req),
    };
    let prompt = run.prompt();
    assert!(prompt.contains("claude"));
    assert!(prompt.contains("anthropic/opus-4.6"));
    assert!(prompt.contains("high"));
    assert!(prompt.contains("3 review round(s)"));
    let parsed: Start = serde_json::from_value(json!(req)).unwrap();
    assert_eq!(parsed, run.request);
}

#[tokio::test]
async fn mao_pipeline_exposes_universal_deliverables_gates_and_control_actions() {
    let state = state();
    let agent = FakeAgent::new("opencode").with_sessions(&[("coder-s", 1), ("rev-s", 2)]);
    state.agent_runtime.attach_for_test(agent.manager()).await;

    let mut req = request();
    req.asid = Some("coder-s".into());
    req.reviewer = Some(ReviewerSpec {
        agent: "opencode".into(),
        model: None,
        variant: None,
    });
    req.max_review_rounds = Some(2);

    let (status, created) = call(&state, "token", "POST", "/api/orchestrations", json!(req)).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let run_id = created["run"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["mao_phase"], "coding");
    assert_eq!(created["coder"]["agent"], "opencode");
    assert_eq!(created["reviewer"]["agent"], "opencode");
    assert_eq!(created["quality_gates"].as_array().unwrap().len(), 4);

    let approve_path = format!("/api/orchestrations/{run_id}/approve");
    let (status, approved) = call(&state, "token", "POST", &approve_path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["accepted"], true);
    assert_eq!(approved["mao_phase"], "approved");
    assert_eq!(approved["phase"], "completed");

    let cancel_path = format!("/api/orchestrations/{run_id}/cancel");
    let (status, cancelled) = call(&state, "token", "POST", &cancel_path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["accepted"], false);
    assert_eq!(cancelled["mao_phase"], "cancelled");
}

#[test]
fn parses_findings_and_strips_protocol_markers_from_summary() {
    let reply = "\
Missing null check before indexing sessions.
- MAO_ISSUE: [HIGH] src/routes.rs:42 | Guard empty session slice before unwrap
* MAO_ISSUE: [LOW] src/tests.rs:19 | Add boundary case
MAO_VERDICT: CHANGES_REQUESTED";

    let findings = super::parse_findings(reply);
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0].severity, "high");
    assert_eq!(findings[0].location, "src/routes.rs:42");
    assert_eq!(
        findings[0].message,
        "Guard empty session slice before unwrap"
    );
    assert_eq!(findings[1].severity, "low");
    assert_eq!(findings[1].location, "src/tests.rs:19");

    let (verdict, summary) = super::parse_verdict(reply);
    assert_eq!(verdict, "changes_requested");
    assert!(!summary.contains("MAO_VERDICT:"));
    assert!(!summary.contains("MAO_ISSUE:"));
    assert!(summary.contains("Missing null check"));
}
