//! T3 V2 shell metadata and ordered projections folded into neutral domain events.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{broadcast, Mutex};

use crate::agents::adapters::memory_mirror::MemoryMirror;
use crate::agents::adapters::t3::mapper;
use crate::agents::adapters::t3::stream::{ShellEvent, ThreadEvent, ThreadWatcher};
use crate::agents::adapters::t3::v2;
use crate::agents::adapters::t3::{T3Driver, T3StreamEvent};
use crate::agents::domain::{
    AgentDomainEvent, AgentPart, AgentSessionId, AgentSessionStatus, TimelineItem,
};
use crate::agents::ports::mirror::SessionMirrorPort;

pub(crate) struct T3EventContext {
    pub driver: Arc<T3Driver>,
    pub mirror: Arc<MemoryMirror>,
    pub tx: broadcast::Sender<AgentDomainEvent>,
    watcher: ThreadWatcher,
    /// `projectId -> workspaceRoot`, from the shell stream, for the session
    /// rows' directory.
    roots: Mutex<HashMap<String, String>>,
    repository_identities: Mutex<HashMap<String, Value>>,
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
            repository_identities: Mutex::new(HashMap::new()),
        }
    }

    fn emit(&self, mut event: AgentDomainEvent) {
        self.mirror.stamp_event(&mut event);
        // A send with no subscribers is not a failure: nobody is watching.
        let _ = self.tx.send(event);
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
        let mut info = { v2::mapper::session(thread, None, &roots)? };
        let asid = info.asid.clone();
        if info.deleted {
            self.forget_session(&asid).await;
            return Some(asid);
        }
        let seq = self.mirror.update_session(info.clone()).await;
        self.mirror.overlay_read_markers(&mut info).await;
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
        self.driver.client().forget_timeline(&asid.0).await;

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
            *ctx.repository_identities.lock().await = v2::projection::rows(&snapshot, "projects")
                .iter()
                .filter_map(|project| {
                    Some((
                        project["id"].as_str()?.to_string(),
                        project.get("repositoryIdentity")?.clone(),
                    ))
                })
                .collect();
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
                let busy = { v2::mapper::shell_status(thread) } == AgentSessionStatus::Busy;
                if held || busy {
                    ctx.update_session(thread).await;
                }
                if busy {
                    ctx.watcher.watch(id);
                }
            }
        }
        ShellEvent::Enrichment {
            snapshot,
            resolved_roots,
        } => {
            let resolved_roots: HashSet<_> = resolved_roots.into_iter().collect();
            let roots = ctx.roots.lock().await;
            let mut repositories = ctx.repository_identities.lock().await;
            for project in v2::projection::rows(&snapshot, "projects") {
                let (Some(id), Some(root), Some(repository)) = (
                    project["id"].as_str(),
                    project["workspaceRoot"].as_str(),
                    project.get("repositoryIdentity"),
                ) else {
                    continue;
                };
                // Never resurrect removed project IDs or install the stale
                // thread rows/high-water mark carried by an enrichment frame.
                if roots.get(id).is_some_and(|held| held == root) && resolved_roots.contains(root) {
                    repositories.insert(id.into(), repository.clone());
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
                if let Some(repository) = project.get("repositoryIdentity") {
                    ctx.repository_identities
                        .lock()
                        .await
                        .insert(id.into(), repository.clone());
                }
            }
        }
        ShellEvent::ProjectRemoved { project_id, .. } => {
            ctx.roots.lock().await.remove(&project_id);
            ctx.repository_identities.lock().await.remove(&project_id);
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
                if ({ v2::mapper::shell_status(&thread) }) == AgentSessionStatus::Busy {
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
        ThreadEvent::V2Snapshot(snapshot) => apply_v2_snapshot(asid, &snapshot, ctx).await,
        ThreadEvent::Deleted { .. } => ctx.forget_session(asid).await,
    }
}

async fn apply_v2_snapshot(asid: &AgentSessionId, snapshot: &Value, ctx: &T3EventContext) {
    let gate = ctx.mirror.timeline_commit_gate(asid).await;
    let _acceptance = gate.lock().await;
    let snapshot = match ctx
        .driver
        .client()
        .order_live_snapshot(&asid.0, snapshot.clone())
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::debug!(%error,"V2 timeline ordering requires resync");
            ctx.emit(AgentDomainEvent::Resync {
                asid: asid.clone(),
                reason: "timeline_order_unresolved".into(),
            });
            return;
        }
    };
    let snapshot = &snapshot;
    if snapshot["gatewayRollbackRefresh"] == true
        && ctx.driver.client().mirrored_sequence(&asid.0)
            < snapshot["snapshotSequence"].as_u64().unwrap_or(0)
        && ctx
            .mirror
            .get_snapshot(asid)
            .await
            .is_some_and(|held| !held.timeline.is_empty())
    {
        ctx.driver
            .client()
            .require_timeline_refresh(&asid.0, snapshot["snapshotSequence"].as_u64().unwrap_or(0));
        ctx.emit(AgentDomainEvent::Resync {
            asid: asid.clone(),
            reason: "timeline_order_unresolved".into(),
        });
        return;
    }
    if snapshot["gatewayOrderCommitted"] != true
        && ctx
            .mirror
            .get_snapshot(asid)
            .await
            .is_some_and(|held| !held.timeline.is_empty())
    {
        // LRU replacement must not relabel a reader's retained window. A
        // user refresh accepts the new incarnation under the mirror gate.
        ctx.emit(AgentDomainEvent::Resync {
            asid: asid.clone(),
            reason: "timeline_order_unresolved".into(),
        });
        return;
    }
    let projection = &snapshot["projection"];
    let roots = ctx.roots.lock().await.clone();
    let Some(mut info) = v2::mapper::session(&projection["thread"], Some(projection), &roots)
    else {
        return;
    };
    if info.deleted {
        ctx.forget_session(asid).await;
        return;
    }
    let seq = ctx.mirror.update_session(info.clone()).await;
    let status = info.status;
    let error = info.error.clone();
    ctx.mirror.overlay_read_markers(&mut info).await;
    ctx.emit(AgentDomainEvent::SessionUpdated {
        asid: asid.clone(),
        info: Box::new(info),
        seq,
    });
    // A V2 run projection is authoritative execution state. Generic metadata
    // merging preserves Busy against Idle reads, so publish the transition too.
    if let Some(seq) = ctx.mirror.update_status(asid, status, error.clone()).await {
        ctx.emit(AgentDomainEvent::StatusChanged {
            asid: asid.clone(),
            status,
            error,
            seq,
        });
    }
    let timeline = v2::mapper::timeline(
        &asid.0,
        projection,
        snapshot["snapshotSequence"].as_u64().unwrap_or(0),
    );
    let permissions = v2::mapper::permissions(&asid.0, projection);
    let forms = v2::mapper::forms(&asid.0, projection);
    match v2::inbox::items(projection) {
        Ok(items) if ctx.mirror.inbox(asid).await != items => {
            let (seq, items) = ctx.mirror.set_inbox(asid, items).await;
            ctx.emit(AgentDomainEvent::InboxChanged {
                asid: asid.clone(),
                items,
                seq,
            });
        }
        Ok(_) => {}
        Err(_) => ctx.emit(AgentDomainEvent::Resync {
            asid: asid.clone(),
            reason: "t3_inbox_projection_invalid".into(),
        }),
    }
    if let Some(held) = ctx.mirror.get_snapshot(asid).await {
        let complete = snapshot["gatewayCompleteCoverage"].as_bool() == Some(true);
        let fresh: HashSet<_> = timeline.iter().map(|row| row.id.as_str()).collect();
        let stale: Vec<_> = held
            .timeline
            .iter()
            .filter(|row| {
                (complete && !fresh.contains(row.id.as_str()))
                    || match &row.part {
                        AgentPart::Approval { request } => {
                            !permissions.iter().any(|open| open.id == request.id)
                        }
                        AgentPart::Form { request } => {
                            !forms.iter().any(|open| open.id == request.id)
                        }
                        _ => false,
                    }
            })
            .map(|row| row.id.clone())
            .collect();
        ctx.remove_items(asid, &stale).await;
        for request in &held.permissions {
            if !permissions.iter().any(|open| open.id == request.id) {
                let seq = ctx.mirror.resolve_permission(asid, &request.id).await;
                ctx.emit(AgentDomainEvent::PermissionResolved {
                    asid: asid.clone(),
                    request_id: request.id.clone(),
                    seq,
                });
            }
        }
        for form in &held.forms {
            if !forms.iter().any(|open| open.id == form.id) {
                let seq = ctx.mirror.resolve_form(asid, &form.id).await;
                ctx.emit(AgentDomainEvent::FormResolved {
                    reason: None,
                    asid: asid.clone(),
                    form_id: form.id.clone(),
                    seq,
                });
            }
        }
    }
    ctx.upsert_items(asid, timeline).await;
    if let Some(generation) = snapshot["gatewayTimelineGeneration"].as_str() {
        ctx.driver.client().accept_live_generation(
            &asid.0,
            generation,
            snapshot["snapshotSequence"].as_u64().unwrap_or(0),
        );
    }
    for request in permissions {
        let seq = ctx.mirror.add_permission(request.clone()).await;
        ctx.emit(AgentDomainEvent::PermissionPending {
            asid: asid.clone(),
            request,
            seq,
        });
    }
    for request in forms {
        let seq = ctx.mirror.add_form(request.clone()).await;
        ctx.emit(AgentDomainEvent::FormPending {
            asid: asid.clone(),
            request,
            seq,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::agents::adapters::t3::{T3Credential, T3Endpoint, T3StreamListener};

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

    #[tokio::test]
    async fn completed_v2_run_clears_live_busy_state() {
        let (ctx, mut rx) = ctx();
        let asid = AgentSessionId("thread".into());
        let mut snapshot: Value =
            serde_json::from_str(include_str!("../adapters/t3/fixtures/v2/projection.json"))
                .unwrap();
        apply_v2_snapshot(&asid, &snapshot, &ctx).await;
        assert_eq!(
            ctx.current_status(&asid).await,
            Some(AgentSessionStatus::Busy)
        );
        drain(&mut rx);
        snapshot["snapshotSequence"] = json!(101);
        snapshot["projection"]["runs"][0]["status"] = json!("completed");
        snapshot["projection"]["runtimeRequests"] = json!([]);
        apply_v2_snapshot(&asid, &snapshot, &ctx).await;
        assert_eq!(
            ctx.current_status(&asid).await,
            Some(AgentSessionStatus::Idle)
        );
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AgentDomainEvent::StatusChanged {
                status: AgentSessionStatus::Idle,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn v2_window_preserves_old_rows_and_complete_post_rollback_read_removes_them() {
        let (ctx, mut rx) = ctx();
        let asid = AgentSessionId("thread".into());
        let mut snapshot: Value =
            serde_json::from_str(include_str!("../adapters/t3/fixtures/v2/projection.json"))
                .unwrap();
        apply_v2_snapshot(&asid, &snapshot, &ctx).await;
        drain(&mut rx);
        snapshot["projection"]["visibleTurnItems"]
            .as_array_mut()
            .unwrap()
            .pop();
        snapshot["projection"]["visibleTurnItems"][0]["item"]["text"] = json!("whole replacement");
        apply_v2_snapshot(&asid, &snapshot, &ctx).await;
        let held = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(
            held.timeline.len(),
            2,
            "bounded coverage must not delete history"
        );
        assert!(
            matches!(&held.timeline[1].part,AgentPart::Text{text} if text=="whole replacement")
        );
        // V2 marks runs/nodes rolled_back and removes their turn items; it
        // does not invent a rolled_back TurnItemStatus. The listener performs
        // a complete historical read after that confirmed run event.
        snapshot["projection"]["runs"][0]["status"] = json!("rolled_back");
        snapshot["projection"]["visibleTurnItems"] = json!([]);
        snapshot["hasMoreHistory"] = json!(false);
        apply_v2_snapshot(&asid, &snapshot, &ctx).await;
        assert!(ctx
            .mirror
            .get_snapshot(&asid)
            .await
            .unwrap()
            .timeline
            .is_empty());
    }

    #[tokio::test]
    async fn v2_control_state_resolves_requests_even_when_history_is_windowed() {
        let (ctx, mut rx) = ctx();
        let asid = AgentSessionId("thread".into());
        let mut snapshot: Value =
            serde_json::from_str(include_str!("../adapters/t3/fixtures/v2/projection.json"))
                .unwrap();
        apply_v2_snapshot(&asid, &snapshot, &ctx).await;
        assert_eq!(
            ctx.mirror
                .get_snapshot(&asid)
                .await
                .unwrap()
                .permissions
                .len(),
            1
        );
        assert_eq!(ctx.mirror.get_snapshot(&asid).await.unwrap().forms.len(), 1);
        drain(&mut rx);
        snapshot["projection"]["runtimeRequests"][0]["status"] = json!("expired");
        snapshot["projection"]["runtimeRequests"][1]["status"] = json!("resolved");
        apply_v2_snapshot(&asid, &snapshot, &ctx).await;
        let held = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert!(held.permissions.is_empty());
        assert!(held.forms.is_empty());
        let events = drain(&mut rx);
        assert!(names(&events).contains(&"agent.permission.resolved"));
        assert!(names(&events).contains(&"agent.form.resolved"));
    }

    #[tokio::test]
    async fn repository_enrichment_changes_only_selected_existing_repository_metadata() {
        let (ctx, mut rx) = ctx();
        handle_shell(ShellEvent::Snapshot(json!({"snapshotSequence":10,"threads":[],"projects":[
            {"id":"alpha","workspaceRoot":"/workspace/alpha","repositoryIdentity":{"value":"old"}},
            {"id":"other","workspaceRoot":"/workspace/other","repositoryIdentity":{"value":"keep"}},
            {"id":"removed","workspaceRoot":"/workspace/removed","repositoryIdentity":{"value":"old removed"}}
        ]})),&ctx).await;
        handle_shell(
            ShellEvent::ProjectRemoved {
                sequence: 11,
                project_id: "removed".into(),
            },
            &ctx,
        )
        .await;
        let before = ctx.roots.lock().await.clone();
        handle_shell(ShellEvent::Enrichment{resolved_roots:vec!["/workspace/alpha".into(),"/workspace/removed".into()],snapshot:json!({"snapshotSequence":12,
            "projects":[{"id":"alpha","workspaceRoot":"/workspace/alpha","repositoryIdentity":{"value":"resolved"},"title":"stale title"},{"id":"removed","workspaceRoot":"/workspace/removed","repositoryIdentity":{"value":"stale"}}],
            "threads":[{"id":"stale-thread","title":"must not be installed"}]
        })},&ctx).await;
        assert_eq!(
            *ctx.roots.lock().await,
            before,
            "absent project roots must remain"
        );
        let repositories = ctx.repository_identities.lock().await;
        assert_eq!(repositories["alpha"], json!({"value":"resolved"}));
        assert_eq!(repositories["other"], json!({"value":"keep"}));
        assert!(!repositories.contains_key("removed"));
        assert!(ctx
            .mirror
            .get_snapshot(&AgentSessionId("stale-thread".into()))
            .await
            .is_none());
        assert!(drain(&mut rx).is_empty());
    }

    fn v2_window(ids: &[&str], sequence: u64, complete: bool) -> Value {
        let mut snapshot: Value =
            serde_json::from_str(include_str!("../adapters/t3/fixtures/v2/projection.json"))
                .unwrap();
        snapshot["snapshotSequence"] = json!(sequence);
        snapshot["hasMoreHistory"] = json!(!complete);
        let sample = snapshot["projection"]["visibleTurnItems"][0].clone();
        snapshot["projection"]["visibleTurnItems"] = Value::Array(
            ids.iter()
                .enumerate()
                .map(|(position, id)| {
                    let mut row = sample.clone();
                    row["position"] = json!(position);
                    row["sourceThreadId"] = json!("thread");
                    row["sourceItemId"] = json!(id);
                    row["item"]["id"] = json!(id);
                    row
                })
                .collect(),
        );
        snapshot
    }

    #[tokio::test]
    async fn page_local_window_refresh_preserves_chronological_mirror_history() {
        let (ctx, _rx) = ctx();
        let asid = AgentSessionId("thread".into());
        apply_v2_snapshot(&asid, &v2_window(&["C", "D"], 10, false), &ctx).await;
        apply_v2_snapshot(&asid, &v2_window(&["A", "B", "C", "D"], 10, true), &ctx).await;
        apply_v2_snapshot(&asid, &v2_window(&["D", "E"], 20, false), &ctx).await;
        let held = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(
            held.timeline
                .iter()
                .map(|row| row.id.rsplit(':').next().unwrap())
                .collect::<Vec<_>>(),
            ["A", "B", "C", "D", "E"]
        );
    }

    #[tokio::test]
    async fn a_replaced_order_cache_requires_refresh_without_reordering_the_reader() {
        let (ctx, mut rx) = ctx();
        let asid = AgentSessionId("thread".into());
        apply_v2_snapshot(&asid, &v2_window(&["A", "B"], 10, false), &ctx).await;
        let before = ctx.mirror.get_snapshot(&asid).await.unwrap();
        drain(&mut rx);
        // Same boundary as an inactive LRU eviction: the mirror stays visible.
        ctx.driver.client().forget_timeline("thread").await;
        apply_v2_snapshot(&asid, &v2_window(&["B", "C"], 20, false), &ctx).await;
        let after = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(
            after
                .timeline
                .iter()
                .map(|row| (&row.id, &row.message_id))
                .collect::<Vec<_>>(),
            before
                .timeline
                .iter()
                .map(|row| (&row.id, &row.message_id))
                .collect::<Vec<_>>()
        );
        assert!(drain(&mut rx).iter().any(|event| matches!(event, AgentDomainEvent::Resync { reason, .. } if reason == "timeline_order_unresolved")));
    }

    #[tokio::test]
    async fn complete_rollback_then_late_history_cannot_restore_rows_or_delete_new_live_rows() {
        let (ctx, mut rx) = ctx();
        let asid = AgentSessionId("thread".into());
        apply_v2_snapshot(&asid, &v2_window(&["A", "B"], 10, false), &ctx).await;
        apply_v2_snapshot(&asid, &v2_window(&["A"], 20, true), &ctx).await;
        let late = ctx
            .driver
            .client()
            .order_snapshot(&asid.0, v2_window(&["A", "B"], 10, true))
            .await
            .unwrap();
        assert_eq!(late["gatewayCompleteCoverage"], false);
        let late_rows = v2::mapper::timeline(&asid.0, &late["projection"], 10);
        ctx.upsert_items(&asid, late_rows).await;
        let held = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(
            held.timeline
                .iter()
                .map(|row| row.id.rsplit(':').next().unwrap())
                .collect::<Vec<_>>(),
            ["A"]
        );
        let mut live = v2_window(&["A", "C"], 21, false);
        live["projection"]["thread"]["title"] = json!("fresh live title");
        apply_v2_snapshot(&asid, &live, &ctx).await;
        drain(&mut rx);
        let mut stale = v2_window(&["A", "B"], 10, true);
        stale["projection"]["thread"]["title"] = json!("stale title");
        apply_v2_snapshot(&asid, &stale, &ctx).await;
        let held = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(held.info.title, "fresh live title");
        assert_eq!(
            held.timeline
                .iter()
                .map(|row| row.id.rsplit(':').next().unwrap())
                .collect::<Vec<_>>(),
            ["A", "C"]
        );
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, AgentDomainEvent::Resync { .. })),
            "stale full snapshots must not emit control replacement or timeline deletion"
        );
    }

    #[tokio::test]
    async fn shell_metadata_broadcast_preserves_gateway_read_marker() {
        let (ctx, mut rx) = ctx();
        let detail: Value =
            serde_json::from_str(include_str!("../adapters/t3/fixtures/thread_detail.json"))
                .unwrap();
        let thread = &detail["thread"];
        let asid = ctx.update_session(thread).await.unwrap();
        drain(&mut rx);
        ctx.mirror.mark_viewed(&asid, u64::MAX).await.unwrap();
        ctx.update_session(thread).await.unwrap();
        let events = drain(&mut rx);
        assert!(
            matches!(&events[..], [AgentDomainEvent::SessionUpdated { info, .. }]
            if info.time_viewed == Some(u64::MAX))
        );
    }
}
