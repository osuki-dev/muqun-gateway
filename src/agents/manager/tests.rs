//! Event-dispatch tests. The frames are real ones, captured from a live
//! OpenCode 2.0.1 session (`tests/fixtures/opencode-events/`).
use super::*;
use crate::agents::adapters::opencode::sse::{read_test_request, OpencodeSseListener};
use crate::agents::domain::AgentPart;
use crate::agents::ports::mirror::SessionMirrorPort;

fn delta(session: &str, message: &str, kind: &str, ordinal: u64, text: &str) -> OpencodeRawEvent {
    OpencodeRawEvent {
        id: None,
        created: None,
        location: None,
        event_type: format!("session.{kind}.delta"),
        data: json!({"sessionID": session, "assistantMessageID": message, "ordinal": ordinal, "delta": text}),
    }
}

#[test]
fn delta_deadline_is_from_first_arrival_and_buffer_is_bounded() {
    let start = tokio::time::Instant::now();
    let mut coalescer = DeltaCoalescer::default();
    assert!(coalescer
        .push(delta("s", "m", "text", 0, "你"), start)
        .is_none());
    let second = delta("s", "m", "text", 0, "🙂");
    assert!(coalescer.can_merge(&second));
    assert!(coalescer
        .push(second, start + std::time::Duration::from_millis(39))
        .is_none());
    assert_eq!(coalescer.deadline(), Some(start + DELTA_FLUSH_DELAY));
    assert_eq!(coalescer.take().unwrap().data["delta"], "你🙂");

    let full = "x".repeat(MAX_PENDING_DELTA_BYTES);
    assert_eq!(
        coalescer
            .push(delta("s", "m", "text", 0, &full), start)
            .unwrap()
            .data["delta"],
        full
    );
    assert!(coalescer.deadline().is_none());
    coalescer.push(delta("s", "m", "text", 0, "x"), start);
    assert!(!coalescer.can_merge(&delta("s", "m", "text", 0, &full)));
}

#[test]
fn delta_runs_never_cross_identity_metadata_or_non_delta_boundaries() {
    let raw = delta("s", "m", "text", 0, "first");
    let mut coalescer = DeltaCoalescer::default();
    coalescer.push(raw.clone(), tokio::time::Instant::now());
    for different in [
        delta("other", "m", "text", 0, "x"),
        delta("s", "other", "text", 0, "x"),
        delta("s", "m", "reasoning", 0, "x"),
        delta("s", "m", "text", 1, "x"),
    ] {
        assert!(!coalescer.can_merge(&different));
    }
    let mut different = raw.clone();
    different.data["metadata"] = json!({"something": "new"});
    assert!(!coalescer.can_merge(&different));
    different = raw.clone();
    different.location = Some(json!({"directory":"other"}));
    assert!(!coalescer.can_merge(&different));
    for kind in [
        "session.text.ended",
        "session.reasoning.ended",
        "session.idle",
        "session.status",
        "session.tool.input.started",
        "form.created",
        "permission.asked",
        "session.deleted",
        "gateway.stream.resync",
    ] {
        different = raw.clone();
        different.event_type = kind.to_string();
        assert!(!coalescer.can_merge(&different), "{kind}");
    }
    different = raw;
    different.data.as_object_mut().unwrap().remove("sessionID");
    assert!(!coalescer.can_merge(&different));
}

#[tokio::test]
async fn pump_drains_and_final_flush_preserves_whole_rows_and_completion_markers() {
    let markers = "\nMUQUN_TASK_DONE: 完成🙂\nMUQUN_TASK_BLOCKED: 等待";
    let raws: Vec<_> = (0..1000)
        .map(|_| delta("s", "m", "text", 0, &"x".repeat(32)))
        .chain(std::iter::once(delta("s", "m", "text", 0, markers)))
        .collect();
    let baseline = ctx();
    let mut baseline_rx = baseline.tx.subscribe();
    let mut baseline_count = 0;
    let mut baseline_bytes = 0;
    for raw in &raws {
        AgentManager::handle_raw_event(raw.clone(), &baseline).await;
        if let AgentDomainEvent::TimelineUpsert { items, .. } = baseline_rx.recv().await.unwrap() {
            baseline_count += 1;
            if let AgentPart::Text { text } = &items[0].part {
                baseline_bytes += text.len();
            }
        } else {
            panic!("whole row required");
        }
    }
    let coalesced = ctx();
    let mut out = coalesced.tx.subscribe();
    let (tx, rx) = broadcast::channel(2048);
    for raw in raws {
        tx.send(raw).unwrap();
    }
    drop(tx);
    AgentManager::pump_events(rx, coalesced.clone()).await;
    let mut count = 0;
    let mut bytes = 0;
    while let Ok(event) = out.try_recv() {
        if let AgentDomainEvent::TimelineUpsert { items, .. } = event {
            count += 1;
            if let AgentPart::Text { text } = &items[0].part {
                bytes += text.len();
            }
        } else {
            panic!("whole row required");
        }
    }
    assert_eq!(baseline_count, 1001);
    assert_eq!(count, 4);
    assert_eq!(baseline_bytes, 16_048_000 + markers.len());
    assert_eq!(bytes, 81_152 + markers.len());
    let before = baseline
        .mirror
        .get_snapshot(&AgentSessionId("s".into()))
        .await
        .unwrap();
    let after = coalesced
        .mirror
        .get_snapshot(&AgentSessionId("s".into()))
        .await
        .unwrap();
    assert_eq!(before.timeline[0].part, after.timeline[0].part);
    assert_eq!(
        after.timeline[0].part,
        AgentPart::Text {
            text: format!("{}{markers}", "x".repeat(32000))
        }
    );
    eprintln!("deterministic whole-row work: {baseline_count} -> {count} emissions; {baseline_bytes} -> {bytes} prefix text bytes");
}

#[tokio::test]
async fn pump_flushes_before_status_fulltext_and_lag_then_drains_close() {
    let context = ctx();
    let mut out = context.tx.subscribe();
    let (tx, rx) = broadcast::channel(16);
    tx.send(delta("s", "m", "text", 0, "before")).unwrap();
    let mut status = delta("s", "m", "text", 0, "ignored");
    status.event_type = "session.idle".into();
    tx.send(status).unwrap();
    tx.send(delta("s", "m", "text", 0, " plus")).unwrap();
    let mut fulltext = delta("s", "m", "text", 0, "unused");
    fulltext.event_type = "session.text.ended".into();
    fulltext.data["text"] = json!("authoritative 完成🙂");
    tx.send(fulltext).unwrap();
    drop(tx);
    AgentManager::pump_events(rx, context.clone()).await;
    for expected in ["before", "status", "before plus", "authoritative 完成🙂"] {
        let event = out.recv().await.unwrap();
        if expected == "status" {
            assert!(matches!(event, AgentDomainEvent::StatusChanged { .. }));
        } else if let AgentDomainEvent::TimelineUpsert { items, .. } = event {
            assert_eq!(
                items[0].part,
                AgentPart::Text {
                    text: expected.into()
                }
            );
        } else {
            panic!("expected text row");
        }
    }
    // An overflow is a boundary even when all retained frames are deltas.
    let (tx, rx) = broadcast::channel(2);
    for _ in 0..4 {
        tx.send(delta("s", "m2", "text", 0, "x")).unwrap();
    }
    drop(tx);
    AgentManager::pump_events(rx, context).await;
    assert!(matches!(
        out.recv().await.unwrap(),
        AgentDomainEvent::Resync { .. }
    ));
    assert!(matches!(
        out.recv().await.unwrap(),
        AgentDomainEvent::TimelineUpsert { .. }
    ));
}

#[tokio::test]
async fn idle_pump_cancellation_releases_context_and_drops_queued_events() {
    let context = ctx();
    let weak = Arc::downgrade(&context);
    let cancel = context.cancel.clone();
    let mut out = context.tx.subscribe();
    let (tx, rx) = broadcast::channel(16);
    let pump = tokio::spawn(AgentManager::pump_events(rx, context));
    tokio::task::yield_now().await;
    cancel.cancel();
    tx.send(delta("s", "m", "text", 0, "stale")).ok();
    tokio::time::timeout(std::time::Duration::from_millis(200), pump)
        .await
        .unwrap()
        .unwrap();
    assert!(weak.upgrade().is_none());
    assert!(out.try_recv().is_err());
}

#[tokio::test]
async fn pump_flush_timer_runs_without_more_upstream_events() {
    let context = ctx();
    let mut out = context.tx.subscribe();
    let (tx, rx) = broadcast::channel(16);
    let pump = tokio::spawn(AgentManager::pump_events(rx, context.clone()));
    tx.send(delta("s", "m", "reasoning", 0, "思考🙂")).unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_millis(200), out.recv())
        .await
        .unwrap()
        .unwrap();
    let AgentDomainEvent::TimelineUpsert { items, .. } = event else {
        panic!("whole row")
    };
    assert_eq!(
        items[0].part,
        AgentPart::Reasoning {
            text: "思考🙂".into(),
            duration_ms: None
        }
    );
    context.cancel.cancel();
    pump.await.unwrap();
}

#[tokio::test]
async fn cancellation_discards_pending_delta_and_interrupts_inflight_hydration() {
    let context = ctx();
    let mut out = context.tx.subscribe();
    let (tx, rx) = broadcast::channel(16);
    let pump = tokio::spawn(AgentManager::pump_events(rx, context.clone()));
    tx.send(delta("s", "m", "text", 0, "must not emit after stop"))
        .unwrap();
    // The local first-delta window has begun, but has not elapsed.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    context.cancel.cancel();
    pump.await.unwrap();
    assert!(out.try_recv().is_err());

    use tokio::io::AsyncReadExt;
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", tcp.local_addr().unwrap());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = tcp.accept().await.unwrap();
        read_test_request(&mut socket).await;
        ready_tx.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
    });
    let (events_tx, mut events_rx) = broadcast::channel(16);
    let driver = Arc::new(OpencodeDriver::new(OpencodeEndpoint {
        url,
        password: None,
        version: None,
        pid: None,
    }));
    let context = Arc::new(EventContext::new(
        driver,
        Arc::new(MemoryMirror::new()),
        events_tx,
    ));
    let weak = Arc::downgrade(&context);
    let cancel = context.cancel.clone();
    let (tx, rx) = broadcast::channel(16);
    let pump = tokio::spawn(AgentManager::pump_events(rx, context));
    let mut start = delta("s", "m", "text", 0, "unused");
    start.event_type = "session.execution.started".into();
    tx.send(start).unwrap();
    ready_rx.await.unwrap();
    cancel.cancel();
    tokio::time::timeout(std::time::Duration::from_millis(200), pump)
        .await
        .unwrap()
        .unwrap();
    assert!(weak.upgrade().is_none());
    assert!(events_rx.try_recv().is_err());
    tokio::time::timeout(std::time::Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn repeated_http_manager_replacement_releases_mirrors_and_reattaches() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", tcp.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for _ in 0..5 {
            let (mut socket, _) = tcp.accept().await.unwrap();
            read_test_request(&mut socket).await;
            let payload = format!(
                "data: {}\n\n",
                serde_json::to_string(&delta("s", "m", "text", 0, "reattached🙂")).unwrap()
            );
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            socket
                .write_all(format!("{:x}\r\n{payload}\r\n", payload.len()).as_bytes())
                .await
                .unwrap();
            let mut byte = [0];
            assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
        }
    });
    let (tx, mut out) = broadcast::channel(32);
    for _ in 0..5 {
        let manager = AgentManager::connect(
            OpencodeEndpoint {
                url: url.clone(),
                password: None,
                version: None,
                pid: None,
            },
            tx.clone(),
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), out.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, AgentDomainEvent::TimelineUpsert { .. }));
        let weak = Arc::downgrade(manager.mirror());
        manager.shutdown();
        drop(manager);
        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement releases old mirror");
        assert!(out.try_recv().is_err());
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

fn ctx() -> Arc<EventContext> {
    // The driver points at a port nothing is listening on: every test here is
    // about what the dispatch does with a frame, and a refetch that fails is
    // logged and skipped.
    let endpoint = OpencodeEndpoint {
        url: "http://127.0.0.1:1".to_string(),
        password: None,
        version: Some("2.0.1".to_string()),
        pid: None,
    };
    let driver = Arc::new(OpencodeDriver::new(endpoint));
    let mirror = Arc::new(MemoryMirror::new());
    let (tx, _rx) = broadcast::channel(256);
    Arc::new(EventContext::new(driver, mirror, tx))
}

fn frames(fixture: &str) -> Vec<OpencodeRawEvent> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/opencode-events")
        .join(fixture);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture {} unreadable: {e}", path.display()));
    raw.split("\n\n")
        .filter_map(OpencodeSseListener::parse_block)
        .collect()
}

async fn replay(ctx: &EventContext, fixture: &str) {
    for frame in frames(fixture) {
        AgentManager::handle_raw_event(frame, ctx).await;
    }
}

#[tokio::test]
async fn a_tool_run_becomes_one_completed_card() {
    let ctx = ctx();
    replay(&ctx, "tool-call.sse").await;

    let asid = AgentSessionId("ses_f511626fdffe0ncuGo6c1jS5Yk".to_string());
    let snapshot = ctx.mirror.get_snapshot(&asid).await.expect("session known");
    let tools: Vec<_> = snapshot
        .timeline
        .iter()
        .filter_map(|it| match &it.part {
            AgentPart::Tool(call) => Some(call),
            _ => None,
        })
        .collect();

    assert_eq!(tools.len(), 1, "the four tool frames join into one card");
    let call = tools[0];
    assert_eq!(call.name, "glob", "the name comes from input.started");
    assert_eq!(call.state, ToolCallStatus::Completed);
    assert_eq!(call.status, ToolCallStatus::Completed);
    assert_eq!(
        call.input.get("pattern").and_then(Value::as_str),
        Some("**/*"),
        "the input comes from tool.called"
    );
    assert!(
        call.output
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("sample.txt"),
        "the result comes from tool.success"
    );
    assert!(
        call.content.is_some(),
        "Tool.Content[] is forwarded verbatim"
    );
    assert_eq!(
        call.metadata
            .as_ref()
            .and_then(|m| m.get("count"))
            .and_then(Value::as_u64),
        Some(1),
        "metadata is forwarded verbatim"
    );
}

#[tokio::test]
async fn a_subagent_call_exposes_its_child_session() {
    let ctx = ctx();
    replay(&ctx, "subagent.sse").await;

    let parent = AgentSessionId("ses_PARENT".to_string());
    let snapshot = ctx
        .mirror
        .get_snapshot(&parent)
        .await
        .expect("parent known");
    let call = snapshot
        .timeline
        .iter()
        .find_map(|it| match &it.part {
            AgentPart::Tool(call) => Some(call),
            _ => None,
        })
        .expect("the subagent call is a tool card, not a todo list");

    assert_eq!(call.name, "subagent");
    assert_eq!(call.child_session_id.as_deref(), Some("ses_CHILD"));

    // The child session arrives on the same stream and keeps its parent.
    let child = AgentSessionId("ses_CHILD".to_string());
    let child_info = ctx.mirror.get_snapshot(&child).await.expect("child known");
    assert_eq!(child_info.info.parent_id.as_deref(), Some("ses_PARENT"));
    assert_eq!(child_info.info.title, "count files");
    assert_eq!(child_info.info.mode.as_deref(), Some("explore"));
}

#[tokio::test]
async fn a_rename_updates_the_title_in_place() {
    let ctx = ctx();
    replay(&ctx, "session-lifecycle.sse").await;

    let asid = AgentSessionId("ses_f5112fddafferdTlgNjwonf8Au".to_string());
    let snapshot = ctx.mirror.get_snapshot(&asid).await.expect("session known");
    assert_eq!(
        snapshot.info.title,
        "Tool availability and directory file count request"
    );
    assert_ne!(snapshot.info.title, "Session");
}

#[tokio::test]
async fn step_usage_never_replaces_cumulative_session_usage() {
    let ctx = ctx();
    replay(&ctx, "session-lifecycle.sse").await;
    let asid = AgentSessionId("ses_f5112fddafferdTlgNjwonf8Au".to_string());
    let mut rx = ctx.tx.subscribe();

    let cumulative = serde_json::json!({
        "sessionID": asid.0,
        "cost": 2.5,
        "tokens": {
            "input": 1000, "output": 200, "reasoning": 100,
            "cache": { "read": 9000, "write": 500 }
        }
    });
    let dispatch = |event_type: &str, data: Value| OpencodeRawEvent {
        id: None,
        event_type: event_type.to_string(),
        created: None,
        location: None,
        data,
    };
    AgentManager::handle_raw_event(dispatch("session.usage.updated", cumulative.clone()), &ctx)
        .await;
    let expected = ctx.mirror.session_info(&asid).await.unwrap();

    let step = serde_json::json!({
        "sessionID": asid.0,
        "cost": 0.1,
        "tokens": {
            "input": 10, "output": 20, "reasoning": 5,
            "cache": { "read": 90, "write": 0 }
        }
    });
    for _ in 0..2 {
        AgentManager::handle_raw_event(dispatch("session.step.ended", step.clone()), &ctx).await;
        let info = ctx.mirror.session_info(&asid).await.unwrap();
        assert_eq!(info.tokens, expected.tokens);
        assert_eq!(info.cost, expected.cost);
    }

    // Cumulative snapshots can also be replayed; they replace rather than add.
    AgentManager::handle_raw_event(dispatch("session.usage.updated", cumulative), &ctx).await;
    let info = ctx.mirror.session_info(&asid).await.unwrap();
    assert_eq!(info.tokens, expected.tokens);
    assert_eq!(info.cost, expected.cost);
    let mut updates = 0;
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::SessionUpdated { info, .. } = event {
            updates += 1;
            assert_eq!(info.tokens, expected.tokens);
            assert_eq!(info.cost, expected.cost);
        }
    }
    assert_eq!(updates, 2, "only cumulative usage is published to the app");
}

#[tokio::test]
async fn a_failed_run_carries_the_error_to_the_client() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    replay(&ctx, "execution-failed.sse").await;

    let mut failure = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::StatusChanged {
            status: AgentSessionStatus::Failed,
            error,
            ..
        } = event
        {
            failure = error;
        }
    }
    let failure = failure.expect("a failed run reports its error");
    assert_eq!(failure.message, "Agent not found: \"Build\"");
    assert_eq!(failure.name, "unknown");
}

#[tokio::test]
async fn reasoning_and_text_at_the_same_ordinal_are_two_rows() {
    let ctx = ctx();
    replay(&ctx, "text-and-reasoning.sse").await;

    let asid = AgentSessionId("ses_f511626fdffe0ncuGo6c1jS5Yk".to_string());
    let snapshot = ctx.mirror.get_snapshot(&asid).await.expect("session known");

    let text: Vec<_> = snapshot
        .timeline
        .iter()
        .filter_map(|it| match &it.part {
            AgentPart::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    let reasoning: Vec<_> = snapshot
        .timeline
        .iter()
        .filter_map(|it| match &it.part {
            AgentPart::Reasoning { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(reasoning.len(), 1, "reasoning survives");
    assert_eq!(text.len(), 1, "assistant text is not overwritten by it");
    assert_eq!(text[0], "Done.");
    assert_eq!(reasoning[0], "Thinking about it.");
}

#[tokio::test]
async fn compaction_is_forwarded_as_its_own_event() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    replay(&ctx, "compaction.sse").await;

    let mut seen = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::CompactionChanged { status, .. } = event {
            seen.push(status);
        }
    }
    assert!(seen.contains(&CompactionStatus::Started));
    assert!(seen.contains(&CompactionStatus::Running));
    assert!(seen.contains(&CompactionStatus::Completed));
}

#[tokio::test]
async fn inbox_events_publish_the_whole_queue() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    replay(&ctx, "inbox.sse").await;

    let mut last = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::InboxChanged { items, .. } = event {
            last = Some(items);
        }
    }
    let items = last.expect("an inbox event reached the client");
    assert!(
        items.is_empty(),
        "the delivered item leaves the queue, got {items:?}"
    );
}

#[tokio::test]
async fn a_deleted_session_is_announced_then_forgotten() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    replay(&ctx, "session-deleted.sse").await;

    let mut deleted = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::SessionUpdated { info, .. } = event {
            deleted |= info.deleted;
        }
    }
    assert!(deleted, "the app is told the session is gone");

    let asid = AgentSessionId("ses_f511626fdffe0ncuGo6c1jS5Yk".to_string());
    assert!(ctx.mirror.get_snapshot(&asid).await.is_none());
}

#[tokio::test]
async fn an_unknown_event_is_ignored_without_panicking() {
    let ctx = ctx();
    let frame = OpencodeSseListener::parse_block(
        r#"data: {"type":"installation.update-available","data":{"version":"2.0.2"}}"#,
    )
    .expect("frame parses");
    AgentManager::handle_raw_event(frame, &ctx).await;
    assert_eq!(ctx.mirror.session_count().await, 0);
}

/// Activating a skill has to show up while the user is looking at the screen.
/// 2.0.1 announces it with `session.skill.activated` and nothing else -- there
/// is no `session.message.*` family -- so this arm is the whole live path.
#[tokio::test]
async fn an_activated_skill_reaches_the_timeline_from_the_event_alone() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    replay(&ctx, "skill-activated.sse").await;

    let asid = AgentSessionId("ses_f4d866f70ffeqEwhLnNlpcFdiF".to_string());
    let snapshot = ctx.mirror.get_snapshot(&asid).await.expect("session known");
    let skills: Vec<_> = snapshot
        .timeline
        .iter()
        .filter(|it| matches!(it.part, AgentPart::Skill { .. }))
        .collect();
    assert_eq!(skills.len(), 1, "one activation is one row");

    let row = skills[0];
    // The row is addressed exactly as a read-back of the stored message would
    // address it: the envelope id is the message id under an `evt_` prefix.
    assert_eq!(row.id, "msg_0b2799099001haVr31cWxhQSIK:p0");
    assert_eq!(row.message_id, "msg_0b2799099001haVr31cWxhQSIK");
    assert_eq!(row.role, TimelineRole::System);
    let AgentPart::Skill { skill, name, text } = &row.part else {
        unreachable!("filtered above");
    };
    assert_eq!(skill, "docs", "the event's `id` is the skill id");
    assert_eq!(name, "docs");
    assert!(
        text.contains("docs connector"),
        "the body is carried, got {text:?}"
    );

    let mut upserted = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::TimelineUpsert { items, .. } = event {
            upserted |= items
                .iter()
                .any(|it| matches!(it.part, AgentPart::Skill { .. }));
        }
    }
    assert!(
        upserted,
        "the row is pushed to the stream, not left for a refetch"
    );
}

/// An envelope without the id the message id is derived from must not invent
/// one: a row id that does not match the read-back becomes a duplicate row.
#[tokio::test]
async fn a_skill_event_with_no_envelope_id_does_not_invent_a_row() {
    let ctx = ctx();
    let frame = OpencodeSseListener::parse_block(
        r#"data: {"type":"session.skill.activated","data":{"sessionID":"ses_1","id":"docs","name":"docs","text":"x"}}"#,
    )
    .expect("frame parses");
    AgentManager::handle_raw_event(frame, &ctx).await;

    let asid = AgentSessionId("ses_1".to_string());
    let rows = ctx
        .mirror
        .get_snapshot(&asid)
        .await
        .map(|s| s.timeline.len())
        .unwrap_or(0);
    // The fallback is a refetch, and the test driver's port is dead, so the
    // timeline stays empty rather than gaining a row nothing can address.
    assert_eq!(rows, 0);
}

/// Staging, withdrawing, staging again and committing, as 2.0.1 announced them
/// on a live session. The app sees one event per step and `info.revert`
/// follows, so a snapshot taken at any point agrees with the stream.
#[tokio::test]
async fn a_revert_reports_every_step_it_goes_through() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    replay(&ctx, "revert.sse").await;

    let mut steps: Vec<(RevertState, Option<String>)> = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::RevertChanged { state, revert, .. } = event {
            steps.push((state, revert.map(|r| r.message_id)));
        }
    }
    let boundary = "msg_0b274d61b001Mc9InWjER3mRSo".to_string();
    assert_eq!(
        steps,
        vec![
            (RevertState::Staged, Some(boundary.clone())),
            (RevertState::Cleared, None),
            (RevertState::Staged, Some(boundary)),
            (RevertState::Committed, None),
        ],
        "stage, clear, stage, commit -- and only `staged` carries a boundary"
    );

    let asid = AgentSessionId("ses_f4d8b2dccffeFm6JB0jEWOAHn4".to_string());
    let snapshot = ctx.mirror.get_snapshot(&asid).await.expect("session known");
    assert!(
        snapshot.info.revert.is_none(),
        "nothing is staged once the rollback is committed"
    );
}

/// The staged boundary is carried whole, not just its message id: `files` is
/// what the app draws in the confirmation before the user commits.
#[tokio::test]
async fn a_staged_revert_carries_the_file_list_it_came_with() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    let frame = OpencodeSseListener::parse_block(
        r#"data: {"id":"evt_1","type":"session.revert.staged","data":{"sessionID":"ses_1","revert":{"messageID":"msg_2","partID":"prt_3","snapshot":"abc123","files":[{"file":"a.ts","patch":"@@","additions":1,"deletions":0,"status":"modified"}]}}}"#,
    )
    .expect("frame parses");
    AgentManager::handle_raw_event(frame, &ctx).await;

    let mut seen = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::RevertChanged { revert, .. } = event {
            seen = revert;
        }
    }
    let revert = seen.expect("the staged boundary reached the client");
    assert_eq!(revert.message_id, "msg_2");
    assert_eq!(revert.part_id.as_deref(), Some("prt_3"));
    assert_eq!(revert.snapshot.as_deref(), Some("abc123"));
    let files = revert.files.expect("files were asked for and came back");
    assert_eq!(files[0]["file"], "a.ts");

    // And the mirror agrees, so a refetched snapshot shows the same thing.
    let asid = AgentSessionId("ses_1".to_string());
    let snapshot = ctx.mirror.get_snapshot(&asid).await.expect("session known");
    assert_eq!(
        snapshot.info.revert.map(|r| r.message_id).as_deref(),
        Some("msg_2")
    );
}

/// A committed rollback deletes the boundary message and everything after it.
/// 2.0.1 announces no removal of its own, so the mirror has to do it or it
/// goes on serving rows that no longer exist.
#[tokio::test]
async fn committing_a_rollback_takes_the_rows_after_it_off_the_timeline() {
    let ctx = ctx();
    let asid = AgentSessionId("ses_1".to_string());
    let row = |id: &str, message_id: &str| TimelineItem {
        id: id.to_string(),
        message_id: message_id.to_string(),
        role: TimelineRole::Assistant,
        part: AgentPart::Text {
            text: "x".to_string(),
        },
        seq: 0,
        updated_ms: 0,
        ordinal: 0,
        attachments: None,
        image_assets: None,
    };
    ctx.mirror
        .upsert_timeline_items(
            &asid,
            vec![
                row("msg_1:t0", "msg_1"),
                row("msg_5:t0", "msg_5"),
                row("msg_9:t0", "msg_9"),
                // A detached shell is not a message and is not in the range.
                row("shell_sh_1", "shell_sh_1"),
            ],
        )
        .await;

    let mut rx = ctx.tx.subscribe();
    let frame = OpencodeSseListener::parse_block(
        r#"data: {"id":"evt_1","type":"session.revert.committed","data":{"sessionID":"ses_1","to":"msg_5"}}"#,
    )
    .expect("frame parses");
    AgentManager::handle_raw_event(frame, &ctx).await;

    let mut removed: Vec<String> = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::TimelineRemoved { ids, .. } = event {
            removed.extend(ids);
        }
    }
    removed.sort();
    assert_eq!(
        removed,
        vec!["msg_5:t0".to_string(), "msg_9:t0".to_string()],
        "the boundary goes too, and a shell row is not a message"
    );

    let snapshot = ctx.mirror.get_snapshot(&asid).await.expect("session known");
    let left: Vec<&str> = snapshot.timeline.iter().map(|it| it.id.as_str()).collect();
    assert_eq!(left, vec!["msg_1:t0", "shell_sh_1"]);
}

/// A tool call's arguments as they arrive.
///
/// The card used to show the tool's name and nothing else until the whole
/// input landed, because the delta frames were read for their state and their
/// text thrown away. Each frame now grows `input_partial` on the row, and the
/// full `input` clears it.
#[tokio::test]
async fn a_streaming_tool_input_grows_on_the_card_and_is_cleared_when_it_lands() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    let asid = AgentSessionId("ses_f4d23e4d4ffeQZ5sBM44YXS24z".to_string());

    let partials = |event: AgentDomainEvent| -> Option<Option<String>> {
        let AgentDomainEvent::TimelineUpsert { items, .. } = event else {
            return None;
        };
        items.iter().find_map(|it| match &it.part {
            AgentPart::Tool(call) => Some(call.input_partial.clone()),
            _ => None,
        })
    };

    let mut seen: Vec<Option<String>> = Vec::new();
    for frame in frames("tool-input-delta.sse") {
        AgentManager::handle_raw_event(frame, &ctx).await;
        while let Ok(event) = rx.try_recv() {
            if let Some(partial) = partials(event) {
                seen.push(partial);
            }
        }
    }

    // input.started, three deltas, input.ended, tool.called -- six tool frames.
    assert_eq!(seen.len(), 6, "one upsert per tool frame, got {seen:?}");
    assert_eq!(seen[0], None, "nothing has streamed yet at input.started");
    assert_eq!(
        seen[1].as_deref(),
        Some(r#"{"command":""#),
        "the first chunk, on its own"
    );
    assert_eq!(
        seen[2].as_deref(),
        Some(r#"{"command":"echo muqun-del"#),
        "the second is appended, not substituted"
    );
    assert_eq!(
        seen[3].as_deref(),
        Some(r#"{"command":"echo muqun-delta-probe"}"#),
        "and the third completes it"
    );
    assert_eq!(
        seen[4], None,
        "input.ended lands the parsed input, so the preview is dropped"
    );
    assert_eq!(seen[5], None, "and it does not come back on tool.called");

    // And the mirror agrees, so a snapshot after the call shows the real input.
    let snapshot = ctx.mirror.get_snapshot(&asid).await.expect("session known");
    let call = snapshot
        .timeline
        .iter()
        .find_map(|it| match &it.part {
            AgentPart::Tool(call) => Some(call),
            _ => None,
        })
        .expect("one tool card");
    assert_eq!(call.name, "shell");
    assert!(call.input_partial.is_none());
    assert_eq!(
        call.input.get("command").and_then(Value::as_str),
        Some("echo muqun-delta-probe")
    );
}

/// Mid-stream, the mirror holds the preview, so a snapshot taken while the
/// arguments are still arriving says what the stream was saying.
#[tokio::test]
async fn a_snapshot_taken_mid_stream_carries_the_partial_input() {
    let ctx = ctx();
    let asid = AgentSessionId("ses_f4d23e4d4ffeQZ5sBM44YXS24z".to_string());

    // Everything up to and including the second delta.
    for frame in frames("tool-input-delta.sse").into_iter().take(4) {
        AgentManager::handle_raw_event(frame, &ctx).await;
    }

    let snapshot = ctx.mirror.get_snapshot(&asid).await.expect("session known");
    let call = snapshot
        .timeline
        .iter()
        .find_map(|it| match &it.part {
            AgentPart::Tool(call) => Some(call),
            _ => None,
        })
        .expect("one tool card");
    assert_eq!(call.state, ToolCallStatus::Streaming);
    assert_eq!(
        call.input_partial.as_deref(),
        Some(r#"{"command":"echo muqun-del"#)
    );
    assert!(
        call.input.is_null(),
        "half an argument list is not an input, and must not be served as one"
    );
}

/// The worktree events, replayed from the cycle that produced them.
///
/// They belong to a project, not a session, so they carry an empty asid --
/// which is what puts them on every agent-session stream and on the
/// device-wide session stream, the same way a global resync travels.
#[tokio::test]
async fn worktree_events_reach_every_stream_with_what_they_carry() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    replay(&ctx, "worktree.sse").await;

    let mut changes = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::WorktreeChanged {
            asid,
            state,
            directory,
            project_id,
            ..
        } = event
        {
            assert!(asid.0.is_empty(), "a worktree belongs to no session");
            changes.push((state, directory, project_id));
        }
    }

    assert_eq!(changes.len(), 2, "resolved and updated, got {changes:?}");

    let (state, directory, project_id) = &changes[0];
    assert_eq!(*state, WorktreeState::Resolved);
    assert_eq!(
        directory.as_deref(),
        Some("/tmp/muqun-gw-wt"),
        "its own field"
    );
    assert_eq!(
        project_id.as_deref(),
        Some("016d5ff1cc0c2a1c4cb80441b37dbfa45f3b08a6")
    );

    let (state, directory, project_id) = &changes[1];
    assert_eq!(*state, WorktreeState::Updated);
    assert_eq!(
        directory.as_deref(),
        Some("/tmp/muqun-gw-wt"),
        "`worktree.updated` names only the project, so the directory is the \
         envelope's own location"
    );
    assert_eq!(
        project_id.as_deref(),
        Some("016d5ff1cc0c2a1c4cb80441b37dbfa45f3b08a6")
    );
}

/// A move is the one thing the app draws from `session.moved`, and the event
/// carries the new directory inline -- so it lands without a refetch, which
/// matters because the test driver has no OpenCode to refetch from.
#[tokio::test]
async fn a_moved_session_reports_its_new_directory() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    replay(&ctx, "worktree.sse").await;

    let mut directories = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::SessionUpdated { info, .. } = event {
            if let Some(directory) = info.directory.clone() {
                directories.push(directory);
            }
        }
    }
    assert_eq!(
        directories.last().map(String::as_str),
        Some("/home/ryu/.local/share/opencode/worktree/016d5f/probe-b"),
        "the app-visible directory follows the move"
    );

    let asid = AgentSessionId("ses_f4d0d2066ffeYtNIffATgHg0Fw".to_string());
    let snapshot = ctx.mirror.get_snapshot(&asid).await.expect("session known");
    assert_eq!(
        snapshot.info.directory.as_deref(),
        Some("/home/ryu/.local/share/opencode/worktree/016d5f/probe-b")
    );
    assert_eq!(
        snapshot.info.project_id.as_deref(),
        Some("016d5ff1cc0c2a1c4cb80441b37dbfa45f3b08a6"),
        "a move carries the project it lands in"
    );
}

/// `worktree.failed` is not emitted by 2.0.1's HTTP worktree flow, but it is
/// in the binary's event registry, so it is accepted rather than dropped.
#[tokio::test]
async fn a_failed_worktree_carries_its_message() {
    let ctx = ctx();
    let mut rx = ctx.tx.subscribe();
    for frame in [
        r#"data: {"id":"evt_1","type":"worktree.ready","location":{"directory":"/repo"},"data":{"name":"probe","branch":"main"}}"#,
        r#"data: {"id":"evt_2","type":"worktree.failed","location":{"directory":"/repo"},"data":{"message":"fatal: invalid reference: nope"}}"#,
    ] {
        let frame = OpencodeSseListener::parse_block(frame).expect("frame parses");
        AgentManager::handle_raw_event(frame, &ctx).await;
    }

    let mut seen = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::WorktreeChanged {
            state,
            name,
            branch,
            error,
            directory,
            ..
        } = event
        {
            seen.push((state, name, branch, error, directory));
        }
    }
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].0, WorktreeState::Ready);
    assert_eq!(seen[0].1.as_deref(), Some("probe"));
    assert_eq!(seen[0].2.as_deref(), Some("main"));
    assert_eq!(seen[1].0, WorktreeState::Failed);
    assert_eq!(seen[1].3.as_deref(), Some("fatal: invalid reference: nope"));
    assert_eq!(
        seen[1].4.as_deref(),
        Some("/repo"),
        "the envelope says which project even when the payload does not"
    );
}

/// The catalog is cached, so the events that change one have to clear it.
///
/// A user who edits an agent file, adds a command or installs a skill expects
/// the picker to show it -- not to show it whenever the cache happens to
/// expire. These frames are what makes caching the catalog honest.
#[tokio::test]
async fn a_changed_catalog_surface_clears_the_cached_catalog() {
    for event_type in [
        "agent.updated",
        "command.updated",
        "skill.updated",
        "catalog.updated",
        "config.updated",
        "provider.updated",
    ] {
        let ctx = ctx();
        // Something to clear.
        ctx.driver.remember_test_catalog("/repo");
        assert!(
            ctx.driver.has_cached_catalog("/repo"),
            "{event_type}: the fixture has to start with something cached"
        );

        let frame = OpencodeSseListener::parse_block(&format!(
            r#"data: {{"id":"evt_1","type":"{event_type}","data":{{}}}}"#
        ))
        .expect("frame parses");
        AgentManager::handle_raw_event(frame, &ctx).await;

        assert!(
            !ctx.driver.has_cached_catalog("/repo"),
            "{event_type} must drop what it invalidates"
        );
    }

    // And an unrelated event leaves it alone, or the cache would never hold.
    let ctx = ctx();
    ctx.driver.remember_test_catalog("/repo");
    let frame = OpencodeSseListener::parse_block(
        r#"data: {"id":"evt_1","type":"session.text.delta","data":{"sessionID":"ses_1"}}"#,
    )
    .expect("frame parses");
    AgentManager::handle_raw_event(frame, &ctx).await;
    assert!(
        ctx.driver.has_cached_catalog("/repo"),
        "a turn's output is not a catalog change"
    );
}
