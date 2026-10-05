//! Additive captured-history API. No native reads or changes to `/output`.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use super::history::{parse_cursor, HistoryError, HistoryScope, MAX_HISTORY_LIMIT};
use crate::{
    api_error, backend, backend_api_error, content_envelope, find_session, require_device,
    terminal_backend, ApiResult, AppState,
};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryQuery {
    limit: Option<usize>,
    before: Option<String>,
    format: Option<String>,
    source: Option<String>,
}

pub(crate) async fn pane_history(
    State(state): State<AppState>,
    Path((session_id, pane_id)): Path<(String, String)>,
    Query(query): Query<HistoryQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let device = require_device(&state, &headers)?;
    let scope = validate_query(&session_id, &pane_id, device, &state.generation, &query)?;
    let session = find_session(&state.config, &session_id)?;
    // Fresh, complete topology establishes liveness and prunes known disappearances.
    // No mutex is held across I/O; failures are real backend errors, not an empty
    // history answer. We never read native backlog to fill missing captures.
    let panes = terminal_backend(session)
        .list_panes()
        .await
        .map_err(backend_api_error)?;
    let live = panes.iter().any(|pane| pane.id.as_str() == pane_id);
    let listing = backend::compat::pane_list(panes);
    state
        .scrollback
        .lock()
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history_lock_failed",
                "failed to lock captured history",
            )
        })?
        .observe_listing(&session_id, &listing);
    if !live {
        return Err(if query.before.is_some() {
            history_error(HistoryError::Gone)
        } else {
            api_error(StatusCode::NOT_FOUND, "pane_not_found", "pane not found")
        });
    }
    let page = super::history::read_page(state.history.as_ref(), scope, query.before.as_deref())
        .await
        .map_err(history_error)?;
    Ok(Json(content_envelope(page)))
}

fn validate_query(
    session: &str,
    pane: &str,
    device: String,
    generation: &str,
    query: &HistoryQuery,
) -> ApiResult<HistoryScope> {
    if [session, pane].iter().any(|id| {
        id.is_empty() || id.len() > 256 || id.contains('/') || id.chars().any(char::is_control)
    }) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_history_identity",
            "invalid history session or pane identity",
        ));
    }
    let limit = query.limit.unwrap_or(200);
    if !(1..=MAX_HISTORY_LIMIT).contains(&limit) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_history_limit",
            "history limit must be between 1 and 500",
        ));
    }
    let format = query.format.as_deref().unwrap_or("text");
    if !matches!(format, "text" | "ansi") {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_format",
            "format must be text or ansi",
        ));
    }
    if query.source.as_deref().unwrap_or("recent-unwrapped") != "recent-unwrapped" {
        if query.before.is_some() {
            return Err(history_error(HistoryError::Mismatch));
        }
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_source",
            "history source must be recent-unwrapped",
        ));
    }
    if let Some(cursor) = &query.before {
        parse_cursor(cursor).map_err(history_error)?;
    }
    Ok(HistoryScope {
        session: session.into(),
        pane: pane.into(),
        format: format.into(),
        device,
        generation: generation.into(),
        limit,
    })
}

fn history_error(error: HistoryError) -> (StatusCode, Json<Value>) {
    match error {
        HistoryError::InvalidCursor => api_error(
            StatusCode::BAD_REQUEST,
            "invalid_history_cursor",
            "invalid history cursor",
        ),
        HistoryError::Gone => api_error(
            StatusCode::GONE,
            "history_cursor_gone",
            "captured history expired or reset; restart without before",
        ),
        HistoryError::Mismatch => api_error(
            StatusCode::CONFLICT,
            "history_cursor_mismatch",
            "history cursor does not match this request; restart without before",
        ),
        HistoryError::TooLarge => api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "history_snapshot_too_large",
            "captured history exceeds snapshot or row byte limits",
        ),
        HistoryError::Unavailable => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "history_storage_unavailable",
            "captured history storage is unavailable",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use tower::ServiceExt as _;

    struct Fixture {
        state: AppState,
        herdr: FakePaneListHerdr,
    }

    impl Fixture {
        async fn new() -> Self {
            let herdr = FakePaneListHerdr::start(json!([
                { "pane_id": "p", "terminal_id": "term", "workspace_id": "w", "tab_id": "t", "width": 80, "height": 4, "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 4 } },
                { "pane_id": "native", "workspace_id": "w", "tab_id": "t", "scroll": { "max_offset_from_bottom": 200, "viewport_rows": 4 } }
            ]));
            let mut state = test_state(
                "admin",
                vec![
                    test_device("d", "token"),
                    test_device("other", "other-token"),
                ],
            );
            state.config.sessions = vec![herdr.session("s"), herdr.session("other")];
            let listing = terminal_backend(&state.config.sessions[0])
                .list_panes()
                .await
                .unwrap();
            {
                let mut store = state.scrollback.lock().unwrap();
                store.observe_listing("s", &backend::compat::pane_list(listing));
                for top in 0..8 {
                    let frame = (top..top + 4)
                        .map(|i| format!("row {i}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    store.record_frame("s", "p", "recent_unwrapped", "text", &frame);
                    let ansi = (top..top + 4)
                        .map(|i| format!("\x1b[31mrow {i}\x1b[0m"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    store.record_frame("s", "p", "recent_unwrapped", "ansi", &ansi);
                }
            }
            Self { state, herdr }
        }

        fn app(&self) -> Router {
            crate::terminal::routes::mount(Router::new())
                .layer(middleware::from_fn_with_state(
                    self.state.clone(),
                    encrypted_transport,
                ))
                .layer(middleware::from_fn(security_headers))
                .with_state(self.state.clone())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.herdr.socket_path);
        }
    }

    async fn get(app: Router, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().uri(uri);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        assert_eq!(response.headers()["cache-control"], "no-store, max-age=0");
        let body = axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&body)
                .unwrap_or_else(|_| json!({"rejection": String::from_utf8_lossy(&body)})),
        )
    }

    #[tokio::test]
    async fn captured_history_http_contract_formats_and_concurrent_first_pages() {
        let fixture = Fixture::new().await;
        let uri = "/api/sessions/s/panes/p/history?limit=2";
        let (a, b) = tokio::join!(
            get(fixture.app(), uri, Some("token")),
            get(fixture.app(), uri, Some("token"))
        );
        assert_eq!(a.0, StatusCode::OK);
        assert_eq!(a, b);
        assert_eq!(a.1["schema_version"], CONTENT_SCHEMA_VERSION);
        assert_eq!(a.1["data"]["rows"], json!(["row 5", "row 6"]));
        assert_eq!(a.1["data"]["generation"], &*fixture.state.generation);
        assert_eq!(a.1["data"]["source"], "gateway-captured");
        assert_eq!(a.1["data"]["includes_live_viewport"], false);
        let cursor = a.1["data"]["next_before"].as_str().unwrap();
        let second = get(
            fixture.app(),
            &format!("{uri}&before={cursor}"),
            Some("token"),
        )
        .await;
        assert_eq!(second.1["data"]["rows"], json!(["row 3", "row 4"]));
        let ansi = get(fixture.app(), &format!("{uri}&format=ansi"), Some("token")).await;
        assert_eq!(
            ansi.1["data"]["rows"],
            json!(["\x1b[31mrow 5\x1b[0m", "\x1b[31mrow 6\x1b[0m"])
        );
        let native = get(
            fixture.app(),
            "/api/sessions/s/panes/native/history",
            Some("token"),
        )
        .await;
        assert_eq!(native.0, StatusCode::OK);
        assert_eq!(native.1["data"]["availability"], "not_captured");
        assert_eq!(native.1["data"]["rows"], json!([]));
        assert!(native.1["data"]["next_before"].is_null());
    }

    #[tokio::test]
    async fn captured_history_auth_invalid_queries_cursor_cross_scope_and_restart() {
        let fixture = Fixture::new().await;
        let uri = "/api/sessions/s/panes/p/history?limit=2";
        for (token, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("bad"), StatusCode::FORBIDDEN),
            (Some("admin"), StatusCode::FORBIDDEN),
        ] {
            assert_eq!(get(fixture.app(), uri, token).await.0, expected);
        }
        for query in [
            "limit=0",
            "limit=501",
            "limit=-1",
            "limit=wat",
            "limit=184467440737095516160",
            "format=html",
            "source=visible",
            "before=not-a-cursor",
            "start=0&end=10",
            "limit=1&limit=2",
        ] {
            assert_eq!(
                get(
                    fixture.app(),
                    &format!("/api/sessions/s/panes/p/history?{query}"),
                    Some("token")
                )
                .await
                .0,
                StatusCode::BAD_REQUEST,
                "{query}"
            );
        }
        let first = get(fixture.app(), uri, Some("token")).await;
        let cursor = first.1["data"]["next_before"].as_str().unwrap();
        for other in [
            format!("{uri}&format=ansi&before={cursor}"),
            format!("/api/sessions/s/panes/native/history?limit=2&before={cursor}"),
            format!("/api/sessions/other/panes/p/history?limit=2&before={cursor}"),
            format!("/api/sessions/s/panes/p/history?limit=3&before={cursor}"),
            format!("{uri}&source=visible&before={cursor}"),
        ] {
            let answer = get(fixture.app(), &other, Some("token")).await;
            assert_eq!(answer.0, StatusCode::CONFLICT, "{other}");
            assert_eq!(answer.1["error"]["code"], "history_cursor_mismatch");
        }
        assert_eq!(
            get(
                fixture.app(),
                &format!("{uri}&before={cursor}"),
                Some("other-token")
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let forged = format!(
            "{}.{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        assert_eq!(
            get(
                fixture.app(),
                &format!("{uri}&before={forged}"),
                Some("token")
            )
            .await
            .0,
            StatusCode::GONE
        );
        *fixture.state.scrollback.lock().unwrap() = scrollback::ScrollbackStore::default();
        assert_eq!(
            get(
                fixture.app(),
                &format!("{uri}&before={cursor}"),
                Some("token")
            )
            .await
            .0,
            StatusCode::GONE
        );
        assert_eq!(
            get(
                fixture.app(),
                "/api/sessions/s/panes/absent/history",
                Some("token")
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(
                fixture.app(),
                "/api/sessions/missing/panes/p/history",
                Some("token")
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn captured_history_sealed_transport_accepts_real_envelope_not_forged_proof() {
        let fixture = Fixture::new().await;
        let key = generate_token();
        let material = transport::decode_key(&key).unwrap();
        fixture.state.devices.lock().unwrap()[0].transport_key = Some(key.clone());
        let uri = "/api/sessions/s/panes/p/history?limit=2";
        assert_eq!(
            get(fixture.app(), uri, Some("token")).await.0,
            StatusCode::FORBIDDEN
        );
        let request = Request::builder()
            .uri(uri)
            .header("authorization", "Bearer token")
            .header(TRANSPORT_PROOF_HEADER, &key)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            fixture.app().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let aad = format!("GET {uri}");
        let payload = EncryptedRequestPayload {
            token: "token".into(),
            content_type: None,
            body: String::new(),
        };
        let envelope = transport::seal(
            &material,
            transport::Direction::Request,
            aad.as_bytes(),
            &serde_json::to_vec(&payload).unwrap(),
            now_unix_ms(),
        )
        .unwrap();
        let nonce = envelope.nonce.clone();
        let request = Request::builder()
            .uri(uri)
            .header(TRANSPORT_HEADER, "1")
            .header(TRANSPORT_DEVICE_HEADER, "d")
            .body(Body::from(serde_json::to_vec(&envelope).unwrap()))
            .unwrap();
        let response = fixture.app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[TRANSPORT_HEADER], "1");
        let body = axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        let envelope: transport::Envelope = serde_json::from_slice(&body).unwrap();
        let opened = transport::open(
            &material,
            transport::Direction::Response,
            format!("{aad}\n{nonce}").as_bytes(),
            &envelope,
            now_unix_ms(),
        )
        .unwrap();
        let payload: Value = serde_json::from_slice(&opened).unwrap();
        assert_eq!(payload["status"], 200);
        let plain = transport::decode_key(payload["body"].as_str().unwrap()).unwrap();
        let page: Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(page["data"]["rows"], json!(["row 5", "row 6"]));
    }

    #[tokio::test]
    async fn captured_history_editor_replacement_resets_cursor_and_cached_first_page() {
        let fixture = Fixture::new().await;
        let uri = "/api/sessions/s/panes/p/history?limit=2";
        let first = get(fixture.app(), uri, Some("token")).await;
        assert_eq!(first.1["data"]["rows"], json!(["row 5", "row 6"]));
        let cursor = first.1["data"]["next_before"].as_str().unwrap();
        let panes = terminal_backend(&fixture.state.config.sessions[0])
            .list_panes()
            .await
            .unwrap();
        let listing = backend::compat::pane_list(panes);
        let mut pane = listing["result"]["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pane| pane["pane_id"] == "p")
            .unwrap()
            .clone();
        pane["foreground_command"] = json!("nvim");
        {
            let mut store = fixture.state.scrollback.lock().unwrap();
            // Same pane/terminal/workspace/tab/size: only capture ownership changes.
            store.observe("s", &pane);
            let fence = store.begin_capture("s", "p");
            assert_eq!(
                store.serve_read_fenced(
                    "s",
                    "p",
                    ("recent_unwrapped", "text"),
                    "editor 0\neditor 1\neditor 2\neditor 3",
                    200,
                    fence
                ),
                "editor 0\neditor 1\neditor 2\neditor 3"
            );
        }
        let gone = get(
            fixture.app(),
            &format!("{uri}&before={cursor}"),
            Some("token"),
        )
        .await;
        assert_eq!(gone.0, StatusCode::GONE);
        assert_eq!(gone.1["error"]["code"], "history_cursor_gone");
        let fresh = get(fixture.app(), uri, Some("token")).await;
        assert_eq!(fresh.0, StatusCode::OK);
        assert_eq!(fresh.1["data"]["rows"], json!([]));
        assert!(fresh.1["data"]["snapshot_id"].is_null());
        assert_eq!(fresh.1["data"]["availability"], "captured");
    }

    #[tokio::test]
    async fn captured_history_unreachable_backend_is_not_an_empty_capture() {
        let state = unreachable_state();
        let err = pane_history(
            State(state),
            Path(("default".into(), "p".into())),
            Query(HistoryQuery::default()),
            bearer_headers("token"),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    #[ignore = "requires tmux; creates only a private socket/server"]
    async fn captured_history_real_isolated_tmux_pages_and_resize_reset() {
        struct IsolatedTmux {
            directory: PathBuf,
            socket: PathBuf,
        }
        impl Drop for IsolatedTmux {
            fn drop(&mut self) {
                let _ = ProcessCommand::new("tmux")
                    .arg("-S")
                    .arg(&self.socket)
                    .arg("kill-server")
                    .output();
                let _ = std::fs::remove_dir_all(&self.directory);
            }
        }
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(".cache/tmp/opencode");
        let directory = if base.is_dir() {
            base
        } else {
            std::env::temp_dir()
        }
        .join(format!("hist-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).unwrap();
        let isolated = IsolatedTmux {
            socket: directory.join("tmux.sock"),
            directory,
        };
        let control = isolated.directory.join("top");
        let script = isolated.directory.join("draw.sh");
        std::fs::write(&control, "0\n").unwrap();
        std::fs::write(&script, format!(
            "printf '\\033[?1049h'\nlast=-1\nwhile :; do\n top=$(cat '{}')\n if [ \"$top\" != \"$last\" ]; then\n  printf '\\033[H\\033[2J'\n  i=$top; count=0\n  while [ $count -lt 7 ]; do\n   printf 'row %s\\033[K' \"$i\"\n   if [ $count -lt 6 ]; then printf '\\r\\n'; fi\n   i=$((i+1)); count=$((count+1))\n  done\n  last=$top\n fi\n sleep 0.05\ndone\n", control.display()
        )).unwrap();
        let started = ProcessCommand::new("tmux")
            .arg("-S")
            .arg(&isolated.socket)
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "history-qa",
                "-x",
                "80",
                "-y",
                "8",
            ])
            .arg("sh")
            .arg(&script)
            .output()
            .unwrap();
        assert!(
            started.status.success(),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        let mut state = test_state("admin", vec![test_device("d", "token")]);
        state.config.sessions = vec![SessionConfig {
            id: "s".into(),
            label: "isolated".into(),
            backend: BackendKind::Tmux,
            socket_path: isolated.socket.to_string_lossy().into_owned(),
        }];
        let backend = terminal_backend(&state.config.sessions[0]);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let pane = loop {
            let panes = backend.list_panes().await.unwrap();
            if let Some(pane) = panes
                .into_iter()
                .find(|pane| pane.alternate_on == Some(true))
            {
                break pane;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "alternate screen did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let panes = backend.list_panes().await.unwrap();
        state
            .scrollback
            .lock()
            .unwrap()
            .observe_listing("s", &backend::compat::pane_list(panes));
        for top in 0..8 {
            std::fs::write(&control, format!("{top}\n")).unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let answer = pane_output(
                    State(state.clone()),
                    Path(("s".into(), pane.id.to_string())),
                    Query(OutputQuery {
                        source: Some("recent-unwrapped".into()),
                        lines: Some(200),
                        format: Some("text".into()),
                        start: None,
                        end: None,
                    }),
                    bearer_headers("token"),
                )
                .await
                .unwrap()
                .0;
                let text = pane_read_text(&answer).unwrap();
                if text
                    .lines()
                    .any(|row| row.trim() == format!("row {}", top + 6))
                {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "frame {top} never arrived"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let app = || {
            crate::terminal::routes::mount(Router::new())
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    encrypted_transport,
                ))
                .layer(middleware::from_fn(security_headers))
                .with_state(state.clone())
        };
        let uri = format!("/api/sessions/s/panes/{}/history?limit=2", pane.id);
        let first = get(app(), &uri, Some("token")).await;
        assert_eq!(first.0, StatusCode::OK);
        assert_eq!(first.1["data"]["rows"], json!(["row 5", "row 6"]));
        let cursor = first.1["data"]["next_before"].as_str().unwrap();
        let second = get(app(), &format!("{uri}&before={cursor}"), Some("token")).await;
        assert_eq!(second.1["data"]["rows"], json!(["row 3", "row 4"]));
        let resized = ProcessCommand::new("tmux")
            .arg("-S")
            .arg(&isolated.socket)
            .args([
                "resize-window",
                "-t",
                "history-qa:0",
                "-x",
                "120",
                "-y",
                "12",
            ])
            .output()
            .unwrap();
        assert!(resized.status.success());
        let gone = get(app(), &format!("{uri}&before={cursor}"), Some("token")).await;
        assert_eq!(gone.0, StatusCode::GONE);
        assert_eq!(gone.1["error"]["code"], "history_cursor_gone");
    }
}
