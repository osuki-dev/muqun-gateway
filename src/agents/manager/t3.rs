//! T3 Code event processing: the counterpart of
//! `AgentManager::handle_raw_event` for what `T3StreamListener` publishes.
//! Each item updates the mirror and emits the same `AgentDomainEvent`s the
//! OpenCode and DeepSeek paths do, so the app sees one vocabulary.
//!
//! - shell `thread-upserted` / `thread-removed` -> `agent.session.updated`;
//! - a thread snapshot (first subscribe, or a replay the server could not
//!   serve) -> the session, its timeline, and its open approvals and forms,
//!   with rows the snapshot no longer has removed;
//! - `thread.message-sent` -> `agent.timeline.upsert`: a streaming delta is
//!   appended to the message's row, a final non-empty text replaces it. T3
//!   message ids are stable across the deltas, so the row streams under the
//!   id its committed message is read back under and nothing is left to
//!   clean up when it closes;
//! - `thread.activity-appended` -> tool cards, plan and status rows, and
//!   `agent.permission.*` / `agent.form.*` for approvals and user-input
//!   requests and their resolutions;
//! - `thread.session-set` -> `agent.status.changed` when the status changes;
//! - `thread.reverted` -> `agent.revert.changed` and the timeline re-read;
//! - the socket dropping -> `agent.resync`.
//!
//! Wire facts: `docs/t3-protocol.md`, sections 7 and 8.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};

use crate::agents::adapters::memory_mirror::MemoryMirror;
use crate::agents::adapters::t3::mapper;
use crate::agents::adapters::t3::stream::{ShellEvent, ThreadEvent, ThreadWatcher};
use crate::agents::adapters::t3::{T3Driver, T3StreamEvent};
use crate::agents::domain::{
    reasoning_item_id, text_item_id, AgentDomainEvent, AgentPart, AgentSessionId,
    AgentSessionStatus, RevertState, TimelineItem,
};
use crate::agents::ports::mirror::SessionMirrorPort;

/// Row anchors remembered per manager (see `mapper::activity_anchor_key`).
/// Bounded because a long-lived gateway sees many tool calls; a forgotten
/// anchor only costs a row that re-sorts to its latest activity.
const MAX_ANCHORS: usize = 2048;

pub(crate) struct T3EventContext {
    pub driver: Arc<T3Driver>,
    pub mirror: Arc<MemoryMirror>,
    pub tx: broadcast::Sender<AgentDomainEvent>,
    watcher: ThreadWatcher,
    /// `projectId -> workspaceRoot`, from the shell stream, for the session
    /// rows' directory.
    roots: Mutex<HashMap<String, String>>,
    /// `(thread id, anchor key) -> ms`: when a multi-activity row, or a
    /// streamed message, was first seen, so it keeps its place.
    anchors: Mutex<HashMap<(String, String), u64>>,
}

impl T3EventContext {
    pub(crate) fn new(
        driver: Arc<T3Driver>,
        mirror: Arc<MemoryMirror>,
        tx: broadcast::Sender<AgentDomainEvent>,
        watcher: ThreadWatcher,
    ) -> Self {
        Self {
            driver,
            mirror,
            tx,
            watcher,
            roots: Mutex::new(HashMap::new()),
            anchors: Mutex::new(HashMap::new()),
        }
    }

    fn emit(&self, mut event: AgentDomainEvent) {
        self.mirror.stamp_event(&mut event);
        // A send with no subscribers is not a failure: nobody is watching.
        let _ = self.tx.send(event);
    }

    /// The time `key` was first seen in `thread`, remembering `now_ms` if
    /// it is new.
    async fn anchor(&self, thread: &str, key: String, now_ms: u64) -> u64 {
        let mut anchors = self.anchors.lock().await;
        if anchors.len() >= MAX_ANCHORS && !anchors.contains_key(&(thread.to_string(), key.clone()))
        {
            if let Some(any) = anchors.keys().next().cloned() {
                anchors.remove(&any);
            }
        }
        *anchors.entry((thread.to_string(), key)).or_insert(now_ms)
    }

    async fn upsert_items(&self, asid: &AgentSessionId, items: Vec<TimelineItem>) {
        if items.is_empty() {
            return;
        }
        let seq = self.mirror.upsert_timeline_items(asid, items.clone()).await;
        self.emit(AgentDomainEvent::TimelineUpsert {
            asid: asid.clone(),
            items,
            seq,
        });
    }

    async fn remove_items(&self, asid: &AgentSessionId, ids: &[String]) {
        if let Some((seq, ids)) = self.mirror.remove_timeline_items(asid, ids).await {
            self.emit(AgentDomainEvent::TimelineRemoved {
                asid: asid.clone(),
                ids,
                seq,
            });
        }
    }

    async fn update_session(&self, thread: &Value) -> Option<AgentSessionId> {
        let roots = self.roots.lock().await.clone();
        let info = mapper::map_session(thread, &roots)?;
        let asid = info.asid.clone();
        if info.deleted {
            self.forget_session(&asid).await;
            return Some(asid);
        }
        let seq = self.mirror.update_session(info.clone()).await;
        self.emit(AgentDomainEvent::SessionUpdated {
            asid: asid.clone(),
            info: Box::new(info),
            seq,
        });
        Some(asid)
    }

    /// A thread T3 deleted: say so once, with the last info the mirror had,
    /// and stop following it.
    async fn forget_session(&self, asid: &AgentSessionId) {
        self.watcher.unwatch(&asid.0);
        self.anchors.lock().await.retain(|(t, _), _| t != &asid.0);
        let Some(snapshot) = self.mirror.get_snapshot(asid).await else {
            return;
        };
        let mut info = snapshot.info;
        info.deleted = true;
        self.mirror.remove_session(asid).await;
        self.emit(AgentDomainEvent::SessionUpdated {
            asid: asid.clone(),
            info: Box::new(info),
            seq: snapshot.seq + 1,
        });
    }

    async fn current_status(&self, asid: &AgentSessionId) -> Option<AgentSessionStatus> {
        self.mirror.get_snapshot(asid).await.map(|s| s.info.status)
    }
}

pub(crate) async fn handle_t3_event(event: T3StreamEvent, ctx: &T3EventContext) {
    match event {
        T3StreamEvent::Shell(shell) => handle_shell(shell, ctx).await,
        T3StreamEvent::Thread { thread_id, event } => {
            handle_thread(&AgentSessionId(thread_id), event, ctx).await
        }
        T3StreamEvent::Disconnected { reason } => {
            tracing::info!(%reason, "t3 stream reconnecting; asking clients to resync");
            ctx.emit(AgentDomainEvent::Resync {
                asid: AgentSessionId(String::new()),
                reason: "stream_reconnected".to_string(),
            });
        }
        T3StreamEvent::ThreadClosed { thread_id, reason } => {
            tracing::debug!(%thread_id, %reason, "t3 thread subscription closed");
        }
    }
}

async fn handle_shell(event: ShellEvent, ctx: &T3EventContext) {
    match event {
        ShellEvent::Snapshot(snapshot) => {
            *ctx.roots.lock().await = mapper::project_roots(&snapshot);
            // Only what the gateway already holds, or what is running, is
            // worth an event: a snapshot lists every thread the host has.
            let threads = snapshot
                .get("threads")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for thread in &threads {
                let Some(id) = thread.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let asid = AgentSessionId(id.to_string());
                let held = ctx.mirror.get_snapshot(&asid).await.is_some();
                let busy = mapper::map_status(thread).0 == AgentSessionStatus::Busy;
                if held || busy {
                    ctx.update_session(thread).await;
                }
                if busy {
                    ctx.watcher.watch(id);
                }
            }
        }
        ShellEvent::Synchronized => {}
        ShellEvent::ProjectUpserted { project, .. } => {
            if let (Some(id), Some(root)) = (
                project.get("id").and_then(Value::as_str),
                project.get("workspaceRoot").and_then(Value::as_str),
            ) {
                ctx.roots
                    .lock()
                    .await
                    .insert(id.to_string(), root.to_string());
            }
        }
        ShellEvent::ProjectRemoved { project_id, .. } => {
            ctx.roots.lock().await.remove(&project_id);
        }
        ShellEvent::ThreadUpserted { thread, .. } => {
            if thread.get("archivedAt").and_then(Value::as_str).is_some() {
                if let Some(id) = thread.get("id").and_then(Value::as_str) {
                    ctx.forget_session(&AgentSessionId(id.to_string())).await;
                }
                return;
            }
            if let Some(asid) = ctx.update_session(&thread).await {
                // A turn started from another client streams to the phone
                // too, approvals included.
                if mapper::map_status(&thread).0 == AgentSessionStatus::Busy {
                    ctx.watcher.watch(&asid.0);
                }
            }
        }
        ShellEvent::ThreadRemoved { thread_id, .. } => {
            ctx.forget_session(&AgentSessionId(thread_id)).await;
        }
    }
}

async fn handle_thread(asid: &AgentSessionId, event: ThreadEvent, ctx: &T3EventContext) {
    match event {
        ThreadEvent::Snapshot(snapshot) => {
            if let Some(thread) = snapshot.get("thread") {
                apply_thread_snapshot(asid, thread, ctx).await;
            }
        }
        ThreadEvent::Synchronized => {}
        ThreadEvent::MessageSent {
            sequence,
            message_id,
            role,
            text,
            streaming,
            created_at,
            updated_at,
            ..
        } => {
            let created =
                crate::agents::adapters::t3::client::ms_from_iso(&created_at).unwrap_or(0);
            let created = ctx
                .anchor(&asid.0, format!("msg:{message_id}"), created)
                .await;
            apply_message(
                asid,
                MessageEvent {
                    sequence,
                    message_id: &message_id,
                    role: &role,
                    text: &text,
                    streaming,
                    created_ms: created,
                    updated_at: &updated_at,
                },
                ctx,
            )
            .await;
        }
        ThreadEvent::ActivityAppended { sequence, activity } => {
            apply_activity(asid, &activity, sequence, ctx).await;
        }
        ThreadEvent::SessionSet { session, .. } => {
            let (status, _, error) = mapper::map_status(&json!({ "session": session }));
            if ctx.current_status(asid).await == Some(status) {
                return;
            }
            if let Some(seq) = ctx.mirror.update_status(asid, status, error.clone()).await {
                ctx.emit(AgentDomainEvent::StatusChanged {
                    asid: asid.clone(),
                    status,
                    error,
                    seq,
                });
            }
        }
        ThreadEvent::Reverted { turn_count, .. } => {
            tracing::debug!(asid = %asid.0, turn_count, "t3 thread reverted");
            let (seq, _) = ctx
                .mirror
                .record_revert(asid, RevertState::Committed, None)
                .await;
            ctx.emit(AgentDomainEvent::RevertChanged {
                asid: asid.clone(),
                state: RevertState::Committed,
                revert: None,
                seq,
            });
            // The revert took messages and activities with it and no event
            // says which, so the thread is read again.
            match ctx.driver.client().thread_snapshot(&asid.0, None).await {
                Ok(detail) => {
                    if let Some(thread) = detail.get("thread") {
                        apply_thread_snapshot(asid, thread, ctx).await;
                    }
                }
                Err(err) => {
                    tracing::debug!(asid = %asid.0, %err, "t3 re-read after a revert failed");
                    ctx.emit(AgentDomainEvent::Resync {
                        asid: asid.clone(),
                        reason: "reverted".to_string(),
                    });
                }
            }
        }
        ThreadEvent::Deleted { .. } => ctx.forget_session(asid).await,
        // The shell stream carries the whole updated row for these.
        ThreadEvent::MetaUpdated { .. }
        | ThreadEvent::TurnDiffCompleted { .. }
        | ThreadEvent::Other { .. } => {}
    }
}

/// Bring the mirror in line with a full thread: the session, every row,
/// the open approvals and forms. Rows the mirror holds that the thread no
/// longer has (a revert, or events missed while away) are removed.
async fn apply_thread_snapshot(asid: &AgentSessionId, thread: &Value, ctx: &T3EventContext) {
    ctx.update_session(thread).await;
    let timeline = mapper::map_thread_timeline(&asid.0, thread);

    // Seed the anchors so live activities for rows already on screen land
    // on those rows rather than beside them.
    {
        let mut anchors = ctx.anchors.lock().await;
        for item in &timeline {
            let raw = mapper::t3_message_id(&item.message_id);
            let at: u64 = item.message_id[..item.message_id.len() - raw.len()]
                .trim_end_matches(':')
                .parse()
                .unwrap_or(0);
            let key = match &item.part {
                AgentPart::Tool(call) => format!("tool:{}", call.id),
                AgentPart::Todo { .. } => format!("plan:{raw}"),
                _ => format!("msg:{raw}"),
            };
            anchors.entry((asid.0.clone(), key)).or_insert(at);
        }
    }

    let fresh: HashSet<&str> = timeline.iter().map(|i| i.id.as_str()).collect();
    if let Some(held) = ctx.mirror.get_snapshot(asid).await {
        let stale: Vec<String> = held
            .timeline
            .iter()
            .filter(|i| !fresh.contains(i.id.as_str()))
            .map(|i| i.id.clone())
            .collect();
        if !stale.is_empty() {
            ctx.remove_items(asid, &stale).await;
        }
        // Requests the snapshot no longer holds open were answered while
        // the gateway was not listening.
        let open_permissions = mapper::map_pending_permissions(&asid.0, thread);
        for request in &held.permissions {
            if !open_permissions.iter().any(|p| p.id == request.id) {
                let seq = ctx.mirror.resolve_permission(asid, &request.id).await;
                ctx.emit(AgentDomainEvent::PermissionResolved {
                    asid: asid.clone(),
                    request_id: request.id.clone(),
                    seq,
                });
            }
        }
        let open_forms = mapper::map_pending_forms(&asid.0, thread);
        for form in &held.forms {
            if !open_forms.iter().any(|f| f.id == form.id) {
                let seq = ctx.mirror.resolve_form(asid, &form.id).await;
                ctx.emit(AgentDomainEvent::FormResolved {
                    asid: asid.clone(),
                    form_id: form.id.clone(),
                    seq,
                });
            }
        }
    }
    ctx.upsert_items(asid, timeline).await;

    for request in mapper::map_pending_permissions(&asid.0, thread) {
        let seq = ctx.mirror.add_permission(request.clone()).await;
        ctx.emit(AgentDomainEvent::PermissionPending {
            asid: asid.clone(),
            request,
            seq,
        });
    }
    for request in mapper::map_pending_forms(&asid.0, thread) {
        let seq = ctx.mirror.add_form(request.clone()).await;
        ctx.emit(AgentDomainEvent::FormPending {
            asid: asid.clone(),
            request,
            seq,
        });
    }
}

struct MessageEvent<'a> {
    sequence: u64,
    message_id: &'a str,
    role: &'a str,
    text: &'a str,
    streaming: bool,
    created_ms: u64,
    updated_at: &'a str,
}

async fn apply_message(asid: &AgentSessionId, message: MessageEvent<'_>, ctx: &T3EventContext) {
    let group = mapper::row_group(message.created_ms, message.message_id);
    let is_reasoning = message.role == "reasoning";
    let is_assistant = matches!(message.role, "assistant" | "reasoning");

    if !is_assistant {
        // A user (or system) message arrives whole.
        let raw = json!({
            "id": message.message_id,
            "role": message.role,
            "text": message.text,
            "createdAt": crate::agents::adapters::t3::client::iso_from_ms(message.created_ms),
            "updatedAt": message.updated_at,
        });
        if let Some(item) = mapper::map_message(&raw, message.sequence) {
            ctx.upsert_items(asid, vec![item]).await;
        }
        return;
    }
    // A final message with no text only closes the stream.
    if !message.streaming && message.text.is_empty() {
        return;
    }
    let item_id = if is_reasoning {
        reasoning_item_id(&group, 0)
    } else {
        text_item_id(&group, 0)
    };
    let seq = if message.streaming {
        ctx.mirror
            .append_text_delta(asid, &item_id, &group, 0, message.text, is_reasoning)
            .await
    } else {
        ctx.mirror
            .set_text_content(asid, &item_id, &group, 0, message.text, is_reasoning)
            .await
    };
    let Some(seq) = seq else {
        return;
    };
    if let Some(item) = ctx.mirror.get_timeline_item(asid, &item_id).await {
        ctx.emit(AgentDomainEvent::TimelineUpsert {
            asid: asid.clone(),
            items: vec![item],
            seq,
        });
    }
}

async fn apply_activity(
    asid: &AgentSessionId,
    activity: &Value,
    sequence: u64,
    ctx: &T3EventContext,
) {
    let kind = activity.get("kind").and_then(Value::as_str).unwrap_or("");
    let request_id = activity
        .get("payload")
        .and_then(|p| p.get("requestId"))
        .and_then(Value::as_str);
    match (kind, request_id) {
        ("approval.requested", _) => {
            if let Some(request) = mapper::map_permission(&asid.0, activity) {
                let seq = ctx.mirror.add_permission(request.clone()).await;
                ctx.emit(AgentDomainEvent::PermissionPending {
                    asid: asid.clone(),
                    request,
                    seq,
                });
            }
        }
        ("user-input.requested", _) => {
            if let Some(request) = mapper::map_form(&asid.0, activity) {
                let seq = ctx.mirror.add_form(request.clone()).await;
                ctx.emit(AgentDomainEvent::FormPending {
                    asid: asid.clone(),
                    request,
                    seq,
                });
            }
        }
        ("approval.resolved", Some(id)) => {
            let seq = ctx.mirror.resolve_permission(asid, id).await;
            ctx.emit(AgentDomainEvent::PermissionResolved {
                asid: asid.clone(),
                request_id: id.to_string(),
                seq,
            });
            remove_request_row(asid, id, ctx).await;
            return;
        }
        ("user-input.resolved", Some(id)) => {
            let seq = ctx.mirror.resolve_form(asid, id).await;
            ctx.emit(AgentDomainEvent::FormResolved {
                asid: asid.clone(),
                form_id: id.to_string(),
                seq,
            });
            remove_request_row(asid, id, ctx).await;
            return;
        }
        _ => {}
    }
    let anchor = match mapper::activity_anchor_key(activity) {
        Some(key) => Some(
            ctx.anchor(&asid.0, key, mapper::activity_created_ms(activity))
                .await,
        ),
        None => None,
    };
    if let Some(item) = mapper::map_activity(&asid.0, activity, sequence, true, anchor) {
        ctx.upsert_items(asid, vec![item]).await;
    }
}

/// Drop the approval or form row of an answered request.
async fn remove_request_row(asid: &AgentSessionId, request_id: &str, ctx: &T3EventContext) {
    let Some(snapshot) = ctx.mirror.get_snapshot(asid).await else {
        return;
    };
    let rows: Vec<String> = snapshot
        .timeline
        .iter()
        .filter(|item| match &item.part {
            AgentPart::Approval { request } => request.id == request_id,
            AgentPart::Form { request } => request.id == request_id,
            _ => false,
        })
        .map(|item| item.id.clone())
        .collect();
    if !rows.is_empty() {
        ctx.remove_items(asid, &rows).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::t3::stream::{parse_shell_item, parse_thread_item};
    use crate::agents::adapters::t3::{T3Credential, T3Endpoint, T3StreamListener};
    use crate::agents::domain::TimelineRole;

    /// Port 9 answers nothing: every test here is about what an item does
    /// to the mirror and the event channel; a re-read that fails is logged.
    fn ctx() -> (Arc<T3EventContext>, broadcast::Receiver<AgentDomainEvent>) {
        let driver = Arc::new(T3Driver::new(T3Endpoint::new(
            "http://127.0.0.1:9",
            T3Credential::None,
        )));
        let listener = T3StreamListener::new(driver.client().clone());
        let mirror = Arc::new(MemoryMirror::for_agent("t3"));
        let (tx, rx) = broadcast::channel(1024);
        (
            Arc::new(T3EventContext::new(driver, mirror, tx, listener.watcher())),
            rx,
        )
    }

    fn drain(rx: &mut broadcast::Receiver<AgentDomainEvent>) -> Vec<AgentDomainEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn names(events: &[AgentDomainEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.event_name()).collect()
    }

    fn captured(name: &str) -> Vec<Value> {
        let raw = match name {
            "thread" => include_str!("../adapters/t3/fixtures/thread_stream_items.json"),
            _ => include_str!("../adapters/t3/fixtures/shell_stream_items.json"),
        };
        serde_json::from_str(raw).unwrap()
    }

    fn thread_id() -> String {
        captured("thread")[0]["snapshot"]["thread"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn replay_thread(ctx: &T3EventContext) {
        let id = thread_id();
        for item in captured("thread") {
            if let Some(event) = parse_thread_item(&item) {
                handle_t3_event(
                    T3StreamEvent::Thread {
                        thread_id: id.clone(),
                        event,
                    },
                    ctx,
                )
                .await;
            }
        }
    }

    #[tokio::test]
    async fn a_captured_approval_turn_becomes_the_domain_events() {
        let (ctx, mut rx) = ctx();
        replay_thread(&ctx).await;
        let events = drain(&mut rx);
        let names = names(&events);
        let asid = AgentSessionId(thread_id());

        // Every event is the thread's and names the agent that owns it.
        assert!(events.iter().all(|e| e.asid() == &asid));
        for event in &events {
            if let AgentDomainEvent::SessionUpdated { info, .. } = event {
                assert_eq!(info.agent_id, "t3");
            }
        }
        assert_eq!(names[0], "agent.session.updated", "the snapshot first");

        // The approval is raised, then answered, and its row goes with it.
        let pending = names
            .iter()
            .position(|n| *n == "agent.permission.pending")
            .expect("pending");
        let resolved = names
            .iter()
            .position(|n| *n == "agent.permission.resolved")
            .expect("resolved");
        assert!(pending < resolved);
        assert!(names[resolved..].contains(&"agent.timeline.removed"));

        // Four `starting` session-sets are one status change, and the turn
        // ends idle.
        let statuses: Vec<AgentSessionStatus> = events
            .iter()
            .filter_map(|e| match e {
                AgentDomainEvent::StatusChanged { status, .. } => Some(*status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            [AgentSessionStatus::Busy, AgentSessionStatus::Idle]
        );

        // The mirror ends where a fresh read of the thread would: the prompt,
        // one tool card, the reply, in that order, nothing pending.
        let snapshot = ctx.mirror.get_snapshot(&asid).await.unwrap();
        let rows: Vec<String> = snapshot
            .timeline
            .iter()
            .map(|i| match &i.part {
                AgentPart::Text { text } if i.role == TimelineRole::User => "user".to_string(),
                AgentPart::Text { text } => format!("assistant:{text}"),
                AgentPart::Tool(call) => format!("tool:{:?}", call.state),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(rows, ["user", "tool:Completed", "assistant:done"]);
        assert!(snapshot.permissions.is_empty());
        assert_eq!(snapshot.info.status, AgentSessionStatus::Idle);

        let detail: Value =
            serde_json::from_str(include_str!("../adapters/t3/fixtures/thread_detail.json"))
                .unwrap();
        let read_back = mapper::map_thread_timeline(&asid.0, &detail["thread"]);
        let ids = |items: &[TimelineItem]| items.iter().map(|i| i.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(&snapshot.timeline),
            ids(&read_back),
            "streamed rows carry the ids a read-back gives them"
        );
    }

    #[tokio::test]
    async fn deltas_append_and_a_final_text_replaces() {
        let (ctx, mut rx) = ctx();
        let asid = AgentSessionId("t".into());
        let sent = |text: &str, streaming: bool| ThreadEvent::MessageSent {
            sequence: 1,
            message_id: "assistant:m".into(),
            role: "assistant".into(),
            text: text.into(),
            streaming,
            turn_id: Some("turn".into()),
            created_at: "2026-09-29T08:45:31.796Z".into(),
            updated_at: "2026-09-29T08:45:31.796Z".into(),
        };
        for (text, streaming) in [("po", true), ("ng", true), ("", false)] {
            handle_thread(&asid, sent(text, streaming), &ctx).await;
        }
        let texts: Vec<String> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                AgentDomainEvent::TimelineUpsert { items, .. } => match &items[0].part {
                    AgentPart::Text { text } => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["po", "pong"], "the empty close emits nothing");
        handle_thread(&asid, sent("pong!", false), &ctx).await;
        let snapshot = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snapshot.timeline.len(), 1, "one row throughout");
        assert!(matches!(&snapshot.timeline[0].part, AgentPart::Text { text } if text == "pong!"));
    }

    #[tokio::test]
    async fn the_shell_stream_updates_sessions_it_holds_or_that_run() {
        let (ctx, mut rx) = ctx();
        for item in captured("shell") {
            if let Some(event) = parse_shell_item(&item) {
                handle_t3_event(T3StreamEvent::Shell(event), &ctx).await;
            }
        }
        let events = drain(&mut rx);
        assert!(!events.is_empty());
        assert!(events
            .iter()
            .all(|e| e.event_name() == "agent.session.updated"));
        for event in &events {
            let AgentDomainEvent::SessionUpdated { info, .. } = event else {
                unreachable!()
            };
            assert_eq!(info.agent_id, "t3");
            assert!(info.directory.is_some(), "the project root is known");
        }

        // A removed thread the mirror holds is announced deleted and forgotten.
        let asid = events[0].asid().clone();
        handle_t3_event(
            T3StreamEvent::Shell(ShellEvent::ThreadRemoved {
                sequence: 1,
                thread_id: asid.0.clone(),
            }),
            &ctx,
        )
        .await;
        let events = drain(&mut rx);
        assert!(
            matches!(&events[..], [AgentDomainEvent::SessionUpdated { info, .. }] if info.deleted)
        );
        assert!(ctx.mirror.get_snapshot(&asid).await.is_none());
    }

    #[tokio::test]
    async fn a_revert_and_a_lost_socket_reach_the_app() {
        let (ctx, mut rx) = ctx();
        let asid = AgentSessionId("t".into());
        handle_thread(
            &asid,
            ThreadEvent::Reverted {
                sequence: 5,
                turn_count: 0,
            },
            &ctx,
        )
        .await;
        let events = drain(&mut rx);
        assert!(matches!(
            &events[0],
            AgentDomainEvent::RevertChanged {
                state: RevertState::Committed,
                ..
            }
        ));
        // Nothing to re-read from here, so the session is resynced instead.
        assert!(matches!(&events[1], AgentDomainEvent::Resync { asid: a, .. } if a == &asid));

        handle_t3_event(
            T3StreamEvent::Disconnected {
                reason: "gone".into(),
            },
            &ctx,
        )
        .await;
        assert!(matches!(
            &drain(&mut rx)[..],
            [AgentDomainEvent::Resync { asid, .. }] if asid.0.is_empty()
        ));
    }

    #[tokio::test]
    async fn a_user_input_request_is_a_form_until_answered() {
        let (ctx, mut rx) = ctx();
        let asid = AgentSessionId("t".into());
        let requested = json!({"id": "a1", "kind": "user-input.requested", "summary": "Question",
            "payload": {"requestId": "r1", "questions": [{"id": "q", "question": "Which?", "options": []}]},
            "turnId": "turn", "createdAt": "2026-09-29T08:35:04.754Z"});
        let resolved = json!({"id": "a2", "kind": "user-input.resolved", "summary": "Answered",
            "payload": {"requestId": "r1", "answers": {"q": "x"}},
            "turnId": "turn", "createdAt": "2026-09-29T08:35:05.754Z"});
        for (sequence, activity) in [(1, requested), (2, resolved)] {
            handle_thread(
                &asid,
                ThreadEvent::ActivityAppended { sequence, activity },
                &ctx,
            )
            .await;
        }
        assert_eq!(
            names(&drain(&mut rx)),
            [
                "agent.form.pending",
                "agent.timeline.upsert",
                "agent.form.resolved",
                "agent.timeline.removed"
            ]
        );
        let snapshot = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert!(snapshot.forms.is_empty());
        assert!(snapshot.timeline.is_empty());
    }
}
