//! `GET /api/agents/transcripts/{agent}/{session_id}` and `.../head`.
//!
//! Device routes behind the same authentication and encrypted transport as
//! every other; nothing here writes. `{agent}` is Herdr's
//! `agent_session.agent` value and `{session_id}` its `agent_session.value`.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::index::{self, ReadError, DEFAULT_PAGE_LIMIT};
use super::{fence, reader_for, root_of, LocateHints, TranscriptReader};
use crate::{api_error, content_envelope, require_device, ApiResult, AppState};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageQuery {
    before: Option<String>,
    limit: Option<usize>,
    cwd: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeadQuery {
    cwd: Option<String>,
}

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/api/agents/transcripts/{agent}/{session_id}",
            get(transcript_page),
        )
        .route(
            "/api/agents/transcripts/{agent}/{session_id}/head",
            get(transcript_head),
        )
}

type Resolved = (&'static dyn TranscriptReader, std::path::PathBuf);

fn resolve(agent: &str, session_id: &str) -> ApiResult<Resolved> {
    let reader = reader_for(agent).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "transcript_agent_unsupported",
            "this agent has no transcript reader",
        )
    })?;
    if !reader.valid_session_id(session_id) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_session_id",
            "invalid session id",
        ));
    }
    // Resolved before any blocking work so a test's root applies.
    let root = root_of(reader).ok_or_else(not_found)?;
    Ok((reader, root))
}

fn not_found() -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::NOT_FOUND,
        "transcript_not_found",
        "no transcript for this session",
    )
}

fn read_error(error: ReadError) -> (StatusCode, Json<Value>) {
    match error {
        ReadError::NotFound => not_found(),
        ReadError::InvalidCursor => {
            api_error(StatusCode::BAD_REQUEST, "invalid_cursor", "invalid cursor")
        }
        ReadError::CursorGone => api_error(
            StatusCode::GONE,
            "transcript_cursor_gone",
            "the transcript was replaced; start again without before",
        ),
        ReadError::Io => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "transcript_unreadable",
            "the transcript could not be read",
        ),
    }
}

/// Locate and fence the file, then run `read` on it, off the async runtime.
async fn with_file<T: Send + 'static>(
    (reader, root): Resolved,
    session_id: String,
    hints: LocateHints,
    read: impl FnOnce(
            &'static dyn TranscriptReader,
            &std::path::Path,
            &std::path::Path,
        ) -> Result<T, ReadError>
        + Send
        + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(move || {
        let root = fence::canonical_root(&root).ok_or(ReadError::NotFound)?;
        let path = super::locate(reader, &root, &session_id, &hints).ok_or(ReadError::NotFound)?;
        read(reader, &root, &path)
    })
    .await
    .map_err(|_| read_error(ReadError::Io))?
    .map_err(read_error)
}

async fn transcript_page(
    State(state): State<AppState>,
    Path((agent, session_id)): Path<(String, String)>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let resolved = resolve(&agent, &session_id)?;
    let PageQuery { before, limit, cwd } = query;
    let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    let page = with_file(
        resolved,
        session_id.clone(),
        LocateHints { cwd },
        move |reader, root, path| index::page(reader, root, path, before.as_deref(), limit),
    )
    .await?;
    Ok(Json(content_envelope(json!({
        "agent": agent,
        "session_id": session_id,
        "items": page.items,
        "order": "oldest-to-newest",
        "has_more": page.has_more,
        "next_before": page.next_before,
        "skipped_lines": page.skipped_lines,
        "head": page.head,
    }))))
}

async fn transcript_head(
    State(state): State<AppState>,
    Path((agent, session_id)): Path<(String, String)>,
    Query(query): Query<HeadQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let resolved = resolve(&agent, &session_id)?;
    let head = with_file(
        resolved,
        session_id.clone(),
        LocateHints { cwd: query.cwd },
        index::head,
    )
    .await?;
    let mut data = json!({ "agent": agent, "session_id": session_id });
    if let (Some(data), Value::Object(head)) = (data.as_object_mut(), json!(head)) {
        data.extend(head);
    }
    Ok(Json(content_envelope(data)))
}

#[cfg(test)]
mod tests {
    use super::super::claude::tests::{assistant, tool_result, user};
    use super::super::test_roots;
    use crate::*;
    use tower::ServiceExt as _;

    const SESSION: &str = "0b5e7c1d-1111-4222-8333-944455556666";

    struct Fixture {
        state: AppState,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("claude-projects-{}", uuid::Uuid::new_v4()));
            let project = root.join("-w-proj");
            std::fs::create_dir_all(&project).unwrap();
            let lines = [
                user("u1", json!("first question")),
                assistant(
                    "a1",
                    "msg_1",
                    json!([{ "type": "tool_use", "id": "toolu_1", "name": "Bash", "input": { "command": "ls" } }]),
                ),
                tool_result("r1", "toolu_1", json!("file.txt"), false),
                assistant(
                    "a2",
                    "msg_2",
                    json!([{ "type": "text", "text": "There is one file." }]),
                ),
                user("u2", json!("second question")),
                assistant(
                    "a3",
                    "msg_3",
                    json!([{ "type": "text", "text": "Answer two." }]),
                ),
            ];
            std::fs::write(
                project.join(format!("{SESSION}.jsonl")),
                lines
                    .iter()
                    .map(|line| format!("{line}\n"))
                    .collect::<String>(),
            )
            .unwrap();
            test_roots::set("claude", root.clone());
            Self {
                state: test_state("admin", vec![test_device("d", "token")]),
                root,
            }
        }

        fn app(&self) -> Router {
            super::mount(Router::new())
                .layer(middleware::from_fn_with_state(
                    self.state.clone(),
                    encrypted_transport,
                ))
                .with_state(self.state.clone())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
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
        let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    fn texts(page: &Value) -> Vec<String> {
        page["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| match item["part"]["type"].as_str().unwrap() {
                "text" => item["part"]["text"].as_str().unwrap().to_owned(),
                other => format!(
                    "<{other} {}>",
                    item["part"]["status"].as_str().unwrap_or("")
                ),
            })
            .collect()
    }

    #[tokio::test]
    async fn pages_walk_back_from_the_newest_entry() {
        let fixture = Fixture::new();
        let base = format!("/api/agents/transcripts/claude/{SESSION}");
        let (status, first) = get(
            fixture.app(),
            &format!("{base}?limit=2&cwd=/w/proj"),
            Some("token"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(texts(&first), ["second question", "Answer two."]);
        assert_eq!(first["data"]["has_more"], true);
        assert_eq!(first["data"]["order"], "oldest-to-newest");
        assert_eq!(first["data"]["head"]["lines"], 6);
        assert_eq!(first["data"]["head"]["entries"], 5);
        let cursor = first["data"]["next_before"].as_str().unwrap().to_owned();

        // Without the cwd hint the file is found by scanning projects.
        let (status, older) = get(
            fixture.app(),
            &format!("{base}?limit=10&before={cursor}"),
            Some("token"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            texts(&older),
            ["first question", "<tool completed>", "There is one file."]
        );
        assert_eq!(older["data"]["items"][1]["part"]["output"], "file.txt");
        assert_eq!(older["data"]["has_more"], false);
        assert_eq!(older["data"]["next_before"], Value::Null);

        let (status, head) = get(fixture.app(), &format!("{base}/head"), Some("token")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(head["data"]["lines"], 6);
        assert_eq!(head["data"]["agent"], "claude");
        assert!(head["data"]["bytes"].as_u64().unwrap() > 0);
        assert!(head["data"]["mtime_ms"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn refusals_are_specific_and_never_touch_other_files() {
        let fixture = Fixture::new();
        let base = "/api/agents/transcripts";
        for (uri, token, status, code) in [
            (
                format!("{base}/claude/{SESSION}"),
                None,
                StatusCode::UNAUTHORIZED,
                "missing_authorization",
            ),
            (
                format!("{base}/aider/{SESSION}"),
                Some("token"),
                StatusCode::NOT_FOUND,
                "transcript_agent_unsupported",
            ),
            (
                format!("{base}/claude/..%2F..%2Fetc%2Fpasswd"),
                Some("token"),
                StatusCode::BAD_REQUEST,
                "invalid_session_id",
            ),
            (
                format!("{base}/claude/00000000-0000-4000-8000-000000000000"),
                Some("token"),
                StatusCode::NOT_FOUND,
                "transcript_not_found",
            ),
            (
                format!("{base}/claude/{SESSION}?before=nonsense"),
                Some("token"),
                StatusCode::BAD_REQUEST,
                "invalid_cursor",
            ),
            (
                format!("{base}/claude/{SESSION}?before=0000000000000000.1"),
                Some("token"),
                StatusCode::GONE,
                "transcript_cursor_gone",
            ),
        ] {
            let (actual, body) = get(fixture.app(), &uri, token).await;
            assert_eq!(actual, status, "{uri}");
            assert_eq!(body["error"]["code"], code, "{uri}");
        }
        // A cwd hint cannot lead out of the root: it only ever names a slug.
        let (status, _) = get(
            fixture.app(),
            &format!("{base}/claude/{SESSION}/head?cwd=/../../etc"),
            Some("token"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get(
            fixture.app(),
            &format!("{base}/claude/{SESSION}?bogus=1"),
            Some("token"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_sealed_request_gets_a_sealed_page() {
        let mut fixture = Fixture::new();
        fixture.state.config.transport_encryption = TransportEncryptionMode::Required;
        let key = generate_token();
        let material = transport::decode_key(&key).unwrap();
        fixture.state.devices.lock().unwrap()[0].transport_key = Some(key);
        let uri = format!("/api/agents/transcripts/claude/{SESSION}?limit=1");
        // Bearer-only is refused once the device holds a transport key.
        assert_eq!(
            get(fixture.app(), &uri, Some("token")).await.0,
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
            .uri(&uri)
            .header(TRANSPORT_HEADER, "1")
            .header(TRANSPORT_DEVICE_HEADER, "d")
            .body(Body::from(serde_json::to_vec(&envelope).unwrap()))
            .unwrap();
        let response = fixture.app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[TRANSPORT_HEADER], "1");
        let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("Answer two."));
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
        assert_eq!(texts(&page), ["Answer two."]);
    }

    /// The transcript routes sit beside `/api/agents/{agent_id}/...` without
    /// a conflict, and the static segment wins.
    #[test]
    fn the_routes_compose_with_the_agent_routes() {
        let router = crate::agents::routes::mount(Router::new());
        let _ = super::mount(router);
    }
}
