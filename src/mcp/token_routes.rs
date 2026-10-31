//! Assistant token management over HTTP. Issuing is for the CLI helper a
//! pane runs (it holds the admin token from `pairing.json`); listing and
//! revoking are for paired devices.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use super::issue::{issue_for_caller, IssueRequest};
use crate::{api_error, require_admin, require_device, ApiResult, AppState};

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/mcp/tokens", post(issue).get(list))
        .route("/api/mcp/tokens/{id}", delete(revoke))
}

fn store_unavailable(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    tracing::warn!(%error, "assistant token store failed");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "token_store_unavailable",
        "the assistant token store is unavailable",
    )
}

async fn issue(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    // Authenticate before looking at the body, so an unauthenticated caller
    // never learns what shape a request should have.
    require_admin(&state.config, &headers)?;
    let request: IssueRequest = serde_json::from_slice(&body)
        .ok()
        .filter(IssueRequest::within_bounds)
        .ok_or_else(|| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid request",
            )
        })?;
    let issued = issue_for_caller(&state, request).await?;
    Ok(Json(json!({
        "token": issued.token,
        "token_id": issued.session.token_id,
        "expires_ms": issued.session.expires_ms,
        "binding": issued.session.binding,
    })))
}

async fn list(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let tokens = state.mcp_tokens.list().await.map_err(store_unavailable)?;
    Ok(Json(json!({ "tokens": tokens })))
}

async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    require_device(&state, &headers)?;
    if state
        .mcp_tokens
        .revoke(&id)
        .await
        .map_err(store_unavailable)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(api_error(
            StatusCode::NOT_FOUND,
            "token_not_found",
            "Token not found",
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::mcp::issue::sweep_closed_panes;
    use crate::mcp::tokens::TokenBinding;
    use crate::test_support::FakePaneListHerdr;
    use crate::{bearer_headers, test_device, test_state};
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use std::net::SocketAddr;
    use tower::ServiceExt as _;

    const SHELL_PID: u32 = 4242;

    fn panes() -> Value {
        json!([{
            "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1",
            "shell_pid": SHELL_PID
        }])
    }

    fn state_for(herdr: &FakePaneListHerdr) -> AppState {
        let mut state = test_state("admin", vec![test_device("d1", "device")]);
        state.config.sessions[0].socket_path = herdr.session("default").socket_path;
        state
    }

    async fn codex_task(state: &AppState) -> crate::tasks::domain::TaskId {
        use crate::tasks::domain::*;
        state
            .tasks
            .insert_for_test(&Task {
                id: TaskId::new(),
                kind: TaskKind::Delegation,
                goal: "synthetic shared task".into(),
                cwd: None,
                binding: TaskBinding::Pane {
                    session_id: "default".into(),
                    pane_id: "%4".into(),
                    tab_id: None,
                    shell_pid: None,
                    agent_id: Some("codex".into()),
                    agent_job: None,
                    model: None,
                    variant: None,
                },
                state: TaskState::Working,
                summary: None,
                exit_code: None,
                created_by: TaskPrincipal {
                    kind: PrincipalKind::Device,
                    id: "d1".into(),
                },
                created_ms: now_ms(),
                updated_ms: now_ms(),
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
    }

    #[tokio::test]
    async fn immutable_task_tokens_require_admin_and_end_with_task_authority() {
        use crate::tasks::domain::{TaskChange, TaskState};
        let state = test_state(
            "admin",
            vec![
                test_device("d1", "device"),
                crate::test_support::test_peer_device("p", "peer"),
            ],
        );
        let task = codex_task(&state).await;
        let request = json!({"task_id": task.0});
        for bearer in ["", "device", "peer"] {
            assert_ne!(
                call(
                    &state,
                    "POST",
                    "/api/mcp/tokens",
                    bearer,
                    Some(request.clone())
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        let (status, issued) = call(
            &state,
            "POST",
            "/api/mcp/tokens",
            "admin",
            Some(request.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        assert_eq!(issued["binding"], json!({"kind":"task", "task_id": task.0}));
        let token = issued["token"].as_str().unwrap();
        assert_eq!(mcp_status(&state, token).await, StatusCode::OK);
        assert_eq!(
            call(
                &state,
                "POST",
                "/api/mcp/tokens",
                token,
                Some(request.clone())
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        // Explicit revocation still acts immediately, even though the task lives.
        state
            .mcp_tokens
            .revoke(issued["token_id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(mcp_status(&state, token).await, StatusCode::FORBIDDEN);
        let (_, renewed) = call(
            &state,
            "POST",
            "/api/mcp/tokens",
            "admin",
            Some(request.clone()),
        )
        .await;
        let token = renewed["token"].as_str().unwrap();
        state
            .tasks
            .move_to(
                &task,
                TaskChange {
                    to: TaskState::Cancelled,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(mcp_status(&state, token).await, StatusCode::FORBIDDEN);
        assert_eq!(
            call(&state, "POST", "/api/mcp/tokens", "admin", Some(request))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn task_renewal_preserves_finished_followups_but_not_deleted_or_unrelated_tasks() {
        use crate::tasks::domain::{TaskChange, TaskState};
        let state = test_state("admin", Vec::new());
        let task = codex_task(&state).await;
        let request = json!({"task_id": task.0});
        for bad in [
            json!({"task_id": task.0, "ancestor_pids": [1]}),
            json!({"task_id":"../task"}),
            json!({"task_id":"t_".to_owned() + &"a".repeat(100)}),
        ] {
            assert_eq!(
                call(&state, "POST", "/api/mcp/tokens", "admin", Some(bad))
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        state
            .tasks
            .move_to(
                &task,
                TaskChange {
                    to: TaskState::Done,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let (status, issued) = call(
            &state,
            "POST",
            "/api/mcp/tokens",
            "admin",
            Some(request.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let token = issued["token"].as_str().unwrap();
        assert_eq!(mcp_status(&state, token).await, StatusCode::OK);
        let id = task.0.clone();
        let session = state
            .mcp_tokens
            .resolve(token)
            .await
            .unwrap()
            .live()
            .unwrap();
        let complete = crate::mcp::lineage::Call {
            tool: "tasks.complete",
            args: Vec::new(),
        };
        assert_eq!(
            crate::mcp::lineage::own_task(&state, &session, &complete, None).await,
            None
        );
        let finished = state.tasks.task(&task).await.unwrap().unwrap();
        state
            .tasks
            .reopen(&finished, finished.updated_ms + 1)
            .await
            .unwrap();
        assert_eq!(
            crate::mcp::lineage::own_task(&state, &session, &complete, None).await,
            Some(task.clone())
        );
        state
            .db
            .call(move |db| {
                db.execute("DELETE FROM tasks WHERE id = ?", [&id])?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(mcp_status(&state, token).await, StatusCode::FORBIDDEN);
        assert_eq!(crate::mcp::issue::sweep_task_tokens(&state).await, 1);
        assert!(state.mcp_tokens.list().await.unwrap().is_empty());
        assert_eq!(
            call(&state, "POST", "/api/mcp/tokens", "admin", Some(request))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let other = codex_task(&state).await;
        let id = other.0.clone();
        state
            .db
            .call(move |db| {
                db.execute(
                    "UPDATE tasks SET binding = replace(binding, 'codex', 'claude') WHERE id = ?",
                    [&id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            call(
                &state,
                "POST",
                "/api/mcp/tokens",
                "admin",
                Some(json!({"task_id":other.0}))
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }

    fn app(state: &AppState) -> Router {
        crate::mcp::mount(Router::new()).with_state(state.clone())
    }

    async fn call(
        state: &AppState,
        method: &str,
        path: &str,
        bearer: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        for (name, value) in bearer_headers(bearer) {
            request = request.header(name.unwrap(), value);
        }
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = app(state)
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

    async fn mcp_status(state: &AppState, token: &str) -> StatusCode {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": { "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0" },
                "io.modelcontextprotocol/clientCapabilities": {}
            }}
        });
        let mut request = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("host", "127.0.0.1:23999")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "tools/list")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo("127.0.0.1:5".parse::<SocketAddr>().unwrap()));
        app(state).oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn the_admin_token_issues_a_token_for_the_pane_of_the_calling_process() {
        let herdr = FakePaneListHerdr::start(panes());
        let state = state_for(&herdr);
        let (status, body) = call(
            &state,
            "POST",
            "/api/mcp/tokens",
            "admin",
            Some(json!({ "ancestor_pids": [77, SHELL_PID, 1], "label": "claude" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["binding"]["kind"], "pane");
        assert_eq!(body["binding"]["pane_id"], "w1:p1");
        let token = body["token"].as_str().unwrap();
        assert_eq!(mcp_status(&state, token).await, StatusCode::OK);

        // Listing carries no secret, and a device can revoke by id.
        let (status, listed) = call(&state, "GET", "/api/mcp/tokens", "device", None).await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        assert_eq!(listed["tokens"][0]["label"], "claude");
        assert!(!listed.to_string().contains(token));
        let path = format!("/api/mcp/tokens/{}", body["token_id"].as_str().unwrap());
        assert_eq!(
            call(&state, "DELETE", &path, "device", None).await.0,
            StatusCode::NO_CONTENT
        );
        let (status, gone) = call(&state, "DELETE", &path, "device", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(gone["error"]["code"], "token_not_found");
        assert_eq!(mcp_status(&state, token).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_device_token_cannot_issue_and_an_unmatched_process_gets_404() {
        let herdr = FakePaneListHerdr::start(panes());
        let state = state_for(&herdr);
        let request = json!({ "ancestor_pids": [SHELL_PID] });
        let (status, _) = call(&state, "POST", "/api/mcp/tokens", "device", Some(request)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, body) = call(
            &state,
            "POST",
            "/api/mcp/tokens",
            "admin",
            Some(json!({ "ancestor_pids": [5, 6] })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "pane_not_found");
        assert!(state.mcp_tokens.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn authentication_comes_before_the_body_is_looked_at() {
        let herdr = FakePaneListHerdr::start(panes());
        let state = state_for(&herdr);
        let bad = Some(json!({ "nope": 1 }));
        let (status, _) = call(&state, "POST", "/api/mcp/tokens", "device", bad.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = call(&state, "POST", "/api/mcp/tokens", "admin", bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    #[tokio::test]
    async fn an_oversized_label_or_ancestry_is_refused() {
        let herdr = FakePaneListHerdr::start(panes());
        let state = state_for(&herdr);
        let long_label = json!({ "ancestor_pids": [SHELL_PID], "label": "x".repeat(65) });
        let mut ancestry: Vec<u32> = (1..=64).collect();
        ancestry.push(SHELL_PID);
        let long_ancestry = json!({ "ancestor_pids": ancestry });
        for request in [long_label, long_ancestry] {
            let (status, body) =
                call(&state, "POST", "/api/mcp/tokens", "admin", Some(request)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["error"]["code"], "invalid_request");
        }
        assert!(state.mcp_tokens.list().await.unwrap().is_empty());

        let at_the_limit = json!({ "ancestor_pids": [SHELL_PID], "label": "é".repeat(64) });
        let (status, _) = call(
            &state,
            "POST",
            "/api/mcp/tokens",
            "admin",
            Some(at_the_limit),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// Review focus: the pane a token was issued for closes, and the next
    /// sweep -- driven directly, not after the 60 s tick -- kills the token.
    #[tokio::test]
    async fn closing_the_pane_kills_its_token_within_one_sweep() {
        let herdr = FakePaneListHerdr::start(panes());
        let state = state_for(&herdr);
        let (_, body) = call(
            &state,
            "POST",
            "/api/mcp/tokens",
            "admin",
            Some(json!({ "ancestor_pids": [SHELL_PID] })),
        )
        .await;
        let token = body["token"].as_str().unwrap().to_owned();
        assert_eq!(mcp_status(&state, &token).await, StatusCode::OK);
        assert_eq!(
            sweep_closed_panes(&state).await,
            0,
            "the pane is still open"
        );
        assert_eq!(mcp_status(&state, &token).await, StatusCode::OK);

        herdr.set_panes(json!([]));
        assert_eq!(sweep_closed_panes(&state).await, 1);
        assert_eq!(mcp_status(&state, &token).await, StatusCode::FORBIDDEN);
        assert!(state.mcp_tokens.list().await.unwrap().is_empty());
        // The row is kept, marked revoked.
        let revoked_rows = state
            .db
            .call(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM assistant_tokens WHERE revoked_ms IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(revoked_rows, 1);
    }

    #[tokio::test]
    async fn an_unreachable_backend_keeps_its_pane_tokens() {
        let mut state = test_state("admin", Vec::new());
        state.config.sessions[0].socket_path = std::env::temp_dir()
            .join(format!("herdr-absent-{}.sock", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        let pane = TokenBinding::Pane {
            session_id: "default".into(),
            pane_id: "w1:p1".into(),
        };
        state
            .mcp_tokens
            .issue(pane, "t".into(), 60_000)
            .await
            .unwrap();
        assert_eq!(sweep_closed_panes(&state).await, 0);
        assert_eq!(state.mcp_tokens.list().await.unwrap().len(), 1);
    }

    /// An agent token has no pane to lose: a sweep over a reachable backend
    /// whose panes are all gone revokes the pane token and keeps it.
    #[tokio::test]
    async fn a_sweep_leaves_agent_tokens_alone() {
        let herdr = FakePaneListHerdr::start(json!([]));
        let state = state_for(&herdr);
        let pane = TokenBinding::Pane {
            session_id: "default".into(),
            pane_id: "w1:p1".into(),
        };
        let agent = TokenBinding::Agent {
            agent_id: "opencode".into(),
        };
        let tokens = &state.mcp_tokens;
        tokens.issue(pane, "t".into(), 60_000).await.unwrap();
        let no_expiry = crate::mcp::tokens::NO_EXPIRY;
        let kept = tokens
            .issue(agent.clone(), "oc".into(), no_expiry)
            .await
            .unwrap();
        assert_eq!(sweep_closed_panes(&state).await, 1);
        let live = tokens.list().await.unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!((&live[0].binding, live[0].expires_ms), (&agent, None));
        assert_eq!(mcp_status(&state, &kept.token).await, StatusCode::OK);
    }
}
