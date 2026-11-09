use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use super::index::{self, Mode, SessionMeta};
use super::sources::codex;
use super::text::{Query, Row};
use super::*;
use crate::agents::domain::{AgentPart, TimelineItem, TimelineRole};

fn temp_dir(name: &str) -> PathBuf {
    let dir = crate::test_state_dir().join(format!("search-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn session(asid: &str, updated_ms: u64) -> SessionMeta {
    SessionMeta {
        agent_id: "codex".into(),
        asid: asid.into(),
        title: format!("title {asid}"),
        directory: Some("/work/project".into()),
        parent_asid: None,
        updated_ms,
    }
}

fn row(id: &str, role: &'static str, text: &str, time_ms: u64) -> Row {
    Row {
        item_id: id.into(),
        message_id: format!("m-{id}"),
        role,
        kind: "text",
        time_ms,
        text: text.into(),
    }
}

async fn put(service: &SearchService, meta: SessionMeta, rows: Vec<Row>, mode: Mode, mark: &str) {
    let mark = mark.to_owned();
    service
        .writer()
        .unwrap()
        .call(move |c| Ok(index::apply(c, &meta, &rows, mode, Some(&mark), 0)?))
        .await
        .unwrap();
}

async fn find(service: &SearchService, q: &str) -> SearchPage {
    service
        .search(SearchRequest {
            query: Query::parse(q).unwrap(),
            limit: 20,
            offset: 0,
            filters: Default::default(),
        })
        .await
        .unwrap()
        .unwrap()
}

fn ids(page: &SearchPage) -> Vec<&str> {
    page.items.iter().map(|i| i.item_id.as_str()).collect()
}

#[tokio::test]
async fn substrings_match_in_cjk_and_any_case_and_short_terms_still_match() {
    let service = SearchService::open(&temp_dir("cjk"), None);
    let now = crate::now_unix_ms() as u64;
    put(
        &service,
        session("s1", now),
        vec![
            row("zh", "user", "我们把数据库迁移到新的集群", now),
            row("ja", "assistant", "テストを実行してください", now),
            row("en", "assistant", "Refactor the PARSER module", now),
        ],
        Mode::Replace,
        "w",
    )
    .await;
    for (q, want) in [
        ("数据库", vec!["zh"]),
        ("迁移", vec!["zh"]),        // two characters: the substring scan
        ("数据库 集群", vec!["zh"]), // every term must match
        ("数据库 nothing", vec![]),
        ("テスト", vec!["ja"]),
        ("parser", vec!["en"]),
        ("ser mod", vec!["en"]), // substrings, not words
        ("he", vec!["en"]),
    ] {
        assert_eq!(ids(&find(&service, q).await), want, "{q}");
    }
    let hit = &find(&service, "迁移").await.items[0];
    assert_eq!(hit.snippet, "我们把数据库迁移到新的集群");
    assert_eq!(hit.highlights, vec![[6, 8]]);
    assert_eq!((hit.asid.as_str(), hit.role.as_str()), ("s1", "user"));
}

#[test]
fn a_task_session_is_indexed_without_its_plumbing() {
    let item = |id: &str, role, part| TimelineItem {
        id: id.into(),
        message_id: id.into(),
        role,
        part,
        seq: 0,
        updated_ms: 1,
        ordinal: 0,
        attachments: None,
        image_assets: None,
        generation: None,
    };
    let goal = format!("Rename the module. {}", crate::mcp::inject::TASK_SENTENCE);
    let mut items = vec![
        item("u", TimelineRole::User, AgentPart::Text { text: goal }),
        item(
            "k",
            TimelineRole::Assistant,
            AgentPart::Tool(
                serde_json::from_value(json!({
                    "id": "k", "name": "skill", "input": {"command": "load secret-skill"},
                    "state": "completed", "status": "completed",
                }))
                .unwrap(),
            ),
        ),
        item(
            "a",
            TimelineRole::Assistant,
            AgentPart::Text {
                text: format!(
                    "Renamed it.\n{} renamed",
                    crate::tasks::pane_agent::DONE_MARKER
                ),
            },
        ),
    ];
    let rows = indexer::searchable(&mut items.clone(), true, 0);
    let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(texts, ["Rename the module.", "Renamed it."]);
    // An ordinary session keeps what its agent wrote; skill loading stays out.
    let rows = indexer::searchable(&mut items, false, 0);
    assert_eq!(rows.len(), 2);
    assert!(rows[1].text.contains("MUQUN_TASK_DONE"));
}

/// A completed item as Codex writes it: the payload type leads the line.
fn rollout_line(turn: &str, item: Value) -> String {
    format!(
        r#"{{"timestamp":"2026-01-01T00:00:00Z","ordinal":1,"type":"event_msg","payload":{{"type":"item_completed","thread_id":"t","turn_id":"{turn}","item":{item},"completed_at_ms":1000}}}}"#
    ) + "\n"
}

#[tokio::test]
async fn a_rollout_is_read_on_from_its_watermark_after_a_restart() {
    let thread = "01a00000-0000-7000-8000-000000000001";
    let dir = temp_dir("rollout");
    let path = dir.join(format!("rollout-2026-01-01T00-00-00-{thread}.jsonl"));
    let mut file = std::fs::File::create(&path).unwrap();
    write!(
        file,
        "{}{}{}{}",
        json!({"type": "session_meta", "payload": {"id": thread}}).to_string() + "\n",
        rollout_line("t1", json!({"type": "UserMessage", "id": "u1", "content": [{"type": "text", "text": "deploy the staging cluster"}]})),
        rollout_line("t1", json!({"type": "CommandExecution", "id": "c1", "command": ["bash", "-lc", "kubectl apply"],
            "parsed_cmd": [{"type": "unknown", "cmd": "kubectl apply -f staging.yaml"}],
            "aggregated_output": "SECRET_OUTPUT_LINE ".repeat(1000)})),
        rollout_line("t1", json!({"type": "AgentMessage", "id": "a1", "content": [{"type": "Text", "text": "Staging is deployed."}]})),
    )
    .unwrap();
    drop(file);

    let state_dir = temp_dir("rollout-index");
    let first = codex::read(&path, thread, None, codex::STEP_BYTES).unwrap();
    assert_eq!(first.mode, Mode::Replace);
    let item_ids: Vec<&str> = first.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(
        item_ids,
        [
            format!("{thread}/t1/u1:t0"),
            format!("{thread}/t1/c1:tool:c1"),
            format!("{thread}/t1/a1:t0"),
        ]
    );
    {
        let service = SearchService::open(&state_dir, None);
        let rows = indexer::searchable(&mut first.items.clone(), false, 0);
        put(
            &service,
            session(thread, 1),
            rows,
            first.mode,
            &first.watermark,
        )
        .await;
        assert_eq!(
            ids(&find(&service, "staging.yaml").await),
            [format!("{thread}/t1/c1:tool:c1")]
        );
        assert!(find(&service, "SECRET_OUTPUT").await.items.is_empty());
    }

    // More of the conversation, and a line still being written.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    write!(
        file,
        "{}{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"item_comp",
        rollout_line("t2", json!({"type": "UserMessage", "id": "u2", "content": [{"type": "text", "text": "now production"}]})),
    )
    .unwrap();
    drop(file);

    // A restarted gateway reads on from the stored watermark.
    let service = SearchService::open(&state_dir, None);
    let stored = service
        .writer()
        .unwrap()
        .call(|c| Ok(index::sessions(c)?))
        .await
        .unwrap();
    assert_eq!(stored[0].1.as_deref(), Some(first.watermark.as_str()));
    let next = codex::read(&path, thread, stored[0].1.as_deref(), codex::STEP_BYTES).unwrap();
    assert_eq!(next.mode, Mode::Merge);
    assert_eq!(next.items.len(), 1, "only the appended item");
    let length = std::fs::metadata(&path).unwrap().len();
    let offset: u64 = next.watermark.split_once(':').unwrap().1.parse().unwrap();
    assert!(
        offset < length,
        "the partial line is left for the next read"
    );
    let rows = indexer::searchable(&mut next.items.clone(), false, 0);
    put(
        &service,
        session(thread, 2),
        rows,
        next.mode,
        &next.watermark,
    )
    .await;
    assert_eq!(find(&service, "production").await.items.len(), 1);
    assert_eq!(
        find(&service, "staging cluster").await.items.len(),
        1,
        "kept"
    );

    // A step budget smaller than a line still moves on line by line.
    let step = codex::read(&path, thread, None, 1).unwrap();
    assert_eq!(step.items.len(), 0, "the session_meta line");
    let step = codex::read(&path, thread, Some(&step.watermark), 1).unwrap();
    assert_eq!(step.items.len(), 1);
}

#[tokio::test]
async fn one_session_contributes_three_hits_and_recent_sessions_rank_first() {
    let service = SearchService::open(&temp_dir("rank"), None);
    let now = crate::now_unix_ms() as u64;
    let old = now - 200 * 86_400_000;
    let rows = (0..5)
        .map(|n| {
            row(
                &format!("old{n}"),
                "assistant",
                "migrate the billing tables",
                old,
            )
        })
        .collect();
    put(&service, session("old", old), rows, Mode::Replace, "w").await;
    put(
        &service,
        session("new", now),
        vec![row("new0", "assistant", "migrate the billing tables", now)],
        Mode::Replace,
        "w",
    )
    .await;
    let page = find(&service, "billing").await;
    let sessions: Vec<(&str, usize)> = page
        .items
        .iter()
        .map(|i| (i.asid.as_str(), i.session_hits))
        .collect();
    assert_eq!(sessions, [("new", 1), ("old", 5), ("old", 5), ("old", 5)]);
    assert!(page.next_cursor.is_none());

    let first = service
        .search(SearchRequest {
            query: Query::parse("billing").unwrap(),
            limit: 2,
            offset: 0,
            filters: index::Filters {
                agent_id: Some("codex".into()),
                directory: Some("/work".into()),
            },
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.items.len(), 2);
    assert_eq!(first.next_cursor.as_deref(), Some("2"));
    for filters in [
        index::Filters {
            agent_id: Some("claude".into()),
            directory: None,
        },
        index::Filters {
            agent_id: None,
            directory: Some("/work/proj".into()),
        },
    ] {
        let page = service
            .search(SearchRequest {
                query: Query::parse("billing").unwrap(),
                limit: 5,
                offset: 0,
                filters,
            })
            .await
            .unwrap()
            .unwrap();
        assert!(page.items.is_empty());
    }

    // A replaced session drops the rows it no longer has.
    put(&service, session("old", old), vec![], Mode::Replace, "w2").await;
    assert_eq!(find(&service, "billing").await.items.len(), 1);
}

#[test]
fn the_query_string_is_validated() {
    let params = |pairs: &[(&str, &str)]| -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let long = "x".repeat(201);
    for (pairs, code) in [
        (vec![], "invalid_query"),
        (vec![("q", "  ")], "invalid_query"),
        (vec![("q", "a\u{7}b")], "invalid_query"),
        (vec![("q", long.as_str())], "invalid_query"),
        (vec![("q", "\"\"")], "invalid_query"),
        (vec![("q", "x"), ("limit", "0")], "invalid_limit"),
        (vec![("q", "x"), ("limit", "51")], "invalid_limit"),
        (vec![("q", "x"), ("limit", "ten")], "invalid_limit"),
        (vec![("q", "x"), ("cursor", "-1")], "invalid_cursor"),
        (vec![("q", "x"), ("cursor", "1234567")], "invalid_cursor"),
        (vec![("q", "x"), ("agent_id", "Claude")], "invalid_agent_id"),
        (vec![("q", "x"), ("directory", "work")], "invalid_directory"),
        (
            vec![("q", "x"), ("directory", "/a\nb")],
            "invalid_directory",
        ),
    ] {
        let (status, body) = routes::request(&params(&pairs))
            .err()
            .unwrap_or_else(|| panic!("{pairs:?}"));
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(body.0["error"]["code"], code, "{pairs:?}");
    }
    let ok = routes::request(&params(&[
        ("q", " 数据 库 "),
        ("limit", "50"),
        ("cursor", "20"),
    ]))
    .unwrap();
    assert_eq!(ok.query.terms, ["数据", "库"]);
    assert_eq!((ok.limit, ok.offset), (50, 20));
}

#[tokio::test]
async fn the_route_needs_a_device_and_answers_in_its_envelope() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    let herdr = crate::test_support::FakeHerdr::start(vec!["x"], None);
    let mut state = crate::test_support::output_state(&herdr);
    let get = |state: &crate::AppState, bearer: Option<&str>| {
        let mut request = Request::builder().uri("/api/search?q=billing");
        if let Some(token) = bearer {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        routes::mount(axum::Router::new())
            .with_state(state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
    };
    let body = |response: axum::response::Response| async {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    };
    assert_eq!(
        get(&state, None).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&state, Some("token")).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "no index"
    );
    let service = SearchService::open(&temp_dir("route"), None);
    put(
        &service,
        session("s", 1),
        vec![row("r", "user", "billing", 1)],
        Mode::Replace,
        "w",
    )
    .await;
    state.search = Arc::new(service);
    let response = get(&state, Some("token")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;
    let item = &page["data"]["items"][0];
    assert_eq!(item["asid"], "s");
    assert_eq!(item["agent_id"], "codex");
    assert_eq!(item["highlights"], json!([[0, 7]]));
    assert_eq!(page["data"]["next_cursor"], Value::Null);
    assert_eq!(page["data"]["indexing"], json!({"done": 0, "total": 0}));
}
