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
        computer_ids: vec![],
        mentions: vec![],
        machine: None,
        auto_approve: None,
        agent_pool: vec![],
        auto_failover: None,
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
        machine: Some("mac-studio".into()),
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
        machine: None,
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
    let rework_path = format!("/api/orchestrations/{run_id}/rework");
    let cancel_path = format!("/api/orchestrations/{run_id}/cancel");

    for forbidden_path in [&approve_path, &rework_path, &cancel_path] {
        let (status, _) = call(&state, "other-token", "POST", forbidden_path, Value::Null).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "non-owner must be forbidden on {forbidden_path}"
        );
    }

    let (status, approved) = call(&state, "token", "POST", &approve_path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["accepted"], true);
    assert_eq!(approved["mao_phase"], "approved");
    assert_eq!(approved["phase"], "completed");

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

#[test]
fn low_severity_only_findings_auto_approve_in_gateway() {
    let reply = "\
Clean fix overall.\n\
- MAO_ISSUE: [LOW] src/routes.rs:12 | Consider renaming helper\n\
MAO_VERDICT: CHANGES_REQUESTED";

    let (verdict, summary) = super::parse_verdict(reply);
    assert_eq!(verdict, "approved");
    assert!(!summary.contains("MAO_VERDICT:"));
}

#[tokio::test]
async fn round_two_rework_tracks_new_rework_task_rather_than_settled_root() {
    let state = state();
    let mut req = request();
    req.reviewer = Some(ReviewerSpec {
        agent: "opencode".into(),
        model: None,
        variant: None,
        machine: None,
    });
    let (mut run, _) = store::reserve(&state.db, "owner".into(), req)
        .await
        .unwrap();
    let root = insert_task(&state, Some(run.task_ref()), None, TaskState::Done).await;
    let rework_task = insert_task(
        &state,
        Some(format!("{}:rework:2", run.task_ref())),
        Some(root.id.clone()),
        TaskState::Working,
    )
    .await;

    run.state.round = 2;
    run.state.reviewed_round = 1;
    run.state.steps.push(super::PipelineStep {
        kind: "rework".into(),
        round: 2,
        title: "Rework #2 · opencode".into(),
        detail: "Fix edge case".into(),
        status: "running".into(),
        asid: None,
        task_id: Some(rework_task.id.0.clone()),
        time_ms: 2,
    });
    store::save_state(&state.db, run.id.clone(), run.state.clone())
        .await
        .unwrap();

    let path = format!("/api/orchestrations/{}", run.id);
    let (status, fetched) = call(&state, "token", "GET", &path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["round"], 2);
    assert_eq!(fetched["mao_phase"], "coding");
    assert!(fetched["reviewer"]["asid"].is_null());
}

#[test]
fn discovers_and_tracks_multiple_git_repositories_in_workspace() {
    let ws = crate::test_state_dir().join(format!("multi-repo-{}", uuid::Uuid::new_v4()));
    let repo_a = ws.join("gateway");
    let repo_b = ws.join("app");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();

    for (repo, file, content) in [
        (&repo_a, "api.rs", "pub fn v() -> u8 { 1 }\n"),
        (&repo_b, "client.ts", "export const v = 1;\n"),
    ] {
        for args in [
            vec!["init"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "test@example.com"],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(repo.join(file), content).unwrap();
        for args in [vec!["add", "."], vec!["commit", "-m", "init"]] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
    }

    let base_commits = super::capture_base_commits(&ws.to_string_lossy(), "Sync gateway and app");
    assert_eq!(base_commits.len(), 2);

    // Modify repo_a in working tree and commit a change in repo_b mid-task.
    std::fs::write(repo_a.join("api.rs"), "pub fn v() -> u8 { 2 }\n").unwrap();
    std::fs::write(repo_b.join("client.ts"), "export const v = 2;\n").unwrap();
    assert!(std::process::Command::new("git")
        .arg("-C")
        .arg(&repo_b)
        .args(["commit", "-am", "bump v"])
        .status()
        .unwrap()
        .success());

    let (files, summary) = super::collect_local_repos_diff(
        &ws.to_string_lossy(),
        "Sync gateway and app",
        &base_commits,
        None,
    );
    let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"gateway/api.rs"), "{paths:?}");
    assert!(paths.contains(&"app/client.ts"), "{paths:?}");
    assert!(summary.contains("Repository: gateway"));
    assert!(summary.contains("Repository: app"));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn extract_timeline_evidence_captures_mcp_and_tool_file_changes() {
    use crate::agents::domain::{AgentPart, ToolCall, ToolCallStatus, ToolTime};

    let parts = [
        AgentPart::Tool(ToolCall {
            id: "tc-1".into(),
            name: "edit".into(),
            title: Some("Apply cross-repo patch".into()),
            input: json!({}),
            output: Some(json!("ok")),
            content: None,
            metadata: Some(json!({
                "files": [{
                    "path": "/remote/repo/app.ts",
                    "additions": 4,
                    "deletions": 1,
                    "diff": "+export const remoteReady = true;\n"
                }]
            })),
            state: ToolCallStatus::Completed,
            status: ToolCallStatus::Completed,
            error: None,
            child_session_id: None,
            background: false,
            input_partial: None,
            truncated: false,
            time: ToolTime::default(),
        }),
        AgentPart::Tool(ToolCall {
            id: "tc-2".into(),
            name: "muqun_tasks_delegate".into(),
            title: Some("Delegate remote mac test".into()),
            input: json!({"goal": "Run cargo test on mac-mini"}),
            output: Some(json!({"task_id": "t_remote_1", "state": "done"})),
            content: None,
            metadata: Some(json!({"exitCode": 0})),
            state: ToolCallStatus::Completed,
            status: ToolCallStatus::Completed,
            error: None,
            child_session_id: None,
            background: false,
            input_partial: None,
            truncated: false,
            time: ToolTime::default(),
        }),
        AgentPart::Diff {
            file: String::new(),
            diff: "+aggregated\n".into(),
        },
        AgentPart::Shell {
            shell_id: "sh-1".into(),
            command: "bun test".into(),
            status: "completed".into(),
            exit: Some(0.0),
            output: Some("2 pass".into()),
            truncated: false,
        },
    ];

    let (files, summary) = super::extract_timeline_evidence(parts.iter());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "/remote/repo/app.ts");
    assert_eq!(files[0].additions, 4);
    assert_eq!(files[0].deletions, 1);

    let mut merged = vec![super::ChangedFile {
        path: "repo/app.ts".into(),
        additions: 2,
        deletions: 1,
    }];
    super::merge_changed_files("/remote", &mut merged, files);
    super::merge_changed_files(
        "/remote",
        &mut merged,
        vec![super::ChangedFile {
            path: String::new(),
            additions: 99,
            deletions: 99,
        }],
    );
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].path, "repo/app.ts");
    assert_eq!(merged[0].additions, 4);
    assert_eq!(merged[0].deletions, 1);
    assert!(summary.contains("Session Tool & MCP Execution Evidence"));
    assert!(summary.contains("muqun_tasks_delegate"));
    assert!(summary.contains("remoteReady"));
    assert!(summary.contains("bun test"));
}

#[tokio::test]
async fn remote_peer_child_task_prevents_skipping_review_when_local_git_empty() {
    let state = state();
    let agent = FakeAgent::new("opencode").with_sessions(&[("coder-s", 1), ("rev-s", 2)]);
    let prompts = agent.prompts.clone();
    state.agent_runtime.attach_for_test(agent.manager()).await;

    let ws = crate::test_state_dir().join(format!("non-git-remote-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&ws).unwrap();
    let mut req = request();
    req.cwd = ws.to_string_lossy().into_owned();
    req.asid = Some("coder-s".into());
    req.reviewer = Some(ReviewerSpec {
        agent: "opencode".into(),
        model: None,
        variant: None,
        machine: None,
    });
    let (status, created) = call(&state, "token", "POST", "/api/orchestrations", json!(req)).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let run_id = created["run"]["id"].as_str().unwrap().to_string();
    let root_tid = created["tasks"][0]["task_id"].as_str().unwrap().to_string();

    // Mark coder done and insert a remote Peer child task delegated via Muqun MCP.
    let task_store = SqliteTaskStore::new(state.db.clone());
    task_store
        .insert(&Task {
            id: TaskId("t_peer_child".into()),
            kind: TaskKind::Delegation,
            goal: "Run verification on remote build Mac".into(),
            cwd: None,
            binding: TaskBinding::Peer {
                peer_id: "peer-1".into(),
                peer_server_id: "srv-mac".into(),
                remote_task_id: "t_remote_99".into(),
                peer_name: "Mac-Build".into(),
            },
            state: TaskState::Done,
            summary: Some("All remote tests passed on Mac-Build".into()),
            exit_code: Some(0),
            created_by: TaskPrincipal {
                kind: PrincipalKind::Device,
                id: "owner".into(),
            },
            created_ms: 10,
            updated_ms: 20,
            seq: 1,
            parent: Some(TaskId(root_tid.clone())),
            origin: TaskOrigin::Device,
            label: Some("remote-check".into()),
            requester_name: None,
            session_title: None,
            mentions: vec!["Mac-Build".into()],
            client_ref: None,
            requester_server_id: None,
            reopened_ms: None,
            reopen_count: 0,
        })
        .await
        .unwrap();
    task_store
        .transition(
            &TaskId(root_tid.clone()),
            crate::tasks::domain::TaskChange {
                reopen: false,
                reopen_count: None,
                reopened_ms: None,
                expected_seq: None,
                to: TaskState::Done,
                source: "test".into(),
                note: None,
                summary: Some("Delegated work to Mac-Build via MCP".into()),
                exit_code: Some(0),
                binding: None,
                session_title: None,
            },
        )
        .await
        .unwrap();

    let path = format!("/api/orchestrations/{run_id}");
    let (status, fetched) = call(&state, "token", "GET", &path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["mao_phase"], "reviewing");

    let captured_prompts = prompts.lock().unwrap();
    let review_prompt = captured_prompts
        .iter()
        .find(|p| p.contains("Read-Only Code Reviewer"))
        .expect("reviewer must be triggered even without local git diff");
    assert!(review_prompt.contains("Muqun MCP & Remote Computer Tasks"));
    assert!(review_prompt.contains("Mac-Build"));
    assert!(review_prompt.contains("All remote tests passed on Mac-Build"));
    drop(captured_prompts);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn preexisting_dirty_files_unchanged_during_turn_are_ignored_by_deliverables_gate() {
    let ws = crate::test_state_dir().join(format!("dirty-preturn-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&ws).unwrap();
    let run_git = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&ws)
            .args(args)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    };
    run_git(&["init"]);
    run_git(&["config", "user.email", "t@example.com"]);
    run_git(&["config", "user.name", "T"]);
    std::fs::write(ws.join("app.rs"), "fn main() {}\n").unwrap();
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "init"]);

    // Pre-existing dirty working tree BEFORE the turn begins.
    std::fs::write(
        ws.join("app.rs"),
        "fn main() { println!(\"old dirty\"); }\n",
    )
    .unwrap();

    let cwd = ws.to_string_lossy().into_owned();
    let base_commits = super::capture_base_commits(&cwd, "Move cards to review");
    let base_fps = super::capture_worktree_fingerprints(&cwd, "Move cards to review");

    // Turn did NOT modify the repo -> pre-existing dirty file must NOT be misattributed!
    let (files, summary) = super::collect_local_repos_diff_since(
        &cwd,
        "Move cards to review",
        &base_commits,
        &base_fps,
        None,
    );
    assert!(
        files.is_empty(),
        "unchanged dirty repo should yield no diff: {files:?}"
    );
    assert!(summary.trim().is_empty());

    // Once the turn actually modifies a file in the dirty repo, deliverables ARE collected!
    std::fs::write(
        ws.join("app.rs"),
        "fn main() { println!(\"new edit in this turn\"); }\n",
    )
    .unwrap();
    let (files2, summary2) =
        super::collect_local_repos_diff_since(&cwd, "Fix main", &base_commits, &base_fps, None);
    assert_eq!(files2.len(), 1);
    assert!(summary2.contains("app.rs"));

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn pre_dispatch_skips_quota_exhausted_coder_and_switches_to_configured_pool_agent() {
    use crate::agents::domain::account_limits::{AccountLimit, AccountLimitWindow, AccountLimits};

    let state = state();
    let mut claude = FakeAgent::new("claude").with_sessions(&[("claude_s1", 1)]);
    claude.account_limits = Some(AccountLimits {
        status: "available",
        reason: None,
        observed_at_ms: 1,
        ordinary_usage_allowed: Some(false),
        reset_credits: None,
        limits: vec![AccountLimit {
            id: "primary".into(),
            name: Some("5h window".into()),
            plan_type: None,
            model_id: None,
            credits: None,
            individual_limit: None,
            spend_control_reached: None,
            limit_reached_reason: Some("5-hour usage limit reached".into()),
            windows: vec![AccountLimitWindow {
                id: "5h".into(),
                label: Some("5h".into()),
                unit: "percent".into(),
                used: Some(100.0),
                limit: Some(100.0),
                remaining: Some(0.0),
                window_duration_minutes: Some(300),
                resets_at_ms: None,
            }],
        }],
    });
    let claude_prompts = claude.prompts.clone();

    let codex = FakeAgent::new("codex").with_sessions(&[("codex_s1", 1)]);
    let codex_prompts = codex.prompts.clone();

    state.agent_runtime.attach_for_test(claude.manager()).await;
    state.agent_runtime.attach_for_test(codex.manager()).await;

    let mut req = request();
    req.client_ref = "pre-failover-1".into();
    req.coordinator = "claude".into();
    req.reviewer = Some(ReviewerSpec {
        agent: "codex".into(),
        model: None,
        variant: None,
        machine: None,
    });
    req.auto_failover = Some(true);
    req.agent_pool = vec![
        ReviewerSpec {
            agent: "claude".into(),
            model: None,
            variant: None,
            machine: None,
        },
        ReviewerSpec {
            agent: "codex".into(),
            model: Some("gpt-5.6".into()),
            variant: None,
            machine: None,
        },
    ];

    let (status, body) = call(&state, "token", "POST", "/api/orchestrations", json!(req)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["coder"]["agent"], "codex");
    assert_eq!(body["coder"]["model"], "gpt-5.6");
    assert_eq!(body["failover_count"], 1);
    assert!(claude_prompts.lock().unwrap().is_empty());
    assert_eq!(codex_prompts.lock().unwrap().len(), 1);
    let steps = body["steps"].as_array().unwrap();
    assert!(steps.iter().any(|s| s["kind"] == "failover"));
}

#[tokio::test]
async fn runtime_coder_quota_failure_auto_switches_to_next_configured_agent() {
    let state = state();
    let claude = FakeAgent::new("claude").with_sessions(&[("claude_s1", 1)]);
    let codex = FakeAgent::new("codex").with_sessions(&[("codex_s1", 1)]);
    let codex_prompts = codex.prompts.clone();
    state.agent_runtime.attach_for_test(claude.manager()).await;
    state.agent_runtime.attach_for_test(codex.manager()).await;

    let mut req = request();
    req.client_ref = "runtime-coder-fo-1".into();
    req.coordinator = "claude".into();
    req.reviewer = Some(ReviewerSpec {
        agent: "codex".into(),
        model: None,
        variant: None,
        machine: None,
    });
    req.auto_failover = Some(true);
    req.agent_pool = vec![
        ReviewerSpec {
            agent: "claude".into(),
            model: None,
            variant: None,
            machine: None,
        },
        ReviewerSpec {
            agent: "codex".into(),
            model: Some("gpt-5.6".into()),
            variant: None,
            machine: None,
        },
    ];

    let (status, started) = call(&state, "token", "POST", "/api/orchestrations", json!(req)).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let run_id = started["run"]["id"].as_str().unwrap().to_string();
    let root_tid = started["run"]["task_id"].as_str().unwrap().to_string();

    // Simulate primary Coder failing due to rate limit / quota exhaustion.
    SqliteTaskStore::new(state.db.clone())
        .transition(
            &TaskId(root_tid),
            crate::tasks::domain::TaskChange {
                reopen: false,
                reopen_count: None,
                reopened_ms: None,
                expected_seq: None,
                to: TaskState::Failed,
                source: "test".into(),
                note: Some("HTTP 429 rate limit exceeded: quota exhausted".into()),
                summary: Some(
                    "HTTP 429 rate limit exceeded: usage limit reached for claude".into(),
                ),
                exit_code: None,
                binding: None,
                session_title: None,
            },
        )
        .await
        .unwrap();

    let (status, fetched) = call(
        &state,
        "token",
        "GET",
        &format!("/api/orchestrations/{run_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["mao_phase"], "coding");
    assert_eq!(fetched["coder"]["agent"], "codex");
    assert_eq!(fetched["coder"]["model"], "gpt-5.6");
    assert_eq!(fetched["failover_count"], 1);
    assert_eq!(codex_prompts.lock().unwrap().len(), 1);
    assert!(fetched["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["kind"] == "failover"));
}

#[tokio::test]
async fn runtime_reviewer_rate_limit_failure_auto_switches_to_next_configured_agent() {
    let ws = crate::test_state_dir().join(format!("rev-fo-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&ws).unwrap();
    let run_git = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&ws)
            .args(args)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    };
    run_git(&["init"]);
    run_git(&["config", "user.email", "t@example.com"]);
    run_git(&["config", "user.name", "T"]);
    std::fs::write(ws.join("lib.rs"), "pub fn a() {}\n").unwrap();
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "init"]);

    let state = state();
    let claude = FakeAgent::new("claude").with_sessions(&[("claude_s1", 1)]);
    let codex = FakeAgent::new("codex").with_sessions(&[("codex_s1", 1)]);
    let antigravity = FakeAgent::new("antigravity").with_sessions(&[("ag_s1", 1)]);
    let ag_prompts = antigravity.prompts.clone();
    state.agent_runtime.attach_for_test(claude.manager()).await;
    state.agent_runtime.attach_for_test(codex.manager()).await;
    state
        .agent_runtime
        .attach_for_test(antigravity.manager())
        .await;

    let mut req = request();
    req.client_ref = "runtime-rev-fo-1".into();
    req.cwd = ws.to_string_lossy().into_owned();
    req.coordinator = "claude".into();
    req.reviewer = Some(ReviewerSpec {
        agent: "codex".into(),
        model: None,
        variant: None,
        machine: None,
    });
    req.auto_failover = Some(true);
    req.agent_pool = vec![
        ReviewerSpec {
            agent: "codex".into(),
            model: None,
            variant: None,
            machine: None,
        },
        ReviewerSpec {
            agent: "antigravity".into(),
            model: Some("gemini-4-neon".into()),
            variant: None,
            machine: None,
        },
    ];

    let (status, started) = call(&state, "token", "POST", "/api/orchestrations", json!(req)).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let run_id = started["run"]["id"].as_str().unwrap().to_string();
    let root_tid = started["run"]["task_id"].as_str().unwrap().to_string();

    // Coder edits file and finishes.
    std::fs::write(ws.join("lib.rs"), "pub fn a() -> i32 { 42 }\n").unwrap();
    let task_store = SqliteTaskStore::new(state.db.clone());
    task_store
        .transition(
            &TaskId(root_tid),
            crate::tasks::domain::TaskChange {
                reopen: false,
                reopen_count: None,
                reopened_ms: None,
                expected_seq: None,
                to: TaskState::Done,
                source: "test".into(),
                note: None,
                summary: Some("Updated lib.rs".into()),
                exit_code: Some(0),
                binding: None,
                session_title: None,
            },
        )
        .await
        .unwrap();

    // First read dispatches Reviewer (codex).
    let (status, reviewing) = call(
        &state,
        "token",
        "GET",
        &format!("/api/orchestrations/{run_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reviewing}");
    assert_eq!(reviewing["mao_phase"], "reviewing");
    let rev_tid = reviewing["run"]["state"]["reviewer_task_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Simulate Reviewer failing due to quota exhaustion.
    task_store
        .transition(
            &TaskId(rev_tid),
            crate::tasks::domain::TaskChange {
                reopen: false,
                reopen_count: None,
                reopened_ms: None,
                expected_seq: None,
                to: TaskState::Failed,
                source: "test".into(),
                note: Some("insufficient_quota: model credits exhausted".into()),
                summary: Some("insufficient_quota: model credits exhausted".into()),
                exit_code: None,
                binding: None,
                session_title: None,
            },
        )
        .await
        .unwrap();

    // Next read automatically switches Reviewer to antigravity!
    let (status, switched) = call(
        &state,
        "token",
        "GET",
        &format!("/api/orchestrations/{run_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{switched}");
    assert_eq!(switched["mao_phase"], "reviewing");
    assert_eq!(switched["reviewer"]["agent"], "antigravity");
    assert_eq!(switched["reviewer"]["model"], "gemini-4-neon");
    assert_eq!(switched["failover_count"], 1);
    assert_eq!(ag_prompts.lock().unwrap().len(), 1);
    assert!(switched["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["kind"] == "failover"));

    let _ = std::fs::remove_dir_all(&ws);
}
