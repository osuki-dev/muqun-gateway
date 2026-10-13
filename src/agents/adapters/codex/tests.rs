//! Synthetic golden payloads follow the published pinned schemas. No local
//! Codex, user account, credentials, terminal, database or network is used.
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::sync::{mpsc, Mutex};

use super::*;
use crate::agents::domain::*;
use crate::agents::ports::agent::{AgentError, AgentPort};

pub(crate) fn golden() -> Value {
    serde_json::from_str(include_str!("fixtures/protocol.json")).unwrap()
}

#[test]
fn user_images_survive_native_item_mapping_without_becoming_prompt_text() {
    // Published v0.162.1 UserInput variants. The data URI is a synthetic
    // one-pixel PNG; no real user transcript or local image is retained.
    let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6qlsAAAAASUVORK5CYII=";
    let rows = mapper::item(
        "thread",
        "turn",
        &json!({"id":"user","type":"userMessage","content":[
            {"type":"text","text":"Inspect this icon","text_elements":[]},
            {"type":"image","url":image},
            {"type":"localImage","path":"/synthetic/clipboard.png"},
            {"type":"image","url":"https://example.test/reference.png"},
            {"type":"image","fileId":"unsupported-file-reference"},
            {"type":"skill","name":"example","path":"/synthetic/SKILL.md"}
        ]}),
        10,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role, TimelineRole::User);
    assert!(matches!(&rows[0].part, AgentPart::Text { text } if text == "Inspect this icon"));
    assert_eq!(
        rows[0].attachments.as_deref(),
        Some(
            [
                image.to_owned(),
                "/synthetic/clipboard.png".into(),
                "https://example.test/reference.png".into(),
            ]
            .as_slice()
        )
    );
}

#[test]
fn image_only_native_user_items_keep_their_attachments() {
    let rows = mapper::item(
        "thread",
        "turn",
        &json!({
            "id":"user", "type":"userMessage", "content":[
                {"type":"image","url":"data:image/png;base64,synthetic"}
            ]
        }),
        10,
    );
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0].part, AgentPart::Text { text } if text.is_empty()));
    assert_eq!(rows[0].attachments.as_ref().unwrap().len(), 1);
    let rows = mapper::item(
        "thread",
        "turn",
        &json!({
            "id":"user", "type":"userMessage", "content":[
                {"type":"text","text":"No image","text_elements":[]}
            ]
        }),
        10,
    );
    assert!(rows[0].attachments.is_none());
}

pub(crate) struct Fixture {
    pub client: Arc<CodexClient>,
    pub events: Option<mpsc::Receiver<CodexEvent>>,
    pub requests: mpsc::Receiver<Value>,
    writer: Arc<Mutex<WriteHalf<tokio::io::DuplexStream>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    pub(crate) async fn new(handler: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Self {
        Self::new_reply(move |r| Ok(handler(r))).await
    }

    async fn new_reply(
        handler: impl Fn(&Value) -> Result<Value, Value> + Send + Sync + 'static,
    ) -> Self {
        Self::new_reply_in_cwd(handler, None).await
    }

    async fn new_in_cwd(
        handler: impl Fn(&Value) -> Value + Send + Sync + 'static,
        cwd: &str,
    ) -> Self {
        Self::new_reply_in_cwd(move |r| Ok(handler(r)), Some(cwd.into())).await
    }

    async fn new_reply_in_cwd(
        handler: impl Fn(&Value) -> Result<Value, Value> + Send + Sync + 'static,
        cwd: Option<String>,
    ) -> Self {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client_io);
        let (client, events) = match cwd {
            Some(cwd) => CodexClient::from_io_at(read, write, cwd),
            None => CodexClient::from_io(read, write),
        };
        let (read, write) = tokio::io::split(server_io);
        let writer = Arc::new(Mutex::new(write));
        let server_writer = writer.clone();
        let (tx, requests) = mpsc::channel(512);
        let task = tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                tx.send(request.clone()).await.unwrap();
                let Some(method) = request["method"].as_str() else {
                    continue;
                };
                if request.get("id").is_none() {
                    continue;
                }
                let result = match method {
                    "initialize" => Ok(golden()["initialize"].clone()),
                    "account/read" => Ok(golden()["account"].clone()),
                    _ => handler(&request),
                };
                let response = match result {
                    Ok(result) => json!({"id":request["id"],"result":result}),
                    Err(error) => json!({"id":request["id"],"error":error}),
                };
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                if server_writer.lock().await.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        Self {
            client,
            events: Some(events),
            requests,
            writer,
            task,
        }
    }

    pub(crate) async fn send(&self, value: Value) {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        self.writer.lock().await.write_all(&bytes).await.unwrap();
    }

    pub(crate) async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }

    pub(crate) async fn ready(&mut self) {
        self.client.initialize().await.unwrap();
        let initialize = self.next().await;
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(
            initialize["params"]["capabilities"]["experimentalApi"],
            false
        );
        assert_eq!(
            initialize["params"]["capabilities"]["explicitGatewayOauth"],
            true
        );
        assert_eq!(self.next().await["method"], "initialized");
        let account = self.next().await;
        assert_eq!(account["method"], "account/read");
        assert_eq!(account["params"]["refreshToken"], false);
        if self.client.config_cwd().is_some() {
            let selection = self.next().await;
            assert_eq!(selection["method"], "config/read");
            assert_eq!(selection["params"]["includeLayers"], false);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.client.stop();
        self.task.abort();
    }
}

#[tokio::test]
async fn codex_history_follows_server_100_cap_for_mapped_tail_and_stable_chronological_ids() {
    for unmapped in [false, true] {
        let mut fixture=Fixture::new(move |r|match r["method"].as_str().unwrap() {
            "thread/read"=>json!({"thread":golden()["thread"]}),
            "thread/items/list"=>{
                let offset=r["params"]["cursor"].as_str().map(|s|s.strip_prefix("synthetic:").unwrap().parse::<usize>().unwrap()).unwrap_or(0);
                let total=if unmapped {403} else {201};
                let requested=r["params"]["limit"].as_u64().unwrap() as usize;
                let end=(offset+requested.min(100)).min(total);
                let rows:Vec<_>=(offset..end).map(|index|{
                    let position=total-1-index;
                    let kind=if unmapped && position%2==0 {"syntheticUnmappedItem"} else {"agentMessage"};
                    json!({"turnId":"synthetic-turn","item":{"type":kind,"id":format!("synthetic-{position}"),"text":format!("text-{position}")},"startedAtMs":0})
                }).collect();
                json!({"data":rows,"nextCursor":if end<total {json!(format!("synthetic:{end}"))} else {Value::Null},"backwardsCursor":null})
            },_=>panic!("unexpected mutation during history read"),
        }).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let id = golden()["thread"]["id"].as_str().unwrap().to_owned();
        let tail = driver.get_timeline(&id, 201).await.unwrap();
        assert_eq!(tail.len(), 201);
        for (index, row) in tail.iter().enumerate() {
            let position = if unmapped { index * 2 + 1 } else { index };
            assert!(
                matches!(&row.part,AgentPart::Text {text} if text==&format!("text-{position}"))
            );
            let expected = mapper::item(
                &id,
                "synthetic-turn",
                &json!({"type":"agentMessage","id":format!("synthetic-{position}"),"text":format!("text-{position}")}),
                0,
            );
            assert_eq!(row.id, expected[0].id);
        }
        let small = driver.get_timeline(&id, 1).await.unwrap();
        assert_eq!(small[0].id, tail.last().unwrap().id);
        let all = driver.get_timeline(&id, 250).await.unwrap();
        assert_eq!(all.len(), 201, "verified end may have fewer mapped rows");
        let requests: Vec<_> = std::iter::from_fn(|| fixture.requests.try_recv().ok()).collect();
        assert!(
            requests
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .count()
                >= 7
        );
        assert!(requests
            .iter()
            .all(|r| r["method"] != "thread/items/list" || r["params"]["sortDirection"] == "desc"));
    }
}

#[tokio::test]
async fn codex_history_unsatisfied_tail_errors_on_budget_cycles_malformed_pages_and_rpc_failure() {
    for mode in ["budget", "cycle", "malformed", "rpc"] {
        let mut fixture=Fixture::new_reply(move |r| {
            if r["method"]=="thread/read" {return Ok(json!({"thread":golden()["thread"]}));}
            assert_eq!(r["method"],"thread/items/list");
            let page=r["params"]["cursor"].as_str().map(|s|s.parse::<usize>().unwrap()).unwrap_or(0);
            if page>0 && mode=="rpc" {return Err(json!({"code":-32000,"message":"synthetic private diagnostic"}));}
            if page>0 && mode=="malformed" {return Ok(json!({"data":"not an array","nextCursor":null}));}
            Ok(json!({"data":[{"turnId":"synthetic-turn","item":{"type":"syntheticUnmappedItem","id":format!("synthetic-{page}")}}],"nextCursor":if mode=="cycle" {"1".into()} else {format!("{}",page+1)}}))
        }).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let id = golden()["thread"]["id"].as_str().unwrap().to_owned();
        let error = driver.get_timeline(&id, 1).await.unwrap_err().to_string();
        assert!(!error.contains("private diagnostic"));
        if mode == "budget" {
            assert!(error.contains("budget"));
        }
        if mode == "cycle" {
            assert!(error.contains("cycle"));
        }
    }
}

#[tokio::test]
async fn codex_project_inventory_reaches_second_page_and_legacy_list_stops_only_at_true_limit() {
    let mut fixture=Fixture::new(|r| {
        assert_eq!(r["method"],"thread/list");
        let offset=r["params"]["cursor"].as_str().map(|s|s.parse::<usize>().unwrap()).unwrap_or(0);
        let end=(offset+r["params"]["limit"].as_u64().unwrap() as usize).min(101).min(offset+100);
        let rows:Vec<_>=(offset..end).map(|index| {let mut thread=golden()["thread"].clone();thread["id"]=json!(format!("synthetic-{index}"));thread["cwd"]=json!(if index<100 {"/synthetic/first-page"} else {"/synthetic/only-second-page"});thread}).collect();
        json!({"data":rows,"nextCursor":if end<101 {json!(end.to_string())} else {Value::Null},"backwardsCursor":null})
    }).await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    let projects = driver.list_projects().await.unwrap();
    assert_eq!(projects.len(), 2);
    assert!(projects
        .iter()
        .any(|p| p.canonical == "/synthetic/only-second-page"));
    assert_eq!(fixture.next().await["params"]["limit"], 100);
    assert_eq!(fixture.next().await["params"]["cursor"], "100");
    let limited = driver
        .list_sessions(&SessionQuery {
            limit: Some(50),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(limited.len(), 50);
    assert_eq!(fixture.next().await["params"]["limit"], 50);
    assert!(fixture.requests.try_recv().is_err());
}

#[tokio::test]
async fn codex_project_inventory_never_returns_partial_on_budget_cycle_malformed_or_page_error() {
    for mode in ["budget", "cycle", "malformed", "rpc"] {
        let mut fixture=Fixture::new_reply(move |r| {
            assert_eq!(r["method"],"thread/list");
            let page=r["params"]["cursor"].as_str().map(|s|s.parse::<usize>().unwrap()).unwrap_or(0);
            if page>0 && mode=="rpc" {return Err(json!({"code":-32000,"message":"synthetic page error"}));}
            if page>0 && mode=="malformed" {return Ok(json!({"data":[],"nextCursor":123,"backwardsCursor":null}));}
            let mut thread=golden()["thread"].clone();thread["id"]=json!(format!("synthetic-{page}"));
            Ok(json!({"data":[thread],"nextCursor":if mode=="cycle" {"1".into()} else {format!("{}",page+1)},"backwardsCursor":null}))
        }).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let error = driver.list_projects().await.unwrap_err().to_string();
        if mode == "budget" {
            assert!(error.contains("budget"));
        }
        if mode == "cycle" {
            assert!(error.contains("cycle"));
        }
    }
}

#[tokio::test]
async fn codex_handshake_gates_calls_and_routes_concurrent_replies() {
    let mut fixture = Fixture::new(|r| json!({"echo":r["params"]})).await;
    assert!(matches!(
        fixture.client.call("thread/list", json!({})).await,
        Err(AgentError::NotAvailable(_))
    ));
    assert!(fixture.requests.try_recv().is_err());
    fixture.ready().await;
    let (a, b) = tokio::join!(
        fixture.client.call("read/a", json!({"thread":"a"})),
        fixture.client.call("read/b", json!({"thread":"b"}))
    );
    assert_eq!(a.unwrap()["echo"]["thread"], "a");
    assert_eq!(b.unwrap()["echo"]["thread"], "b");
    fixture.client.stop();
    assert!(!fixture.client.is_ready());
}

#[tokio::test]
async fn codex_new_thread_first_and_followup_turn_keep_exact_thread_ids() {
    let next_turn = std::sync::atomic::AtomicUsize::new(2);
    let mut fixture = Fixture::new(move |r|match r["method"].as_str().unwrap() {
        "config/read" => json!({"config":{"model_provider":"openai"}}),
        "model/list" => {let mut catalog=golden()["modelList"].clone();catalog["data"][0]["model"]=json!("requested-model");catalog},
        "thread/start" => json!({"thread":golden()["thread"],"model":"actual-model","modelProvider":"openai","reasoningEffort":"medium"}),
        "turn/start" => json!({"turn":{"id":format!("019a0000-0000-7000-8000-{:012}",next_turn.fetch_add(1,std::sync::atomic::Ordering::Relaxed)),"status":"inProgress"}}),
        "thread/read" => {
            assert_eq!(r["params"],json!({"threadId":golden()["thread"]["id"],"includeTurns":false}));
            assert!(next_turn.load(std::sync::atomic::Ordering::Relaxed) >= 3,"metadata read follows turn acceptance");
            let mut thread=golden()["thread"].clone();
            thread["model"]=json!("actual-model");thread["reasoningEffort"]=json!("medium");thread["status"]=json!({"type":"active"});
            json!({"thread":thread})
        }
        _ => panic!("unexpected RPC"),
    }).await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    let model = ModelRef {
        provider_id: "openai".into(),
        model_id: "requested-model".into(),
        variant: Some("high".into()),
    };
    let session = driver
        .create_session(None, Some(&model), None)
        .await
        .unwrap();
    assert_eq!(session.model.as_ref().unwrap().model_id, "actual-model");
    assert_eq!(
        session.model.as_ref().unwrap().variant.as_deref(),
        Some("medium")
    );
    assert_eq!(fixture.next().await["method"], "config/read");
    assert_eq!(fixture.next().await["method"], "model/list");
    let start = fixture.next().await;
    assert_eq!(start["method"], "thread/start");
    assert!(start["params"].get("approvalPolicy").is_none());
    assert!(start["params"].get("sandbox").is_none());
    let created = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.events.as_mut().unwrap().recv(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(created,CodexEvent::Snapshot {thread,revision} if revision > 0 && thread["id"] == session.asid.0 && thread["model"] == "actual-model" && thread["reasoningEffort"] == "medium")
    );
    for (index, text) in ["first synthetic prompt", "followup synthetic prompt"]
        .into_iter()
        .enumerate()
    {
        driver
            .send_prompt(&session.asid.0, text, &[], None)
            .await
            .unwrap();
        let turn = fixture.next().await;
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(turn["params"]["threadId"], session.asid.0);
        assert_eq!(turn["params"]["input"][0]["text"], text);
        assert!(turn["params"].get("model").is_none());
        assert!(turn["params"].get("effort").is_none());
        let metadata = fixture.next().await;
        assert_eq!(metadata["method"], "thread/read");
        assert_eq!(
            metadata["params"],
            json!({"threadId":session.asid.0,"includeTurns":false})
        );
        let refreshed = tokio::time::timeout(
            Duration::from_secs(2),
            fixture.events.as_mut().unwrap().recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(refreshed,CodexEvent::Snapshot {thread,revision} if revision > 0 && thread["id"] == session.asid.0 && thread["model"] == "actual-model" && thread["modelProvider"] == "openai" && thread["reasoningEffort"] == "medium" && thread["status"]["type"] == "active")
        );
        assert!(driver
            .send_prompt(&session.asid.0, "must not steer an active turn", &[], None)
            .await
            .is_err());
        fixture.send(json!({"method":"turn/completed","params":{"threadId":session.asid.0,"turn":{"id":format!("019a0000-0000-7000-8000-{:012}",index+2),"status":"completed"}}})).await;
        let completed = tokio::time::timeout(
            Duration::from_secs(2),
            fixture.events.as_mut().unwrap().recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(completed,CodexEvent::Notification {method,params} if method == "turn/completed" && params["threadId"] == session.asid.0 && params["turn"]["id"] == format!("019a0000-0000-7000-8000-{:012}",index+2) && params["turn"]["status"] == "completed")
        );
        assert!(
            fixture.requests.try_recv().is_err(),
            "exactly one metadata read per accepted turn; no resume or extra submission"
        );
    }
}

#[tokio::test]
async fn codex_session_pages_preserve_cursors_order_and_do_not_resume() {
    let mut fixture = Fixture::new(|r| {
        assert_eq!(r["method"],"thread/list");
        json!({"data":[golden()["thread"].clone()],"nextCursor":"synthetic-next","backwardsCursor":"synthetic-back"})
    }).await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    let query = SessionQuery {
        cursor: Some("opaque-synthetic".into()),
        order: Some("asc".into()),
        limit: Some(1),
        ..Default::default()
    };
    let page = driver.list_sessions_page(&query).await.unwrap();
    assert_eq!(page.cursor.next.as_deref(), Some("synthetic-next"));
    assert_eq!(page.cursor.previous, None);
    let request = fixture.next().await;
    assert_eq!(request["params"]["cursor"], "opaque-synthetic");
    assert_eq!(request["params"]["sortDirection"], "asc");
    assert_eq!(request["params"]["useStateDbOnly"], true);
    assert!(driver
        .list_sessions_page(&SessionQuery {
            cursor: Some("\n".into()),
            ..Default::default()
        })
        .await
        .is_err());
    assert!(fixture.requests.try_recv().is_err());
}

#[tokio::test]
async fn codex_both_history_storage_modes_use_stable_live_item_ids() {
    for mode in ["legacy", "paginated"] {
        let mut thread = golden()["thread"].clone();
        thread["historyMode"] = json!(mode);
        let id = thread["id"].as_str().unwrap().to_string();
        let item = json!({"type":"agentMessage","id":"019a0000-0000-7000-8000-000000000004","text":"full synthetic text"});
        let turn = "019a0000-0000-7000-8000-000000000003";
        let item_clone = item.clone();
        let mut fixture = Fixture::new(move |r|match r["method"].as_str().unwrap() {
            "thread/read" => {
                let mut thread = thread.clone();
                if r["params"]["includeTurns"] == true {thread["turns"] = json!([{"id":turn,"items":[item_clone.clone()]}]);}
                json!({"thread":thread})
            }
            "thread/items/list" => json!({"data":[{"turnId":turn,"item":item_clone}],"nextCursor":null,"backwardsCursor":null}),
            _ => panic!("unexpected history RPC"),
        }).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let rows = driver.get_timeline(&id, 10).await.unwrap();
        assert_eq!(rows[0].id, mapper::item(&id, turn, &item, 0)[0].id);
        assert!(driver.get_timeline(&id, 0).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn codex_approval_decisions_are_scoped_exactly_and_expire() {
    let mut fixture = Fixture::new(|_| json!({})).await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    let params = json!({"threadId":"synthetic-a","turnId":"synthetic-turn","itemId":"synthetic-tool","command":"echo synthetic","startedAtMs":100});
    fixture
        .send(json!({"id":7,"method":"item/commandExecution/requestApproval","params":params}))
        .await;
    fixture.events.as_mut().unwrap().recv().await.unwrap();
    let pending = driver.get_pending_permissions("synthetic-a").await.unwrap();
    assert_eq!(pending.len(), 1);
    let id = pending[0].id.clone();
    assert_eq!(Some(id.clone()), fixture.client.request_id(&json!(7)));
    assert!(!pending[0]
        .options
        .iter()
        .any(|o| o.decision == PermissionDecision::AllowAlways));
    assert!(driver
        .reply_permission("synthetic-b", &id, PermissionDecision::Allow, None)
        .await
        .is_err());
    assert!(matches!(
        driver
            .reply_permission("synthetic-a", &id, PermissionDecision::AllowAlways, None)
            .await,
        Err(AgentError::Unsupported(_))
    ));
    driver
        .reply_permission("synthetic-a", &id, PermissionDecision::Deny, None)
        .await
        .unwrap();
    let reply = fixture.next().await;
    assert_eq!(reply, json!({"id":7,"result":{"decision":"decline"}}));
    assert!(driver
        .reply_permission("synthetic-a", &id, PermissionDecision::Allow, None)
        .await
        .is_err());
    fixture.send(json!({"id":"synthetic-r","method":"item/permissions/requestApproval","params":{"threadId":"synthetic-a","turnId":"synthetic-turn","itemId":"synthetic-tool","permissions":{"network":{"enabled":true},"fileSystem":null}}})).await;
    // A local resolution event precedes this new request.
    loop {
        if matches!(
            fixture.events.as_mut().unwrap().recv().await.unwrap(),
            CodexEvent::Request { .. }
        ) {
            break;
        }
    }
    let id = driver.get_pending_permissions("synthetic-a").await.unwrap()[0]
        .id
        .clone();
    driver
        .reply_permission("synthetic-a", &id, PermissionDecision::Allow, None)
        .await
        .unwrap();
    let reply = fixture.next().await;
    assert_eq!(
        reply["result"],
        json!({"permissions":{"network":{"enabled":true}},"scope":"turn"})
    );
}

#[tokio::test]
async fn codex_questions_validate_options_and_keep_original_rpc_id() {
    let mut fixture = Fixture::new(|_| json!({})).await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    fixture.send(json!({"id":"synthetic-question","method":"item/tool/requestUserInput","params":{"threadId":"synthetic-a","turnId":"synthetic-turn","itemId":"synthetic-tool","questions":[{"id":"q","header":"Choice","question":"Which?","isOther":false,"isSecret":false,"options":[{"label":"A","description":"First"}]}]}})).await;
    fixture.events.as_mut().unwrap().recv().await.unwrap();
    let form = driver
        .get_pending_forms("synthetic-a")
        .await
        .unwrap()
        .remove(0);
    assert!(driver
        .reply_form("synthetic-a", &form.id, json!({"q":"not-offered"}))
        .await
        .is_err());
    driver
        .reply_form("synthetic-a", &form.id, json!({"q":"A"}))
        .await
        .unwrap();
    assert_eq!(
        fixture.next().await,
        json!({"id":"synthetic-question","result":{"answers":{"q":{"answers":["A"]}}}})
    );
}

#[tokio::test]
async fn codex_interrupt_uses_observed_turn_and_not_child_termination() {
    let mut fixture = Fixture::new(|_| json!({})).await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    assert!(driver.interrupt("synthetic-a").await.is_err());
    fixture.send(json!({"method":"turn/started","params":{"threadId":"synthetic-a","turn":{"id":"synthetic-turn"}}})).await;
    fixture.events.as_mut().unwrap().recv().await.unwrap();
    driver.interrupt("synthetic-a").await.unwrap();
    assert_eq!(
        fixture.next().await["params"],
        json!({"threadId":"synthetic-a","turnId":"synthetic-turn"})
    );
    assert!(fixture.client.is_ready());
    assert_eq!(
        fixture.client.active_turn("synthetic-a").as_deref(),
        Some("synthetic-turn")
    );
    fixture.send(json!({"method":"turn/completed","params":{"threadId":"synthetic-a","turn":{"id":"synthetic-turn","status":"interrupted"}}})).await;
    fixture.events.as_mut().unwrap().recv().await.unwrap();
    assert!(fixture.client.active_turn("synthetic-a").is_none());
}

#[tokio::test]
async fn codex_catalog_is_dynamic_and_optional_features_fail_closed() {
    let mut fixture = Fixture::new(|r| {
        if r["method"] == "config/read" {
            json!({"config":{"model_provider":"openai"}})
        } else {
            golden()["modelList"].clone()
        }
    })
    .await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    let catalog = driver.get_catalog(None).await.unwrap();
    assert_eq!(catalog.models[0].variants.as_ref().unwrap()[0].id, "high");
    assert_eq!(catalog.defaults.model.unwrap().model_id, "fixture-model");
    fixture.next().await;
    fixture.next().await;
    assert!(matches!(
        driver
            .revert_session("synthetic-a", "synthetic-message")
            .await,
        Err(AgentError::Unsupported(_))
    ));
    assert!(driver
        .stage_revert("synthetic-a", "synthetic-message", None)
        .await
        .is_err());
    assert!(driver.list_worktrees(None).await.is_err());
    assert!(driver.switch_mode("synthetic-a", "plan").await.is_err());
    assert!(fixture.requests.try_recv().is_err());
}

#[tokio::test]
async fn codex_read_only_selection_does_not_resume_but_submission_and_fork_are_explicit() {
    let thread_id = golden()["thread"]["id"].as_str().unwrap().to_string();
    let original = thread_id.clone();
    let accepted = std::sync::atomic::AtomicBool::new(false);
    let mut fixture = Fixture::new(move |r|match r["method"].as_str().unwrap() {
        "thread/read" => {
            assert_eq!(r["params"],json!({"threadId":original,"includeTurns":false}));
            let mut thread=golden()["thread"].clone();
            if accepted.load(std::sync::atomic::Ordering::Acquire) {
                thread["model"]=json!("actual-resumed");thread["reasoningEffort"]=json!("low");thread["status"]=json!({"type":"active"});
            }
            json!({"thread":thread})
        },
        "thread/resume" => json!({"thread":golden()["thread"],"model":"actual-resumed","modelProvider":"openai","reasoningEffort":"low"}),
        "turn/start" => {accepted.store(true,std::sync::atomic::Ordering::Release);json!({"turn":{"id":"synthetic-turn","status":"inProgress"}})},
        "thread/fork" => {
            let mut thread = golden()["thread"].clone();thread["id"] = json!("synthetic-fork");thread["forkedFromId"] = json!(original);
            json!({"thread":thread,"model":"actual-fork","modelProvider":"openai","reasoningEffort":"medium"})
        }
        "thread/name/set" | "thread/delete" => json!({}),
        _ => panic!("unexpected RPC"),
    }).await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    let saved = driver.get_session(&thread_id).await.unwrap();
    assert_eq!(saved.status, AgentSessionStatus::Idle);
    assert_eq!(saved.model.unwrap().model_id, "fixture-model");
    let read = fixture.next().await;
    assert_eq!(read["method"], "thread/read");
    assert_eq!(
        read["params"],
        json!({"threadId":thread_id,"includeTurns":false})
    );
    assert!(fixture.requests.try_recv().is_err());
    driver
        .send_prompt(&thread_id, "explicit selected-thread prompt", &[], None)
        .await
        .unwrap();
    let before_resume = fixture.next().await;
    assert_eq!(before_resume["method"], "thread/read");
    assert_eq!(
        before_resume["params"],
        json!({"threadId":thread_id,"includeTurns":false})
    );
    let resume = fixture.next().await;
    assert_eq!(resume["method"], "thread/resume");
    assert_eq!(resume["params"]["threadId"], thread_id);
    let turn = fixture.next().await;
    assert_eq!(turn["method"], "turn/start");
    assert_eq!(turn["params"]["threadId"], thread_id);
    assert_eq!(
        turn["params"]["input"][0]["text"],
        "explicit selected-thread prompt"
    );
    // The asynchronous read has its own causal boundary. Await its actual
    // source snapshot before unrelated fork/rename RPCs, not a global ordering
    // assumption between background reads and those mutations.
    let metadata = fixture.next().await;
    assert_eq!(metadata["method"], "thread/read");
    assert_eq!(
        metadata["params"],
        json!({"threadId":thread_id,"includeTurns":false})
    );
    let resumed = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.events.as_mut().unwrap().recv(),
    )
    .await
    .unwrap()
    .unwrap();
    let resume_revision = match resumed {
        CodexEvent::Snapshot { thread, revision } => {
            assert_eq!(thread["id"], thread_id);
            assert_eq!(thread["model"], "actual-resumed");
            assert_eq!(thread["reasoningEffort"], "low");
            revision
        }
        other => panic!("expected explicit resume snapshot, got {other:?}"),
    };
    let refreshed = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.events.as_mut().unwrap().recv(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(refreshed,CodexEvent::Snapshot {thread,revision} if revision > resume_revision && thread["id"] == thread_id && thread["model"] == "actual-resumed" && thread["modelProvider"] == "openai" && thread["reasoningEffort"] == "low" && thread["status"]["type"] == "active")
    );
    assert!(
        fixture.requests.try_recv().is_err(),
        "one accepted-turn metadata read and no automatic extra resume/submission"
    );
    let fork = driver.fork_session(&thread_id, None).await.unwrap();
    assert_eq!(fork.fork.as_ref().unwrap().session_id, thread_id);
    assert_eq!(fork.model.as_ref().unwrap().model_id, "actual-fork");
    assert_eq!(fixture.next().await["method"], "thread/fork");
    driver
        .rename_session(&fork.asid.0, "Synthetic renamed")
        .await
        .unwrap();
    let rename = fixture.next().await;
    assert_eq!(rename["method"], "thread/name/set");
    assert_eq!(
        rename["params"],
        json!({"threadId":"synthetic-fork","name":"Synthetic renamed"})
    );
    driver.delete_session(&fork.asid.0).await.unwrap();
    let delete = fixture.next().await;
    assert_eq!(delete["method"], "thread/delete");
    assert_eq!(delete["params"], json!({"threadId":"synthetic-fork"}));
    assert!(fixture.requests.try_recv().is_err());
}

#[tokio::test]
async fn codex_opaque_item_ids_keep_producer_history_order_after_clock_regression() {
    let mut fixture = Fixture::new(|r|match r["method"].as_str().unwrap() {
        "thread/read" => json!({"thread":golden()["thread"]}),
        "thread/items/list" => json!({"data":[
            {"turnId":"synthetic-turn","item":{"type":"agentMessage","id":"a-later","text":"second"},"startedAtMs":100},
            {"turnId":"synthetic-turn","item":{"type":"agentMessage","id":"z-earlier","text":"first"},"startedAtMs":200}
        ],"nextCursor":null,"backwardsCursor":null}),
        _ => panic!("unexpected RPC"),
    }).await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    let id = golden()["thread"]["id"].as_str().unwrap().to_string();
    let rows = driver.get_timeline(&id, 10).await.unwrap();
    assert!(matches!(&rows[0].part,AgentPart::Text{text} if text == "first"));
    assert!(matches!(&rows[1].part,AgentPart::Text{text} if text == "second"));
    fixture.next().await;
    fixture.next().await;
}

#[tokio::test]
async fn codex_loaded_switch_uses_real_next_turn_controls_and_configured_non_openai_provider() {
    let applied = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let state = applied.clone();
    let config_changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let changed = config_changed.clone();
    let mut fixture = Fixture::new_in_cwd(move |r| {
        let mut thread = golden()["thread"].clone();thread["modelProvider"] = json!("synthetic-oss");
        if state.load(std::sync::atomic::Ordering::Acquire) {thread["model"] = json!("synthetic-new");thread["reasoningEffort"] = json!("high");}
        match r["method"].as_str().unwrap() {
            "config/read" => {
                assert_eq!(r["params"]["cwd"],"/synthetic/catalog-context");
                assert_eq!(r["params"]["includeLayers"],false);
                json!({"config":{"model_provider":if changed.load(std::sync::atomic::Ordering::Acquire) {"synthetic-reconfigured-provider"} else {"synthetic-oss"},"model":"fixture-model","model_reasoning_effort":"medium","api_key":"synthetic-discarded-secret"},"layers":[{"private":"synthetic"}]})
            },
            "model/list" => {let mut catalog=golden()["modelList"].clone();let mut next=catalog["data"][0].clone();next["model"]=json!("synthetic-new");catalog["data"].as_array_mut().unwrap().push(next);catalog},
            "thread/start" => {assert_eq!(r["params"]["modelProvider"],"synthetic-oss");json!({"thread":thread,"model":"fixture-model","modelProvider":"synthetic-oss","reasoningEffort":"medium"})},
            "thread/read" => json!({"thread":thread}),
            // Faithful loaded/subscribed behavior: resume returns old selection.
            "thread/resume" => json!({"thread":thread,"model":"fixture-model","modelProvider":"synthetic-oss","reasoningEffort":"medium"}),
            "turn/start" => {
                assert_eq!(r["params"]["model"],"synthetic-new");assert_eq!(r["params"]["effort"],"high");
                assert!(r["params"].get("modelProvider").is_none());state.store(true,std::sync::atomic::Ordering::Release);
                json!({"turn":{"id":"synthetic-turn","status":"inProgress"}})
            },
            _ => panic!("unexpected RPC"),
        }
    },"/synthetic/catalog-context").await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    let catalog = driver.get_catalog(None).await.unwrap();
    assert_eq!(catalog.models[0].provider_id, "synthetic-oss");
    let initial = ModelRef {
        provider_id: catalog.models[0].provider_id.clone(),
        model_id: catalog.models[0].id.clone(),
        variant: None,
    };
    let session = driver
        .create_session(None, Some(&initial), None)
        .await
        .unwrap();
    let choice = ModelRef {
        provider_id: catalog.models[1].provider_id.clone(),
        model_id: catalog.models[1].id.clone(),
        variant: Some("high".into()),
    };
    driver.switch_model(&session.asid.0, &choice).await.unwrap();
    assert_eq!(
        driver
            .get_session(&session.asid.0)
            .await
            .unwrap()
            .model
            .unwrap()
            .model_id,
        "fixture-model",
        "no fake upstream/UI switch before actual turn"
    );
    driver
        .send_prompt(&session.asid.0, "synthetic next turn", &[], None)
        .await
        .unwrap();
    assert_eq!(
        driver
            .get_session(&session.asid.0)
            .await
            .unwrap()
            .model
            .unwrap(),
        choice
    );
    while let Ok(request) = fixture.requests.try_recv() {
        assert_ne!(request["method"], "thread/resume");
    }
    let config = fixture
        .client
        .call(
            "config/read",
            json!({"includeLayers":false,"cwd":"/synthetic/catalog-context"}),
        )
        .await
        .unwrap();
    assert!(config.get("layers").is_none());
    assert!(config["config"].get("api_key").is_none());
    config_changed.store(true, std::sync::atomic::Ordering::Release);
    let error = driver.get_catalog(None).await.unwrap_err().to_string();
    assert!(
        error.contains("restart Gateway"),
        "do not relabel an existing catalog after external provider config changes"
    );
}

#[tokio::test]
async fn codex_overlapping_history_tails_preserve_shared_order_anchors() {
    let mut fixture = Fixture::new(|r|match r["method"].as_str().unwrap() {
        "thread/read" => json!({"thread":golden()["thread"]}),
        "thread/items/list" => {
            let rows = [
                json!({"turnId":"synthetic-turn","item":{"type":"agentMessage","id":"a-later","text":"second"}}),
                json!({"turnId":"synthetic-turn","item":{"type":"agentMessage","id":"z-earlier","text":"first"}}),
            ];
            let limit = r["params"]["limit"].as_u64().unwrap() as usize;
            json!({"data":rows[..limit.min(2)],"nextCursor":null,"backwardsCursor":null})
        }
        _ => panic!("unexpected history RPC"),
    }).await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    let id = golden()["thread"]["id"].as_str().unwrap().to_string();
    let tail = driver.get_timeline(&id, 1).await.unwrap();
    let expanded = driver.get_timeline(&id, 2).await.unwrap();
    assert_eq!(tail[0].id, expanded[1].id);
    assert_eq!(tail[0].message_id, expanded[1].message_id);
    assert!(expanded[0].sort_key() < expanded[1].sort_key());
    let latest = driver.get_timeline(&id, 1).await.unwrap();
    assert_eq!(latest[0].message_id, tail[0].message_id);
}

#[tokio::test]
async fn codex_reasoning_expansion_limit_fails_without_silently_clipping_parts() {
    let mut fixture = Fixture::new(|_| json!({})).await;
    fixture.ready().await;
    let driver = CodexDriver::new(fixture.client.clone());
    let item = json!({"type":"reasoning","id":"synthetic-reasoning","summary":vec!["";MAX_REASONING_PARTS+1],"content":[]});
    assert!(matches!(
        driver.ordered_items("synthetic-thread", "synthetic-turn", &item, 0),
        Err(AgentError::Protocol(_))
    ));
    assert!(!fixture.client.is_ready());
}

#[tokio::test]
async fn codex_stale_approval_handle_cannot_answer_a_reused_rpc_id_after_restart() {
    let mut first = Fixture::new(|_| json!({})).await;
    first.ready().await;
    first.send(json!({"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"synthetic-a","turnId":"synthetic-first","itemId":"synthetic-first-tool"}})).await;
    first.events.as_mut().unwrap().recv().await.unwrap();
    let driver = CodexDriver::new(first.client.clone());
    let old = driver
        .get_pending_permissions("synthetic-a")
        .await
        .unwrap()
        .remove(0)
        .id;
    first.client.stop();
    let mut second = Fixture::new(|_| json!({})).await;
    second.ready().await;
    second.send(json!({"id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"synthetic-a","turnId":"synthetic-second","itemId":"synthetic-second-tool"}})).await;
    second.events.as_mut().unwrap().recv().await.unwrap();
    let driver = CodexDriver::new(second.client.clone());
    assert!(driver
        .reply_permission("synthetic-a", &old, PermissionDecision::Allow, None)
        .await
        .is_err());
    assert!(second.requests.try_recv().is_err());
    let current = driver
        .get_pending_permissions("synthetic-a")
        .await
        .unwrap()
        .remove(0)
        .id;
    assert_ne!(old, current);
    driver
        .reply_permission("synthetic-a", &current, PermissionDecision::Deny, None)
        .await
        .unwrap();
    assert_eq!(
        second.next().await,
        json!({"id":7,"result":{"decision":"decline"}})
    );
}

#[tokio::test]
async fn codex_terminal_runtime_blocks_controls_before_any_native_rpc() {
    use crate::agents::terminal_runtime::TerminalRuntime;
    let herdr = crate::test_support::FakeHerdr::start(vec![], None);
    let id = "00000000-0000-4000-8000-000000000001";
    herdr.set_agents(json!([{"pane_id":"w1:p1","terminal_id":"fixture-terminal","agent":"codex","agent_status":"blocked","shell_pid":42,"agent_session":{"kind":"id","agent":"codex","value":id}}]));
    let mut fixture = Fixture::new(|_| panic!("native RPC must not follow external binding")).await;
    fixture.ready().await;
    let mut driver = CodexDriver::new(fixture.client.clone());
    driver.observe_terminals(Some(TerminalRuntime::new(&[herdr.session()])));
    let runtime = driver.session_runtime(id).await.unwrap().unwrap();
    assert_eq!(runtime.source, RuntimeSource::Terminal);
    assert_eq!(runtime.status, Some(TerminalRuntimeStatus::Blocked));
    let model = ModelRef {
        provider_id: "openai".into(),
        model_id: "synthetic".into(),
        variant: None,
    };
    for result in [
        driver.send_prompt(id, "fixture", &[], None).await,
        driver.interrupt(id).await,
        driver.switch_model(id, &model).await,
        driver.delete_session(id).await,
    ] {
        assert!(matches!(result, Err(AgentError::TerminalInteraction(_))));
    }
    assert!(fixture.requests.try_recv().is_err());
    assert!(herdr
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|call| call["method"] == "agent.list"));
    // A fresh mutation check must not rely on the preceding cached view.
    herdr.set_agents(json!([]));
    assert_eq!(
        driver.runtime(id, true).await.unwrap().interaction,
        RuntimeInteraction::Native
    );
    assert_eq!(
        driver.runtime(id, false).await.unwrap().source,
        RuntimeSource::Unobserved
    );
}

#[tokio::test]
async fn codex_explicit_refresh_rereads_provider_without_resuming_or_replacing_cached_reads() {
    use crate::agents::adapters::memory_mirror::MemoryMirror;
    use crate::agents::use_cases::SessionService;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = reads.clone();
    let mut fixture = Fixture::new(move |request| match request["method"].as_str().unwrap() {
        "thread/read" => {
            counter.fetch_add(1, Ordering::Relaxed);
            json!({"thread":golden()["thread"]})
        }
        "thread/items/list" => json!({"data":[],"nextCursor":null,"backwardsCursor":null}),
        _ => panic!("refresh must never resume or send"),
    })
    .await;
    fixture.ready().await;
    let mut driver = CodexDriver::new(fixture.client.clone());
    driver.observe_terminals(Some(crate::agents::terminal_runtime::TerminalRuntime::new(
        &[],
    )));
    let service =
        SessionService::with_memory_mirror(Arc::new(driver), Arc::new(MemoryMirror::new()));
    let id = AgentSessionId(golden()["thread"]["id"].as_str().unwrap().into());
    let first = service.get_snapshot(&id).await.unwrap();
    let before = reads.load(Ordering::Relaxed);
    let cached = service.get_snapshot(&id).await.unwrap();
    assert_eq!(first.seq, cached.seq);
    assert_eq!(reads.load(Ordering::Relaxed), before);
    let fresh = service.refresh_snapshot(&id).await.unwrap();
    assert!(reads.load(Ordering::Relaxed) > before);
    assert!(fresh.seq > cached.seq);
    assert_eq!(
        fresh.info.runtime.unwrap().source,
        RuntimeSource::Unobserved
    );
    assert_eq!(fresh.info.status, AgentSessionStatus::Unknown);
}

#[tokio::test]
async fn codex_unload_invalidates_only_its_own_live_callbacks_and_turn() {
    for method in ["thread/closed", "thread/status/changed", "thread/deleted"] {
        let mut fixture = Fixture::new(|_| panic!("unload must not answer callbacks")).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        for id in ["synthetic-a", "synthetic-b"] {
            fixture.send(json!({"method":"turn/started","params":{"threadId":id,"turn":{"id":"synthetic-turn"}}})).await;
            fixture.events.as_mut().unwrap().recv().await.unwrap();
            fixture.send(json!({"id":id,"method":"item/tool/requestUserInput","params":{"threadId":id,"turnId":"synthetic-turn","itemId":"synthetic-tool","questions":[{"id":"q","header":"Choice","question":"Which?","isOther":false,"isSecret":false,"options":[{"label":"A","description":"First"}]}]}})).await;
            fixture.events.as_mut().unwrap().recv().await.unwrap();
        }
        let form = driver
            .get_pending_forms("synthetic-a")
            .await
            .unwrap()
            .remove(0);
        fixture.send(json!({"method":method,"params":{"threadId":"synthetic-a","status":{"type":"notLoaded"}}})).await;
        fixture.events.as_mut().unwrap().recv().await.unwrap();
        assert!(fixture.client.active_turn("synthetic-a").is_none());
        assert!(driver
            .get_pending_forms("synthetic-a")
            .await
            .unwrap()
            .is_empty());
        assert!(driver
            .reply_form("synthetic-a", &form.id, json!({"q":"A"}))
            .await
            .is_err());
        assert_eq!(
            driver.get_pending_forms("synthetic-b").await.unwrap().len(),
            1
        );
        assert!(fixture.client.active_turn("synthetic-b").is_some());
        assert!(fixture.requests.try_recv().is_err());
    }
}
