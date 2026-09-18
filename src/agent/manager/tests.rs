//! Event-dispatch tests. The frames are real ones, captured from a live
//! OpenCode 2.0.1 session (`tests/fixtures/opencode-events/`).
use super::*;
use crate::agent::adapters::opencode::sse::OpencodeSseListener;
use crate::agent::domain::AgentPart;
use crate::agent::ports::mirror::SessionMirrorPort;

fn ctx() -> Arc<EventContext> {
    // The driver points at a port nothing is listening on: every test here is
    // about what the dispatch does with a frame, and a refetch that fails is
    // logged and skipped.
    let endpoint = OpencodeEndpoint {
        url: "http://127.0.0.1:1".to_string(),
        password: None,
        version: Some("2.0.1".to_string()),
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
    assert!(call.content.is_some(), "Tool.Content[] is forwarded verbatim");
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
    let snapshot = ctx.mirror.get_snapshot(&parent).await.expect("parent known");
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
    assert_eq!(child_info.info.agent.as_deref(), Some("explore"));
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
    assert!(text.contains("docs connector"), "the body is carried, got {text:?}");

    let mut upserted = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentDomainEvent::TimelineUpsert { items, .. } = event {
            upserted |= items.iter().any(|it| matches!(it.part, AgentPart::Skill { .. }));
        }
    }
    assert!(upserted, "the row is pushed to the stream, not left for a refetch");
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
