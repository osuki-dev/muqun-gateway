use super::*;
use crate::agents::adapters::t3::client::ModelSelection;
use crate::agents::domain::{AgentPart, PermissionDecision};
use serde_json::{json, Value};

pub fn fixture() -> Value {
    let mut snapshot: Value =
        serde_json::from_str(include_str!("../fixtures/v2/projection.json")).unwrap();
    snapshot["projection"]["providerSessions"][0]["capabilities"] = json!({"turns":{
        "supportsQueuedMessages":true,"supportsActiveSteering":true,"supportsSteeringByInterruptRestart":true
    }});
    snapshot
}

fn queued_snapshot() -> Value {
    let mut snapshot = fixture();
    snapshot["projection"]["runs"].as_array_mut().unwrap().push(json!({
        "id":"queued-run","threadId":"thread","ordinal":3,"status":"queued","queuePosition":1,"userMessageId":"queued-message"
    }));
    snapshot["projection"]["messages"].as_array_mut().unwrap().push(json!({
        "id":"queued-message","threadId":"thread","runId":"queued-run","role":"user","text":"later","attachments":[]
    }));
    snapshot
}

#[test]
fn steering_requires_the_active_root_provider_and_its_native_capability() {
    let snapshot = queued_snapshot();
    let p = &snapshot["projection"];
    assert_eq!(
        inbox::dispatch_mode(p, Some("steer")).unwrap(),
        json!({"type":"steer_active","targetRunId":"root-run"})
    );
    assert_eq!(inbox::items(p).unwrap()[0]["id"], "queued-run");
    assert_eq!(inbox::items(p).unwrap()[0]["payload"]["text"], "later");
    let mut no_queue = snapshot.clone();
    no_queue["projection"]["providerSessions"][0]["capabilities"]["turns"]
        ["supportsQueuedMessages"] = json!(false);
    assert!(matches!(
        inbox::dispatch_mode(&no_queue["projection"], Some("queue")),
        Err(AgentError::Unsupported(_))
    ));
    let mut idle = snapshot.clone();
    idle["projection"]["runs"][0]["status"] = json!("completed");
    assert_eq!(
        inbox::dispatch_mode(&idle["projection"], Some("queue")).unwrap()["type"],
        "start_immediately"
    );
    assert_eq!(
        inbox::dispatch_mode(&idle["projection"], Some("steer")).unwrap()["type"],
        "start_immediately"
    );
    for mutation in ["unsupported", "replaced-session", "non-root", "finished"] {
        let mut broken = snapshot.clone();
        match mutation {
            "unsupported" => {
                broken["projection"]["providerSessions"][0]["capabilities"]["turns"]
                    ["supportsActiveSteering"] = json!(false)
            }
            "replaced-session" => {
                broken["projection"]["providerThreads"][0]["providerSessionId"] = json!("other")
            }
            "non-root" => broken["projection"]["nodes"][0]["parentNodeId"] = json!("parent"),
            _ => broken["projection"]["runs"][0]["status"] = json!("completed"),
        }
        assert!(
            inbox::steer_target(&broken["projection"]).is_err(),
            "{mutation}"
        );
    }
    let mut finished = snapshot.clone();
    finished["projection"]["runs"][1]["status"] = json!("running");
    assert!(matches!(
        inbox::queued_run(&finished["projection"], "queued-run"),
        Err(AgentError::InboxItemNotFound(_))
    ));
}

#[tokio::test]
async fn native_queue_operations_keep_run_identity_and_never_restart() {
    let server = fake_server(Some(2)).await;
    *server.state.snapshot.lock().unwrap() = queued_snapshot();
    let client = client(&server);
    let driver = T3Driver::from_client(client.clone());
    assert_eq!(
        driver.get_inbox("thread").await.unwrap()[0]["id"],
        "queued-run"
    );
    driver
        .edit_inbox_item("thread", "queued-run", "changed")
        .await
        .unwrap();
    driver
        .set_inbox_delivery("thread", "queued-run", "steer")
        .await
        .unwrap();
    driver
        .cancel_inbox_item("thread", "queued-run")
        .await
        .unwrap();
    server.state.snapshot.lock().unwrap()["projection"]["runs"][0]["status"] = json!("completed");
    driver
        .set_inbox_delivery("thread", "queued-run", "steer")
        .await
        .unwrap();
    assert!(matches!(
        driver.cancel_inbox_item("thread", "gone").await,
        Err(AgentError::InboxItemNotFound(_))
    ));
    let seen = server.state.seen.lock().unwrap().clone();
    let commands: Vec<_> = seen
        .iter()
        .filter(|row| row["rpc"] == "orchestration.dispatchCommand")
        .map(|row| &row["payload"])
        .collect();
    assert_eq!(commands.len(), 4);
    assert_eq!(commands[0]["text"], "changed");
    assert!(
        commands[0].get("attachments").is_none(),
        "text edits retain upstream attachments"
    );
    assert_eq!(commands[1]["queuedRunId"], "queued-run");
    assert_eq!(commands[1]["targetRunId"], "root-run");
    assert_eq!(commands[2]["runId"], "queued-run");
    assert_eq!(commands[3]["type"], "queued-run.start");
    assert_eq!(commands[3]["runId"], "queued-run");
    client.shutdown();
}

#[test]
fn asynchronous_questions_are_nonblocking_in_forms_and_timeline() {
    let mut snapshot = fixture();
    let question = snapshot["projection"]["turnItems"][1].clone();
    snapshot["projection"]["visibleTurnItems"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "position":2,"visibility":"local","sourceThreadId":"thread",
            "sourceItemId":"form-item","item":question
        }));
    let projection = &snapshot["projection"];
    let forms = mapper::forms("thread", projection);
    assert_eq!(forms.len(), 1);
    assert!(forms[0].nonblocking);
    // The same projection must give identical interaction semantics in both views.
    let rows = mapper::timeline("thread", projection, 1);
    let form = rows
        .iter()
        .find_map(|row| match &row.part {
            AgentPart::Form { request } => Some(request),
            _ => None,
        })
        .expect("question timeline row");
    assert!(form.nonblocking);

    let mut live = snapshot.clone();
    live["projection"]["runtimeRequests"][1]["responseCapability"] =
        json!({"type":"live", "providerSessionId":"provider-session"});
    assert!(!mapper::forms("thread", &live["projection"])[0].nonblocking);
    live["projection"]["runtimeRequests"][1]["status"] = json!("resolved");
    assert!(mapper::forms("thread", &live["projection"]).is_empty());
}

pub(crate) fn local_window(ids: &[&str], sequence: u64) -> Value {
    let mut snapshot = fixture();
    let template = snapshot["projection"]["visibleTurnItems"][0].clone();
    snapshot["snapshotSequence"] = json!(sequence);
    snapshot["projection"]["visibleTurnItems"] = Value::Array(
        ids.iter()
            .enumerate()
            .map(|(position, id)| {
                let mut row = template.clone();
                row["position"] = json!(position);
                row["sourceThreadId"] = json!("thread");
                row["sourceItemId"] = json!(id);
                row["item"]["id"] = json!(id);
                row["item"]["text"] = json!(format!("body-{id}"));
                row
            })
            .collect(),
    );
    snapshot
}

#[tokio::test]
async fn complete_rollback_watermark_blocks_late_history_resurrection() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let generation = client.timeline_generation("thread").await.unwrap();
    client
        .order_snapshot_in_generation("thread", local_window(&["A", "B"], 10), &generation, true)
        .await
        .unwrap();
    let mut rollback = local_window(&["A"], 20);
    rollback["hasMoreHistory"] = json!(false);
    client
        .order_snapshot_in_generation("thread", rollback, &generation, true)
        .await
        .unwrap();
    let mut late = local_window(&["A", "B"], 10);
    late["hasMoreHistory"] = json!(false);
    let late = client
        .order_snapshot_in_generation("thread", late, &generation, false)
        .await
        .unwrap();
    assert_eq!(
        projection::rows(&late["projection"], "visibleTurnItems")
            .iter()
            .map(|row| row["sourceItemId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["A"]
    );
    assert_eq!(late["snapshotSequence"], 10);
    assert_eq!(
        late["gatewayCompleteCoverage"], false,
        "a stale historical end marker is not deletion authority"
    );
    client
        .order_snapshot_in_generation("thread", local_window(&["A", "C"], 21), &generation, true)
        .await
        .unwrap();
    assert!(
        client.order_live_snapshot("thread", late).await.is_err(),
        "stale control snapshots must not replace live state"
    );
    let current = client
        .order_snapshot_in_generation("thread", local_window(&["A", "C"], 21), &generation, true)
        .await
        .unwrap();
    assert_eq!(
        projection::rows(&current["projection"], "visibleTurnItems").len(),
        2
    );
    client.shutdown();
}

#[tokio::test]
async fn late_http_timeline_upsert_cannot_restore_a_remotely_rolled_back_row() {
    use crate::agents::adapters::memory_mirror::MemoryMirror;
    use crate::agents::ports::mirror::SessionMirrorPort;
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let driver = Arc::new(T3Driver::from_client(client.clone()));
    let asid = crate::agents::domain::AgentSessionId("thread".into());
    let mirror = MemoryMirror::for_agent("t3");
    let baseline = client
        .order_live_snapshot("thread", local_window(&["A", "B"], 10))
        .await
        .unwrap();
    let before = mapper::timeline("thread", &baseline["projection"], 10);
    let removed = before
        .iter()
        .find(|row| row.id.ends_with(":B"))
        .unwrap()
        .id
        .clone();
    mirror.upsert_timeline_items(&asid, before).await;
    let mut old_http = local_window(&["A", "B"], 10);
    old_http["hasMoreHistory"] = json!(false);
    old_http["holdBoundedResponse"] = json!(true);
    *server.state.snapshot.lock().unwrap() = old_http;
    let pending = tokio::spawn(async move { driver.get_timeline("thread", 0).await });
    tokio::time::timeout(
        Duration::from_secs(3),
        server.state.bounded_started.notified(),
    )
    .await
    .unwrap();
    let mut complete = local_window(&["A"], 20);
    complete["hasMoreHistory"] = json!(false);
    complete["projection"]["visibleTurnItems"][0]["item"]["text"] = json!("A after rollback");
    *server.state.snapshot.lock().unwrap() = complete.clone();
    let complete = client
        .order_live_snapshot("thread", complete)
        .await
        .unwrap();
    mirror.remove_timeline_items(&asid, &[removed]).await;
    mirror
        .upsert_timeline_items(
            &asid,
            mapper::timeline("thread", &complete["projection"], 20),
        )
        .await;
    server.state.bounded_release.notify_one();
    let late = pending.await.unwrap().unwrap();
    assert_eq!(late.len(), 1);
    // This is the same upsert-only port operation the session service performs.
    mirror.upsert_timeline_items(&asid, late).await;
    let held = mirror.get_snapshot(&asid).await.unwrap();
    assert_eq!(held.timeline.len(), 1);
    assert!(held.timeline[0].id.ends_with(":A"));
    assert!(matches!(&held.timeline[0].part,AgentPart::Text{text} if text=="A after rollback"));
    client.shutdown();
}

#[tokio::test]
async fn timeline_deletion_authority_is_thread_and_incarnation_local() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let old_generation = client.timeline_generation("thread").await.unwrap();
    let old = client
        .order_snapshot_in_generation(
            "thread",
            local_window(&["A", "B"], 10),
            &old_generation,
            true,
        )
        .await
        .unwrap();
    let mut complete = local_window(&["A"], 20);
    complete["hasMoreHistory"] = json!(false);
    client
        .order_snapshot_in_generation("thread", complete, &old_generation, true)
        .await
        .unwrap();
    let mut other = local_window(&["A", "B"], 1);
    other["projection"]["thread"]["id"] = json!("other");
    for row in other["projection"]["visibleTurnItems"]
        .as_array_mut()
        .unwrap()
    {
        row["sourceThreadId"] = json!("other");
        row["item"]["threadId"] = json!("other");
    }
    let other = client.order_live_snapshot("other", other).await.unwrap();
    assert_eq!(
        projection::rows(&other["projection"], "visibleTurnItems").len(),
        2,
        "another thread has no inherited deletion watermark"
    );
    client.forget_timeline("thread").await;
    let new_generation = client.timeline_generation("thread").await.unwrap();
    assert_ne!(old_generation, new_generation);
    assert!(
        client
            .order_snapshot_in_generation(
                "thread",
                local_window(&["A", "B"], 10),
                &old_generation,
                false
            )
            .await
            .is_err(),
        "an in-flight old-incarnation read must not repopulate a reset thread"
    );
    assert!(
        client.order_live_snapshot("thread", old).await.is_err(),
        "queued old-incarnation snapshots must also be rejected"
    );
    let mut fresh = local_window(&["A", "B"], 1);
    fresh["hasMoreHistory"] = json!(false);
    let fresh = client
        .order_snapshot_in_generation("thread", fresh, &new_generation, true)
        .await
        .unwrap();
    assert_eq!(
        projection::rows(&fresh["projection"], "visibleTurnItems").len(),
        2,
        "new incarnation does not inherit old tombstones or sequence floors"
    );
    client.shutdown();
    assert!(
        client
            .order_snapshot_in_generation(
                "thread",
                local_window(&["A", "B"], 2),
                &new_generation,
                false
            )
            .await
            .is_err(),
        "closed clients cannot deliver old reads into a replacement runtime"
    );
}

#[tokio::test]
async fn late_history_cannot_render_a_retained_resolved_request_as_clickable() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let mut old = fixture();
    old["hasMoreHistory"] = json!(false);
    old["projection"]["visibleTurnItems"] = json!([{"position":0,"visibility":"local","sourceThreadId":"thread","sourceItemId":"approval-item","item":old["projection"]["turnItems"][0]}]);
    client
        .order_live_snapshot("thread", old.clone())
        .await
        .unwrap();
    let mut resolved = old.clone();
    resolved["snapshotSequence"] = json!(20);
    resolved["projection"]["runtimeRequests"][0]["status"] = json!("resolved");
    resolved["projection"]["turnItems"][0]["status"] = json!("completed");
    resolved["projection"]["visibleTurnItems"][0]["item"]["status"] = json!("completed");
    client
        .order_live_snapshot("thread", resolved)
        .await
        .unwrap();
    let late = client.order_snapshot("thread", old).await.unwrap();
    assert!(mapper::timeline("thread",&late["projection"],10).is_empty(),"cached terminal request rows cannot borrow stale pending control state from a history response");
    client.shutdown();
}

#[tokio::test]
async fn page_local_positions_stitch_and_rolling_windows_keep_stable_keys() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let first = client
        .order_snapshot("thread", local_window(&["C", "D"], 10))
        .await
        .unwrap();
    let first_rows = mapper::timeline("thread", &first["projection"], 10);
    let mut history = local_window(&["C", "D"], 10);
    let older = local_window(&["A", "B"], 5);
    projection::merge_history(&mut history,&json!({"snapshotSequence":5,"items":older["projection"]["visibleTurnItems"],"nextCursor":null,"hasMoreHistory":false})).unwrap();
    assert_eq!(
        projection::rows(&history["projection"], "visibleTurnItems")
            .iter()
            .map(|row| row["sourceItemId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["A", "B", "C", "D"]
    );
    let history = client.order_snapshot("thread", history).await.unwrap();
    let all = mapper::timeline("thread", &history["projection"], 10);
    assert_eq!(all[2].message_id, first_rows[0].message_id);
    assert_eq!(all[3].message_id, first_rows[1].message_id);
    let rolling = client
        .order_snapshot("thread", local_window(&["D", "E"], 20))
        .await
        .unwrap();
    let tail = mapper::timeline("thread", &rolling["projection"], 20);
    assert_eq!(tail[0].message_id, all[3].message_id);
    let mut combined = all[..3].to_vec();
    combined.extend(tail);
    combined.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    assert_eq!(
        combined
            .iter()
            .map(|row| row.id.rsplit(':').next().unwrap())
            .collect::<Vec<_>>(),
        ["A", "B", "C", "D", "E"]
    );
    assert!(
        client
            .order_snapshot("thread", local_window(&["X", "Y"], 30))
            .await
            .is_err(),
        "no overlap is not evidence for appending arbitrary entities"
    );
    client.shutdown();
}

#[test]
fn opaque_id_text_and_timestamp_order_are_not_timeline_ordering_evidence() {
    let mut order = projection::TimelineOrder::default();
    let mut current = local_window(&["zeta", "alpha"], 10);
    for row in current["projection"]["visibleTurnItems"]
        .as_array_mut()
        .unwrap()
    {
        row["item"]["text"] = json!("identical text");
    }
    order.install(&mut current).unwrap();
    let mut historical = local_window(&["zeta", "alpha"], 10);
    let prefix = local_window(&["omega", "beta"], 1);
    projection::merge_history(&mut historical,&json!({"snapshotSequence":1,"items":prefix["projection"]["visibleTurnItems"],"nextCursor":null,"hasMoreHistory":false})).unwrap();
    order.install(&mut historical).unwrap();
    let mapped = mapper::timeline("thread", &historical["projection"], 10);
    assert_eq!(
        mapped
            .iter()
            .map(|row| row.id.rsplit(':').next().unwrap())
            .collect::<Vec<_>>(),
        ["omega", "beta", "zeta", "alpha"]
    );
}

#[tokio::test]
async fn history_finishing_after_live_refresh_keeps_live_payload_and_order() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let mut historical = local_window(&["C", "D"], 10);
    client
        .order_snapshot("thread", historical.clone())
        .await
        .unwrap();
    let mut live = local_window(&["D", "E"], 20);
    live["projection"]["visibleTurnItems"][0]["item"]["text"] = json!("new live D");
    let live = client.order_snapshot("thread", live).await.unwrap();
    let page = local_window(&["A", "B"], 1);
    projection::merge_history(&mut historical,&json!({"snapshotSequence":1,"items":page["projection"]["visibleTurnItems"],"nextCursor":null,"hasMoreHistory":false})).unwrap();
    let historical = client.order_snapshot("thread", historical).await.unwrap();
    assert_eq!(
        historical["snapshotSequence"], 10,
        "old history never installs a live cursor/control baseline"
    );
    let rows = mapper::timeline("thread", &historical["projection"], 10);
    let tail = mapper::timeline("thread", &live["projection"], 20);
    assert!(matches!(&rows[3].part,AgentPart::Text{text} if text=="new live D"));
    assert_eq!(rows[3].message_id, tail[0].message_id);
    assert!(rows[3].message_id < tail[1].message_id);
    client.shutdown();
}

#[tokio::test]
async fn history_work_budget_keeps_a_valid_partial_window_and_opaque_cursor() {
    for (mapped, end, limit, success) in [
        (false, None, 100, true),
        (true, None, 100, true),
        (true, None, 66, true),
        (true, Some(64), 100, true),
    ] {
        let server = fake_server(Some(2)).await;
        {
            let mut snapshot = server.state.snapshot.lock().unwrap();
            snapshot["historyCursor"] = json!("page:0");
            snapshot["historyFixture"] = json!({"mapped":mapped,"endAt":end});
        }
        let client = client(&server);
        let result = client.timeline_snapshot("thread", limit).await;
        assert_eq!(result.is_ok(), success);
        assert_eq!(
            server
                .state
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|row| row["http"] == "history")
                .count(),
            64
        );
        if let Ok(snapshot) = result {
            assert!(!mapper::timeline("thread", &snapshot["projection"], 0).is_empty());
            if end.is_none() {
                assert_eq!(snapshot["hasMoreHistory"], true);
                assert_eq!(snapshot["historyCursor"], "page:64");
            }
        }
        client.shutdown();
    }
}

#[tokio::test]
async fn inactive_thread_and_payload_pressure_do_not_refuse_the_next_session() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let first = client.timeline_generation("first").await.unwrap();
    let prepared = client.prepare_timeline_snapshot("thread", 1).await.unwrap();
    let pinned = prepared.detail["gatewayTimelineGeneration"]
        .as_str()
        .unwrap()
        .to_owned();
    for n in 0..129 {
        client
            .timeline_generation(&format!("thread-{n}"))
            .await
            .unwrap();
    }
    assert_eq!(
        client.timeline_generation("thread").await.unwrap(),
        pinned,
        "a prepared read pins its incarnation"
    );
    client.commit_timeline("thread", prepared, true).unwrap();
    assert_ne!(client.timeline_generation("first").await.unwrap(), first);
    for n in 0..4 {
        let mut detail = local_window(&["large"], n);
        detail["projection"]["visibleTurnItems"][0]["item"]["text"] =
            json!("x".repeat(9 * 1024 * 1024));
        client
            .order_snapshot(&format!("large-{n}"), detail)
            .await
            .unwrap();
    }
    client.shutdown();
}

#[tokio::test]
async fn discarded_history_preparations_preserve_pending_native_rows_and_cursor() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let driver = T3Driver::from_client(client.clone());
    drop(driver.prepare_timeline("thread", 1).await.unwrap());
    assert!(matches!(
        driver.load_earlier_timeline("thread", 1).await,
        Err(AgentError::HistoryCursorLost)
    ));
    let newest = driver.get_timeline("thread", 1).await.unwrap();
    assert_eq!(newest.len(), 1);
    let prepared = driver.prepare_earlier_timeline("thread", 1).await.unwrap();
    let older_id = prepared.items[0].id.clone();
    assert!(
        older_id.contains("parent:user-item"),
        "unserved rows of the bounded native page precede its cursor"
    );
    assert_ne!(older_id, newest[0].id);
    drop(prepared);
    let repeated = driver.prepare_earlier_timeline("thread", 1).await.unwrap();
    assert_eq!(
        repeated.items[0].id, older_id,
        "cancellation must not consume the native page tail"
    );
    repeated.commit.unwrap()().unwrap();
    let oldest = driver.load_earlier_timeline("thread", 1).await.unwrap();
    assert_eq!(
        oldest.len(),
        1,
        "upstream reads: {:?}",
        server.state.seen.lock().unwrap()
    );
    assert!(driver
        .load_earlier_timeline("thread", 1)
        .await
        .unwrap()
        .is_empty());
    let current = client
        .order_live_snapshot("thread", fixture())
        .await
        .unwrap();
    let mapped = mapper::timeline("thread", &current["projection"], 0);
    assert_eq!(
        mapped.last().unwrap().message_id,
        newest[0].message_id,
        "paging must not relabel the visible tail"
    );
    client.shutdown();
}

#[tokio::test]
async fn cancelled_reads_release_lru_pins_and_rollback_fences_old_http_cuts() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let generation = client.timeline_generation("thread").await.unwrap();
    server.state.snapshot.lock().unwrap()["holdBoundedResponse"] = json!(true);
    let reader = client.clone();
    let read = tokio::spawn(async move { reader.prepare_timeline_snapshot("thread", 1).await });
    server.state.bounded_started.notified().await;
    for n in 0..129 {
        client
            .timeline_generation(&format!("reading-{n}"))
            .await
            .unwrap();
    }
    read.abort();
    assert!(matches!(read.await, Err(error) if error.is_cancelled()));
    server.state.bounded_release.notify_waiters();
    for n in 0..129 {
        client
            .timeline_generation(&format!("released-{n}"))
            .await
            .unwrap();
    }
    assert_ne!(
        client.timeline_generation("thread").await.unwrap(),
        generation
    );
    let reader = client.clone();
    let late = tokio::spawn(async move { reader.prepare_timeline_snapshot("thread", 1).await });
    server.state.bounded_started.notified().await;
    client.require_timeline_refresh("thread", 20);
    server.state.bounded_release.notify_waiters();
    assert!(matches!(
        late.await.unwrap(),
        Err(AgentError::HistoryCursorLost)
    ));
    client.shutdown();
}

#[tokio::test]
async fn byte_work_budget_does_not_consume_the_page_that_did_not_fit() {
    let server = fake_server(Some(2)).await;
    {
        let mut snapshot = server.state.snapshot.lock().unwrap();
        snapshot["historyCursor"] = json!("page:0");
        snapshot["historyFixture"] = json!({"mapped":true,"endAt":4,"paddingBytes":4*1024*1024});
    }
    let client = client(&server);
    let prepared = client
        .prepare_timeline_snapshot("thread", 100)
        .await
        .unwrap();
    assert_eq!(prepared.detail["hasMoreHistory"], true);
    assert_eq!(prepared.detail["historyCursor"], "page:2");
    client.commit_timeline("thread", prepared, true).unwrap();
    let older = client
        .prepare_earlier_snapshot("thread", 1)
        .await
        .unwrap()
        .unwrap();
    assert!(older.detail["projection"]["visibleTurnItems"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["sourceItemId"] == "older-2"));
    client.shutdown();
}

#[test]
fn large_valid_pages_and_rolling_windows_keep_prefix_and_tail_order_anchors() {
    let ids: Vec<_> = (0..20_100).map(|n| format!("row-{n}")).collect();
    let refs: Vec<_> = ids.iter().map(String::as_str).collect();
    let mut detail = local_window(&["tail"], 1);
    let page = local_window(&refs, 1);
    projection::merge_history(&mut detail, &json!({"snapshotSequence":1,"items":page["projection"]["visibleTurnItems"],"nextCursor":"older","hasMoreHistory":true})).unwrap();
    let mut order = projection::TimelineOrder::default();
    order.install(&mut detail).unwrap();
    let prefix = detail["projection"]["visibleTurnItems"][0]["position"].clone();
    let tail = detail["projection"]["visibleTurnItems"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["position"]
        .clone();
    let mut live = local_window(&["tail", "new"], 2);
    order.install(&mut live).unwrap();
    assert_eq!(live["projection"]["visibleTurnItems"][0]["position"], tail);
    let mut older = local_window(&["older", "row-0"], 2);
    older["gatewayHistoricalPrefix"] = json!(true);
    order.install(&mut older).unwrap();
    assert_eq!(
        older["projection"]["visibleTurnItems"][1]["position"],
        prefix
    );
    assert!(
        order.retained_bytes() < 1024 * 1024,
        "old payloads are not retained with the anchored windows"
    );
}

#[tokio::test]
async fn chunked_pretty_history_enforces_actual_cumulative_body_bytes() {
    let server = fake_server(Some(2)).await;
    {
        let mut snapshot = server.state.snapshot.lock().unwrap();
        snapshot["historyCursor"] = json!("page:0");
        snapshot["historyFixture"] = json!({"mapped":true,"endAt":4,"paddingBytes":4*1024*1024});
    }
    let response = reqwest::get(format!(
        "{}/api/orchestration/threads/thread/bounded",
        server.url
    ))
    .await
    .unwrap();
    assert!(response.content_length().is_none());
    let actual = response.bytes().await.unwrap();
    assert!(actual.len() > 4 * 1024 * 1024);
    let parsed: Value = serde_json::from_slice(&actual).unwrap();
    assert_eq!(parsed["snapshotSequence"], 10);
    let client = client(&server);
    assert!(matches!(
        client.timeline_snapshot("thread", 0).await,
        Err(AgentError::Protocol(_))
    ));
    client.shutdown();
}

#[tokio::test]
async fn escaped_utf8_wire_bytes_are_not_estimated_from_decoded_json() {
    let server = fake_server(Some(2)).await;
    {
        let mut snapshot = server.state.snapshot.lock().unwrap();
        snapshot["historyCursor"] = json!("page:0");
        snapshot["historyFixture"] =
            json!({"mapped":true,"endAt":4,"unicodeChars":800_000,"escapeUnicode":true});
    }
    let response = reqwest::get(format!(
        "{}/api/orchestration/threads/thread/history?cursor=page:0",
        server.url
    ))
    .await
    .unwrap();
    assert!(response.content_length().is_none());
    let actual = response.bytes().await.unwrap();
    assert!(actual.len() > 4_800_000);
    let parsed: Value = serde_json::from_slice(&actual).unwrap();
    assert_eq!(
        parsed["items"][0]["item"]["text"].as_str().unwrap(),
        "é".repeat(800_000)
    );
    let client = client(&server);
    assert!(matches!(
        client.timeline_snapshot("thread", 0).await,
        Err(AgentError::Protocol(_))
    ));
    client.shutdown();
}

#[tokio::test]
async fn omitted_custom_answer_flag_allows_nonoption_and_explicit_false_rejects() {
    let server = fake_server(Some(2)).await;
    {
        let mut snapshot = server.state.snapshot.lock().unwrap();
        snapshot["projection"]["turnItems"][1]["questions"][0]
            .as_object_mut()
            .unwrap()
            .remove("allowCustomAnswer");
    }
    let client = client(&server);
    let driver = T3Driver::from_client(client.clone());
    let forms = driver.get_pending_forms("thread").await.unwrap();
    assert!(matches!(
        &forms[0].fields[0],
        crate::agents::domain::FormField::String { custom: true, .. }
    ));
    driver
        .reply_form("thread", "question", json!({"choice":"custom color"}))
        .await
        .unwrap();
    server.state.snapshot.lock().unwrap()["projection"]["turnItems"][1]["questions"][0]
        ["allowCustomAnswer"] = json!(false);
    assert!(driver
        .reply_form("thread", "question", json!({"choice":"custom color"}))
        .await
        .is_err());
    client.shutdown();
}

#[tokio::test]
async fn live_editable_answers_preserve_empty_whitespace_but_message_answers_need_text() {
    let server = fake_server(Some(2)).await;
    {
        let mut snapshot = server.state.snapshot.lock().unwrap();
        snapshot["projection"]["runtimeRequests"][1]["responseCapability"] =
            json!({"type":"live","providerSessionId":"provider-session"});
        let question = &mut snapshot["projection"]["turnItems"][1]["questions"][0];
        question["initialAnswer"] = json!("editable");
        question
            .as_object_mut()
            .unwrap()
            .remove("allowCustomAnswer");
    }
    let client = client(&server);
    let driver = T3Driver::from_client(client.clone());
    for text in ["", "   "] {
        driver
            .reply_form("thread", "question", json!({"choice":text}))
            .await
            .unwrap();
    }
    assert!(driver
        .reply_form("thread", "question", json!({}))
        .await
        .is_err());
    let sent: Vec<_> = server
        .state
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|row| row["payload"]["type"] == "runtime-request.respond")
        .map(|row| row["payload"]["answers"]["choice"].clone())
        .collect();
    assert_eq!(sent, vec![json!(""), json!("   ")]);
    server.state.snapshot.lock().unwrap()["projection"]["runtimeRequests"][1]
        ["responseCapability"] = json!({"type":"message"});
    for text in ["", "   "] {
        assert!(driver
            .reply_form("thread", "question", json!({"choice":text}))
            .await
            .is_err());
    }
    client.shutdown();
}

#[tokio::test]
async fn repository_enrichment_high_water_does_not_discard_application_replay() {
    use crate::agents::adapters::t3::{stream::ShellEvent, T3StreamEvent, T3StreamListener};
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let mut listener = T3StreamListener::new(client.clone());
    let mut received = listener.start();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if server
                .state
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|row| row["rpc"] == "orchestration.subscribeShell")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.state.stream_tx.send(json!({"fixtureShell":{"kind":"snapshot","resolvedRepositoryIdentityRoots":["/workspace/alpha"],"snapshot":{"snapshotSequence":12,"projects":[],"threads":[{"id":"stale-thread"}]}}})).unwrap();
    let created = json!({"kind":"thread.updated","sequence":11,"location":"active","thread":{"id":"new-thread","title":"created"}});
    let updated = json!({"kind":"thread.updated","sequence":12,"location":"active","thread":{"id":"new-thread","title":"updated"}});
    for frame in [&created, &updated, &updated] {
        server
            .state
            .stream_tx
            .send(json!({"fixtureShell":frame}))
            .unwrap();
    }
    let mut replay = Vec::new();
    let mut saw_enrichment = false;
    tokio::time::timeout(Duration::from_secs(3), async {
        while replay.len() < 2 {
            match received.recv().await.expect("listener ended") {
                T3StreamEvent::Shell(ShellEvent::Enrichment { snapshot, .. }) => {
                    assert_eq!(snapshot["snapshotSequence"], 12);
                    saw_enrichment = true;
                }
                T3StreamEvent::Shell(ShellEvent::ThreadUpserted { sequence, thread }) => {
                    replay.push((sequence, thread["title"].clone()))
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(saw_enrichment);
    assert_eq!(replay, vec![(11, json!("created")), (12, json!("updated"))]);
    assert!(
        tokio::time::timeout(Duration::from_millis(40), async {
            loop {
                if matches!(
                    received.recv().await,
                    Some(T3StreamEvent::Shell(ShellEvent::ThreadUpserted { .. }))
                ) {
                    break;
                }
            }
        })
        .await
        .is_err(),
        "duplicate application event should not be applied twice"
    );
    listener.stop();
    client.shutdown();
}

#[test]
fn native_projection_uses_visible_order_not_local_ordinals_or_time() {
    let snapshot = fixture();
    projection::validate_snapshot(&snapshot, "thread").unwrap();
    let rows = mapper::timeline("thread", &snapshot["projection"], 10);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].id.contains("parent:user-item"));
    assert!(rows[0].message_id < rows[1].message_id);
    assert!(matches!(&rows[1].part,AgentPart::Text{text} if text=="Current whole message"));
    assert_eq!(
        mapper::interrupt_run(&snapshot["projection"]).unwrap(),
        "root-run"
    );
}

#[test]
fn historical_page_does_not_replace_newer_live_entity_or_control_sequence() {
    let mut snapshot = fixture();
    let mut old = snapshot["projection"]["visibleTurnItems"][1].clone();
    old["item"]["text"] = json!("stale page text");
    old["position"] = json!(1);
    let mut earlier = snapshot["projection"]["visibleTurnItems"][1].clone();
    earlier["sourceItemId"] = json!("older");
    earlier["position"] = json!(0);
    let page = json!({"snapshotSequence":4,"items":[earlier,old],"nextCursor":null,"hasMoreHistory":false});
    let control = snapshot["projection"]["runs"].clone();
    projection::merge_history(&mut snapshot, &page).unwrap();
    assert_eq!(snapshot["snapshotSequence"], 10);
    assert_eq!(snapshot["projection"]["runs"], control);
    let rows = mapper::timeline("thread", &snapshot["projection"], 10);
    assert_eq!(rows.len(), 3);
    assert!(matches!(&rows[2].part,AgentPart::Text{text} if text=="Current whole message"));
}

#[test]
fn approvals_offer_only_exact_representable_choices() {
    let mut snapshot = fixture();
    let p = &mut snapshot["projection"];
    let permissions = mapper::permissions("thread", p);
    assert_eq!(permissions[0].options.len(), 3);
    assert_eq!(
        mapper::approval_decision(p, "approval", PermissionDecision::AllowAlways).unwrap(),
        "acceptAlways"
    );
    p["turnItems"][0]["options"] = json!([{"decision":"acceptForSession","label":"Session only"}]);
    assert!(mapper::approval_decision(p, "approval", PermissionDecision::AllowAlways).is_err());
    assert!(mapper::permissions("thread", p).is_empty());
}

#[test]
fn expired_cancelled_resolved_and_nonresumable_requests_are_not_clickable() {
    for status in ["expired", "cancelled", "resolved"] {
        let mut snapshot = fixture();
        snapshot["projection"]["runtimeRequests"][0]["status"] = json!(status);
        assert!(mapper::permissions("thread", &snapshot["projection"]).is_empty());
    }
    let mut snapshot = fixture();
    snapshot["projection"]["runtimeRequests"][0]["responseCapability"] =
        json!({"type":"not_resumable","reason":"Provider closed"});
    assert!(mapper::permissions("thread", &snapshot["projection"]).is_empty());
    snapshot["projection"]["runtimeRequests"][0]["responseCapability"] = json!({"type":"message"});
    assert!(mapper::permissions("thread", &snapshot["projection"]).is_empty());
}

#[test]
fn live_response_requires_current_provider_identity() {
    let mut snapshot = fixture();
    snapshot["projection"]["providerThreads"][0]["providerSessionId"] = json!("replaced-session");
    assert!(mapper::permissions("thread", &snapshot["projection"]).is_empty());
    let mut snapshot = fixture();
    snapshot["projection"]["providerSessions"][0]["status"] = json!("stopped");
    assert!(mapper::permissions("thread", &snapshot["projection"]).is_empty());
}

#[test]
fn message_capability_is_only_the_proven_question_response_command() {
    let snapshot = fixture();
    let p = &snapshot["projection"];
    assert_eq!(mapper::forms("thread", p).len(), 1);
    mapper::validate_answers(p, "question", &json!({"choice":"blue"})).unwrap();
    for answers in [
        json!({}),
        json!({"choice":"red"}),
        json!({"choice":["blue"]}),
        json!({"extra":"blue"}),
    ] {
        assert!(mapper::validate_answers(p, "question", &answers).is_err());
    }
    let command = commands::respond("thread", "question", None, Some(json!({"choice":"blue"})));
    assert_eq!(command["type"], "runtime-request.respond");
    assert!(command.get("text").is_none());
}

#[test]
fn commands_are_flat_v2_and_default_delivery_never_interrupts() {
    let model = ModelSelection {
        instance_id: "codex".into(),
        model: "gpt-5".into(),
        options: vec![],
    };
    let create = commands::thread_create(
        "thread",
        "project",
        "title",
        &model,
        "approval-required",
        "default",
    );
    assert!(create.get("createdAt").is_none());
    assert_eq!(create["createdBy"], "user");
    assert_eq!(create["creationSource"], "mobile");
    assert_eq!(create["modelSelection"]["instanceId"], "codex");
    assert!(create["branch"].is_null());
    for (active, mode) in [(false, "start_immediately"), (true, "queue_after_active")] {
        let command = commands::message_dispatch("thread", "message", "text", Some(&model), active);
        assert_eq!(command["dispatchMode"]["type"], mode);
        assert_eq!(command["messageId"], "message");
        assert_eq!(command["attachments"], json!([]));
        assert!(command.get("message").is_none());
        assert!(command.get("runtimeMode").is_none());
        assert_eq!(command["createdBy"], "user");
        assert_eq!(command["creationSource"], "mobile");
    }
    let interrupt = commands::interrupt("thread", "root-run");
    assert_eq!(interrupt["runId"], "root-run");
    assert!(interrupt.get("turnId").is_none());
    assert_eq!(
        commands::set_model("thread", &model)["type"],
        "thread.model-selection.set"
    );
}

#[test]
fn outer_event_sequence_and_known_decode_failures_are_distinct_from_extensions() {
    let event = json!({"kind":"event","sequence":42,"event":{"type":"message.updated","threadId":"thread","payload":{"id":"message","threadId":"thread","role":"assistant","text":"whole text","createdAt":"2026-10-13T10:00:00.000Z","updatedAt":"2026-10-13T10:00:02.000Z"}}});
    assert!(projection::validate_event(&event, "thread").unwrap());
    let malformed = json!({"kind":"event","sequence":43,"event":{"type":"message.updated","threadId":"thread","payload":{}}});
    assert!(projection::validate_event(&malformed, "thread").is_err());
    let extension = json!({"kind":"event","sequence":44,"event":{"type":"thread.future-feature"}});
    assert!(!projection::validate_event(&extension, "thread").unwrap());
    // v0.1.0's one new type is known -- accepted and ignored, not skipped as
    // an extension -- and held to the same envelope checks as the rest.
    let link = json!({"kind":"event","sequence":45,"event":{"type":"thread.pull-request-link-updated","threadId":"thread","payload":{"url":"https://example.invalid/pull/1"}}});
    assert!(projection::validate_event(&link, "thread").unwrap());
    let other_thread = json!({"kind":"event","sequence":46,"event":{"type":"thread.pull-request-link-updated","threadId":"other","payload":{}}});
    assert!(projection::validate_event(&other_thread, "thread").is_err());
}

use crate::agents::adapters::t3::{
    endpoint::EnvironmentDescriptor, rpc::RpcConfig, T3Client, T3Credential, T3Driver, T3Endpoint,
};
use crate::agents::ports::agent::{AgentError, AgentPort, AttachmentMode};
use axum::response::IntoResponse;
use axum::{
    extract::{Query, State, WebSocketUpgrade},
    http::HeaderMap,
    routing::{get, post},
    Json,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
struct FakeState {
    protocol: Arc<Mutex<Option<u64>>>,
    environment: Arc<Mutex<String>>,
    snapshot: Arc<Mutex<Value>>,
    seen: Arc<Mutex<Vec<Value>>>,
    lose_dispatch_reply: Arc<Mutex<bool>>,
    stream_tx: tokio::sync::broadcast::Sender<Value>,
    bounded_started: Arc<tokio::sync::Notify>,
    bounded_release: Arc<tokio::sync::Notify>,
}

struct FakeServer {
    state: FakeState,
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn descriptor(state: &FakeState) -> Value {
    let mut value = json!({"environmentId":*state.environment.lock().unwrap(),"label":"fixture","serverVersion":"release-fixture"});
    if let Some(protocol) = *state.protocol.lock().unwrap() {
        value["orchestrationProtocolVersion"] = json!(protocol);
    }
    value
}

fn record_http(state: &FakeState, headers: &HeaderMap, path: &str) {
    state.seen.lock().unwrap().push(json!({"http":path,"authorization":headers.get("authorization").and_then(|h|h.to_str().ok()),"protocol":headers.get("x-t3-orchestration-protocol").and_then(|h|h.to_str().ok())}));
}

async fn fake_server(protocol: Option<u64>) -> FakeServer {
    let state = FakeState {
        protocol: Arc::new(Mutex::new(protocol)),
        environment: Arc::new(Mutex::new("fixture-environment".into())),
        snapshot: Arc::new(Mutex::new(fixture())),
        seen: Arc::new(Mutex::new(Vec::new())),
        lose_dispatch_reply: Arc::new(Mutex::new(false)),
        stream_tx: tokio::sync::broadcast::channel(16).0,
        bounded_started: Arc::new(tokio::sync::Notify::new()),
        bounded_release: Arc::new(tokio::sync::Notify::new()),
    };
    let app=axum::Router::new()
        .route("/.well-known/t3/environment",get(|State(state):State<FakeState>|async move {Json(descriptor(&state))}))
        .route("/oauth/token",post(|State(state):State<FakeState>,body:String|async move {
            state.seen.lock().unwrap().push(json!({"exchange":body}));
            Json(json!({"access_token":"fixture-bearer","expires_in":3600,"scope":"orchestration:read orchestration:operate"}))
        }))
        .route("/api/auth/websocket-ticket",post(|State(state):State<FakeState>,headers:HeaderMap|async move {
            record_http(&state,&headers,"ticket");Json(json!({"ticket":"fixture-ticket"}))
        }))
        .route("/api/orchestration/shell",get(|State(state):State<FakeState>,headers:HeaderMap|async move {
            record_http(&state,&headers,"shell");
            let version=*state.protocol.lock().unwrap();
            if version==Some(2) {Json(json!({"snapshotSequence":10,"projects":[],"threads":[{"id":"thread","projectId":"project","title":"Fixture","modelSelection":{"instanceId":"claudeAgent","model":"claude-haiku-4-5"},"status":"running","updatedAt":"2026-10-13T10:00:02.000Z","archivedAt":null,"deletedAt":null}]}))}
            else {Json(serde_json::from_str::<Value>(include_str!("../fixtures/shell_snapshot.json")).unwrap())}
        }))
        .route("/api/projects/mutate",post(|State(state):State<FakeState>,headers:HeaderMap,Json(command):Json<Value>|async move {
            record_http(&state,&headers,"project-mutate");state.seen.lock().unwrap().push(json!({"projectMutation":command}));Json(json!({"id":command["projectId"]}))
        }))
        .route("/api/orchestration/threads/{id}/bounded",get(|State(state):State<FakeState>,headers:HeaderMap|async move {
            record_http(&state,&headers,"bounded");let snapshot=state.snapshot.lock().unwrap().clone();
            if snapshot["holdBoundedResponse"]==true {state.bounded_started.notify_one();state.bounded_release.notified().await;}
            fixture_response(snapshot.clone(),&snapshot["historyFixture"])
        }))
        .route("/api/orchestration/threads/{id}/history",get(|State(state):State<FakeState>,headers:HeaderMap,Query(query):Query<std::collections::HashMap<String,String>>|async move {
            record_http(&state,&headers,"history");state.seen.lock().unwrap().push(json!({"cursor":query.get("cursor")}));
            let snapshot=state.snapshot.lock().unwrap().clone();let config=&snapshot["historyFixture"];
            let page=query.get("cursor").and_then(|cursor|cursor.strip_prefix("page:")).and_then(|page|page.parse::<usize>().ok()).unwrap_or(0);
            let mut older=fixture()["projection"]["visibleTurnItems"][1].clone();older["position"]=json!(0);
            let id=if config.is_object(){format!("older-{page}")}else{"older".into()};older["sourceItemId"]=json!(id);older["item"]["id"]=json!(id);
            let next=if config.is_object() {if config["endAt"].as_u64().is_some_and(|end|page as u64+1>=end){Value::Null}else{json!(format!("page:{}",page+1))}}
                else {snapshot.get("historyTestNextCursor").cloned().unwrap_or(Value::Null)};
            if config["mapped"]==false {older["item"]["type"]=json!("checkpoint");older["item"]["checkpointId"]=json!(id);older["item"]["scopeId"]=json!("fixture-scope");older["item"]["files"]=json!([]);}
            if let Some(size)=config["unicodeChars"].as_u64(){older["item"]["text"]=json!("é".repeat(size as usize));}
            let more=!next.is_null();
            fixture_response(json!({"snapshotSequence":1,"items":[older],"nextCursor":next,"hasMoreHistory":more}),config)
        }))
        .route("/api/orchestration/threads/{id}",get(|State(state):State<FakeState>,headers:HeaderMap|async move {
            record_http(&state,&headers,"thread-v1");Json(serde_json::from_str::<Value>(include_str!("../fixtures/thread_detail.json")).unwrap())
        }))
        .route("/ws",get(|State(state):State<FakeState>,Query(query):Query<std::collections::HashMap<String,String>>,upgrade:WebSocketUpgrade|async move {
            state.seen.lock().unwrap().push(json!({"wsQuery":query}));
            upgrade.on_upgrade(move |socket|fake_socket(socket,state))
        }))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    FakeServer { state, url, task }
}

async fn fake_socket(mut socket: axum::extract::ws::WebSocket, state: FakeState) {
    use axum::extract::ws::Message;
    let mut events = state.stream_tx.subscribe();
    let mut thread_subscription: Option<Value> = None;
    let mut shell_subscription: Option<Value> = None;
    loop {
        let text = tokio::select! {
            frame=socket.recv()=>match frame {Some(Ok(Message::Text(text)))=>text,_=>break},
            event=events.recv()=>{
                if let Ok(event)=event {
                    let (value,id)=if let Some(shell)=event.get("fixtureShell"){(shell,shell_subscription.as_ref())}else{(&event,thread_subscription.as_ref())};
                    if let Some(id)=id {
                    if socket.send(Message::Text(json!({"_tag":"Chunk","requestId":id,"values":[value]}).to_string().into())).await.is_err(){break;}
                    }
                }
                continue;
            }
        };
        let frame: Value = serde_json::from_str(&text).unwrap();
        if frame["_tag"] == "Ping" {
            let _ = socket
                .send(Message::Text(json!({"_tag":"Pong"}).to_string().into()))
                .await;
            continue;
        }
        if frame["_tag"] != "Request" {
            continue;
        }
        state
            .seen
            .lock()
            .unwrap()
            .push(json!({"rpc":frame["tag"],"payload":frame["payload"]}));
        let value = match frame["tag"].as_str().unwrap() {
            "server.getConfig" => {
                serde_json::from_str::<Value>(include_str!("../fixtures/server_config.json"))
                    .unwrap()
            }
            "orchestration.dispatchCommand" => {
                if *state.protocol.lock().unwrap() == Some(2) {
                    validate_v2_command_shape(&frame["payload"]);
                }
                if *state.lose_dispatch_reply.lock().unwrap() {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
                let command = &frame["payload"];
                if command["type"] == "thread.create" {
                    let mut snapshot = state.snapshot.lock().unwrap();
                    snapshot["projection"]["thread"]["id"] = command["threadId"].clone();
                    for field in [
                        "runs",
                        "nodes",
                        "runtimeRequests",
                        "messages",
                        "turnItems",
                        "visibleTurnItems",
                    ] {
                        snapshot["projection"][field] = json!([]);
                    }
                    snapshot["hasMoreHistory"] = json!(false);
                }
                if command["type"] == "message.dispatch"
                    && command["dispatchMode"]["type"] == "start_immediately"
                {
                    let mut snapshot = state.snapshot.lock().unwrap();
                    snapshot["projection"]["runs"] = fixture()["projection"]["runs"].clone();
                    snapshot["projection"]["nodes"] = fixture()["projection"]["nodes"].clone();
                }
                json!({"sequence":11})
            }
            "orchestration.subscribeShell" | "orchestration.subscribeThread" => {
                if frame["tag"] == "orchestration.subscribeThread" {
                    thread_subscription = Some(frame["id"].clone());
                }
                if frame["tag"] == "orchestration.subscribeShell" {
                    shell_subscription = Some(frame["id"].clone());
                }
                let _=socket.send(Message::Text(json!({"_tag":"Chunk","requestId":frame["id"],"values":[{"kind":"synchronized"}]}).to_string().into())).await;
                continue;
            }
            _ => json!({}),
        };
        if socket.send(Message::Text(json!({"_tag":"Exit","requestId":frame["id"],"exit":{"_tag":"Success","value":value}}).to_string().into())).await.is_err() {break;}
    }
}

fn fixture_response(value: Value, config: &Value) -> axum::response::Response {
    if !config.is_object() {
        return Json(value).into_response();
    }
    let mut encoded = serde_json::to_string_pretty(&value).unwrap();
    if config["escapeUnicode"] == true {
        encoded = encoded.replace('é', "\\u00e9");
    }
    if let Some(padding) = config["paddingBytes"].as_u64() {
        encoded.push_str(&" ".repeat(padding as usize));
    }
    let chunks: Vec<_> = encoded
        .as_bytes()
        .chunks(8192)
        .map(|bytes| Ok::<Vec<u8>, std::io::Error>(bytes.to_vec()))
        .collect();
    axum::response::Response::builder()
        .header("content-type", "application/json")
        .body(axum::body::Body::from_stream(futures::stream::iter(chunks)))
        .unwrap()
}

/// Required command fields from the pinned released union. The fixture must
/// not silently accept a V1 payload or missing V2 creation provenance.
fn validate_v2_command_shape(command: &Value) {
    for field in ["type", "commandId", "threadId"] {
        assert!(command[field].is_string(), "missing {field}");
    }
    match command["type"].as_str().unwrap() {
        "thread.create" => {
            for field in [
                "projectId",
                "title",
                "createdBy",
                "creationSource",
                "runtimeMode",
                "interactionMode",
            ] {
                assert!(command[field].is_string(), "missing {field}");
            }
            assert!(command["modelSelection"]["instanceId"].is_string());
            assert!(command["modelSelection"]["model"].is_string());
            assert!(command.get("branch").is_some());
            assert!(command.get("worktreePath").is_some());
        }
        "message.dispatch" => {
            for field in ["createdBy", "creationSource", "messageId", "text"] {
                assert!(command[field].is_string(), "missing {field}");
            }
            assert!(command["attachments"].is_array());
            assert!(matches!(
                command["dispatchMode"]["type"].as_str(),
                Some("start_immediately" | "queue_after_active" | "steer_active")
            ));
            assert!(
                command.get("message").is_none(),
                "V1 nested message is not V2"
            );
        }
        "run.interrupt" => assert!(command["runId"].is_string()),
        "queued-run.cancel" | "queued-run.edit" | "queued-run.start" => {
            assert!(command["runId"].is_string())
        }
        "queued-message.promote-to-steer" => {
            assert!(command["queuedRunId"].is_string());
            assert!(command["targetRunId"].is_string());
            assert!(command.get("runId").is_none());
        }
        "runtime-request.respond" => assert!(command["requestId"].is_string()),
        "thread.model-selection.set" => {
            assert!(command["modelSelection"]["instanceId"].is_string());
            assert!(command["modelSelection"]["model"].is_string());
        }
        "thread.metadata.update" => assert!(command["title"].is_string()),
        "thread.delete" => {}
        _ => panic!("unsupported V2 fixture command"),
    }
}

#[tokio::test]
async fn snapshot_replay_live_dedup_filtered_sequences_and_decode_resync() {
    use crate::agents::adapters::t3::{stream::ThreadEvent, T3StreamEvent, T3StreamListener};
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let mut listener = T3StreamListener::new(client.clone());
    let mut received = listener.start();
    listener.watch_thread("thread");
    async fn next_projection(received: &mut tokio::sync::mpsc::Receiver<T3StreamEvent>) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(T3StreamEvent::Thread {
                    event: ThreadEvent::V2Snapshot(snapshot),
                    ..
                }) = received.recv().await
                {
                    return snapshot;
                }
            }
        })
        .await
        .unwrap()
    }
    assert_eq!(next_projection(&mut received).await["snapshotSequence"], 10);
    // Wait for the subscription so the mock does not drop an event sent before
    // any stream exists. This is fixture coordination, not a production retry.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if server
                .state
                .seen
                .lock()
                .unwrap()
                .iter()
                .any(|row| row["rpc"] == "orchestration.subscribeThread")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let event = |sequence, text: &str| json!({"kind":"event","sequence":sequence,"event":{"type":"message.updated","threadId":"thread","payload":{"id":"assistant","threadId":"thread","role":"assistant","text":text,"createdAt":"2026-10-13T10:00:01.000Z","updatedAt":"2026-10-13T10:00:02.000Z"}}});
    {
        let mut snapshot = server.state.snapshot.lock().unwrap();
        snapshot["snapshotSequence"] = json!(20);
        snapshot["projection"]["visibleTurnItems"][0]["item"]["text"] =
            json!("replacement, not a delta");
    }
    server
        .state
        .stream_tx
        .send(event(15, "replacement, not a delta"))
        .unwrap();
    let replacement = next_projection(&mut received).await;
    assert_eq!(replacement["snapshotSequence"], 20);
    let timeline = mapper::timeline("thread", &replacement["projection"], 20);
    assert!(matches!(&timeline[1].part,AgentPart::Text{text} if text=="replacement, not a delta"));
    server.state.stream_tx.send(event(15, "duplicate")).unwrap();
    server
        .state
        .stream_tx
        .send(json!({"kind":"event","sequence":28,"event":{"type":"future.extension"}}))
        .unwrap();
    {
        let mut snapshot = server.state.snapshot.lock().unwrap();
        snapshot["snapshotSequence"] = json!(40);
    }
    server
        .state
        .stream_tx
        .send(event(31, "whole entity"))
        .unwrap();
    assert_eq!(next_projection(&mut received).await["snapshotSequence"], 40);
    // A refresh can cover run.updated before that queued event is polled.
    // Detect confirmed rollback in runs, not a nonexistent turn-item status.
    {
        let mut snapshot = server.state.snapshot.lock().unwrap();
        snapshot["snapshotSequence"] = json!(50);
        snapshot["projection"]["runs"][0]["status"] = json!("rolled_back");
        snapshot["projection"]["visibleTurnItems"] =
            json!([fixture()["projection"]["visibleTurnItems"][1].clone()]);
    }
    server
        .state
        .stream_tx
        .send(event(45, "covered by rollback projection"))
        .unwrap();
    let rolled_back = next_projection(&mut received).await;
    assert_eq!(rolled_back["snapshotSequence"], 50);
    assert_eq!(
        rolled_back["hasMoreHistory"], true,
        "rollback cannot claim deletion authority from a bounded window"
    );
    assert_eq!(rolled_back["gatewayRollbackRefresh"], true);
    assert!(
        !server
            .state
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|row| row["http"] == "history"),
        "rollback must not force a whole-conversation download"
    );
    assert!(!mapper::timeline("thread", &rolled_back["projection"], 50)
        .iter()
        .any(|row| row.id.contains("assistant-item")));
    // A malformed known payload must resync without acknowledging sequence 61.
    server.state.stream_tx.send(json!({"kind":"event","sequence":61,"event":{"type":"message.updated","threadId":"thread","payload":{}}})).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                received.recv().await,
                Some(T3StreamEvent::Disconnected { .. })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(next_projection(&mut received).await["snapshotSequence"], 50);
    let seen = server.state.seen.lock().unwrap().clone();
    assert!(seen
        .iter()
        .any(|row| row["rpc"] == "orchestration.subscribeThread"
            && row["payload"]["afterSequence"] == 10));
    assert!(!seen
        .iter()
        .any(|row| row["rpc"] == "orchestration.subscribeThread"
            && row["payload"]["afterSequence"] == 61));
    listener.stop();
    client.shutdown();
}

fn client(server: &FakeServer) -> Arc<T3Client> {
    let descriptor: EnvironmentDescriptor =
        serde_json::from_value(descriptor(&server.state)).unwrap();
    Arc::new(T3Client::with_config(
        T3Endpoint::new(&server.url, T3Credential::Bearer("fixture-bearer".into()))
            .with_descriptor(&descriptor)
            .unwrap(),
        RpcConfig {
            request_timeout: Duration::from_millis(400),
            connect_wait: Duration::from_millis(600),
            backoff_initial: Duration::from_millis(20),
            backoff_max: Duration::from_millis(40),
            ..Default::default()
        },
    ))
}

#[tokio::test]
async fn v2_uses_bearer_ticket_transport_with_exact_header_and_query() {
    for protocol in [Some(2)] {
        let server = fake_server(protocol).await;
        let client = client(&server);
        client.get_config().await.unwrap();
        client.shell_snapshot().await.unwrap();
        let seen = server.state.seen.lock().unwrap().clone();
        let shell = seen.iter().find(|row| row["http"] == "shell").unwrap();
        assert_eq!(shell["authorization"], "Bearer fixture-bearer");
        assert_eq!(
            shell["protocol"],
            if protocol == Some(2) {
                json!("2")
            } else {
                Value::Null
            }
        );
        let query = &seen
            .iter()
            .find(|row| row.get("wsQuery").is_some())
            .unwrap()["wsQuery"];
        assert_eq!(query["wsTicket"], "fixture-ticket");
        assert_eq!(
            query["orchestrationProtocol"],
            if protocol == Some(2) {
                json!("2")
            } else {
                Value::Null
            }
        );
        assert!(!query.to_string().contains("fixture-bearer"));
        client.shutdown();
    }
}

#[tokio::test]
async fn legacy_and_unknown_protocols_fail_before_any_pairing_exchange() {
    for protocol in [None, Some(0), Some(1), Some(3), Some(999)] {
        let server = fake_server(protocol).await;
        let endpoint = T3Endpoint::new(&server.url, T3Credential::Pairing("fixture-once".into()));
        assert!(endpoint
            .exchange_pairing(&reqwest::Client::new(), "fixture-once", "fixture")
            .await
            .is_err());
        assert!(!server
            .state
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|row| row.get("exchange").is_some()));
    }
}

#[tokio::test]
async fn interrupt_uses_only_current_thread_control_state() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let driver = T3Driver::from_client(client.clone());
    driver.interrupt("thread").await.unwrap();
    let seen = server.state.seen.lock().unwrap().clone();
    assert!(seen.iter().any(|row| row["http"] == "bounded"));
    assert!(!seen.iter().any(|row| row["http"] == "shell"));
    let dispatched: Vec<_> = seen
        .iter()
        .filter(|row| row["rpc"] == "orchestration.dispatchCommand")
        .collect();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0]["payload"]["type"], "run.interrupt");
    assert_eq!(dispatched[0]["payload"]["runId"], "root-run");
    client.shutdown();
}

#[tokio::test]
async fn creates_project_over_http_then_flat_session_prompt_queue_and_root_interrupt() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let driver = T3Driver::from_client(client.clone());
    let directory = std::env::temp_dir();
    let session = driver
        .create_session(directory.to_str(), None, None)
        .await
        .unwrap();
    driver
        .send_prompt(
            &session.asid.0,
            "first",
            &["/fixture/path.txt".into()],
            None,
        )
        .await
        .unwrap();
    driver
        .send_prompt(&session.asid.0, "follow-up", &[], None)
        .await
        .unwrap();
    driver.interrupt(&session.asid.0).await.unwrap();
    assert_eq!(driver.attachment_mode(), AttachmentMode::ByPath);
    let seen = server.state.seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter()
            .filter(|row| row.get("projectMutation").is_some())
            .count(),
        1
    );
    let commands: Vec<_> = seen
        .iter()
        .filter(|row| row["rpc"] == "orchestration.dispatchCommand")
        .map(|row| &row["payload"])
        .collect();
    assert_eq!(commands[0]["type"], "thread.create");
    assert_eq!(commands[1]["type"], "message.dispatch");
    assert_eq!(commands[1]["dispatchMode"]["type"], "start_immediately");
    assert!(commands[1]["text"]
        .as_str()
        .unwrap()
        .contains("/fixture/path.txt"));
    assert_eq!(commands[1]["attachments"], json!([]));
    assert_eq!(commands[2]["dispatchMode"]["type"], "queue_after_active");
    assert_eq!(commands[3]["type"], "run.interrupt");
    assert_eq!(commands[3]["runId"], "root-run");
    client.shutdown();
}

#[tokio::test]
async fn approval_form_and_optional_limits_keep_the_existing_app_api() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let driver = T3Driver::from_client(client.clone());
    driver
        .reply_permission("thread", "approval", PermissionDecision::Allow, None)
        .await
        .unwrap();
    driver
        .reply_form("thread", "question", json!({"choice":"blue"}))
        .await
        .unwrap();
    assert!(matches!(
        driver
            .reply_permission(
                "thread",
                "approval",
                PermissionDecision::Deny,
                Some("reason")
            )
            .await,
        Err(AgentError::Unsupported(_))
    ));
    assert!(matches!(
        driver.revert_session("thread", "arbitrary-message").await,
        Err(AgentError::Unsupported(_))
    ));
    driver
        .send_prompt("thread", "text", &[], Some("steer"))
        .await
        .unwrap();
    let rows = driver.get_timeline("thread", 0).await.unwrap();
    assert_eq!(rows.len(), 3);
    let seen = server.state.seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter()
            .filter(|row| row["rpc"] == "orchestration.dispatchCommand")
            .count(),
        3
    );
    assert!(seen
        .iter()
        .any(|row| row["cursor"] == "opaque/older+cursor="));
    client.shutdown();
}

#[tokio::test]
async fn ambiguous_dispatch_is_not_retried_after_disconnect() {
    let server = fake_server(Some(2)).await;
    *server.state.lose_dispatch_reply.lock().unwrap() = true;
    let client = client(&server);
    let driver = T3Driver::from_client(client.clone());
    assert!(driver
        .send_prompt("thread", "only once", &[], None)
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        server
            .state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row["rpc"] == "orchestration.dispatchCommand")
            .count(),
        1
    );
    client.shutdown();
}

#[tokio::test]
async fn protocol_or_environment_change_makes_the_old_driver_unhealthy() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let driver = T3Driver::from_client(client.clone());
    assert!(driver.probe().await.unwrap());
    *server.state.environment.lock().unwrap() = "replacement".into();
    assert!(!driver.probe().await.unwrap());
    *server.state.environment.lock().unwrap() = "fixture-environment".into();
    *server.state.protocol.lock().unwrap() = Some(1);
    assert!(!driver.probe().await.unwrap());
    let tickets_before = server
        .state
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|row| row["http"] == "ticket")
        .count();
    assert!(
        client.shell_snapshot().await.is_err(),
        "old-generation HTTP reads must also stop"
    );
    assert_eq!(
        server
            .state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row["http"] == "ticket")
            .count(),
        tickets_before
    );
    client.shutdown();
}

#[tokio::test]
async fn v2_cross_provider_model_selection_is_not_subject_to_v1_driver_binding() {
    let server = fake_server(Some(2)).await;
    let client = client(&server);
    let driver = T3Driver::from_client(client.clone());
    let target = crate::agents::domain::ModelRef {
        provider_id: "codex".into(),
        model_id: "gpt-6-sol".into(),
        variant: Some("high".into()),
    };
    driver.switch_model("thread", &target).await.unwrap();
    let seen = server.state.seen.lock().unwrap().clone();
    let command = &seen
        .iter()
        .find(|row| row["rpc"] == "orchestration.dispatchCommand")
        .unwrap()["payload"];
    assert_eq!(command["type"], "thread.model-selection.set");
    assert_eq!(command["modelSelection"]["instanceId"], "codex");
    assert_eq!(
        command["modelSelection"]["options"][0],
        json!({"id":"effort","value":"high"})
    );
    client.shutdown();
}

#[tokio::test]
async fn opaque_history_cursor_cycles_fail_with_a_bounded_read() {
    let server = fake_server(Some(2)).await;
    server.state.snapshot.lock().unwrap()["historyTestNextCursor"] = json!("opaque/older+cursor=");
    let client = client(&server);
    assert!(matches!(
        client.timeline_snapshot("thread", 0).await,
        Err(AgentError::Protocol(_))
    ));
    assert_eq!(
        server
            .state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row["http"] == "history")
            .count(),
        1
    );
    client.shutdown();
}

#[test]
fn subagent_status_and_child_identity_are_preserved() {
    let mut snapshot = fixture();
    snapshot["projection"]["visibleTurnItems"] = json!([{
        "position":1,"visibility":"local","sourceThreadId":"thread","sourceItemId":"child-item",
        "item":{"id":"child-item","threadId":"thread","type":"subagent","status":"completed",
        "childThreadId":"thread:provider:cursor:native-thread:child%3Atask","title":"Audit","result":"Finished"}
    }]);
    let rows = mapper::timeline("thread", &snapshot["projection"], 10);
    let tool = rows
        .iter()
        .find_map(|row| match &row.part {
            AgentPart::Tool(tool) => Some(tool),
            _ => None,
        })
        .expect("subagent tool");
    assert_eq!(
        tool.child_session_id.as_deref(),
        Some("thread:provider:cursor:native-thread:child%3Atask")
    );
    assert_eq!(tool.metadata.as_ref().unwrap()["status"], "completed");
    assert_eq!(tool.output.as_ref().unwrap(), "Finished");
}

#[test]
fn omitted_and_failed_file_changes_do_not_become_empty_successful_diffs() {
    for status in ["completed", "failed", "running"] {
        let mut snapshot = fixture();
        snapshot["projection"]["visibleTurnItems"] = json!([{
            "position":1,"visibility":"local","sourceThreadId":"thread","sourceItemId":"edit",
            "item":{"id":"edit","threadId":"thread","type":"file_change","status":status,
            "fileName":"src/example.ts","additions":1,"deletions":0}
        }]);
        let rows = mapper::timeline("thread", &snapshot["projection"], 10);
        let tool = rows
            .iter()
            .find_map(|row| match &row.part {
                AgentPart::Tool(tool) => Some(tool),
                _ => None,
            })
            .expect("file edit retains its execution state without fabricating a diff");
        assert_eq!(tool.input["file_path"], "src/example.ts");
        assert_eq!(tool.metadata.as_ref().unwrap()["additions"], 1);
        assert_eq!(
            tool.state,
            match status {
                "completed" => crate::agents::domain::ToolCallStatus::Completed,
                "failed" => crate::agents::domain::ToolCallStatus::Failed,
                _ => crate::agents::domain::ToolCallStatus::Running,
            }
        );
    }
}

#[tokio::test]
async fn opaque_child_identity_round_trips_http_and_mutation_without_app_rules() {
    let server = fake_server(Some(2)).await;
    let native = "thread:provider:child%3Atask%0Aitem";
    let public = super::super::session_identity::public_id(native);
    {
        let mut snapshot = server.state.snapshot.lock().unwrap();
        snapshot["hasMoreHistory"] = json!(false);
        snapshot["projection"]["thread"]["id"] = json!(native);
        snapshot["projection"]["thread"]["lineage"] = json!({"parentThreadId":native});
        snapshot["projection"]["visibleTurnItems"] = json!([{
            "position":1,"visibility":"local","sourceThreadId":native,"sourceItemId":"child",
            "item":{"id":"child","threadId":native,"type":"subagent","status":"completed","childThreadId":native,"result":"done"}
        }]);
    }
    let client = client(&server);
    let snapshot = client.thread_snapshot(&public, None).await.unwrap();
    assert_eq!(snapshot["projection"]["thread"]["id"], public);
    assert_eq!(
        snapshot["projection"]["thread"]["lineage"]["parentThreadId"],
        public
    );
    let driver = T3Driver::from_client(client.clone());
    let info = driver.get_session(&public).await.unwrap();
    assert_eq!(info.asid.0, public);
    let prepared = driver.prepare_timeline(&public, 100).await.unwrap();
    assert!(prepared.items.iter().any(|row| matches!(&row.part, AgentPart::Tool(call) if call.child_session_id.as_deref() == Some(public.as_str()))));
    drop(prepared);
    client.dispatch(json!({"type":"thread.metadata.update","commandId":"rename-child","threadId":public,"title":"QA"})).await.unwrap();
    let seen = server.state.seen.lock().unwrap();
    assert!(seen
        .iter()
        .any(|row| row["rpc"] == "orchestration.dispatchCommand"
            && row["payload"]["threadId"] == native));
    client.shutdown();
}
